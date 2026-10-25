//! Crash-point seam for the collector (P2b-6 §27.A, I6).
//!
//! A "crash" here is a typed abort at a named point: the run stops, the store is left in whatever state
//! that point implies, and the test reopens it. That is enough to check the properties the real crash
//! table promises — what matters is *which step was reached*, and a returning error reaches the same state
//! a process death would, minus the fsync ambiguity the storage layer already covers.
//!
//! Every test must print the reached point and compare it with the expected one: "reopened and verified
//! clean" is also true of a run that never got that far.

use std::cell::Cell;

use crate::error::{WorkspaceError, WorkspaceResult};

/// The steps a run can be stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcStep {
    /// Point 1: marking has visited at least one root and is not finished.
    Marking = 1,
    /// Point 2: the deletion transaction has committed, the files are not unlinked yet.
    RowsDeletedFilesKept = 2,
    /// Point 3: a replacement pack is durable in `packs/`, the index still points at the old one.
    RepackStaged = 3,
    /// Point 4: the switch transaction committed, the old pack's file is still there.
    RepackSwitched = 4,
    /// Point 5: some orphans or temporaries are gone, others are not.
    OrphanSweep = 5,
}

impl GcStep {
    /// The number the report prints (I6).
    #[must_use]
    pub fn number(self) -> u8 {
        self as u8
    }
}

thread_local! {
    /// Which step (if any) **this thread's** next run must stop at. `0` means "run normally".
    ///
    /// Thread-local on purpose: the first version was a process-wide static, and `cargo test`'s threads
    /// meant one test's armed step aborted another test's run — exactly the class of leak this project has
    /// already paid for twice (a global `SWEEP_ONCE`, a process-level `pack_read_bytes`). A seam shared
    /// across tests cannot tell "the injection worked" from "the injection escaped".
    static ARMED: Cell<u8> = const { Cell::new(0) };
}

/// Arms the seam: the next run stops at `step`.
#[cfg(feature = "gc-test-hooks")]
pub fn arm(step: GcStep) {
    ARMED.with(|armed| armed.set(step.number()));
}

/// Disarms the seam.
#[cfg(feature = "gc-test-hooks")]
pub fn disarm() {
    ARMED.with(|armed| armed.set(0));
}

/// Called at each point; returns a typed abort when that point is armed.
///
/// # Errors
///
/// [`WorkspaceError::Inconsistent`] naming the step, when this point is the armed one.
pub fn reach(step: GcStep) -> WorkspaceResult<()> {
    let armed = ARMED.with(Cell::get) == step.number();
    if armed {
        // Disarm on the way out: a seam that stayed armed would abort the *next* run for no reason, which
        // is the "no residue" property §8's third step checks for the other injections.
        ARMED.with(|cell| cell.set(0));
        return Err(WorkspaceError::Inconsistent {
            detail: format!("simulated crash at gc step {}", step.number()),
        });
    }
    Ok(())
}
