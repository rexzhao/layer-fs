//! The main database handle: schema creation, validation and policy (ADR-006 §4, §11.1).
//!
//! This is a **second** connection to `store.sqlite` beside cas-store's own. They are
//! separate because they have different jobs and different durability needs; the cost is
//! that a workspace's publication cannot be one transaction with the object writes, which
//! is exactly why ADR-006 §9 orders them (objects first, then the referencing rows).
//!
//! The connection sits behind a mutex so a handle can be shared: `rusqlite::Connection`
//! is `Send` but not `Sync`, and a workspace open on one thread must not make the whole
//! repository unusable on another.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::schema;

/// The workspace layer's view of one store.
#[derive(Debug)]
pub struct MainDb {
    connection: Mutex<Connection>,
    metadata_path: PathBuf,
}

impl MainDb {
    /// Opens the store's metadata database, creating the workspace tables when `create`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotInitialised`] when the tables are absent and `create` is
    /// false, [`WorkspaceError::SchemaMismatch`] when another build wrote them.
    pub fn open(metadata_path: impl AsRef<Path>, create: bool) -> WorkspaceResult<Self> {
        let metadata_path = metadata_path.as_ref().to_path_buf();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(&metadata_path, flags)?;
        // F2 / §11.1: the main database's connections wait a bounded time for a competing
        // short transaction. The overlay connection deliberately does not (it is
        // exclusive, so contention there is a bug).
        connection.busy_timeout(std::time::Duration::from_millis(
            schema::MAIN_BUSY_TIMEOUT_MS,
        ))?;
        let db = Self {
            connection: Mutex::new(connection),
            metadata_path,
        };
        db.prepare(create)?;
        Ok(db)
    }

    /// The path this handle was opened on.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.metadata_path
    }

    /// Takes the connection lock.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One policy value.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::SchemaMismatch`] when the key is missing.
    pub fn policy(&self, key: &str) -> WorkspaceResult<String> {
        policy(&self.lock(), key)
    }

    fn prepare(&self, create: bool) -> WorkspaceResult<()> {
        let connection = self.lock();
        let exists: bool = connection.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'workspace_policy'",
            [],
            |row| row.get(0),
        )?;
        if exists {
            return validate(&connection);
        }
        if !create {
            return Err(WorkspaceError::NotInitialised);
        }
        for statement in schema::DDL {
            connection.execute(statement, [])?;
        }
        for (key, value) in [
            ("schema_version", schema::SCHEMA_VERSION.to_string()),
            ("ddl", schema::ddl_text()),
            ("snapshot_profile", hex(&fs_snapshot::Profile::V1.id())),
            ("overlay_page_bytes", schema::OVERLAY_PAGE_BYTES.to_string()),
            (
                "main_busy_timeout_ms",
                schema::MAIN_BUSY_TIMEOUT_MS.to_string(),
            ),
        ] {
            connection.execute(
                "INSERT INTO workspace_policy (key, value) VALUES (?1, ?2)",
                (key, value),
            )?;
        }
        Ok(())
    }
}

/// Reads one policy row.
pub(crate) fn policy(connection: &Connection, key: &str) -> WorkspaceResult<String> {
    connection
        .query_row(
            "SELECT value FROM workspace_policy WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| WorkspaceError::SchemaMismatch {
            expected: format!("workspace_policy key {key}"),
            found: "missing".to_owned(),
        })
}

/// Checks that the tables on disk are the ones this build writes.
fn validate(connection: &Connection) -> WorkspaceResult<()> {
    let version = policy(connection, "schema_version")?;
    if version != schema::SCHEMA_VERSION.to_string() {
        return Err(WorkspaceError::SchemaMismatch {
            expected: format!("schema_version {}", schema::SCHEMA_VERSION),
            found: version,
        });
    }
    let stored = policy(connection, "ddl")?;
    if stored != schema::ddl_text() {
        return Err(WorkspaceError::SchemaMismatch {
            expected: schema::ddl_text(),
            found: stored,
        });
    }
    let profile = policy(connection, "snapshot_profile")?;
    let expected = hex(&fs_snapshot::Profile::V1.id());
    if profile != expected {
        return Err(WorkspaceError::SchemaMismatch {
            expected: format!("snapshot_profile {expected}"),
            found: profile,
        });
    }
    // §11.1: the wait is recorded in the database, so a handle opened by a build with a
    // different policy is refused instead of quietly behaving differently.
    let busy = policy(connection, "main_busy_timeout_ms")?;
    if busy != schema::MAIN_BUSY_TIMEOUT_MS.to_string() {
        return Err(WorkspaceError::SchemaMismatch {
            expected: format!("main_busy_timeout_ms {}", schema::MAIN_BUSY_TIMEOUT_MS),
            found: busy,
        });
    }
    Ok(())
}

/// Lowercase hex, the form every id in this layer is stored in.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Parses exactly 32 bytes of lowercase hex, the form scopes are stored in.
pub(crate) fn unhex_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}

/// Now, in Unix epoch nanoseconds.
pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0)
}
