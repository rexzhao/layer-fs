//! The write view: overlay transactions over the base snapshot (ADR-006 §7.3).
//!
//! Every rule runs inside one overlay transaction, so the state a rule reads is the
//! state it modifies (K3). There is no check-then-use window because there is no other
//! writer: the file is opened `locking_mode=EXCLUSIVE` and the handle serialises its
//! own threads.

use rusqlite::{Connection, OptionalExtension};

use fs_namespace::{FileId, InodeView, Kind, NamespaceError, NamespaceResult, TreeRead, TreeWrite};
use fs_snapshot::InodeRecord;

use crate::base::BaseLayer;
use crate::error::{OverlayError, OverlayResult};
use crate::schema;
use crate::view::{
    entry_row, inode_impl, inode_row, is_dir_empty_impl, lookup_impl, read_dir_impl,
};

/// Write view of one workspace's tree.
#[derive(Debug)]
pub struct OverlayWrite<'a> {
    pub(crate) transaction: rusqlite::Transaction<'a>,
    pub(crate) base: &'a BaseLayer,
}

impl TreeRead for OverlayWrite<'_> {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>> {
        lookup_impl(&self.transaction, self.base, parent, name).map_err(crate::view::into_namespace)
    }

    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>> {
        inode_impl(&self.transaction, self.base, id).map_err(crate::view::into_namespace)
    }

    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        read_dir_impl(&self.transaction, self.base, dir, after, limit)
            .map_err(crate::view::into_namespace)
    }

    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool> {
        is_dir_empty_impl(&self.transaction, self.base, dir).map_err(crate::view::into_namespace)
    }
}

impl TreeWrite for OverlayWrite<'_> {
    fn truncate(&mut self, id: FileId, new_size: u64) -> NamespaceResult<()> {
        // Same implementation the content API uses, run against **this** transaction: the size change and the
        // zeroing of the exposed range commit together with the disposition's decision.
        crate::content::truncate_impl(&self.transaction, self.base, id, new_size)
            .map_err(crate::view::into_namespace)
    }

    fn bind(&mut self, parent: FileId, name: &[u8], child: FileId) -> NamespaceResult<()> {
        bind_impl(&self.transaction, self.base, parent, name, child)
            .map_err(crate::view::into_namespace)
    }

    fn unbind(&mut self, parent: FileId, name: &[u8]) -> NamespaceResult<()> {
        unbind_impl(&self.transaction, self.base, parent, name).map_err(crate::view::into_namespace)
    }

    fn create_inode(&mut self, kind: Kind) -> NamespaceResult<FileId> {
        create_inode_impl(&self.transaction, kind).map_err(crate::view::into_namespace)
    }

    fn tombstone(&mut self, id: FileId) -> NamespaceResult<()> {
        tombstone_impl(&self.transaction, self.base, id).map_err(crate::view::into_namespace)
    }
}

// ------------------------------------------------------------------ bodies

/// Allocates the next serial from this workspace's reserved range (ADR-006 §6).
pub(crate) fn alloc_serial(connection: &Connection) -> OverlayResult<u64> {
    let next: i64 = meta_i64(connection, "serial_next")?;
    let end: i64 = meta_i64(connection, "serial_end")?;
    if next >= end {
        // Extending the range means reserving from the shared allocator, which is
        // fs-workspace's job (P2a-6). Reported rather than silently colliding with the
        // base's serials.
        return Err(OverlayError::InvalidRecord {
            field: "ov_meta.serial_next",
            detail: format!("reserved range is exhausted at {next} (end {end})"),
        });
    }
    connection.execute(
        "UPDATE ov_meta SET value = ?1 WHERE key = 'serial_next'",
        [(next + 1).to_string()],
    )?;
    Ok(next as u64)
}

pub(crate) fn create_inode_impl(connection: &Connection, kind: Kind) -> OverlayResult<FileId> {
    let serial = alloc_serial(connection)?;
    let now = now_ns();
    connection.execute(
        "INSERT INTO ov_inode
         (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, 0, 0)",
        (
            serial as i64,
            schema::ORIGIN_LOCAL,
            schema::LIVE,
            kind_tag(kind),
            now,
        ),
    )?;
    Ok(FileId::from_serial(serial))
}

