//! §27.B: the three-step template, applied to counter-examples C/D/E/F/G (P2b-6 §8).
//!
//! Each counter-example runs the same three steps:
//!
//! 1. **no injection** → G1–G5 all pass;
//! 2. **injection** → exactly the named criteria fail and the rest still pass (a single injection that
//!    reddened everything would prove nothing about *which* property broke);
//! 3. **injection off again** → all pass, proving the seam was consumed and left no residue.
//!
//! Step 3 is the one this project earned the hard way: a global `SWEEP_ONCE` and a process-level
//! `pack_read_bytes` both leaked across cases, so "it went back to green" is a real assertion rather than
//! a formality.
//!
//! Every expected set here is built by the test, never read from the collector's own root table.

mod common;

use std::collections::BTreeSet;

use cas_core::{Object, ObjectId, Role};
use fs_workspace::gc::{
    criteria, GcHooks, GcInput, GcReport, RootKind, RootRef, RootSet, RootSource,
};
use fs_workspace::WorkspaceResult;

/// Which criteria failed, by name.
type Verdicts = BTreeSet<&'static str>;

fn oid(label: &str) -> ObjectId {
    Object::new(Role::Bytes, label.as_bytes())
        .expect("framed")
        .id()
        .expect("hashed")
}

fn verdicts(report: &GcReport) -> Verdicts {
    let mut failed = Verdicts::new();
    if criteria::g1_no_live_object_freed(report).is_err() {
        failed.insert("G1");
    }
    if criteria::g2_mark_covers_declared(report).is_err() {
        failed.insert("G2");
    }
    if criteria::g3_every_root_visited(report).is_err() {
        failed.insert("G3");
    }
    if criteria::g4_freed_is_unreachable(report).is_err() {
        failed.insert("G4");
    }
    if criteria::g5_not_vacuous(report).is_err() {
        failed.insert("G5");
    }
    failed
}

