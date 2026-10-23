//! The P1 table backend: SQLite implementation of [`TreeStore`] (ADR-006 §7.1).
//!
//! This is the transitional backend. It is what P1 shipped, moved behind the trait so
//! the path rules could be written once. It disappears in P2a-7 once the overlay
//! backend covers the same surface.
//!
//! # Transactions
//!
//! `BEGIN IMMEDIATE` for writes (ADR-003 §6), `BEGIN DEFERRED` for reads. Reads used
//! to run outside any transaction, so a multi-statement rule read could straddle
//! another process's commit; wrapping them is a correction, not an optimisation.
//!
//! # Nesting
//!
//! The scopes are not reentrant. A rule that captured the outer store and called
//! `read` from inside `write` would otherwise either deadlock on the connection
//! borrow or silently open a second transaction. A thread-local flag turns that into
//! an explicit [`NamespaceError::NestedTransaction`].
//!
//! # Content
//!
//! File bytes are **not** in the trait. `content_root` and the extent rows are P1's
//! representation; the overlay has neither. They stay here as inherent methods until
//! this backend is retired.

use std::cell::{Cell, RefCell};

use rusqlite::{Connection, OptionalExtension};

use crate::db::NamespaceDb;
use crate::{Kind, NamespaceError, NamespaceResult, RootRecord};

// Per-thread flag: is a scope currently open on some backend?
//
// The callback only receives the transaction object, so nesting cannot happen through
// it — it can only happen if a rule captures the surrounding store. That is a caller
// bug, and it must surface as an error rather than a hang (ADR-006 §7.1 TX4).
thread_local! {
    static SCOPE_OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Marks a scope as open, restoring the previous state on drop.
pub(crate) struct ScopeGuard;

impl ScopeGuard {
    pub(crate) fn enter() -> NamespaceResult<Self> {
        SCOPE_OPEN.with(|open| {
            if open.get() {
                return Err(NamespaceError::NestedTransaction);
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

/// The SQLite-backed tree.
#[derive(Debug)]
pub struct SqlTree {
    name: String,
    /// The connection is behind a `RefCell` because `write(&self, …)` only has
    /// `&self`, while a `rusqlite::Transaction` borrows the connection mutably for
    /// its whole life. The nesting guard means the two borrows never overlap.
    connection: RefCell<Connection>,
    read_only: bool,
}

/// The read view: borrows the connection for the scope's duration.
pub struct SqlRead<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) name: &'a str,
}

/// The write view: owns a transaction, which commits when `write` says so.
///
/// The namespace name is **owned**, not borrowed. An earlier version held
/// `name: &'a str` taken from `self` alongside the transaction, so the view carried
/// two unrelated lifetimes and `Self::Write<'_>` could not be satisfied. `String` is a
/// clone per write scope — cheap next to the SQLite round trip, and it keeps the view
/// borrowing from exactly one place.
pub struct SqlWrite<'a> {
    pub(crate) transaction: rusqlite::Transaction<'a>,
    pub(crate) name: String,
}

impl std::fmt::Debug for SqlRead<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlRead").field("name", &self.name).finish()
    }
}

impl std::fmt::Debug for SqlWrite<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlWrite")
            .field("name", &self.name)
            .finish()
    }
}

impl SqlTree {
    /// Opens `name` in `store`'s namespace tables.
    ///
    /// # Errors
    ///
    /// See [`NamespaceDb::open`], plus [`NamespaceError::NamespaceNotFound`] when
    /// opening an existing database without a matching namespace.
    pub fn open(
        store: &cas_store::Store,
        name: &str,
        mode: cas_store::OpenMode,
    ) -> NamespaceResult<Self> {
        let db = NamespaceDb::open(store, mode)?;
        let read_only = db.is_read_only();
        let connection = RefCell::new(db.take_connection());
        let tree = Self {
            name: name.to_owned(),
            connection,
            read_only,
        };
        tree.prepare(name, mode)?;
        Ok(tree)
    }

