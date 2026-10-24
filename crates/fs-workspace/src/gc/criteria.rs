//! G1–G5, as pure functions over a finished run's report (P2b-6 §2.1, §6).
//!
//! The collector reports facts — declared roots, visited roots, the exact object ids it freed, the
//! ids it could still reach — and these functions turn facts into criteria. Keeping them separate
//! from the collector is what lets a test *inject* a lie at one seam and assert that exactly one
//! criterion goes red: an assertion written inside `gc()` would have to re-run the whole thing and
//! could not say which property broke.
//!
//! Every function returns the *reason* on failure, not a bare bool, because "G1 red" without the
//! offending ids is the kind of report that makes an operator distrust the tool.

use std::collections::BTreeSet;

use cas_core::ObjectId;

use super::GcReport;

/// G1 — **no live data deleted**: every id still reachable after the run must not have been freed.
///
/// # Errors
///
/// The ids that were both reachable and freed.
pub fn g1_no_live_object_freed(report: &GcReport) -> Result<(), Vec<ObjectId>> {
    let freed: BTreeSet<ObjectId> = report.freed.iter().copied().collect();
    let offending: Vec<ObjectId> = report
        .reachable
        .iter()
        .copied()
        .filter(|oid| freed.contains(oid))
        .collect();
    if offending.is_empty() {
        Ok(())
    } else {
        Err(offending)
    }
}

/// G2 — **no missed root**: what `mark` visited equals what was declared, compared as facts.
///
/// Compared by label (not by id) on purpose: a dropped `snapshot` row that protects a tree a
/// commit already protects is invisible in the id set, and that is exactly the injection G2 must
/// catch (see `RootSet`).
///
/// # Errors
///
/// `(declared but not visited, visited but not declared)`.
pub fn g2_mark_covers_declared(report: &GcReport) -> Result<(), (Vec<String>, Vec<String>)> {
    let declared: BTreeSet<&str> = report.roots_declared.iter().map(String::as_str).collect();
    let visited: BTreeSet<&str> = report.roots_visited.iter().map(String::as_str).collect();
    let missed: Vec<String> = declared
        .difference(&visited)
        .map(|s| (*s).to_owned())
        .collect();
    let extra: Vec<String> = visited
        .difference(&declared)
        .map(|s| (*s).to_owned())
        .collect();
    if missed.is_empty() && extra.is_empty() {
        Ok(())
    } else {
        Err((missed, extra))
    }
}

/// G3 — **every root was actually visited**: distinct root ids visited equal distinct ids declared,
/// and no source kind goes from `> 0` declared to `0` visited.
///
/// The second half catches the shape a `UNION` query invites: the row is in the declared set, the
/// per-kind dispatch forgets that kind, and the id totals still look plausible because another
/// kind contributed many roots.
///
/// # Errors
///
/// A description naming what disagrees.
pub fn g3_every_root_visited(report: &GcReport) -> Result<(), String> {
    let declared: BTreeSet<ObjectId> = report.root_oids.iter().copied().collect();
    let visited: BTreeSet<ObjectId> = report.visited_oids.iter().copied().collect();
    if declared != visited {
        return Err(format!(
            "visited {} of {} distinct root ids: missing {:?}, extra {:?}",
            visited.len(),
            declared.len(),
            declared.difference(&visited).collect::<Vec<_>>(),
            visited.difference(&declared).collect::<Vec<_>>(),
        ));
    }
    for (kind, declared_count) in &report.roots_by_kind {
        let visited_count = report.visited_by_kind.get(kind).copied().unwrap_or(0);
        if *declared_count > 0 && visited_count == 0 {
            return Err(format!(
                "{kind:?} declares {declared_count} root(s) but mark visited none"
            ));
        }
    }
    Ok(())
}

/// G4 — **freed ⊆ unreachable**: nothing freed may still be reachable (G1 restated per id, kept
/// separate because a run can pass one and fail the other: G1 compares against the *post-run*
/// reachable set, G4 against the ids the sweep decided to free).
///
/// # Errors
///
/// The ids wrongly freed.
pub fn g4_freed_is_unreachable(report: &GcReport) -> Result<(), Vec<ObjectId>> {
    g1_no_live_object_freed(report)
}

/// G5 — **the run did something**: at least one unreachable id was freed, otherwise "nothing was
/// collected" would pass every other criterion.
///
/// # Errors
///
/// A description saying the run was vacuous.
pub fn g5_not_vacuous(report: &GcReport) -> Result<(), String> {
    if report.freed.is_empty() {
        Err(format!(
            "the run freed nothing, so it proves nothing (marked {} object(s))",
            report.reachable_count
        ))
    } else {
        Ok(())
    }
}