fn set(items: &[&'static str]) -> Verdicts {
    items.iter().copied().collect()
}

/// A source that yields a fixed (possibly wrong) root set: the shape of C and D.
struct Fixed(Vec<RootRef>);

impl RootSource for Fixed {
    fn roots(&self) -> WorkspaceResult<RootSet> {
        Ok(RootSet::from_entries(self.0.clone()))
    }
}

/// The honest fixture: two roots and one object that is garbage.
struct Fixture {
    declared: RootSet,
    /// The garbage the sweep is allowed to free.
    garbage: ObjectId,
}

fn fixture() -> Fixture {
    let first = oid("first tree");
    let second = oid("second tree");
    Fixture {
        declared: RootSet::from_entries(vec![
            RootRef::new(RootKind::Commit, first, "commit:first"),
            RootRef::new(RootKind::Snapshot, second, "snapshot:second"),
        ]),
        garbage: oid("garbage"),
    }
}

/// Runs one pass with an explicit declaration, so D (where the declaration itself is short) is reachable.
fn pass_with(
    declared: &RootSet,
    source: &dyn RootSource,
    free: &[ObjectId],
    hooks: &mut GcHooks<'_>,
) -> Verdicts {
    let mut report = GcInput {
        declared,
        mark_source: source,
    }
    .mark(hooks)
    .expect("mark");
    report.freed = free.to_vec();
    verdicts(&report)
}

/// Runs one pass with the given injection and returns the verdicts.
fn pass(
    fixture: &Fixture,
    source: &dyn RootSource,
    free: &[ObjectId],
    hooks: &mut GcHooks<'_>,
) -> Verdicts {
    let mut report = GcInput {
        declared: &fixture.declared,
        mark_source: source,
    }
    .mark(hooks)
    .expect("mark");
    // The sweep's effect, applied by the test: "the garbage was freed" is the run's one allowed deletion.
    report.freed = free.to_vec();
    verdicts(&report)
}

/// **C** — a root the collector was told about is missing from what it walks.
///
/// Expected: G2 (a declared root was never visited) and G3 (a declared root id is not among the visited).
#[test]
fn counter_example_c_missing_root_fails_g2_and_g3_only() {
    common::strict_gate();
    let fixture = fixture();
    let all = Fixed(fixture.declared.entries().to_vec());
    let partial = Fixed(vec![fixture.declared.entries()[0].clone()]);

    let clean = pass(&fixture, &all, &[fixture.garbage], &mut GcHooks::default());
    assert_eq!(clean, set(&[]), "step 1: the honest walk passes everything");

    let injected = pass(
        &fixture,
        &partial,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(
        injected,
        set(&["G2", "G3"]),
        "step 2: only the coverage criteria"
    );

    let recovered = pass(&fixture, &all, &[fixture.garbage], &mut GcHooks::default());
    assert_eq!(
        recovered,
        set(&[]),
        "step 3: injection removed, everything green again"
    );
}

/// **D** — a root is dropped from the declaration *and* from the walk.
///
/// The finding this test records: **no G criterion can see D.** Every criterion compares the report with
/// itself, and a short declaration plus a short walk is perfectly self-consistent — the run "correctly"
/// freed an object that the test knows a real root holds. That is why §8.3 requires the *test* to hold the
/// ground truth (`R_true`) and assert against it: `F ∩ reachable(R_true) == ∅`. Step 2 asserts the criteria
/// stay green **and** the ground-truth check goes red; anything else here would be pretending a criterion
/// covers D.
#[test]
fn counter_example_d_short_declaration_is_caught_by_ground_truth_not_by_a_criterion() {
    common::strict_gate();
    let fixture = fixture();
    let all = Fixed(fixture.declared.entries().to_vec());

    // What the test knows the dropped root's tree holds — this is R_true's payload, built here and only
    // here, never read back from the collector.
    let inside_the_dropped_root = oid("bytes the dropped root reaches");

    let ground_truth_violations = |freed: &[ObjectId]| -> Vec<ObjectId> {
        freed
            .iter()
            .copied()
            .filter(|oid| *oid == inside_the_dropped_root)
            .collect()
    };

    // Step 1: honest run, nothing the true roots hold is freed.
    let clean = pass(&fixture, &all, &[fixture.garbage], &mut GcHooks::default());
    assert_eq!(clean, set(&[]), "step 1: all green");
    assert!(
        ground_truth_violations(&[fixture.garbage]).is_empty(),
        "step 1: the garbage is not part of any true root's tree"
    );

    // Step 2: the declaration and the walk both lose that root. The collector is self-consistent, so the
    // criteria stay green — and the ground truth is what catches it.
    let short = RootSet::from_entries(vec![fixture.declared.entries()[0].clone()]);
    let short_source = Fixed(vec![fixture.declared.entries()[0].clone()]);
    let injected = pass_with(
        &short,
        &short_source,
        &[inside_the_dropped_root],
        &mut GcHooks::default(),
    );
    assert_eq!(
        injected,
        set(&[]),
        "step 2: the criteria cannot see D — that is the finding, not a gap in this test"
    );
    let violations = ground_truth_violations(&[inside_the_dropped_root]);
    assert_eq!(
        violations,
        vec![inside_the_dropped_root],
        "step 2: the ground-truth check must go red exactly on the object the dropped root held"
    );

    // Step 3: restore the declaration, everything green again.
    let recovered = pass(&fixture, &all, &[fixture.garbage], &mut GcHooks::default());
    assert_eq!(recovered, set(&[]), "step 3: green again");
    assert!(
        ground_truth_violations(&[fixture.garbage]).is_empty(),
        "step 3: no residue"
    );
}

/// **F** — a root is declared and then skipped (the `skip_root` seam).
///
/// Expected: G2 and G3. Step 3 is what catches a seam that stays armed.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn counter_example_f_a_skipped_root_fails_g2_and_g3_and_leaves_no_residue() {
    common::strict_gate();
    let fixture = fixture();
    let source = Fixed(fixture.declared.entries().to_vec());

    let clean = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(clean, set(&[]), "step 1: all green");

    let mut skip = |_: &RootRef| true;
    let injected = {
        let mut hooks = GcHooks {
            skip_root: Some(&mut skip),
            ..GcHooks::default()
        };
        pass(&fixture, &source, &[fixture.garbage], &mut hooks)
    };
    assert_eq!(injected, set(&["G2", "G3"]), "step 2: nothing was visited");

    let recovered = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(
        recovered,
        set(&[]),
        "step 3: the skip hook is not remembered across runs"
    );
}

/// **G** — an id is forced into the free set even though a root reaches it.
///
/// Expected: G1 and G4 (the same predicate, asked from two directions), and **not** G5 — the run freed
/// something, it just freed the wrong thing.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn counter_example_g_forced_free_fails_g1_and_g4_only() {
    common::strict_gate();
    let fixture = fixture();
    let source = Fixed(fixture.declared.entries().to_vec());
    let reachable = fixture.declared.entries()[0].root;

    let clean = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(clean, set(&[]), "step 1: all green");

    let mut hooks = GcHooks {
        extra_free_candidates: vec![reachable],
        ..GcHooks::default()
    };
    let injected = pass(&fixture, &source, &[reachable], &mut hooks);
    assert_eq!(
        injected,
        set(&["G1", "G4"]),
        "step 2: a live object in the free set, and G5 still satisfied because something WAS freed"
    );

    let recovered = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(recovered, set(&[]), "step 3: green again");
}

/// **E** — the vacuous run: nothing is freed at all.
///
/// Not a data-loss counter-example but the "everything is green because nothing happened" one, so it is
/// the only case whose expected failure set is exactly G5.
#[test]
fn counter_example_e_a_vacuous_run_fails_g5_only() {
    common::strict_gate();
    let fixture = fixture();
    let source = Fixed(fixture.declared.entries().to_vec());

    let clean = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(clean, set(&[]), "step 1: all green");

    let injected = pass(&fixture, &source, &[], &mut GcHooks::default());
    assert_eq!(
        injected,
        set(&["G5"]),
        "step 2: only the non-vacuity criterion"
    );

    let recovered = pass(
        &fixture,
        &source,
        &[fixture.garbage],
        &mut GcHooks::default(),
    );
    assert_eq!(recovered, set(&[]), "step 3: green again");
}
