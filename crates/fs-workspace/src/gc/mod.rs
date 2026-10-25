//! Offline garbage collection (ADR-008 §6, P2b-6).
//!
//! The collector is **offline** (P5): `Repo::open_exclusive` takes the main database and every
//! overlay, and refuses rather than waiting. Inside that window nothing else can publish, which is
//! what makes "an object with no root is garbage" safe to act on.
//!
//! This module currently holds the piece the rest of the design hangs from: the root set and the
//! replaceable source it comes from ([`roots`]). Mark, sweep, repacking and the CLI add
//! themselves on top of it, and each one consumes [`roots::RootSet`] rather than re-querying the
//! database — a collector with two views of "what is live" cannot be tested for G2.

pub mod apply;
pub mod crash;
pub mod criteria;
pub mod mark;
pub mod roots;
pub mod run;
pub mod source;
pub mod sweep;

pub use apply::{apply, Applied};
pub use crash::GcStep;
pub use mark::{mark_roots, Marker};
pub use roots::{MaintenanceRoots, RepoRoots, RootKind, RootRef, RootRow, RootSet, RootSource};
pub use run::{run, GcOptions, GcOutcome};
pub use source::MaintenanceSource;
pub use sweep::{plan, PackDecision, SweepPlan};

use std::collections::BTreeMap;
use std::time::Duration;

use cas_core::ObjectId;

use crate::error::WorkspaceResult;

/// The facts one GC run reports (P2b-6 §6).
///
/// Everything the criteria need is a **field**, not a log line: G1–G5 are computed from this value
/// by [`criteria`], so a test can assert exactly one property without re-running the collector.
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    /// The labels of the roots the caller declared (G2's right-hand side).
    pub roots_declared: Vec<String>,
    /// The labels `mark` actually visited (G2's left-hand side).
    pub roots_visited: Vec<String>,
    /// Distinct root object ids declared.
    pub root_oids: Vec<ObjectId>,
    /// Distinct root object ids visited.
    pub visited_oids: Vec<ObjectId>,
    /// Every object id the sweep freed, **one entry per id** (G4's intersection, G5's witness).
    pub freed: Vec<ObjectId>,
    /// Objects still reachable after the run (G1: none of these may be in `freed`).
    pub reachable: Vec<ObjectId>,
    /// How many objects mark reached.
    pub reachable_count: usize,
    /// How many objects the marked set held. Separate from `reachable_count` because G5 asks "was
    /// anything marked?" and must not be answered by a byte count (P2b-6 §15).
    pub marked_objects: usize,
    /// Bytes of the objects that were deleted, summed from the index `length` (P2b-6 §15).
    pub freed_object_bytes: u64,
    /// Bytes reclaimed by repacking: dead frames that never had a row of their own. Kept apart from
    /// `freed_object_bytes` because "reclaimed 10 KiB" and "deleted 3 objects" are different facts.
    pub reclaimed_dead_bytes: u64,
    /// Packs rewritten because their survivor ratio fell below the threshold.
    pub packs_repacked: usize,
    /// Packs deleted because nothing in them survived.
    pub packs_deleted: usize,
    /// Wall-clock duration of the run.
    pub duration: Duration,
    /// Declared roots per source kind (G3's second half).
    pub roots_by_kind: BTreeMap<RootKind, usize>,
    /// Visited roots per source kind.
    pub visited_by_kind: BTreeMap<RootKind, usize>,
}

impl GcReport {
    /// Builds a report from the declared root set, before any mark or sweep has run.
    #[must_use]
    pub fn skeleton(declared: &RootSet) -> Self {
        let mut roots_by_kind = BTreeMap::new();
        for entry in declared.entries() {
            *roots_by_kind.entry(entry.kind).or_insert(0) += 1;
        }
        Self {
            roots_declared: declared.entries().iter().map(|e| e.label.clone()).collect(),
            root_oids: declared.by_root(),
            roots_by_kind,
            ..Self::default()
        }
    }

    /// Records one visited root (G2/G3 bookkeeping).
    pub fn record_visit(&mut self, entry: &RootRef) {
        self.roots_visited.push(entry.label.clone());
        if !self.visited_oids.contains(&entry.root) {
            self.visited_oids.push(entry.root);
        }
        *self.visited_by_kind.entry(entry.kind).or_insert(0) += 1;
    }
}

/// Callbacks the collector offers so a test can observe a run without a `Vec` of every object
/// (ADR-008 §5.4: memory must not grow with the store).
#[derive(Default)]
pub struct GcHooks<'a> {
    /// Called once per visited root (streaming; a test collects it into a set).
    pub on_root_visited: Option<&'a mut dyn FnMut(RootKind, ObjectId)>,
    /// Test seam (injection F): the predicate answers **"should this root be skipped?"** —
    /// `true` = skip it, `false` = visit it as usual.
    ///
    /// One direction only, and it is the *skip* direction: a root that the filter rejects is
    /// therefore absent from `roots_visited` / `visited_by_kind`, which is what makes G3 (and, when
    /// mark never reports the label, G2) go red. A predicate that returned "should visit" would
    /// make an empty closure mean "visit nothing" — a silent way to fail every run.
    ///
    /// Only compiled with `gc-test-hooks`, so a production build has no way to lie about what it
    /// visited.
    #[cfg(feature = "gc-test-hooks")]
    pub skip_root: Option<&'a mut dyn FnMut(&RootRef) -> bool>,
    /// Test seam (injection G): ids forced into the free set.
    #[cfg(feature = "gc-test-hooks")]
    pub extra_free_candidates: Vec<ObjectId>,
}

impl std::fmt::Debug for GcHooks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("GcHooks");
        #[cfg(feature = "gc-test-hooks")]
        debug
            .field("skip_root", &self.skip_root.is_some())
            .field("extra_free_candidates", &self.extra_free_candidates.len());
        debug.finish_non_exhaustive()
    }
}

/// The two inputs of a run: what must be protected, and the source `mark` actually walks.
///
/// Kept separate on purpose (P2b-6 §6). A single `gc(roots)` could only produce counter-example C;
/// splitting "what should be there" from "what mark walks" makes D (a missing root) and F
/// (registered but skipped) constructible, and G is the hook above.
#[derive(Debug)]
pub struct GcInput<'a, S: roots::RootSource + ?Sized = dyn roots::RootSource> {
    /// The roots the caller says must be protected.
    pub declared: &'a RootSet,
    /// The source whose roots `mark` will actually walk.
    pub mark_source: S,
}

impl<S: roots::RootSource> GcInput<'_, S> {
    /// Runs the mark side: walks every root the source yields and records each visit.
    ///
    /// Sweep and repacking land in a later commit; this exists so the criteria seam is real —
    /// `GcReport::record_visit` is the same call the full collector will make.
    ///
    /// # Errors
    ///
    /// Whatever the source reported.
    pub fn mark(&self, hooks: &mut GcHooks<'_>) -> WorkspaceResult<GcReport> {
        let mut report = GcReport::skeleton(self.declared);
        let walked = self.mark_source.roots()?;
        for entry in walked.entries() {
            #[cfg(feature = "gc-test-hooks")]
            if let Some(skip) = hooks.skip_root.as_mut() {
                if skip(entry) {
                    continue;
                }
            }
            report.record_visit(entry);
            report.reachable.push(entry.root);
            if let Some(sink) = hooks.on_root_visited.as_mut() {
                sink(entry.kind, entry.root);
            }
        }
        report.reachable_count = report.reachable.len();
        Ok(report)
    }
}
