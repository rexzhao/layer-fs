//! The collector, end to end: open exclusively, mark, plan, apply (ADR-008 §6).
//!
//! The data flow is one direction and each step is somebody else's job: the maintenance handle owns
//! the locks, the root source says what must survive, the object source serves bytes over the same
//! connection, the marker records what is reachable, the plan decides what is not, and the apply moves
//! it. Nothing reaches around anything — which is what makes `--dry-run` an honest mode instead of a
//! flag threaded through the destructive code.

use std::path::Path;

use crate::error::WorkspaceResult;
use crate::gc::apply::{apply, Applied};
use crate::gc::mark::{mark_roots, Marker};
use crate::gc::roots::{MaintenanceRoots, RootSet, RootSource};
use crate::gc::source::MaintenanceSource;
use crate::gc::sweep::{plan, SweepPlan};
use crate::gc::{GcHooks, GcReport};
use crate::repo::MaintenanceRepo;

/// How a run is configured.
#[derive(Debug, Clone, Copy)]
pub struct GcOptions {
    /// Compute and report, change nothing. The marker is still created and removed (it is the
    /// collector's own scratch space); no object, pack or index row moves.
    pub dry_run: bool,
    /// `gc_repack_threshold`: a pack whose survivor ratio is **strictly below** this is copy-repacked.
    /// Must be inside `[0, 1]`.
    pub repack_threshold: f64,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            repack_threshold: 0.5,
        }
    }
}

/// Everything one run produced.
#[derive(Debug)]
pub struct GcOutcome {
    /// The facts the criteria read.
    pub report: GcReport,
    /// What the sweep decided — present for a dry run too, because deciding is what a dry run is for.
    pub plan: SweepPlan,
    /// What an apply actually did; `None` for a dry run.
    pub applied: Option<Applied>,
}

/// Runs the collector over the store at `store_dir`.
///
/// The maintenance handle is created and dropped inside this call: a handle that outlives the run
/// would keep the store locked and make the *next* `Repo::open` fail for no reason the caller can see
/// (ADR-008 §6.1).
///
/// # Errors
///
/// [`crate::WorkspaceError::StoreInUse`] when a writer or an open overlay holds the store,
/// [`crate::WorkspaceError::Snapshot`] when an object cannot be read (the run aborts and nothing is
/// deleted), or a SQLite/IO failure.
pub fn run(store_dir: impl AsRef<Path>, options: GcOptions) -> WorkspaceResult<GcOutcome> {
    let store_dir = store_dir.as_ref();
    let mut maintenance = MaintenanceRepo::open(store_dir)?;

    let roots = MaintenanceRoots {
        maintenance: &maintenance,
    }
    .roots()?;
    let declared = RootSet::from_entries(roots.entries().to_vec());

    let mut marker = Marker::create_in(store_dir)?;
    let mut hooks = GcHooks::default();
    let mut report = GcReport::skeleton(&declared);

    // Mark root by root: a root names its own profile, and one store may hold roots written under
    // different ones (ADR-007 §7). The marker is shared, so a subtree reached twice is walked once.
    let mut marked_from: Vec<cas_core::ObjectId> = Vec::new();
    for entry in declared.entries() {
        if marked_from.contains(&entry.root) {
            // P2b-6 §29: the *tree* has already been walked, but this **label** has not been recorded —
            // and G2 compares facts (labels), not object ids. Skipping the record here made two labels on
            // one tree look exactly like a missed root, which is the failure G2 exists to catch. Marking
            // stays deduplicated by oid (§13.1's "do not walk a shared subtree twice"); only the
            // bookkeeping is per label.
            report.record_visit(entry);
            if let Some(sink) = hooks.on_root_visited.as_mut() {
                sink(entry.kind, entry.root);
            }
            continue;
        }
        marked_from.push(entry.root);
        let single = RootSet::from_entries(vec![entry.clone()]);
        let source = MaintenanceSource::new(maintenance.connection(), store_dir);
        let reader = fs_snapshot::SnapshotReader::open_any(&source, entry.root)?;
        let partial = mark_roots(&declared, &single, &reader, &mut marker, &mut hooks)?;
        for label in partial.roots_visited {
            if !report.roots_visited.contains(&label) {
                report.roots_visited.push(label);
            }
        }
        for oid in partial.visited_oids {
            if !report.visited_oids.contains(&oid) {
                report.visited_oids.push(oid);
            }
        }
        for (kind, count) in partial.visited_by_kind {
            *report.visited_by_kind.entry(kind).or_insert(0) += count;
        }
    }

    // Point 1: roots have been visited, expansion may still be running.
    crate::gc::crash::reach(crate::gc::GcStep::Marking)?;

    report.reachable = marker.all()?;
    report.reachable_count = report.reachable.len();
    report.marked_objects = report.reachable.len();

    let decided = plan(
        maintenance.connection(),
        &marker,
        store_dir,
        options.repack_threshold,
    )?;
    report.freed_object_bytes = decided.freed_object_bytes;
    report.packs_repacked = decided.packs_repack.len();
    report.packs_deleted = decided.packs_delete.len();
    // Dead frames that never had a row of their own: what repacking reclaims, counted apart from the
    // objects it does not delete (P2b-6 §15).
    report.reclaimed_dead_bytes = decided
        .packs_repack
        .iter()
        .map(|pack| pack.objects.saturating_sub(pack.survivors))
        .sum();

    // K8a: a `pack` row whose file is gone is a **finding**, never something to repair by deleting the
    // row. A real run refuses before touching anything; a dry run says so and continues, because
    // "tell me what you would do" must still be answerable on a damaged store.
    if !options.dry_run && !decided.missing_packs.is_empty() {
        let pack_id = decided.missing_packs[0].clone();
        return Err(crate::WorkspaceError::Store(
            cas_store::StoreError::MissingPack { pack_id },
        ));
    }

    let applied = if options.dry_run {
        None
    } else {
        let applied = apply(maintenance.connection_mut(), &decided, store_dir)?;
        report.freed = decided.dead_objects.clone();
        Some(applied)
    };

    marker.remove()?;
    drop(maintenance);
    Ok(GcOutcome {
        report,
        plan: decided,
        applied,
    })
}
