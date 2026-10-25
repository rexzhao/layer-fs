//! `delete` tests (P3-1 §1): every state × event cell, then the three properties that matter.

use fs_windows::delete::{step, Event, Marked, State, Step};
use fs_windows::error::Failure;
use std::collections::BTreeSet;

/// The state space, enumerated explicitly so the walk covers it (4 states × 5 events = 20 cells).
const STATES: [State; 6] = [
    State::Clean { handles: 0 },
    State::Clean { handles: 1 },
    State::Pending { handles: 0 },
    State::Pending { handles: 1 },
    State::PendingDropped,
    State::Deleted,
];

#[test]
fn every_state_and_event_has_a_defined_transition() {
    let mut cells = BTreeSet::new();
    for state in STATES {
        for event in Event::ALL {
            match step(state, event) {
                Step::Next(next) => {
                    assert!(
                        cells.insert((format!("{state:?}"), format!("{event:?}"))),
                        "cell state={state:?} event={event:?} was visited twice"
                    );
                    // A transition may never revive a deleted entry.
                    if state == State::Deleted {
                        panic!("cell state=Deleted event={event:?} produced {next:?}");
                    }
                }
                Step::Refused(failure) => {
                    assert!(cells.insert((format!("{state:?}"), format!("{event:?}"))));
                    let allowed = match event {
                        Event::Open => {
                            matches!(failure, Failure::DeletePending | Failure::LeafMissing)
                        }
                        _ => failure == Failure::LeafMissing,
                    };
                    assert!(allowed, "cell state={state:?} event={event:?}: {failure:?}");
                }
            }
        }
    }
    assert_eq!(
        cells.len(),
        STATES.len() * Event::ALL.len(),
        "missing cell: the walk must cover states × events = {} × {}",
        STATES.len(),
        Event::ALL.len()
    );
}

/// The property the two-phase design exists for: after `SetDelete`, a **new open is refused**, and the entry
/// is still there (nothing has been removed yet).
#[test]
fn set_delete_does_not_remove_anything_and_blocks_new_opens() {
    let marked = match step(State::Clean { handles: 1 }, Event::SetDelete) {
        Step::Next(next) => next,
        Step::Refused(failure) => panic!("SetDelete was refused: {failure:?}"),
    };
    assert_eq!(marked, State::Pending { handles: 1 }, "cell SetDelete");
    assert!(marked.exists(), "the entry still exists while marked");
    assert_eq!(
        step(marked, Event::Open),
        Step::Refused(Failure::DeletePending),
        "cell open-while-pending"
    );
}

/// Another handle may cancel the mark, and then opens work again.
#[test]
fn a_mark_can_be_cancelled_by_another_handle() {
    let cancelled = match step(State::Pending { handles: 1 }, Event::ClearDelete) {
        Step::Next(next) => next,
        Step::Refused(failure) => panic!("ClearDelete was refused: {failure:?}"),
    };
    assert_eq!(cancelled, State::Clean { handles: 1 });
    assert!(matches!(
        step(cancelled, Event::Open),
        Step::Next(State::Clean { handles: 2 })
    ));
}

/// Only the last handle's `CleanupDelete` removes the entry; with a handle left, it merely decrements.
#[test]
fn removal_happens_only_on_the_last_cleanup_delete() {
    assert_eq!(
        step(State::Pending { handles: 2 }, Event::CleanupDelete),
        Step::Next(State::Pending { handles: 1 }),
        "cell pending/handles=2: not the last handle, so nothing is removed"
    );
    assert_eq!(
        step(State::Pending { handles: 1 }, Event::CleanupDelete),
        Step::Next(State::Deleted),
        "cell pending/handles=1: the last handle removes the entry"
    );
    assert_eq!(
        step(State::PendingDropped, Event::CleanupDelete),
        Step::Next(State::Deleted),
        "cell pending/handles=0: a mark left with no handles removes on the next cleanup"
    );
}

#[test]
fn a_deleted_entry_refuses_every_event() {
    for event in Event::ALL {
        assert_eq!(
            step(State::Deleted, event),
            Step::Refused(Failure::LeafMissing),
            "cell state=Deleted event={event:?}"
        );
    }
    let _ = Marked::ALL;
}
