//! Per-thread totals of time spent queued for a shared resource.
//!
//! A component that makes a caller wait (an admission queue, a shared
//! connection's lock) adds the wait to the calling thread's total for its
//! kind. A timing phase reads the totals before and after it ran and reports
//! the difference as a node, so waiting is attributed to the phase that
//! waited. Recording costs one thread-local addition per actual wait; nothing
//! is recorded for an uncontended entry.
use std::{cell::Cell, time::Duration};

/// What a thread waited for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// The Store writer connection's admission queue.
    Writer,
    /// A read-only Store connection's admission queue.
    Reader,
    /// A daemon read session's connection lock.
    ReadSession,
}

/// Number of [`Wait`] kinds.
pub const WAITS: usize = 3;
/// Node names of the [`Wait`] kinds, in order.
pub const WAIT_NAMES: [&str; WAITS] = ["wait.writer", "wait.reader", "wait.read_session"];

thread_local! {
    static TOTALS: Cell<[u64; WAITS]> = const { Cell::new([0; WAITS]) };
}

/// Adds `waited` to this thread's total for `kind`.
pub fn add(kind: Wait, waited: Duration) {
    let ns = waited.as_nanos().min(u64::MAX as u128) as u64;
    TOTALS.with(|totals| {
        let mut value = totals.get();
        value[kind as usize] = value[kind as usize].saturating_add(ns);
        totals.set(value);
    });
}

/// This thread's wait totals in nanoseconds, by kind.
pub fn totals() -> [u64; WAITS] {
    TOTALS.with(Cell::get)
}
