//! Two-phase delete (P3-1 §1 `delete`, ADR-009 §5.3).
//!
//! Windows deletion is a **state**, not an action: `SetDelete(TRUE)` marks the entry, every later open fails
//! with `STATUS_DELETE_PENDING`, another handle may cancel the mark, and only the last handle's
//! `CleanupDelete` actually removes the bytes. A frontend that deletes at `SetDelete` time breaks the first
//! rule of sharing: somebody else's open handle starts reading a file that no longer exists.
//!
//! The state machine is small enough to enumerate, so the table walks **every state × every event** and the
//! assertion carries the cell coordinates.

use crate::error::Failure;

/// What happened to the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The entry exists and is not marked.
    Clean { handles: usize },
    /// The entry exists, is marked, and still has handles.
    Pending { handles: usize },
    /// The entry exists, is marked, and nothing holds it.
    PendingDropped,
    /// The entry is gone.
    Deleted,
}

impl State {
    /// Whether the entry still exists.
    #[must_use]
    pub fn exists(self) -> bool {
        !matches!(self, State::Deleted)
    }
}

/// One event the driver can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A second handle opened the entry.
    Open,
    /// A handle set the delete mark.
    SetDelete,
    /// A handle cleared the delete mark.
    ClearDelete,
    /// A handle closed (`Cleanup`).
    Cleanup,
    /// The last handle closed with the mark still set.
    CleanupDelete,
}

impl Event {
    /// Every event, for the completeness walk.
    pub const ALL: [Event; 5] = [
        Event::Open,
        Event::SetDelete,
        Event::ClearDelete,
        Event::Cleanup,
        Event::CleanupDelete,
    ];
}

/// The outcome of one transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The new state.
    Next(State),
    /// The event is refused and the state does not change.
    Refused(Failure),
}

/// The transition function, spelled out for every (state, event) pair — 4 × 5 = 20 cells, no wildcard.
#[must_use]
pub fn step(state: State, event: Event) -> Step {
    use State::{Clean, Deleted, Pending, PendingDropped};
    match (state, event) {
        // A marked entry cannot be opened again, and neither can a deleted one.
        (Clean { handles }, Event::Open) => Step::Next(Clean {
            handles: handles + 1,
        }),
        (Pending { .. } | PendingDropped, Event::Open) => Step::Refused(Failure::DeletePending),
        (Deleted, Event::Open) => Step::Refused(Failure::LeafMissing),
        // Setting the mark is idempotent; clearing it on a clean entry is a no-op rather than an error
        // (Win32 allows it, and refusing would make well-behaved callers fail).
        (Clean { handles }, Event::SetDelete) => Step::Next(Pending { handles }),
        (Pending { handles }, Event::SetDelete) => Step::Next(Pending { handles }),
        (PendingDropped, Event::SetDelete) => Step::Next(PendingDropped),
        (Deleted, Event::SetDelete) => Step::Refused(Failure::LeafMissing),
        (Clean { handles }, Event::ClearDelete) => Step::Next(Clean { handles }),
        (Pending { handles }, Event::ClearDelete) => Step::Next(Clean { handles }),
        (PendingDropped, Event::ClearDelete) => Step::Next(Clean { handles: 0 }),
        (Deleted, Event::ClearDelete) => Step::Refused(Failure::LeafMissing),
        // A plain cleanup decrements; a marked entry with no handles left becomes "pending, dropped".
        (Clean { handles: 0 }, Event::Cleanup) => Step::Next(Clean { handles: 0 }),
        (Clean { handles }, Event::Cleanup) => Step::Next(Clean {
            handles: handles - 1,
        }),
        (Pending { handles: 0 } | PendingDropped, Event::Cleanup) => Step::Next(PendingDropped),
        (Pending { handles }, Event::Cleanup) => Step::Next(Pending {
            handles: handles - 1,
        }),
        (Deleted, Event::Cleanup) => Step::Refused(Failure::LeafMissing),
        // CleanupDelete is the only event that removes bytes, and the removal happens on the **last**
        // handle: with one left, this call consumes it and the entry goes; with more, it only decrements.
        (PendingDropped, Event::CleanupDelete) => Step::Next(Deleted),
        (Clean { handles }, Event::CleanupDelete) => Step::Next(if handles <= 1 {
            Deleted
        } else {
            Clean {
                handles: handles - 1,
            }
        }),
        (Pending { handles }, Event::CleanupDelete) => Step::Next(if handles <= 1 {
            Deleted
        } else {
            Pending {
                handles: handles - 1,
            }
        }),
        (Deleted, Event::CleanupDelete) => Step::Refused(Failure::LeafMissing),
    }
}
