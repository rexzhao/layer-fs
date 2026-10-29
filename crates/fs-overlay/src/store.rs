//! Opening the overlay file: pragmas, schema validation and the exclusive lock.
//! (ADR-006 §5.1, §5.2, W2.)

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use cas_store::Store;
use fs_snapshot::Profile;

use crate::base::BaseLayer;
use crate::error::{OverlayError, OverlayResult};
use crate::schema;

// Per-thread flag: is an overlay scope open?
//
// The callback only receives the view, so nesting can only happen if a rule captures
// the store and calls back into it. That is a caller bug and must be an error, not a
// hang (TX4).
thread_local! {
    static SCOPE_OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Marks a scope open, restoring the previous state on drop.
pub(crate) struct ScopeGuard;

impl ScopeGuard {
    pub(crate) fn enter() -> OverlayResult<Self> {
        SCOPE_OPEN.with(|open| {
            if open.get() {
                return Err(OverlayError::NestedTransaction);
            }
            open.set(true);
            Ok(Self)
        })
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE_OPEN.with(|open| open.set(false));
    }
}

/// How a workspace's overlay is opened.
#[derive(Debug, Clone)]
pub struct OverlayOptions {
    /// The snapshot the workspace starts from.
    pub base_root: cas_core::ObjectId,
    /// The snapshot profile the base was written under.
    pub profile: Profile,
    /// The serial scope this workspace belongs to (ADR-006 §6).
    ///
    /// Every workspace in one lineage shares it, so their serials cannot collide. A
    /// reopened file must be handed the same value it was created with; a mismatch is a
    /// wiring error, not a recoverable state.
    pub scope_id: [u8; 32],
    /// The serial range this workspace may allocate from, `[first, end)`.
    ///
    /// Reserving it is fs-workspace's job (ADR-006 §6, `scope_allocator`). `first` is only
    /// used when the file is created; `end` is also applied when it extends an existing
    /// reservation, which is how a long-lived workspace gets more serials.
    pub serial_first: u64,
    /// One past the last serial this workspace may allocate.
    pub serial_end: u64,
}

/// A workspace's overlay: an exclusive SQLite file composed over a snapshot base.
#[derive(Debug)]
pub struct OverlayStore {
    pub(crate) connection: Mutex<Connection>,
    pub(crate) base: BaseLayer,
    pub(crate) scope_id: [u8; 32],
    path: PathBuf,
}

impl OverlayStore {
    /// Opens (or creates) the overlay file at `path` over `base`.
    ///
    /// # Errors
    ///
    /// [`OverlayError::WorkspaceInUse`] when another handle holds it,
    /// [`OverlayError::SchemaMismatch`] when the file was written by another build,
    /// [`OverlayError::PragmaNotApplied`] when a pragma did not take, or a SQLite
    /// failure.
    pub fn open(
        path: impl AsRef<Path>,
        base_store: Store,
        options: &OverlayOptions,
    ) -> OverlayResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| OverlayError::InvalidRecord {
                field: "overlay path",
                detail: error.to_string(),
            })?;
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // **A BUSY here means "another handle has this overlay open"** — the file is opened `locking_mode=EXCLUSIVE`, so the pragma fails before anything else can. Reporting
        // rusqlite's "database is locked" instead would hide the rule behind the mechanism (measured: `checkout feat` from inside a mount answered the raw message).
        if let Err(error) = apply_pragmas(&connection) {
            if let OverlayError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = &error {
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) {
                    return Err(OverlayError::WorkspaceInUse {
                        path: path.display().to_string(),
                    });
                }
            }
            return Err(error);
        }

        let mut store = Self {
            connection: Mutex::new(connection),
            base: BaseLayer::new(base_store, options.base_root, options.profile),
            scope_id: [0u8; 32],
            path,
        };
        store.initialize(options)?;
        Ok(store)
    }

    /// The overlay file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The scope this workspace's serials belong to (ADR-005 S8).
    #[must_use]
    pub fn scope_id(&self) -> [u8; 32] {
        self.scope_id
    }

    /// The base snapshot this overlay composes over.
    #[must_use]
    pub fn base_root(&self) -> cas_core::ObjectId {
        self.base.root()
    }

    /// Takes the handle lock.
    ///
    /// One mutex per handle serialises the workspace's own threads (ADR-006 §7.4); the
    /// file's `locking_mode=EXCLUSIVE` covers other processes. A poisoned mutex stays
    /// usable: a panic in one operation must not make the workspace permanently
    /// unusable, and the transaction it was in has already rolled back.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Brings the meta table into existence, or validates an existing one.
    fn initialize(&mut self, options: &OverlayOptions) -> OverlayResult<()> {
        let connection = self.lock();
        let created: bool = connection.query_row(
            "SELECT COUNT(*) = 0 FROM sqlite_master WHERE type = 'table' AND name = 'ov_meta'",
            [],
            |row| row.get(0),
        )?;
        if created {
            for statement in schema::DDL {
                connection.execute(statement, [])?;
            }
            for (key, value) in [
                ("schema_version", schema::SCHEMA_VERSION.to_string()),
                ("ddl", ddl_text()),
                ("workspace_id", String::new()),
                ("base_root", options.base_root.to_string()),
                ("scope_id", hex(&options.scope_id)),
                ("serial_next", options.serial_first.to_string()),
                ("serial_end", options.serial_end.to_string()),
                ("pending_root", String::new()),
            ] {
                connection.execute(
                    "INSERT INTO ov_meta (key, value) VALUES (?1, ?2)",
                    (key, value),
                )?;
            }
        } else {
            validate(&connection)?;
            let stored_scope = meta(&connection, "scope_id")?;
            if stored_scope != hex(&options.scope_id) {
                return Err(OverlayError::SchemaMismatch {
                    expected: format!("scope_id {}", hex(&options.scope_id)),
                    found: stored_scope,
                });
            }
            let stored_base = meta(&connection, "base_root")?;
            if stored_base != options.base_root.to_string() {
                // The one recoverable disagreement: the main database has already adopted
                // this exact root and step 4 has not run yet. `pending_root` is the record
                // of it (ADR-006 §9.3); anything else is a wiring error.
                let pending = meta(&connection, "pending_root")?;
                if pending != options.base_root.to_string() {
                    return Err(OverlayError::SchemaMismatch {
                        expected: format!("base_root {}", options.base_root),
                        found: stored_base,
                    });
                }
            }
            // A reopening workspace brings a fresh reservation; keep the larger range so
            // neither an earlier open nor this one loses serials it was given.
            let stored_end = meta(&connection, "serial_end")?.parse::<u64>().unwrap_or(0);
            if options.serial_end > stored_end {
                connection.execute(
                    "UPDATE ov_meta SET value = ?1 WHERE key = 'serial_end'",
                    [options.serial_end.to_string()],
                )?;
            }
        }
        // The file is the authority on its own base. fs-workspace's row can be one step
        // ahead of it during recovery, and `adopt_base` is what closes that gap.
        let (scope, base) = (
            meta(&connection, "scope_id")?,
            meta(&connection, "base_root")?,
        );
        drop(connection);
        self.scope_id = parse_scope(&scope)?;
        self.base.set_root(parse_base(&base)?);
        Ok(())
    }
}

