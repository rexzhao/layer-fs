//! Publication with a named crash point, used only by the crash-injection tests.
//!
//! The point of this module is to make the *real* publication sequence
//! interruptible at the steps ADR-002 §5 names, without a simulation that could
//! drift from production code:
//!
//! - it calls the same [`cas_core`] framing and the same durability primitives
//!   (`sync_file`, `rename_replace`, `sync_directory`) the `Store` uses;
//! - it writes to the same paths through [`Store::layout`];
//! - at the named step it terminates its own process with
//!   [`std::process::abort`], which is a genuine process death — no unwinding,
//!   no destructors, no flush of anything the code did not explicitly make
//!   durable.
//!
//! The only code it does not share with `Store::put` is the ordering wrapper
//! itself, because the production entry point deliberately contains no
//! "stop here" branch.

use std::fs::File;
use std::io::Write;

use cas_core::{Object, Role};

use crate::durability::{rename_replace, sync_directory, sync_file};
use crate::layout::temporary_name;
use crate::{OpenMode, Store, StoreResult};

/// The publication steps a crash can be injected at, in order.
///
/// `before-commit` is the step immediately preceding the index transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// The staged file has been written but not synced.
    Staged,
    /// The staged file has been synced and is durable.
    Synced,
    /// The staged file has been renamed into `objects/`.
    Renamed,
    /// The object file exists; the index row has not been committed.
    BeforeCommit,
}

impl CrashPoint {
    /// Parses the name used by the crash tests.
    #[must_use]
    pub fn parse(step: &str) -> Option<CrashPoint> {
        match step {
            "staged" => Some(CrashPoint::Staged),
            "synced" => Some(CrashPoint::Synced),
            "renamed" => Some(CrashPoint::Renamed),
            "before-commit" => Some(CrashPoint::BeforeCommit),
            _ => None,
        }
    }
}

fn crash(at: CrashPoint) -> ! {
    // A real, unmaskable process death: no unwinding, so nothing that the code
    // did not explicitly sync can reach stable storage behind our back.
    //
    // Announce on stderr, which is unbuffered, and flush it: `abort` skips every
    // destructor, so a buffered stdout line would be lost and the parent waiting
    // for a "step reached" signal would never see it.
    eprintln!("STEP {at:?}");
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
    std::process::abort();
}

/// Publishes an object, stopping the process at `step`.
///
/// Every defined crash point aborts, so this function never returns normally;
/// it is declared `!`-shaped by returning `StoreResult<PutOutcome>` only to keep
/// the signature uniform with a future non-crashing step.
///
/// # Errors
///
/// Propagates framing and storage failures.
pub fn put_with_crash_point(store: &Store, role: Role, value: &[u8], step: &str) -> ! {
    let at = CrashPoint::parse(step).expect("known crash point");
    let object = Object::new(role, value).unwrap_or_else(|error| {
        eprintln!("framing failed: {error}");
        std::process::exit(3)
    });
    let frame = object.to_vec();
    let oid = object.id().unwrap_or_else(|error| {
        eprintln!("identity failed: {error}");
        std::process::exit(3)
    });

    let layout = store.layout();
    let directory = layout.object_directory(oid);
    std::fs::create_dir_all(&directory).unwrap_or_else(|error| {
        eprintln!("mkdir failed: {error}");
        std::process::exit(3)
    });
    let target = directory.join(oid.to_string());
    let staged = layout.temporaries().join(temporary_name());

    {
        let mut file = File::create(&staged).unwrap_or_else(|error| {
            eprintln!("stage failed: {error}");
            std::process::exit(3)
        });
        file.write_all(&frame).unwrap_or_else(|error| {
            eprintln!("write failed: {error}");
            std::process::exit(3)
        });
        if at == CrashPoint::Staged {
            crash(at);
        }
        sync_file(&file).unwrap_or_else(|error| {
            eprintln!("sync failed: {error}");
            std::process::exit(3)
        });
        if at == CrashPoint::Synced {
            crash(at);
        }
    }
    rename_replace(&staged, &target).unwrap_or_else(|error| {
        eprintln!("rename failed: {error}");
        std::process::exit(3)
    });
    sync_directory(&directory).unwrap_or_else(|error| {
        eprintln!("dir sync failed: {error}");
        std::process::exit(3)
    });
    crash(at);
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
