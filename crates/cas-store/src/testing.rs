//! Publication with a named crash point, for the crash-injection tests.
//!
//! This module contains **no publication sequence of its own**. It calls the
//! same `object::put_observed` entry point the production `Store::put` calls and
//! passes an observer that terminates the process when its step arrives, so the
//! code under test is the code that ships: a change to the staging, rename,
//! directory-sync or index-commit order is exercised here automatically.
//!
//! The only thing this module adds is the observer, and the only thing the
//! observer does is [`std::process::abort`] — a real process death with no
//! unwinding and no destructors, so nothing the code did not explicitly make
//! durable can reach stable storage behind our back.
//!
//! Only compiled with the `crash-points` feature (ADR-002 §13.1), which this
//! crate enables for its own tests and which no consumer gets by default.

use cas_core::Role;

use crate::object::PublishStep;
use crate::{OpenMode, Store, StoreResult};

/// The publication steps a crash can be injected at, in order.
///
/// Every variant maps onto one [`PublishStep`], which is the production
/// sequence; there is no separate list of "test steps" that could drift from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// The staging file has been written but not synced.
    Staged,
    /// The staging file has been synced and is durable.
    StagedSynced,
    /// The staging file has been renamed into `objects/`; no index row yet.
    Renamed,
    /// The parent directory has been synced; still no index row.
    DirectorySynced,
    /// The index row has been committed: the object is published.
    Committed,
}

impl CrashPoint {
    /// The production step this point observes.
    #[must_use]
    pub fn step(self) -> PublishStep {
        match self {
            CrashPoint::Staged => PublishStep::Staged,
            CrashPoint::StagedSynced => PublishStep::StagedSynced,
            CrashPoint::Renamed => PublishStep::Renamed,
            CrashPoint::DirectorySynced => PublishStep::DirectorySynced,
            CrashPoint::Committed => PublishStep::Committed,
        }
    }

    /// Parses the name used by the crash tests.
    #[must_use]
    pub fn parse(step: &str) -> Option<CrashPoint> {
        match step {
            "staged" => Some(CrashPoint::Staged),
            "synced" => Some(CrashPoint::StagedSynced),
            "renamed" => Some(CrashPoint::Renamed),
            "dirsynced" => Some(CrashPoint::DirectorySynced),
            "committed" => Some(CrashPoint::Committed),
            _ => None,
        }
    }
}

fn crash(at: CrashPoint) -> ! {
    // A real, unmaskable process death: no unwinding, so nothing the code did
    // not explicitly sync can reach stable storage behind our back.
    //
    // Announce on stderr, which is unbuffered, and flush it: `abort` runs no
    // destructors, so a buffered stdout line would be lost and the parent
    // waiting for a "step reached" signal would never see it.
    eprintln!("STEP {at:?}");
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
    std::process::abort();
}

/// Publishes an object, stopping the process at `step`.
///
/// Every defined crash point aborts, so this function only returns by exiting
/// the process.
///
/// # Panics
///
/// Panics when `step` is not a known crash point name.
pub fn put_with_crash_point(store: &Store, role: Role, value: &[u8], step: &str) -> ! {
    let at = CrashPoint::parse(step).expect("known crash point");
    let target_step = at.step();
    let mut observer = |observed: PublishStep| {
        if observed == target_step {
            crash(at);
        }
    };

    // The production sequence, exactly: `Store::put` is this same call with the
    // no-op observer.
    match store.put_observed(role, value, &mut observer) {
        Ok(published) => {
            // Unreachable while every crash point aborts; if a point were
            // removed, report it instead of silently "passing".
            eprintln!("STEP not-reached: {step} (outcome {:?})", published.outcome);
            std::process::exit(4)
        }
        Err(error) => {
            eprintln!("put failed before the crash point: {error}");
            std::process::exit(3)
        }
    }
}

/// Opens a Store for a crash test child, creating it when absent.
///
/// # Errors
///
/// Propagates open failures.
pub fn open_for_child(root: &std::path::Path) -> StoreResult<Store> {
    if root.join("store.sqlite").is_file() {
        Store::open(root, OpenMode::ReadWrite)
    } else {
        Store::open(root, OpenMode::Create)
    }
}
