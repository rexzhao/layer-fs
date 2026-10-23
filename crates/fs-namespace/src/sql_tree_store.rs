//! [`TreeStore`] for the SQLite backend (ADR-006 §7.1).

use rusqlite::{Connection, OptionalExtension};

use crate::sql_tree::{ScopeGuard, SqlRead, SqlTree, SqlWrite};
use crate::tree_store::{OpLabel, TreeRead, TreeStore, TreeWrite};
use crate::{FileId, InodeView, Kind, NamespaceError, NamespaceResult};

impl TreeStore for SqlTree {
    type Read<'a> = SqlRead<'a>;
    type Write<'a> = SqlWrite<'a>;

    fn read<T>(
        &self,
        body: impl FnOnce(&Self::Read<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.conn();
        // ADR-006 §7.1: reads run inside `BEGIN DEFERRED`, so every statement in the
        // body sees one state. P1 ran them outside any transaction, which let a
        // multi-statement rule read straddle another writer's commit.
        connection.execute_batch("BEGIN DEFERRED")?;
        let read = SqlRead {
            connection: &connection,
            name: self.name(),
        };
        let outcome = body(&read);
        // Close the scope whatever the body decided; a failed statement already
        // leaves nothing to roll back, and swallowing the result would hide the
        // body's error behind the COMMIT's.
        let _ = connection.execute_batch("COMMIT");
        outcome
    }

    fn write<T>(
        &self,
        op: OpLabel,
        body: impl FnOnce(&mut Self::Write<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        let _guard = ScopeGuard::enter()?;
        // The guard must be bound to a local *before* the transaction is created: a
        // chained `self.conn_mut().transaction()` would make the `RefMut` a temporary
        // that dies at the end of the statement, while the transaction outlives it
        // (E0597/E0716).
        let mut connection = self.conn_mut();
        let transaction = connection.transaction()?;
        // The view borrows **only** from the transaction; the namespace name is owned,
        // so `Self::Write<'_>` has a single source lifetime.
        let mut write = SqlWrite {
            transaction,
            name: self.name().to_owned(),
        };
        // TX1: an `Err` from the body drops the transaction, rolling it back; a panic
        // unwinds through the same drop.
        let value = body(&mut write)?;
        // TX5: the label names the change for the P1 root chain. It must be appended
        // **inside** the same transaction — a data commit with no root row would
        // leave the D+ chain describing a tree that never existed.
        append_root(&write.transaction, self.name(), op.as_str())?;
        write.transaction.commit()?;
        Ok(value)
    }
}

/// Appends one row to the D+ root chain (ADR-003 §4.1).
///
/// This is the backend's own bookkeeping — the reason [`OpLabel`] exists. It runs
/// inside the same transaction as the change it describes, because the root row names
/// what the committed tree is.
///
/// The label is a `&str` rather than an [`OpLabel`] so the content paths can keep the
/// exact strings P1 recorded (`"write_at"`, `"truncate"`): those are audit data that
/// existing tests and the CLI read back.
pub(crate) fn append_root(connection: &Connection, name: &str, op: &str) -> NamespaceResult<u64> {
    let head: i64 = connection.query_row(
        "SELECT head_root FROM namespace WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?;
    let root_id: i64 = connection.query_row(
        "SELECT COALESCE(MAX(root_id) + 1, 0) FROM root",
        [],
        |row| row.get(0),
    )?;
    connection.execute(
        "INSERT INTO root (root_id, namespace, parent_root_id, op, created_ns)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (root_id, name, Some(head), op, crate::namespace::now_ns()),
    )?;
    connection.execute(
        // `head_root` names the row just inserted, so it takes `root_id` — **not**
        // `head + 1`. With more than one namespace the global `root_id` sequence and
        // each namespace's head diverge, and writing `head + 1` makes the chain point
        // at another namespace's row (validate_root_chain then reports a missing
        // parent).
        "UPDATE namespace SET head_root = ?1 WHERE name = ?2",
        (root_id, name),
    )?;
    Ok(root_id as u64)
}

// ------------------------------------------------------------------ TreeRead

impl SqlRead<'_> {
    /// The shared `lookup` body, used by both scopes.
    pub(crate) fn lookup_impl(
        connection: &Connection,
        name: &str,
        parent: FileId,
        entry_name: &[u8],
    ) -> NamespaceResult<Option<FileId>> {
        let found: Option<i64> = connection
            .query_row(
                "SELECT child_id FROM directory_entry
                 WHERE namespace = ?1 AND parent_id = ?2 AND name = ?3",
                (name, parent.serial() as i64, entry_name),
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.map(|serial| FileId::from_serial(serial as u64)))
    }

    pub(crate) fn inode_impl(
        connection: &Connection,
        name: &str,
        id: FileId,
    ) -> NamespaceResult<Option<InodeView>> {
        let found = connection
            .query_row(
                "SELECT kind, size, created_ns, modified_ns FROM file
                 WHERE namespace = ?1 AND file_id = ?2",
                (name, id.serial() as i64),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind_tag, size, created_ns, modified_ns)) = found else {
            return Ok(None);
        };
        let Some(kind) = Kind::from_tag(kind_tag) else {
            return Err(NamespaceError::InvalidRecord {
                field: "file.kind",
                detail: format!("unknown kind {kind_tag}"),
            });
        };
        Ok(Some(InodeView {
            id,
            kind,
            size: size as u64,
            created_ns,
            modified_ns,
        }))
    }

    pub(crate) fn read_dir_impl(
        connection: &Connection,
        name: &str,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        let after = after.unwrap_or(&[]);
        let mut statement = connection.prepare(
            "SELECT name, child_id FROM directory_entry
             WHERE namespace = ?1 AND parent_id = ?2 AND name > ?3
             ORDER BY name LIMIT ?4",
        )?;
        let rows = statement
            .query_map((name, dir.serial() as i64, after, limit as i64), |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })?;
        let mut entries = Vec::new();
        for row in rows {
            let (entry_name, child) = row?;
            entries.push((entry_name, FileId::from_serial(child as u64)));
        }
        Ok(entries)
    }

