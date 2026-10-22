//! Namespace operations over the content-addressed Store (ADR-003).
//!
//! The current tree lives in mutable tables; `root` is an append-only record of
//! publications (D+, ADR-003 §4.1). Path validation happens before any touched
//! state, so an invalid path never opens a transaction.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};

use cas_store::Store;

use crate::db::NamespaceDb;
use crate::error::{NamespaceError, NamespaceResult};
use crate::path::Path;

/// What kind of entry a [`FileId`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Directory = 1,
    /// A regular file.
    File = 2,
}

impl Kind {
    pub(crate) fn from_tag(tag: i64) -> Option<Kind> {
        match tag {
            1 => Some(Kind::Directory),
            2 => Some(Kind::File),
            _ => None,
        }
    }
}

/// The logical identity of a file or directory within a namespace.
///
/// Serial 0 is the root directory; allocated serials start at 1 and are never
/// reused (ADR-003 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u64);

impl FileId {
    /// The root directory's identity.
    pub const ROOT: FileId = FileId(0);

    /// The raw serial.
    #[must_use]
    pub const fn serial(self) -> u64 {
        self.0
    }

    /// Wraps a raw serial.
    #[must_use]
    pub const fn from_serial(serial: u64) -> Self {
        Self(serial)
    }
}

/// A directory entry as `read_dir` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's name, byte-for-byte as stored.
    pub name: String,
    /// What the name refers to.
    pub id: FileId,
    /// Whether it is a directory or a file.
    pub kind: Kind,
}

/// What `stat` reports about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// The logical identity.
    pub id: FileId,
    /// Directory or file.
    pub kind: Kind,
    /// Logical size in bytes (0 for directories).
    pub size: u64,
    /// The content root, for a regular file that has content.
    pub content_root: Option<cas_core::ObjectId>,
    /// Creation time, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Last modification time, Unix epoch nanoseconds.
    pub modified_ns: i64,
}

/// One row of the append-only publication chain (ADR-003 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    /// Positional serial, allocated and never reused.
    pub root_id: u64,
    /// The previous publication, or `None` for the first one.
    pub parent_root_id: Option<u64>,
    /// The operation that caused it (`mkdir`, `rmdir`, `rename`, `unlink`, …).
    pub op: String,
    /// When it was published, Unix epoch nanoseconds.
    pub created_ns: i64,
}

/// The namespace layer over one Store.
#[derive(Debug)]
pub struct Namespace {
    db: NamespaceDb,
    pub(crate) name: String,
}

impl Namespace {
    /// Opens `name` in the namespace tables of `store`, creating the tables and
    /// the namespace when `mode` is [`cas_store::OpenMode::Create`].
    ///
    /// # Errors
    ///
    /// See [`NamespaceDb::open`]; also [`NamespaceError::NamespaceNotFound`] when
    /// opening an existing database without a matching namespace.
    pub fn open(store: &Store, name: &str, mode: cas_store::OpenMode) -> NamespaceResult<Self> {
        let db = NamespaceDb::open(store, mode)?;
        let namespace = Self {
            db,
            name: name.to_owned(),
        };
        let exists: Option<i64> = namespace
            .conn()
            .query_row(
                "SELECT head_root FROM namespace WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        match (exists, mode) {
            (Some(_), _) => namespace.validate_root_chain()?,
            (None, cas_store::OpenMode::Create) if !namespace.db.is_read_only() => {
                namespace.initialize()?
            }
            (None, _) => {
                return Err(NamespaceError::NamespaceNotFound {
                    name: name.to_owned(),
                })
            }
        }
        Ok(namespace)
    }

    pub(crate) fn conn(&self) -> &Connection {
        self.db.connection()
    }

    /// The connection, for planting a deterministic fault in tests.
    ///
    /// Available only with `failure-injection` (ADR-003 §12.2), so a consumer
    /// cannot reach the raw connection.
    #[cfg(feature = "failure-injection")]
    pub fn connection_for_tests(&self) -> &Connection {
        self.db.connection()
    }

    /// Creates the empty tree: a root directory record, the first publication
    /// and the identity allocator.
    fn initialize(&self) -> NamespaceResult<()> {
        let now = now_ns();
        self.conn().execute(
            "INSERT INTO file (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
             VALUES (?1, 0, ?2, 0, NULL, ?3, ?3)",
            (&self.name, Kind::Directory as i64, now),
        )?;
        self.conn().execute(
            "INSERT INTO file_id_allocator (namespace, next_serial) VALUES (?1, 1)",
            [&self.name],
        )?;
        self.conn().execute(
            "INSERT INTO namespace (name, head_root) VALUES (?1, 0)",
            [&self.name],
        )?;
        self.publish_tx("init", None)?;
        Ok(())
    }

