//! Directory cursors: one snapshot per handle, resumed by marker (P3-3a card §3 criterion 6).
//!
//! The snapshot is **the interface promise**, not an optimisation: `read_dir` during enumeration must not re-order, and
//! must not start showing entries created after the enumeration began. So the entries are taken once, sorted once, and
//! later pages are cut from that list by marker. A backend call on the second page would break the promise whenever
//! something changed in between — which is exactly what a UI doing "list while a build writes files" does.
//!
//! Two deliberate omissions:
//!
//! - **no `.`/`..`**: the adapter synthesises those. A backend that returned them would make the adapter show them
//!   twice, and a cursor that synthesised them would be unable to say where the real entries begin;
//! - **no de-duplication**: `A` and `a` are different entries (raw-byte bindings, ADR-009 §4.1). De-duplicating by a
//!   folded key would silently hide one of them, which is a data-loss bug wearing a "cleaner listing" costume.

use crate::ops::{DirEntry, Kind};

/// One entry in a snapshot, with the folded key it sorts by.
///
/// The folded key is stored rather than recomputed: sorting by a key that is recomputed per comparison would make the
/// order depend on a function that could, in principle, differ between calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortEntry {
    /// The raw name bytes, as the backend returned them.
    pub name: Vec<u8>,
    /// The folding of `name` used as the primary sort key.
    pub folded: Vec<u8>,
    /// File or directory.
    pub kind: Kind,
}

/// The cursor for one directory handle.
///
/// The first read fills it; every later read is answered from the stored list. There is no way to refresh it: a handle
/// that wants a fresh view closes and reopens, which is the NT behaviour this mirrors.
#[derive(Debug, Default)]
pub struct DirentCursor {
    snapshot: Option<Vec<SortEntry>>,
}

impl DirentCursor {
    /// An empty cursor (no snapshot yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the snapshot has been taken.
    #[must_use]
    pub fn is_filled(&self) -> bool {
        self.snapshot.is_some()
    }

    /// Takes the snapshot on the first call, and returns the whole stored list on every call.
    ///
    /// `entries` is only consulted when the snapshot is empty, which is how "the second page cannot see new entries"
    /// is guaranteed rather than remembered.
    ///
    /// # Panics
    ///
    /// Never: the sort is on owned keys.
    pub fn fill(
        &mut self,
        entries: Vec<DirEntry>,
        fold: impl Fn(&[u8]) -> Vec<u8>,
    ) -> &[SortEntry] {
        if self.snapshot.is_none() {
            let mut list: Vec<SortEntry> = entries
                .into_iter()
                .map(|entry| SortEntry {
                    folded: fold(&entry.name),
                    name: entry.name,
                    kind: entry.kind,
                })
                .collect();
            // Stable sort on (folded, raw name): equal folded keys keep a deterministic order, and the raw name as the
            // secondary key means `A` always precedes `a` rather than "whichever the backend happened to return first".
            list.sort_by(|left, right| {
                left.folded
                    .cmp(&right.folded)
                    .then_with(|| left.name.cmp(&right.name))
            });
            self.snapshot = Some(list);
        }
        self.snapshot.as_deref().expect("just filled")
    }

    /// The stored snapshot, if any.
    #[must_use]
    pub fn snapshot(&self) -> Option<&[SortEntry]> {
        self.snapshot.as_deref()
    }

    /// The entries strictly **after** `marker`, at most `limit` of them.
    ///
    /// A marker that is no longer in the snapshot yields an empty page rather than restarting from the beginning: a
    /// restart would make a stale marker silently duplicate the entire directory, and duplication is worse than an
    /// empty page for a caller that is iterating.
    #[must_use]
    pub fn page(&self, marker: Option<&[u8]>, limit: usize) -> &[SortEntry] {
        let Some(list) = self.snapshot.as_deref() else {
            return &[];
        };
        let start = match marker {
            None => 0,
            Some(marker) => match list.iter().position(|entry| entry.name == marker) {
                Some(index) => index + 1,
                None => list.len(),
            },
        };
        let end = start.saturating_add(limit).min(list.len());
        &list[start.min(list.len())..end]
    }
}
