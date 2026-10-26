//! Per-thread counters for the tree's mutating calls (P3-2 G2, test-only).
//!
//! The core's criterion is "**exactly one** `tombstone` per removal". That cannot be observed from the outcome — a
//! second tombstone of an already-dead row is a no-op at the SQL level — so the count has to come from the backend. This
//! is the same shape as `cas-store`'s per-thread pack-read counter: a `Cell` behind `thread_local`, gated by a feature the
//! production build does not enable.
//!
//! Note the two counts are different things: this one counts how often the **core** tombstones an inode; the adapter
//! mock's M12 counts how often it **calls** `remove_at`.

use std::cell::Cell;

thread_local! {
    static TOMBSTONES: Cell<u64> = const { Cell::new(0) };
}

/// Records one tombstone of `serial`.
pub(crate) fn note_tombstone() {
    TOMBSTONES.with(|cell| cell.set(cell.get() + 1));
}

/// How many tombstone calls this thread has made since the last [`reset_tombstones`].
#[must_use]
pub fn tombstones() -> u64 {
    TOMBSTONES.with(Cell::get)
}

/// Resets this thread's tombstone counter.
pub fn reset_tombstones() {
    TOMBSTONES.with(|cell| cell.set(0));
}
