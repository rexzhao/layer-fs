//! The publication probe: an event log of what a batch actually did on disk (ADR-008 §5).
//!
//! Compiled only with the `sync-probe` feature, which also turns on `rusqlite/trace`; a
//! production consumer never gets it, so the write path is unchanged without it. The hooks
//! live **inside the production functions** (`durability`, `pack::write`, `Batch`) and all
//! they do is append to this log under a global sequence — no I/O, no sleeping, no
//! transaction of their own. The transaction boundaries do not come from this module at all:
//! they come from SQLite's own `trace` callback, which is the independent witness the
//! acceptance criteria ask for.
//!
//! What the log is for: proving two properties that no amount of green tests would otherwise
//! distinguish from "we never measured" —
//!
//! 1. **every `fsync` happens outside the index transaction** (KI-7), and
//! 2. **one pack costs one file sync, one directory sync and one short transaction**, so the
//!    write lock's duration does not grow with the number of objects.

use std::sync::Mutex;
#[cfg(feature = "sync-probe")]
use std::sync::OnceLock;

/// One observable event, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A staging attempt starts. Failed attempts (a concurrent opener swept `tmp/`) are
    /// reported separately so they cannot pollute the per-pack counts: the acceptance
    /// criteria count *published* packs, and a failed attempt publishes nothing.
    Attempt(u32),
    /// A pack's bytes were written to `tmp/` (frames + tail), not yet synced.
    PackWrite,
    /// `durability::sync_file` was called.
    SyncFile,
    /// `durability::rename_replace` was called (staging pack into `packs/`).
    Rename,
    /// `durability::sync_directory` was called.
    SyncDir,
    /// SQLite reported a statement (from `trace`); `BEGIN`/`COMMIT` appear here too.
    Sql(String),
}

impl Event {
    /// Whether this is a durability event (`PackWrite`/`SyncFile`/`Rename`/`SyncDir`).
    ///
    /// These are the ones the criteria say must never fall inside a transaction.
    #[must_use]
    pub fn is_durability(&self) -> bool {
        matches!(
            self,
            Event::PackWrite | Event::SyncFile | Event::Rename | Event::SyncDir
        )
    }

    /// The SQL text, if this is a statement event.
    #[must_use]
    pub fn sql(&self) -> Option<&str> {
        match self {
            Event::Sql(sql) => Some(sql),
            _ => None,
        }
    }

    /// Whether this reads as the start of a transaction.
    #[must_use]
    pub fn is_begin(&self) -> bool {
        self.sql().is_some_and(|sql| {
            let sql = sql.trim_start().to_ascii_uppercase();
            sql.starts_with("BEGIN")
        })
    }

    /// Whether this is specifically `BEGIN IMMEDIATE` (ADR-008 §5 step 3).
    ///
    /// Checked separately because "some BEGIN happened" would let a `DEFERRED` regression pass
    /// silently until something reads before writing inside the transaction.
    #[must_use]
    pub fn is_begin_immediate(&self) -> bool {
        self.sql().is_some_and(|sql| {
            let sql = sql.trim().to_ascii_uppercase();
            sql.starts_with("BEGIN") && sql.contains("IMMEDIATE")
        })
    }

    /// Whether this reads as the end of a transaction.
    #[must_use]
    pub fn is_commit(&self) -> bool {
        self.sql().is_some_and(|sql| {
            let sql = sql.trim_start().to_ascii_uppercase();
            sql.starts_with("COMMIT") || sql.starts_with("END")
        })
    }
}

/// A guard that keeps probe-reading tests from observing each other.
///
/// The log and the timings are process-global, so a test that resets them and then reads them
/// must not run beside another one doing the same: libtest runs a binary's tests in parallel.
/// Holding this guard for the whole reset → act → read sequence is what makes the observation
/// atomic, and putting it here (rather than in one test file) means a new test cannot forget.
#[cfg(any(feature = "sync-probe", feature = "lock-probe"))]
pub fn serialised() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(feature = "sync-probe")]
fn log() -> &'static Mutex<Vec<Event>> {
    static LOG: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Starts a staging attempt and records its number (1-based, process-wide).
#[cfg(feature = "sync-probe")]
pub(crate) fn begin_attempt() {
    static ATTEMPT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let number = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    record(Event::Attempt(number));
}

/// Appends one event. Called from the production functions; does nothing else.
#[cfg(feature = "sync-probe")]
pub(crate) fn record(event: Event) {
    if let Ok(mut log) = log().lock() {
        log.push(event);
    }
}

/// Empties the log, so a test observes only its own batch.
#[cfg(feature = "sync-probe")]
pub fn reset() {
    if let Ok(mut log) = log().lock() {
        log.clear();
    }
}

/// Everything recorded since the last [`reset`].
#[must_use]
#[cfg(feature = "sync-probe")]
pub fn events() -> Vec<Event> {
    log().lock().map(|log| log.clone()).unwrap_or_default()
}

/// Whether the next transaction should contain an extra `sync_file` (the positive control).
#[cfg(feature = "sync-probe")]
static INJECT_SYNC: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();

#[cfg(feature = "sync-probe")]
fn injection() -> &'static Mutex<Option<std::path::PathBuf>> {
    INJECT_SYNC.get_or_init(|| Mutex::new(None))
}

/// Arms the positive control: the **next** index transaction calls `sync_file` on `scratch`
/// from inside itself.
///
/// Without this, "no sync appears inside a transaction" could simply mean the probe is not
/// wired up. With it, the same checker that passes for a real batch must fail, which is what
/// makes the negative result meaningful.
#[cfg(feature = "sync-probe")]
pub fn inject_sync_inside_next_transaction(scratch: impl Into<std::path::PathBuf>) {
    if let Ok(mut slot) = injection().lock() {
        *slot = Some(scratch.into());
    }
}

/// Timing of the index transactions, so "the lock's duration does not grow with the number of
/// objects" is measured rather than asserted (ADR-008 §5, acceptance D).
static TXN_START: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TXN_MAX_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TXN_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Called right after `BEGIN IMMEDIATE`.
pub(crate) fn note_transaction_begin() {
    TXN_START.store(now_nanos(), std::sync::atomic::Ordering::Relaxed);
}

/// Called right after `COMMIT`, recording how long the write lock was held.
pub(crate) fn note_transaction_end() {
    let start = TXN_START.load(std::sync::atomic::Ordering::Relaxed);
    if start != 0 {
        let elapsed = now_nanos().saturating_sub(start);
        TXN_MAX_NANOS.fetch_max(elapsed, std::sync::atomic::Ordering::Relaxed);
        TXN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The longest single transaction observed since the last [`reset_timings`], in nanoseconds.
#[must_use]
pub fn max_transaction_nanos() -> u64 {
    TXN_MAX_NANOS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many transactions were timed since the last [`reset_timings`].
#[must_use]
pub fn transaction_count() -> u64 {
    TXN_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Clears the timing accumulators.
pub fn reset_timings() {
    TXN_MAX_NANOS.store(0, std::sync::atomic::Ordering::Relaxed);
    TXN_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Called by the write path right after `BEGIN`; performs the injection when armed.
#[cfg(feature = "sync-probe")]
pub(crate) fn run_injection() {
    let armed = injection()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .filter(|_| true);
    if let Some(path) = armed {
        if let Ok(file) = std::fs::File::open(path) {
            // Deliberately the *same* primitive the production path uses, so the checker sees
            // a real `SyncFile` event inside the transaction.
            let _ = crate::durability::sync_file(&file);
        }
    }
}