fn apply_pragmas(connection: &Connection) -> OverlayResult<()> {
    for statement in schema::PRAGMAS {
        connection.execute_batch(statement)?;
    }
    let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if mode.to_ascii_lowercase() != schema::EXPECTED_JOURNAL_MODE {
        return Err(OverlayError::PragmaNotApplied {
            pragma: "journal_mode",
            found: mode,
        });
    }
    Ok(())
}

/// Checks that the file on disk is the one this build writes.
fn validate(connection: &Connection) -> OverlayResult<()> {
    let version: String = meta(connection, "schema_version")?;
    if version != schema::SCHEMA_VERSION.to_string() {
        return Err(OverlayError::SchemaMismatch {
            expected: format!("schema_version {}", schema::SCHEMA_VERSION),
            found: version,
        });
    }
    let stored: String = meta(connection, "ddl")?;
    if stored != ddl_text() {
        return Err(OverlayError::SchemaMismatch {
            expected: ddl_text(),
            found: stored,
        });
    }
    Ok(())
}

pub(crate) fn meta(connection: &Connection, key: &str) -> OverlayResult<String> {
    connection
        .query_row("SELECT value FROM ov_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| OverlayError::InvalidRecord {
            field: "ov_meta",
            detail: format!("missing key {key}"),
        })
}

/// The DDL text stored beside the data, so a schema change is detected rather than
/// discovered halfway through a read.
fn ddl_text() -> String {
    schema::DDL.join(";\n")
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Parses the stored base root.
fn parse_base(text: &str) -> OverlayResult<cas_core::ObjectId> {
    text.parse::<cas_core::ObjectId>()
        .map_err(|error| OverlayError::InvalidRecord {
            field: "ov_meta.base_root",
            detail: error.to_string(),
        })
}

fn parse_scope(text: &str) -> OverlayResult<[u8; 32]> {
    if text.len() != 64 {
        return Err(OverlayError::InvalidRecord {
            field: "ov_meta.scope_id",
            detail: format!("expected 64 hex characters, found {}", text.len()),
        });
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk).map_err(|error| OverlayError::InvalidRecord {
            field: "ov_meta.scope_id",
            detail: error.to_string(),
        })?;
        bytes[index] =
            u8::from_str_radix(pair, 16).map_err(|error| OverlayError::InvalidRecord {
                field: "ov_meta.scope_id",
                detail: error.to_string(),
            })?;
    }
    Ok(bytes)
}
