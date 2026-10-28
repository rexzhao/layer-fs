//! The publication gate (ADR-010 §10.3): one read/write lock that keeps a `commit` from running **through** a file operation.
//!
//! # What it is for
//!
//! `commit` copies the overlay's rows into a new tree and then adopts that tree as the workspace's base. A write that lands **between** those two steps is in
//! neither place: the tree was read before it, and the overlay row is discarded when the base moves. That is a lost write, and it is not a race that a test can
//! hope to hit — it is a window that has to be closed by ordering.
//!
//! So the gate is held **for reading** by every data operation (many may run at once) and **for writing** by the publication steps (exactly one, alone). A
//! `commit` therefore cannot overlap a write, in either direction: writes already running finish first (G3), and writes arriving during the commit wait (G1)
//! and then land in the adopted base (they are still alive).
//!
//! # Why the depth counter exists
//!
//! A single public call may pass through several internal ones; taking the read lock at every layer is how a thread deadlocks against a waiting writer (a
//! reader can be starved, or re-entering a read lock that a writer is queued on is not re-entrant in every implementation). `depth()` records how many read
//! guards the **current thread** holds so the nesting can be asserted in debug builds instead of being discovered in production.

/// The gate itself.
///
/// The guarded value is empty for now: what the writer must own is the *permission*, not a data structure — step 2 of the S2 card moves the base state in here.
#[derive(Debug, Default)]
pub struct Gate {
    lock: std::sync::RwLock<()>,
}

impl Gate {
    /// Creates a gate nobody holds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the gate for **reading**: one data operation, many at a time.
    ///
    /// The returned [`ReadGuard`] is what makes `depth()` usable: it decrements on drop, so "how many read guards does this thread hold" is true at every
    /// moment rather than only on the way in (G4a). Without the decrement the number only grew, and an assertion on it could never distinguish "one" from
    /// "two" — an instrument that reads the same under the defect it is supposed to catch.
    #[must_use = "dropping the guard immediately releases the gate"]
    pub fn read(&self) -> ReadGuard<'_> {
        let guard = self
            .lock
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // **Checked before the counter moves**, so a refused nested read leaves the depth as it was: a panic that left it incremented would make the next
        // acquisition on this thread look like a third level and report a defect that is not there.
        let held = DEPTH.with(|depth| {
            let now = depth.get();
            if now >= 1 {
                if STRICT.load(std::sync::atomic::Ordering::Relaxed) {
                    panic!(
                        "gate: nested read on one thread (depth {}) — a public data method is calling another",
                        now + 1
                    );
                }
                if DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("gate: nested read, depth {}", now + 1);
                }
            }
            depth.set(now + 1);
            now + 1
        });
        let _ = held;
        ReadGuard { guard: Some(guard) }
    }

    /// Takes the gate for **writing**: one publication step, alone.
    #[must_use = "dropping the guard immediately releases the gate"]
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.lock
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A read guard that keeps [`depth`] honest: it decrements the thread's count when it drops (G4a).
///
/// The lock itself would be released by the standard guard either way; what this wrapper adds is the **count**, which is what the nesting assertion reads. A
/// wrapper that only counted on the way in (the S2 state) is the reason G4 could not have caught a real nesting: the number never came back down.
pub struct ReadGuard<'a> {
    guard: Option<std::sync::RwLockReadGuard<'a, ()>>,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // Released **before** the count drops, so no window exists in which the depth says "free" while the lock is still held.
        drop(self.guard.take());
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

impl std::ops::Deref for ReadGuard<'_> {
    type Target = ();

    fn deref(&self) -> &Self::Target {
        &()
    }
}

impl std::fmt::Debug for ReadGuard<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ReadGuard").field("held", &self.guard.is_some()).finish()
    }
}

thread_local! {
    /// How many read guards this thread holds: `0` outside, `1` inside a data operation.
    ///
    /// The decrement happens when the guard drops, which is what makes the number usable in an assertion at the *start* of a call — that is the moment a
    /// nested acquisition would show up.
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a nested read **fails** instead of merely being counted.
///
/// Off by default, and set by a criterion: the measurement showed nothing nests, so turning it on is a decision made against a reading rather than against an
/// assumption. A test seam rather than an environment variable, because this crate forbids `unsafe` and reading the environment in a test means `set_var`.
/// **These are the only two switches there are** (G4c): they are set through [`set_nesting_assertion`] / [`set_nesting_report`], and no environment variable is
/// read anywhere in this crate — an earlier version of these comments named one that nothing read, which is how S2's "measured zero nesting" turned out to be
/// a measurement of nothing at all.
static STRICT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether nesting is reported as it happens (the **measurement** half of G4).
static DEBUG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns the nesting assertion on or off; returns the previous setting.
///
/// **Test seam, and only that**: production never calls it, and a nested read is a defect in the caller rather than a configuration choice.
pub fn set_nesting_assertion(on: bool) -> bool {
    STRICT.swap(on, std::sync::atomic::Ordering::Relaxed)
}

/// Turns the nesting report on or off; returns the previous setting.
pub fn set_nesting_report(on: bool) -> bool {
    DEBUG.swap(on, std::sync::atomic::Ordering::Relaxed)
}

/// How many read guards the **current thread** holds right now (G4's instrument).
#[must_use]
pub fn depth() -> usize {
    DEPTH.with(std::cell::Cell::get)
}

// (the guard above carries the decrement: `ReadGuard::drop` is what makes `depth()` return to zero, so the assertion can tell "one" from "two")

#[cfg(test)]
mod tests {
    use super::{depth, Gate};

    /// **G4a**: the depth is a *current* count, not a running total. In S2 the counter only grew, so the unit test below could pass while the number it reads
    /// was already meaningless on the second call — the defect the card found by reading the code rather than by running it.
    #[test]
    fn the_depth_follows_the_guard() {
        let gate = Gate::new();
        assert_eq!(depth(), 0, "cell depth: nothing held");
        {
            let _guard = gate.read();
            assert_eq!(depth(), 1, "cell depth: one reader inside");
        }
        assert_eq!(depth(), 0, "cell G4a: the counter must come back down");
        // Three take/drop rounds: a counter that only decremented once would leave 2 here.
        for round in 0..3 {
            {
                let _guard = gate.read();
            }
            assert_eq!(depth(), 0, "cell G4a: round {round} must end at zero");
        }
    }

    #[test]
    fn a_writer_can_take_the_gate_when_no_reader_holds_it() {
        let gate = Gate::new();
        {
            let _writer = gate.write();
        }
        let _reader = gate.read();
    }

    /// **G4**: the nesting assertion can fire — an instrument that has never been seen to fail has not been tested.
    ///
    /// Measured first (a full `fs-workspace`+`fs-api`+`fs-winfsp` run with `LAYERFS_DEBUG_GATE=1` printed **no** nesting), so the assertion line is
    /// "depth > 1"; this test drives depth to 2 directly and requires the failure.
    #[test]
    fn a_second_read_on_one_thread_is_refused_in_strict_mode() {
        let gate = Gate::new();
        let previous = super::set_nesting_assertion(true);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _first = gate.read();
            let _second = gate.read();
        }));
        super::set_nesting_assertion(previous);
        assert!(
            outcome.is_err(),
            "cell G4: a nested read must be refused when the assertion is on"
        );
    }
}
