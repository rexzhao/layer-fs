//! Admission: the gate that lets `restore` / `switch` stop **new** opens while it changes the view (S3 card §3).
//!
//! # Why counting handles before the change is not enough
//!
//! "Count the handles, and if there are none, restore" has a window: between the count and the publication gate, an `Open` callback can resolve a path against
//! the **old** view and be about to register a handle. After the restore that handle names an object that no longer exists. The publication gate cannot stop
//! it — the callback's read lock was released when the path was resolved, and the handle is registered afterwards.
//!
//! So "check" and "prevent new opens" have to happen **under one lock**, and that lock has to cover resolve **and** register. That is this type: a thread that
//! is admitted holds a ticket for the whole callback, and a quiesce is complete only when every ticket is back.
//!
//! # Two ways to be in the way
//!
//! - **Paused** (restore): arrivals **wait**. A restore does not need the dispatcher, so waiting cannot deadlock — the opener is merely slower.
//! - **Closing** (switch): arrivals **fail immediately**. Switching unmounts, and unmounting waits for the dispatcher to exit; an opener that waited would be a
//!   deadlock. Windows behaves the same way on a volume that is being dismounted.
//!
//! Both are deliberate, and neither is a fallback: they are the difference between an operation that ends and one that hangs.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Whether new opens are allowed in right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Normal: every open is admitted immediately.
    Open,
    /// A `restore` is running: opens **wait** until it is done.
    Paused,
    /// A `switch` is running: opens **fail** immediately, because the volume is going away.
    Closing,
}

/// What a caller wants to do, which decides how arrivals are treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiesceKind {
    /// `restore`: arrivals wait.
    Restore,
    /// `switch`: arrivals fail.
    Switch,
}

impl QuiesceKind {
    fn state(self) -> Admission {
        match self {
            Self::Restore => Admission::Paused,
            Self::Switch => Admission::Closing,
        }
    }
}

/// Why an open was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A `switch` is running: the volume is going away, and waiting would deadlock the unmount.
    Closing,
    /// The quiesce did not see the last ticket come back in time.
    TimedOut,
}

#[derive(Debug)]
struct State {
    admission: Admission,
    in_flight: usize,
}

/// The admission gate. One per mounted adapter.
#[derive(Debug)]
pub struct Gatekeeper {
    state: Mutex<State>,
    changed: Condvar,
}

/// A ticket held for the duration of one path-entering callback.
///
/// Dropping it is what lets a quiesce finish, so it must be held for the **whole** callback — resolve, backend call, and handle registration included.
#[derive(Debug)]
pub struct Ticket<'a> {
    gate: &'a Gatekeeper,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight = state.in_flight.saturating_sub(1);
        drop(state);
        self.gate.changed.notify_all();
    }
}

/// Proof that no open is in flight and none will be admitted until this is dropped.
#[derive(Debug)]
pub struct Quiesced<'a> {
    gate: &'a Gatekeeper,
}

impl Drop for Quiesced<'_> {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Back to `Open` only from the state this guard set: if something else moved it (a failed quiesce that already resumed, say), overwriting would let this
        // guard resurrect a gate that a newer quiesce had closed.
        if state.admission != Admission::Open {
            state.admission = Admission::Open;
        }
        drop(state);
        self.gate.changed.notify_all();
    }
}

/// The outcome of asking the mount to quiesce.
#[derive(Debug)]
pub enum Quiesce<'a> {
    /// No open is in flight, and none will be admitted until the guard drops.
    Quiesced(Quiesced<'a>),
    /// A **file** handle is still open: the operation would pull the object out from under it.
    ///
    /// Directories are reported but do not block (D8 = B): the shell that started the command holds one, and a stale directory view loses no data.
    HandlesOpen {
        /// Open file handles — the blocking kind.
        files: usize,
        /// Open directory handles — reported, not blocking.
        dirs: usize,
    },
    /// The in-flight opens did not come back in time.
    TimedOut {
        /// How many were still in flight when the wait expired.
        in_flight: usize,
    },
}

impl Gatekeeper {
    /// A gate that admits everything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                admission: Admission::Open,
                in_flight: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// What the gate says right now (for assertions and for the message a refusal carries).
    #[must_use]
    pub fn admission(&self) -> Admission {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .admission
    }

    /// Takes a ticket for one path-entering callback.
    ///
    /// # Errors
    ///
    /// [`Refused::Closing`] when a `switch` is running: the volume is going away, so the open must fail rather than wait.
    pub fn admit(&self) -> Result<Ticket<'_>, Refused> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match state.admission {
                Admission::Open => {
                    state.in_flight += 1;
                    return Ok(Ticket { gate: self });
                }
                // **Waiting** is safe here because a restore never needs the dispatcher, and it is what makes a `restore` observable as "the open arrived during
                // it and then landed in the new view" rather than as an error the caller has to retry.
                Admission::Paused => {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                Admission::Closing => return Err(Refused::Closing),
            }
        }
    }

    /// Stops new opens, waits for the ones in flight, and then decides based on **file** handles.
    ///
    /// `counts` is injected rather than reaching into the handle table: the rule is this module's, the table is the adapter's, and a caller that passes the
    /// wrong counter object still has to satisfy the same rule at the call site.
    pub fn quiesce(
        &self,
        kind: QuiesceKind,
        counts: impl Fn() -> (usize, usize),
        wait: Duration,
    ) -> Quiesce<'_> {
        let deadline = Instant::now() + wait;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.admission = kind.state();
        drop(state);

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.in_flight > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let in_flight = state.in_flight;
                state.admission = Admission::Open;
                drop(state);
                self.changed.notify_all();
                return Quiesce::TimedOut { in_flight };
            }
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(50)))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if timeout.timed_out() && state.in_flight > 0 {
                // Re-check the deadline on the next turn; the bounded wait exists so a missing notification cannot make this spin forever.
                continue;
            }
        }
        drop(state);

        let (files, dirs) = counts();
        if files > 0 {
            // **Restored before returning**: a refusal must leave the mount exactly as it was, or the next open would hang on a gate nobody will reopen.
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.admission != Admission::Open {
                state.admission = Admission::Open;
            }
            drop(state);
            self.changed.notify_all();
            return Quiesce::HandlesOpen { files, dirs };
        }
        Quiesce::Quiesced(Quiesced { gate: self })
    }
}

impl Default for Gatekeeper {
    fn default() -> Self {
        Self::new()
    }
}