pub(crate) fn bind_impl(
    connection: &Connection,
    base: &BaseLayer,
    parent: FileId,
    name: &[u8],
    child: FileId,
) -> OverlayResult<()> {
    let already = entry_row(connection, parent.serial(), name)?;
    if let Some(Some(_)) = already {
        // Only reachable if a caller binds over a live name; the rules check first.
        return Err(OverlayError::Namespace(NamespaceError::AlreadyExists {
            path: String::from_utf8_lossy(name).into_owned(),
        }));
    }
    let visible_before = match already {
        Some(None) => false,
        Some(Some(_)) => true,
        None => base_binds(base, parent, name)?,
    };
    ensure_dir_row(connection, base, parent)?;
    connection.execute(
        "INSERT INTO ov_entry (parent, name, child) VALUES (?1, ?2, ?3)
         ON CONFLICT(parent, name) DO UPDATE SET child = excluded.child",
        (parent.serial() as i64, name, child.serial() as i64),
    )?;
    if !visible_before {
        bump_entries(connection, parent, 1)?;
    }
    Ok(())
}

pub(crate) fn unbind_impl(
    connection: &Connection,
    base: &BaseLayer,
    parent: FileId,
    name: &[u8],
) -> OverlayResult<()> {
    match entry_row(connection, parent.serial(), name)? {
        // Already a whiteout: nothing is visible, nothing to do.
        Some(None) => Ok(()),
        Some(Some(_)) => {
            // Whether deleting the row is enough depends on the *base*: if the base still
            // binds this name, deleting the overlay row would reveal the base binding again
            // and the entry would reappear. That is exactly what happened when a name from
            // the base was removed, re-created and removed again — the re-creation reused
            // the same `ov_entry` row, so the second removal must turn it back into a
            // whiteout rather than drop it.
            if base_binds(base, parent, name)? {
                connection.execute(
                    "UPDATE ov_entry SET child = NULL WHERE parent = ?1 AND name = ?2",
                    (parent.serial() as i64, name),
                )?;
            } else {
                connection.execute(
                    "DELETE FROM ov_entry WHERE parent = ?1 AND name = ?2",
                    (parent.serial() as i64, name),
                )?;
            }
            bump_entries(connection, parent, -1)?;
            Ok(())
        }
        None => {
            if base_binds(base, parent, name)? {
                // The name is only in the base, so removal needs a whiteout; deleting
                // an `ov_entry` row would just reveal the base binding again.
                ensure_dir_row(connection, base, parent)?;
                connection.execute(
                    "INSERT INTO ov_entry (parent, name, child) VALUES (?1, ?2, NULL)
                     ON CONFLICT(parent, name) DO UPDATE SET child = NULL",
                    (parent.serial() as i64, name),
                )?;
                bump_entries(connection, parent, -1)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn tombstone_impl(
    connection: &Connection,
    base: &BaseLayer,
    id: FileId,
) -> OverlayResult<()> {
    #[cfg(feature = "gc-test-hooks")]
    crate::counting::note_tombstone();
    match inode_row(connection, id.serial())? {
        Some(row) if row.origin == schema::ORIGIN_LOCAL => {
            // A **local** inode is tombstoned, not deleted (ADR-009, single deletion semantics): a held id must keep
            // meaning "this entry, deleted", and a missing row would let a later allocation reuse the serial's slot
            // semantics silently.
            connection.execute(
                "UPDATE ov_inode SET live = ?1, size = 0, base_limit = 0, entries = 0
                 WHERE serial = ?2",
                (schema::TOMBSTONE, id.serial() as i64),
            )?;
        }
        Some(_) => {
            // A base inode stays as a tombstone: a row must remain to say "deleted",
            // otherwise the next read would fall through and resurrect it.
            connection.execute(
                "UPDATE ov_inode SET live = ?1, size = 0, base_limit = 0, entries = 0
                 WHERE serial = ?2",
                (schema::TOMBSTONE, id.serial() as i64),
            )?;
        }
        None => {
            let Some(record) = base.inode(id.serial())? else {
                // Neither the overlay nor the base has it: nothing to delete.
                return Ok(());
            };
            connection.execute(
                "INSERT INTO ov_inode
                 (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0)",
                (
                    id.serial() as i64,
                    schema::ORIGIN_BASE,
                    schema::TOMBSTONE,
                    kind_tag(base_kind(&record)),
                    record.size as i64,
                    record.created_ns,
                    record.modified_ns,
                ),
            )?;
        }
    }
    // K1/K2: the content goes with the inode. Pages are the overlay's only
    // representation of content, so dropping them is the whole cascade here.
    connection.execute(
        "DELETE FROM ov_page WHERE serial = ?1",
        [id.serial() as i64],
    )?;
    Ok(())
}

/// Makes sure a directory has an `ov_inode` row, so its view entry count is tracked.
///
/// Creating the row copies the base's metadata and counts its entries once. That count
/// is O(size of the base directory) and happens the first time a workspace touches a
/// directory; afterwards it is maintained incrementally. Recorded as a known cost in
/// the crate docs rather than hidden.
fn ensure_dir_row(connection: &Connection, base: &BaseLayer, serial: FileId) -> OverlayResult<()> {
    if inode_row(connection, serial.serial())?.is_some() {
        return Ok(());
    }
    let count = match base.directory(serial.serial())? {
        Some(record) => {
            let entries = count_base_entries(base, &record)?;
            connection.execute(
                "INSERT INTO ov_inode
                 (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, 0, ?7)",
                (
                    serial.serial() as i64,
                    schema::ORIGIN_BASE,
                    schema::LIVE,
                    1i64,
                    record.created_ns,
                    record.modified_ns,
                    entries as i64,
                ),
            )?;
            return Ok(());
        }
        None => 0usize,
    };
    connection.execute(
        "INSERT INTO ov_inode
         (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries)
         VALUES (?1, ?2, ?3, 1, 0, ?4, ?4, 0, ?5)",
        (
            serial.serial() as i64,
            schema::ORIGIN_LOCAL,
            schema::LIVE,
            now_ns(),
            count as i64,
        ),
    )?;
    Ok(())
}

fn count_base_entries(base: &BaseLayer, record: &InodeRecord) -> OverlayResult<usize> {
    let mut total = 0usize;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = base.read_dir(record, after.as_deref(), 256)?;
        if page.is_empty() {
            return Ok(total);
        }
        total += page.len();
        after = page.last().map(|(name, _)| name.clone());
    }
}

fn bump_entries(connection: &Connection, dir: FileId, delta: i64) -> OverlayResult<()> {
    connection.execute(
        "UPDATE ov_inode SET entries = MAX(0, entries + ?1) WHERE serial = ?2",
        (delta, dir.serial() as i64),
    )?;
    Ok(())
}

fn base_binds(base: &BaseLayer, parent: FileId, name: &[u8]) -> OverlayResult<bool> {
    match base.directory(parent.serial())? {
        Some(record) => Ok(base.lookup(&record, name)?.is_some()),
        None => Ok(false),
    }
}

fn meta_i64(connection: &Connection, key: &str) -> OverlayResult<i64> {
    let text: String = connection
        .query_row("SELECT value FROM ov_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| OverlayError::InvalidRecord {
            field: "ov_meta",
            detail: format!("missing key {key}"),
        })?;
    text.parse::<i64>()
        .map_err(|error| OverlayError::InvalidRecord {
            field: "ov_meta",
            detail: error.to_string(),
        })
}

fn kind_tag(kind: Kind) -> i64 {
    match kind {
        Kind::Directory => 1,
        Kind::File => 2,
    }
}

fn base_kind(record: &InodeRecord) -> Kind {
    match record.kind {
        fs_snapshot::Kind::Directory => Kind::Directory,
        fs_snapshot::Kind::File => Kind::File,
    }
}

pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0)
}