    /// Whether this handle refuses writes.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The namespace name this tree belongs to.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Borrows the connection directly.
    pub(crate) fn conn(&self) -> std::cell::Ref<'_, Connection> {
        self.connection.borrow()
    }

    /// Borrows the connection mutably.
    pub(crate) fn conn_mut(&self) -> std::cell::RefMut<'_, Connection> {
        self.connection.borrow_mut()
    }

    /// Runs `body` inside one immediate write transaction, for the **content** paths.
    ///
    /// Content is deliberately outside [`TreeStore`] (ADR-006 §7.1): the overlay has
    /// no `content_root` and no extent rows, so putting these in the trait would force
    /// it to imitate P1's tables. That leaves them here as inherent methods that talk
    /// to the table directly, which means they also manage their own transaction.
    ///
    /// `body` returning `Err` (or panicking) drops the transaction, so a failed
    /// content write leaves no partial rows — the same guarantee `TreeStore::write`
    /// gives the rules.
    pub(crate) fn with_content_tx<T>(
        &self,
        body: impl FnOnce(&Connection) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        let _guard = ScopeGuard::enter()?;
        let mut connection = self.conn_mut();
        let transaction = connection.transaction()?;
        let value = body(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// Appends a D+ publication row on `connection` (ADR-003 §4.1).
    ///
    /// The content paths publish through this so their root record is written inside
    /// the same transaction as the content rows.
    ///
    /// Takes the label as a `&str` so the content paths keep the exact operation
    /// strings P1 recorded (`"write_at"`, `"truncate"`), which tests and the CLI read
    /// back.
    pub(crate) fn publish_content(
        &self,
        connection: &Connection,
        op: &str,
    ) -> NamespaceResult<u64> {
        crate::sql_tree_store::append_root(connection, &self.name, op)
    }

    /// The connection, for planting a deterministic fault in tests.
    ///
    /// Returns a `Ref` rather than `&Connection` because the connection lives behind
    /// a `RefCell`; [`crate::inject::plant`] accepts anything that dereferences to a
    /// connection, so the existing call sites stay unchanged (ADR-003 §12.2).
    #[cfg(feature = "failure-injection")]
    #[must_use]
    pub fn connection_for_tests(&self) -> std::cell::Ref<'_, Connection> {
        self.connection.borrow()
    }

    /// Ensures the namespace exists (creating it when allowed) and its root chain is
    /// intact.
    fn prepare(&self, name: &str, mode: cas_store::OpenMode) -> NamespaceResult<()> {
        let exists: Option<i64> = self
            .connection
            .borrow()
            .query_row(
                "SELECT head_root FROM namespace WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        match (exists, mode) {
            (Some(_), _) => self.validate_root_chain(name)?,
            (None, cas_store::OpenMode::Create) if !self.read_only => self.initialize(name)?,
            (None, _) => {
                return Err(NamespaceError::NamespaceNotFound {
                    name: name.to_owned(),
                })
            }
        }
        Ok(())
    }

    /// Creates the empty tree: a root directory record, the first publication and
    /// the identity allocator.
    fn initialize(&self, name: &str) -> NamespaceResult<()> {
        let now = crate::namespace::now_ns();
        let connection = self.connection.borrow();
        connection.execute(
            "INSERT INTO file (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
             VALUES (?1, 0, ?2, 0, NULL, ?3, ?3)",
            (name, Kind::Directory as i64, now),
        )?;
        connection.execute(
            "INSERT INTO file_id_allocator (namespace, next_serial) VALUES (?1, 1)",
            [name],
        )?;
        connection.execute(
            "INSERT INTO namespace (name, head_root) VALUES (?1, 0)",
            [name],
        )?;
        // The init publication has no parent: it is the start of this namespace's
        // chain. `root_id` is a **global** surrogate key (the `root` table is shared
        // between namespaces), so it must come from MAX+1 like every other row —
        // hardcoding 0 collides the moment a store holds a second namespace.
        let root_id: i64 = connection.query_row(
            "SELECT COALESCE(MAX(root_id) + 1, 0) FROM root",
            [],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT INTO root (root_id, namespace, parent_root_id, op, created_ns)
             VALUES (?1, ?2, NULL, 'init', ?3)",
            (root_id, name, now),
        )?;
        connection.execute(
            "UPDATE namespace SET head_root = ?1 WHERE name = ?2",
            (root_id, name),
        )?;
        Ok(())
    }

    /// Checks that the D+ root chain is well formed (ADR-003 §4.1).
    ///
    /// Both counts come from **one** statement on purpose. Reading them in two
    /// queries is not atomic against another connection's commit: a row inserted
    /// between them makes the second count exceed the first and the difference goes
    /// negative. One statement runs in one implicit transaction, so the two numbers
    /// describe the same state (TX3).
    fn validate_root_chain(&self, name: &str) -> NamespaceResult<()> {
        let connection = self.connection.borrow();
        let (rows, linked): (i64, i64) = connection.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE
                        WHEN r.parent_root_id IS NULL
                             OR EXISTS (SELECT 1 FROM root p
                                        WHERE p.namespace = r.namespace
                                          AND p.root_id = r.parent_root_id)
                        THEN 1 ELSE 0 END), 0)
             FROM root r WHERE r.namespace = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if rows != linked {
            return Err(NamespaceError::InvalidRecord {
                field: "root",
                detail: format!("{} of {rows} rows have a missing parent", rows - linked),
            });
        }
        Ok(())
    }
}

impl SqlTree {
    /// The current publication serial, if any (ADR-003 §4.1).
    ///
    /// D+ is the **table backend's** representation, not part of the tree contract
    /// (ADR-006 §7.1), so these accessors live here and are reached through
    /// `Namespace::backend()`.
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

    /// Every publication record in this namespace, oldest first.
    ///
    /// The acceptance matrix (S4) needs the whole chain to assert that earlier records
    /// are byte-for-byte unchanged after later writes.
    ///
    /// # Errors
    ///
    /// Store failures while reading the table.
    pub fn root_records(&self) -> NamespaceResult<Vec<RootRecord>> {
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT root_id, parent_root_id, op, created_ns
             FROM root WHERE namespace = ?1 ORDER BY root_id",
        )?;
        let rows = statement.query_map([&self.name], |row| {
            Ok(RootRecord {
                root_id: row.get::<_, i64>(0)? as u64,
                parent_root_id: row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                op: row.get(2)?,
                created_ns: row.get(3)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
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
}