    /// Runs `body` inside one immediate write transaction (ADR-003 §6).
    ///
    /// `BEGIN IMMEDIATE` matches `cas-store`'s choice: the write lock is taken up
    /// front, so two concurrent writers serialise instead of interleaving their
    /// statements. Any error from `body` drops the transaction, which rolls back
    /// — so a failed operation leaves no partial rows and no root record.
    ///
    /// This is the only way a mutating operation touches the database.
    pub(crate) fn with_write_tx<T>(
        &self,
        body: impl FnOnce(&Connection) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        // The connection was set to `Immediate` at open (`db::NamespaceDb`), so
        // this takes the write lock up front; `unchecked_transaction` borrows
        // `&self` and still rolls back on drop.
        let transaction = self.conn().unchecked_transaction()?;
        let value = body(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// The only place a root record is appended (ADR-003 §4.1, §6).
    ///
    /// Returns the new root serial. Every mutating operation calls this exactly
    /// once, inside its [`with_write_tx`](Self::with_write_tx) body; read-only
    /// and no-op paths never call it.
    pub(crate) fn publish_tx(&self, op: &str, parent: Option<u64>) -> NamespaceResult<u64> {
        let conn = self.conn();
        let head: i64 = conn.query_row(
            "SELECT head_root FROM namespace WHERE name = ?1",
            [&self.name],
            |row| row.get(0),
        )?;
        let parent = parent.or(if op == "init" {
            None
        } else {
            Some(head as u64)
        });
        // Per namespace: the root table is shared, so `head_root` names the
        // current publication and each namespace's chain is closed by its own
        // parent links. `root_id` is a global surrogate key so rows stay unique
        // across namespaces (ADR-003 §4.1, multi-namespace).
        let root_id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(root_id) + 1, 0) FROM root",
            [],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO root (root_id, namespace, parent_root_id, op, created_ns)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                root_id,
                &self.name,
                parent.map(|value| value as i64),
                op,
                now_ns(),
            ),
        )?;
        conn.execute(
            "UPDATE namespace SET head_root = ?1 WHERE name = ?2",
            (root_id, &self.name),
        )?;
        Ok(root_id as u64)
    }

    /// Checks that the publication chain is unbroken (ADR-003 §12.1 D3).
    fn validate_root_chain(&self) -> NamespaceResult<()> {
        let conn = self.conn();
        let mut current: i64 = conn.query_row(
            "SELECT head_root FROM namespace WHERE name = ?1",
            [&self.name],
            |row| row.get(0),
        )?;
        loop {
            let next: Option<Option<i64>> = conn
                .query_row(
                    "SELECT parent_root_id FROM root WHERE root_id = ?1",
                    [current],
                    |row| row.get(0),
                )
                .optional()?;
            match next.flatten() {
                Some(parent) => current = parent,
                // Either the head has no parent (empty history) or the chain
                // reached its head: both are terminal.
                None => return Ok(()),
            }
        }
    }

    /// The current publication serial, if any.
    #[must_use]
    pub fn head_root(&self) -> Option<u64> {
        self.conn()
            .query_row(
                "SELECT head_root FROM namespace WHERE name = ?1",
                [&self.name],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|value| value as u64)
    }

    /// One publication record by serial.
    #[must_use]
    pub fn root_record(&self, root_id: u64) -> Option<RootRecord> {
        self.conn()
            .query_row(
                "SELECT root_id, parent_root_id, op, created_ns
                 FROM root WHERE namespace = ?1 AND root_id = ?2",
                (&self.name, root_id as i64),
                |row| {
                    Ok(RootRecord {
                        root_id: row.get::<_, i64>(0)? as u64,
                        parent_root_id: row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                        op: row.get(2)?,
                        created_ns: row.get(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// How many publication records exist, for tests and T5 hooks.
    #[must_use]
    pub fn root_count(&self) -> u64 {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM root WHERE namespace = ?1",
                [&self.name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64
    }

    pub(crate) fn next_serial(&self) -> NamespaceResult<u64> {
        let conn = self.conn();
        let serial: i64 = conn.query_row(
            "SELECT next_serial FROM file_id_allocator WHERE namespace = ?1",
            [&self.name],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE file_id_allocator SET next_serial = next_serial + 1 WHERE namespace = ?1",
            [&self.name],
        )?;
        Ok(serial as u64)
    }

    pub(crate) fn lookup(&self, parent: FileId, name: &str) -> NamespaceResult<Option<FileId>> {
        let id: Option<i64> = self
            .conn()
            .query_row(
                "SELECT child_id FROM directory_entry
                 WHERE namespace = ?1 AND parent_id = ?2 AND name = ?3",
                (&self.name, parent.serial() as i64, name.as_bytes()),
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(|value| FileId::from_serial(value as u64)))
    }

    pub(crate) fn kind_of(&self, id: FileId) -> NamespaceResult<Option<Kind>> {
        let tag: Option<i64> = self
            .conn()
            .query_row(
                "SELECT kind FROM file WHERE namespace = ?1 AND file_id = ?2",
                (&self.name, id.serial() as i64),
                |row| row.get(0),
            )
            .optional()?;
        Ok(tag.and_then(Kind::from_tag))
    }

    pub(crate) fn resolve(&self, path: &Path) -> NamespaceResult<FileId> {
        let mut current = FileId::ROOT;
        for segment in path.segments() {
            let Some(next) = self.lookup(current, segment)? else {
                return Err(NamespaceError::NotFound {
                    path: path.display(),
                });
            };
            current = next;
        }
        Ok(current)
    }

    pub(crate) fn resolve_parent(&self, path: &Path) -> NamespaceResult<FileId> {
        let mut current = FileId::ROOT;
        for segment in path.parent_segments() {
            let Some(next) = self.lookup(current, segment)? else {
                return Err(NamespaceError::NotFound {
                    path: path.display(),
                });
            };
            if self.kind_of(next)? != Some(Kind::Directory) {
                return Err(NamespaceError::NotADirectory {
                    path: path.display(),
                });
            }
            current = next;
        }
        Ok(current)
    }
}

pub(crate) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0)
}
