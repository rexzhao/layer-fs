//! How long a caller actually waited before a contention error (ADR-006 §11.1).
//!
//! `WorkspaceError::Busy { waited_ms }` used to carry the *policy* constant, so a caller that
//! failed after 5 ms was told "the main database stayed locked for 5000 ms" — a statement
//! about the configuration dressed up as a measurement. The thread-local clock below is set
//! when an operation starts touching a contended database and read when the failure is
//! translated, so the number reported is the time the caller actually spent.

use std::cell::Cell;
use std::time::Instant;

thread_local! {
    static SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Starts (or restarts) the clock for the current thread.
pub(crate) fn start() {
    SINCE.with(|cell| cell.set(Some(Instant::now())));
}

/// Milliseconds since [`start`] on this thread; `0` when no operation is in flight.
pub(crate) fn waited_ms() -> u64 {
    SINCE.with(|cell| {
        cell.get()
            .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_wait_is_the_time_that_passed_not_a_constant() {
        start();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let waited = waited_ms();
        assert!(
            (15..1000).contains(&waited),
            "20 ms of sleep must report roughly 20 ms, got {waited}"
        );
        assert_ne!(
            waited,
            crate::schema::MAIN_BUSY_TIMEOUT_MS,
            "the reported wait must not be the policy constant"
        );
    }
}