    pub(crate) fn is_dir_empty_impl(
        connection: &Connection,
        name: &str,
        dir: FileId,
    ) -> NamespaceResult<bool> {
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM directory_entry WHERE namespace = ?1 AND parent_id = ?2",
            (name, dir.serial() as i64),
            |row| row.get(0),
        )?;
        Ok(count == 0)
    }
}

impl TreeRead for SqlRead<'_> {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>> {
        Self::lookup_impl(self.connection, self.name, parent, name)
    }

    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>> {
        Self::inode_impl(self.connection, self.name, id)
    }

    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        Self::read_dir_impl(self.connection, self.name, dir, after, limit)
    }

    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool> {
        Self::is_dir_empty_impl(self.connection, self.name, dir)
    }
}

// ------------------------------------------------------------------ TreeWrite

impl TreeRead for SqlWrite<'_> {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>> {
        SqlRead::lookup_impl(&self.transaction, &self.name, parent, name)
    }

    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>> {
        SqlRead::inode_impl(&self.transaction, &self.name, id)
    }

    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        SqlRead::read_dir_impl(&self.transaction, &self.name, dir, after, limit)
    }

    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool> {
        SqlRead::is_dir_empty_impl(&self.transaction, &self.name, dir)
    }
}

impl TreeWrite for SqlWrite<'_> {
    fn bind(&mut self, parent: FileId, name: &[u8], child: FileId) -> NamespaceResult<()> {
        bind_on(&self.transaction, &self.name, parent, name, child)
    }

    fn unbind(&mut self, parent: FileId, name: &[u8]) -> NamespaceResult<()> {
        self.transaction.execute(
            "DELETE FROM directory_entry WHERE namespace = ?1 AND parent_id = ?2 AND name = ?3",
            (&self.name, parent.serial() as i64, name),
        )?;
        Ok(())
    }

    fn create_inode(&mut self, kind: Kind) -> NamespaceResult<FileId> {
        create_inode_on(&self.transaction, &self.name, kind)
    }

    fn drop_inode(&mut self, id: FileId) -> NamespaceResult<()> {
        // A1's lesson, made structural: the cascade lives with the backend, so no
        // caller can drop the inode and forget what hangs off it.
        self.transaction.execute(
            "DELETE FROM file_extent WHERE namespace = ?1 AND file_id = ?2",
            (&self.name, id.serial() as i64),
        )?;
        self.transaction.execute(
            "DELETE FROM file WHERE namespace = ?1 AND file_id = ?2",
            (&self.name, id.serial() as i64),
        )?;
        Ok(())
    }
}

/// Allocates a serial and inserts its inode row (ADR-003 §4.3).
///
/// Shared by [`TreeWrite::create_inode`] and the content paths, which allocate
/// against a raw connection inside their own transaction.
pub(crate) fn create_inode_on(
    connection: &Connection,
    name: &str,
    kind: Kind,
) -> NamespaceResult<FileId> {
    let serial: i64 = connection.query_row(
        "UPDATE file_id_allocator SET next_serial = next_serial + 1
         WHERE namespace = ?1 RETURNING next_serial - 1",
        [name],
        |row| row.get(0),
    )?;
    let now = crate::namespace::now_ns();
    connection.execute(
        "INSERT INTO file (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
         VALUES (?1, ?2, ?3, 0, NULL, ?4, ?4)",
        (name, serial, kind as i64, now),
    )?;
    Ok(FileId::from_serial(serial as u64))
}

/// Binds a name to a child on `connection` (ADR-003 §3.2).
///
/// P2-0a: the uniqueness check **is** the insert. Translating the constraint
/// violation here rather than checking first removes the check-then-use window that
/// let two concurrent creators both pass the check, with one of them then surfacing a
/// raw SQLite message instead of `AlreadyExists`.
///
/// The error carries the name because only a parent id and a name are known here; the
/// rule layer knows the full path and replaces it via `map_err`.
pub(crate) fn bind_on(
    connection: &Connection,
    namespace: &str,
    parent: FileId,
    name: &[u8],
    child: FileId,
) -> NamespaceResult<()> {
    let result = connection.execute(
        "INSERT INTO directory_entry (namespace, parent_id, name, child_id)
         VALUES (?1, ?2, ?3, ?4)",
        (
            namespace,
            parent.serial() as i64,
            name,
            child.serial() as i64,
        ),
    );
    match result {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(NamespaceError::AlreadyExists {
                path: String::from_utf8_lossy(name).into_owned(),
            })
        }
        Err(other) => Err(other.into()),
    }
}
