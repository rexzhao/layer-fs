//! Sweep: decide what is garbage, then act (ADR-008 §6.3, P2b-6 §18.3).
//!
//! This module is split in two on purpose. [`plan`] decides and **writes nothing**; applying a plan
//! is a separate step. The split is what makes `--dry-run` honest — a dry run is "compute the plan and
//! stop", not "run the sweep with the deletes commented out" — and it is what lets the criteria be
//! computed from a plan before any byte moves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cas_core::ObjectId;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::gc::mark::Marker;

/// One pack the sweep has to do something about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackDecision {
    /// The pack's file name.
    pub pack_id: String,
    /// How many of its frames are still reachable.
    pub survivors: u64,
    /// How many frames the index says it holds.
    pub objects: u64,
    /// The pack's file name, as it appears on disk (`<pack_id>.pack`).
    pub file_name: String,
    /// The file, when it exists.
    pub path: PathBuf,
}

/// What a sweep would do, in full, before anything is deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// Index rows whose object is unreachable, in index order.
    pub dead_objects: Vec<ObjectId>,
    /// Sum of `length` over [`SweepPlan::dead_objects`] — the "objects deleted" part of the byte
    /// accounting, kept apart from the bytes repacking reclaims (P2b-6 §15).
    pub freed_object_bytes: u64,
    /// Packs with no survivors: delete the rows, then the file.
    pub packs_delete: Vec<PackDecision>,
    /// Packs whose survivor ratio fell below the threshold: copy-repack.
    pub packs_repack: Vec<PackDecision>,
    /// Packs left alone.
    pub packs_keep: usize,
    /// Pack **rows** whose file is absent. Never "repaired" by deleting the row: a missing file is a
    /// finding, and deleting the row would hide it (K8a, P2b-6 §18.3).
    pub missing_packs: Vec<String>,
    /// Files under `packs/` that no row claims.
    pub orphan_pack_files: Vec<PathBuf>,
    /// Index rows examined, for a non-vacuous check.
    pub rows_checked: usize,
    /// Every object the marker called reachable (needed by the repack step to know what to copy).
    pub live_objects: Vec<ObjectId>,
}

impl SweepPlan {
    /// How many objects the sweep would delete.
    #[must_use]
    pub fn freed_objects(&self) -> usize {
        self.dead_objects.len()
    }

    /// The reachable ids as a set, for the repack step.
    #[must_use]
    pub fn reachable_ids(&self) -> std::collections::BTreeSet<ObjectId> {
        self.live_objects.iter().copied().collect()
    }

    /// Whether there is nothing to do at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dead_objects.is_empty()
            && self.packs_delete.is_empty()
            && self.packs_repack.is_empty()
            && self.orphan_pack_files.is_empty()
            && self.missing_packs.is_empty()
    }
}

/// Computes the plan for one store, against a marker.
///
/// `repack_threshold` is a **policy** value in `[0, 1]`: a pack is copy-repacked when
/// `survivors / objects` is strictly below it, and deleted when `survivors == 0`. A pack that is
/// missing its file is reported in [`SweepPlan::missing_packs`] and its rows are left untouched —
/// deleting them would turn "a file vanished" into "nothing to see here" (K8a).
///
/// # Errors
///
/// A SQLite failure, an index row that is not an object id, or a threshold outside `[0, 1]`.
pub fn plan(
    connection: &rusqlite::Connection,
    marker: &Marker,
    store_dir: &Path,
    repack_threshold: f64,
) -> WorkspaceResult<SweepPlan> {
    if !(0.0..=1.0).contains(&repack_threshold) {
        return Err(WorkspaceError::Inconsistent {
            detail: format!("gc_repack_threshold {repack_threshold} is outside [0, 1]"),
        });
    }
    let rows = cas_store::index::index_rows(connection)?;
    let packs = cas_store::index::pack_rows(connection)?;

    let mut out = SweepPlan {
        rows_checked: rows.len(),
        ..SweepPlan::default()
    };

    // Per-pack survivor counts, so a pack is judged once rather than per frame.
    let mut survivors: BTreeMap<String, u64> =
        packs.iter().map(|pack| (pack.pack_id.clone(), 0)).collect();

    for row in &rows {
        let oid = parse_oid(&row.oid)?;
        if marker.contains(oid)? {
            out.live_objects.push(oid);
            if row.kind == cas_store::index::kind_pack() {
                *survivors.entry(row.location.clone()).or_insert(0) += 1;
            }
            continue;
        }
        out.dead_objects.push(oid);
        out.freed_object_bytes = out.freed_object_bytes.saturating_add(row.length);
    }

    let packs_dir = store_dir.join("packs");
    let mut claimed: Vec<String> = Vec::new();
    for pack in &packs {
        claimed.push(format!("{}.pack", pack.pack_id));
        // The row stores the id; the file carries a `.pack` suffix (ADR-008 section 3). Getting this
        // wrong makes every pack look missing *and* every file look orphaned, so it is named once.
        let file_name = format!("{}.pack", pack.pack_id);
        let path = packs_dir.join(&file_name);
        let live = survivors.get(&pack.pack_id).copied().unwrap_or(0);
        if !path.is_file() {
            // K8a: a row without a file is reported, never "fixed" by dropping the row.
            out.missing_packs.push(pack.pack_id.clone());
            continue;
        }
        let decision = PackDecision {
            pack_id: pack.pack_id.clone(),
            file_name,
            survivors: live,
            objects: pack.objects,
            path,
        };
        if live == 0 {
            out.packs_delete.push(decision);
        } else if pack.objects > 0 && ratio(live, pack.objects) < repack_threshold {
            out.packs_repack.push(decision);
        } else {
            out.packs_keep += 1;
        }
    }

    // Files nobody claims are garbage by definition: no row can ever read them.
    if let Ok(entries) = std::fs::read_dir(&packs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_file() && !claimed.contains(&name) {
                out.orphan_pack_files.push(entry.path());
            }
        }
    }
    out.orphan_pack_files.sort();

    Ok(out)
}

fn ratio(live: u64, total: u64) -> f64 {
    // `total > 0` is checked by the caller; the cast is precise enough for a policy comparison and
    // the comparison is strict (`<`), so an exact-threshold pack is kept.
    (live as f64) / (total as f64)
}

fn parse_oid(text: &str) -> WorkspaceResult<ObjectId> {
    text.parse::<ObjectId>()
        .map_err(|_| WorkspaceError::Inconsistent {
            detail: format!("an index row names {text:?}, which is not an object id"),
        })
}
