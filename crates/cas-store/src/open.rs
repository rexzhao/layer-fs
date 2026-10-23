//! Opening, creating and recovering a Store (ADR-002 §7).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{StoreError, StoreResult};
use crate::layout::Layout;
use crate::schema;

/// Whether a Store may be created, written or only read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Create the Store if absent, then allow reads and writes.
    Create,
    /// Open an existing Store for reads and writes.
    ReadWrite,
    /// Open an existing Store for reads only; every write is refused.
    ReadOnly,
}

impl OpenMode {
    fn may_write(self) -> bool {
        !matches!(self, OpenMode::ReadOnly)
    }

    fn sqlite_flags(self) -> OpenFlags {
        match self {
            // `open_with_flags` does not add CREATE on its own, so a new Store
            // database would fail to open without it.
            OpenMode::Create => {
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
            }
            OpenMode::ReadWrite => {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
            }
            OpenMode::ReadOnly => {
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
            }
        }
    }
}

/// What `open` had to do to bring the Store to a usable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    /// Whether the Store directory and database were created.
    pub created: bool,
    /// How many stale files were removed from `tmp/`.
    pub temporaries_removed: usize,
}

/// The Store's SQLite connection, wrapped so the owning `Store` can derive
/// `Debug` without exposing the connection itself.
pub(crate) struct Database {
    pub(crate) connection: Connection,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish_non_exhaustive()
    }
}

/// Opens the Store at `root` in `mode`, running the recovery protocol.
///
/// Order (ADR-002 §7): open/create the directory shape, open SQLite, validate
/// the schema and policy, clear `tmp/`. Orphaned object files are left alone
/// (ADR-002 §10).
pub(crate) fn open(root: &Path, mode: OpenMode) -> StoreResult<(Database, Layout, Opened)> {
    let layout = Layout::new(root);
    let already_present = layout.database().is_file();

    // `Create` may build the directory shape; ReadWrite/ReadOnly require it to
    // exist, so a typo in a path is reported instead of materialised.
    layout
        .ensure(mode == OpenMode::Create || already_present)
        .map_err(|error| {
            StoreError::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "preparing store directories under {}: {error}",
                    layout.root().display()
                ),
            ))
        })?;

    let mut connection = Connection::open_with_flags(layout.database(), mode.sqlite_flags())?;
    configure(&connection)?;

    if already_present {
        schema::validate(&connection)?;
    } else {
        if !mode.may_write() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "no store at {}: {} does not exist",
                    layout.root().display(),
                    layout.database().display()
                ),
            )));
        }
        schema::create(&mut connection)?;
    }

    let temporaries_removed = if mode.may_write() {
        clear_temporaries(&layout)?
    } else {
        0
    };

    Ok((
        Database { connection },
        layout,
        Opened {
            created: !already_present,
            temporaries_removed,
        },
    ))
}

/// Applies the connection profile every Store connection uses.
///
/// `synchronous=FULL` is chosen deliberately: ADR-002 §9 requires a committed
/// index change to be durable, which is what makes "index commit == publish"
/// meaningful across a process crash.
fn configure(connection: &Connection) -> StoreResult<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "busy_timeout", 0)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Removes every file left in `tmp/` by a crash.
///
/// Safe unconditionally: nothing in `tmp/` was ever published, because
/// publication is the index commit that follows the rename out of `tmp/`
/// (ADR-002 §5).
///
/// Two openers racing here is normal (two processes starting at once), so a
/// staging file that another opener already removed, or that is being renamed
/// away this instant, is not an error: the goal is "tmp/ ends up empty", and a
/// concurrent cleanup may reach that first. A file that is still present after
/// the sweep is the only failure worth reporting, and a concurrent writer's
/// staging file that it finished publishing is exactly what must not be treated
/// as an error.
fn clear_temporaries(layout: &Layout) -> StoreResult<usize> {
    let directory = layout.temporaries();
    let mut removed = 0;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(StoreError::Io(error)),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            // Reachable only under concurrency: another process may remove the
            // same staging file between the directory read and this call, or hold
            // it briefly on Windows. Not covered by a single-process test; the
            // multi-process crash suite exercises the path. Exempted from coverage
            // in docs/reports/p1-baseline.md §9.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    Ok(removed)
}
