//! What GC must keep alive, and the seam that lets a test lie about it (ADR-008 §6.2).
//!
//! The root set is the whole safety property of a mark/sweep collector: an object that no root
//! reaches is garbage, and "no root reaches it" is only as trustworthy as the enumeration behind
//! it. So the enumeration is **a value produced by a replaceable source**, not a query buried
//! inside `gc()`:
//!
//! ```text
//! let roots = source.roots()?;          // RepoRoots in production, a test double in tests
//! let marked = mark(&roots, ...)?;      // visits every root, records what it visited
//! ```
//!
//! Three acceptance criteria live or die on this shape (P2b-6 §2.1):
//!
//! - **G1 (must not delete live data)** — a `snapshot` row dropped from the enumeration makes the
//!   objects it protects look unreachable; the test injects exactly that and expects a red run;
//! - **G2 (must not miss a root)** — the roots `mark` visited are compared with the roots the
//!   source claims, so an enumeration that forgets a row is visible as a set difference;
//! - **G3 (every root is visited)** — each entry carries a label and the mark phase records a visit
//!   per label, so "registered but skipped" cannot pass silently.
//!
//! G4/G5 (freed ∩ reachable = ∅, freed_unreachable ≠ ∅) are about the sweep side; they consume the
//! same `RootSet` so the injections above stay meaningful.

use cas_core::ObjectId;

use crate::error::WorkspaceResult;

/// Where a root came from (ADR-008 §6.2).
///
/// The kind is part of the value because G2 compares *sets*, and two roots can be the same object
/// id reached through different rows: a set of ids alone would hide "the snapshot row was dropped
/// but the commit still protects that tree".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RootKind {
    /// A branch head and its ancestors.
    Commit,
    /// A `snapshot` row (a GC root that does not move a branch).
    Snapshot,
    /// A workspace's `base_root` / `root(base_commit)`.
    WorkspaceBase,
    /// A workspace's overlay `pending_root`.
    OverlayPending,
    /// An overlay inode's `sealed_content`.
    SealedContent,
}

impl RootKind {
    /// A short label, used in reports and in G2's diff.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RootKind::Commit => "commit",
            RootKind::Snapshot => "snapshot",
            RootKind::WorkspaceBase => "workspace-base",
            RootKind::OverlayPending => "overlay-pending",
            RootKind::SealedContent => "sealed-content",
        }
    }
}

/// One root, with enough identity to compare enumerations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RootRef {
    /// Which kind of row produced it.
    pub kind: RootKind,
    /// The tree root this entry protects.
    pub root: ObjectId,
    /// A stable label (for example `snapshot:<id>`), so G2 can name what moved.
    pub label: String,
}

impl RootRef {
    /// Builds a root reference.
    #[must_use]
    pub fn new(kind: RootKind, root: ObjectId, label: impl Into<String>) -> Self {
        Self {
            kind,
            root,
            label: label.into(),
        }
    }
}

/// The roots one GC run must protect.
///
/// Deduplicated **by label**, not by object id: two rows protecting the same tree are two facts,
/// and G2 compares facts (`M = R`). `by_root()` gives the id set the sweep actually needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootSet {
    entries: Vec<RootRef>,
}

impl RootSet {
    /// An empty set (a repository with no history at all).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a set from entries, deduplicating by label and sorting for stable diffs.
    #[must_use]
    pub fn from_entries(mut entries: Vec<RootRef>) -> Self {
        entries.sort();
        entries.dedup();
        Self { entries }
    }

    /// The entries, in stable order.
    #[must_use]
    pub fn entries(&self) -> &[RootRef] {
        &self.entries
    }

    /// The distinct object ids the marks must start from.
    #[must_use]
    pub fn ids(&self) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self.entries.iter().map(|entry| entry.root).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Where a GC run gets its roots from.
///
/// Production uses [`RepoRoots`]; tests substitute a double to construct the negative cases
/// (drop a snapshot row → G1 must go red; omit a root → G2 must go red; register but skip → G3).
pub trait RootSource {
    /// The roots this run must protect.
    /// The roots this run must protect.
    ///
    /// # Errors
    ///
    /// Whatever reading the repository reported.
    fn roots(&self) -> WorkspaceResult<RootSet>;
}

/// A borrowed source is a source, so `GcInput` can take `&dyn RootSource` (tests pass a reference
/// to their fixture instead of handing ownership to the collector).
impl<T: RootSource + ?Sized> RootSource for &T {
    fn roots(&self) -> WorkspaceResult<RootSet> {
        (**self).roots()
    }
}

/// The production source: every row ADR-008 §6.2 lists.
///
/// Not yet complete — `branch_log` ancestry and the overlay references (`pending_root`,
/// `sealed_content`) arrive with the overlay/GC work in this card; each addition is a
/// [`RootKind`] so the enumeration stays comparable.
#[derive(Debug)]
pub struct RepoRoots<'a> {
    /// The repository to enumerate.
    pub repo: &'a crate::repo::Repo,
}

impl RootSource for RepoRoots<'_> {
    fn roots(&self) -> WorkspaceResult<RootSet> {
        let mut entries = Vec::new();
        for root in self.repo.roots()? {
            entries.push(RootRef::new(RootKind::Commit, root, format!("root:{root}")));
        }
        Ok(RootSet::from_entries(entries))
    }
}
