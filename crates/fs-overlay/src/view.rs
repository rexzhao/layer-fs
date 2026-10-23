//! The read view: overlay rows composed over the base snapshot (ADR-006 §7.2).
//!
//! Three rules, and they are the whole of the read path:
//!
//! 1. an `ov_inode` row wins — including a tombstone, which means "deleted" and does
//!    **not** fall back to the base;
//! 2. a name with an `ov_entry` row wins — `child IS NULL` is a whiteout that masks the
//!    base's binding;
//! 3. anything else falls through to the base.
//!
//! Falling through is keyed by **serial**, never by path (W5), which is why renaming a
//! directory needs only two entry rows and leaves the whole subtree untouched.

use rusqlite::{Connection, OptionalExtension};

use fs_namespace::{FileId, InodeView, Kind, NamespaceError, NamespaceResult, TreeRead};

use crate::base::BaseLayer;
use crate::error::{OverlayError, OverlayResult};
use crate::schema;

/// Read-only view of one workspace's tree.
#[derive(Debug)]
pub struct OverlayRead<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) base: &'a BaseLayer,
}

impl<'a> OverlayRead<'a> {
    pub(crate) fn new(connection: &'a Connection, base: &'a BaseLayer) -> Self {
        Self { connection, base }
    }
}

/// One overlay inode row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeRow {
    pub(crate) origin: i64,
    pub(crate) live: i64,
    pub(crate) kind: i64,
    pub(crate) size: i64,
    pub(crate) created_ns: i64,
    pub(crate) modified_ns: i64,
    pub(crate) base_limit: i64,
    pub(crate) entries: i64,
}

/// Reads the overlay row for `serial`, if any.
pub(crate) fn inode_row(connection: &Connection, serial: u64) -> OverlayResult<Option<InodeRow>> {
    let row = connection
        .query_row(
            "SELECT origin, live, kind, size, created_ns, modified_ns, base_limit, entries
             FROM ov_inode WHERE serial = ?1",
            [serial as i64],
            |row| {
                Ok(InodeRow {
                    origin: row.get(0)?,
                    live: row.get(1)?,
                    kind: row.get(2)?,
                    size: row.get(3)?,
                    created_ns: row.get(4)?,
                    modified_ns: row.get(5)?,
                    base_limit: row.get(6)?,
                    entries: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Reads the overlay binding for `(parent, name)`.
///
/// `Some(Some(child))` is a binding, `Some(None)` is a whiteout, `None` means the
/// overlay says nothing and the base decides.
pub(crate) fn entry_row(
    connection: &Connection,
    parent: u64,
    name: &[u8],
) -> OverlayResult<Option<Option<u64>>> {
    let row: Option<Option<i64>> = connection
        .query_row(
            "SELECT child FROM ov_entry WHERE parent = ?1 AND name = ?2",
            (parent as i64, name),
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.map(|child| child.map(|value| value as u64)))
}

/// Every overlay binding under `parent` with a name greater than `after`, in name
/// order.
pub(crate) fn entries_after(
    connection: &Connection,
    parent: u64,
    after: Option<&[u8]>,
) -> OverlayResult<Vec<(Vec<u8>, Option<u64>)>> {
    let after = after.unwrap_or(&[]);
    let mut statement = connection.prepare(
        "SELECT name, child FROM ov_entry
         WHERE parent = ?1 AND name > ?2
         ORDER BY name",
    )?;
    let rows = statement.query_map((parent as i64, after), |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (name, child) = row?;
        entries.push((name, child.map(|value| value as u64)));
    }
    Ok(entries)
}

impl TreeRead for OverlayRead<'_> {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>> {
        lookup_impl(self.connection, self.base, parent, name).map_err(into_namespace)
    }

    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>> {
        inode_impl(self.connection, self.base, id).map_err(into_namespace)
    }

    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        read_dir_impl(self.connection, self.base, dir, after, limit).map_err(into_namespace)
    }

    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool> {
        is_dir_empty_impl(self.connection, self.base, dir).map_err(into_namespace)
    }
}

// ------------------------------------------------------------------ shared bodies

/// Turns an overlay failure back into the rules' error type.
///
/// The typed overlay failures a caller can act on — `WorkspaceInUse`, `SchemaMismatch`
/// — only arise when the workspace is **opened**, and `OverlayStore::open` returns
/// [`OverlayError`] directly. Inside the contract the rules only speak
/// `NamespaceError`, so an overlay-specific failure becomes a store record error
/// carrying its message rather than being flattened to a generic one.
pub(crate) fn into_namespace(error: OverlayError) -> NamespaceError {
    match error {
        OverlayError::Namespace(error) => error,
        other => NamespaceError::Store(cas_store::StoreError::InvalidRecord {
            field: "overlay",
            detail: other.to_string(),
        }),
    }
}

pub(crate) fn lookup_impl(
    connection: &Connection,
    base: &BaseLayer,
    parent: FileId,
    name: &[u8],
) -> OverlayResult<Option<FileId>> {
    match entry_row(connection, parent.serial(), name)? {
        Some(Some(child)) => Ok(Some(FileId::from_serial(child))),
        // A whiteout masks the base: the name is gone even if the base binds it.
        Some(None) => Ok(None),
        None => match base.directory(parent.serial())? {
            Some(dir) => Ok(base.lookup(&dir, name)?),
            None => Ok(None),
        }
        .map(|found| found.map(FileId::from_serial)),
    }
}

pub(crate) fn inode_impl(
    connection: &Connection,
    base: &BaseLayer,
    id: FileId,
) -> OverlayResult<Option<InodeView>> {
    if let Some(row) = inode_row(connection, id.serial())? {
        if row.live == schema::TOMBSTONE {
            return Ok(None);
        }
        return Ok(Some(InodeView {
            id,
            kind: kind_from_tag(row.kind)?,
            size: row.size as u64,
            created_ns: row.created_ns,
            modified_ns: row.modified_ns,
        }));
    }
    Ok(base.inode(id.serial())?.map(|record| InodeView {
        id,
        kind: match record.kind {
            fs_snapshot::Kind::Directory => Kind::Directory,
            fs_snapshot::Kind::File => Kind::File,
        },
        size: record.size,
        created_ns: record.created_ns,
        modified_ns: record.modified_ns,
    }))
}

pub(crate) fn read_dir_impl(
    connection: &Connection,
    base: &BaseLayer,
    dir: FileId,
    after: Option<&[u8]>,
    limit: usize,
) -> OverlayResult<Vec<(Vec<u8>, FileId)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let overlay = entries_after(connection, dir.serial(), after)?;
    let base_dir = base.directory(dir.serial())?;

    // Both sides arrive in name order, so one merge pass suffices; the overlay wins on
    // equal names, and a whiteout removes the base's entry.
    let mut out = Vec::with_capacity(limit.min(64));
    let mut base_page: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut base_cursor: Option<Vec<u8>> = after.map(<[u8]>::to_vec);
    let mut base_done = base_dir.is_none();
    let mut overlay_index = 0usize;

    while out.len() < limit {
        // Pull base entries as needed, one page at a time.
        if !base_done && base_page.is_empty() {
            match &base_dir {
                Some(record) => {
                    base_page = base.read_dir(record, base_cursor.as_deref(), 64)?;
                    if base_page.is_empty() {
                        base_done = true;
                    }
                }
                None => base_done = true,
            }
        }

        let overlay_name = overlay.get(overlay_index).map(|(name, _)| name.clone());
        let base_name = base_page.first().map(|(name, _)| name.clone());

        match (overlay_name, base_name) {
            (None, None) => break,
            (Some(name), None) => {
                if let Some(child) = overlay[overlay_index].1 {
                    out.push((name.clone(), FileId::from_serial(child)));
                }
                overlay_index += 1;
            }
            (None, Some(_)) => {
                let (name, child) = base_page.remove(0);
                base_cursor = Some(name.clone());
                out.push((name, FileId::from_serial(child)));
            }
            (Some(left), Some(right)) => {
                if left < right {
                    if let Some(child) = overlay[overlay_index].1 {
                        out.push((left, FileId::from_serial(child)));
                    }
                    overlay_index += 1;
                } else if right < left {
                    let (name, child) = base_page.remove(0);
                    base_cursor = Some(name.clone());
                    out.push((name, FileId::from_serial(child)));
                } else {
                    // Same name: the overlay decides, and a whiteout drops it.
                    if let Some(child) = overlay[overlay_index].1 {
                        out.push((left, FileId::from_serial(child)));
                    }
                    overlay_index += 1;
                    base_page.remove(0);
                    base_cursor = Some(right);
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn is_dir_empty_impl(
    connection: &Connection,
    base: &BaseLayer,
    dir: FileId,
) -> OverlayResult<bool> {
    // A row means this directory has been touched and its view count is maintained
    // incrementally, so the answer is O(1) — which is what `rmdir` and rename's R6
    // need (ADR-006 §5.2).
    if let Some(row) = inode_row(connection, dir.serial())? {
        if row.live == schema::TOMBSTONE {
            return Ok(true);
        }
        return Ok(row.entries == 0);
    }
    match base.directory(dir.serial())? {
        Some(record) => base.is_dir_empty(&record),
        // An inode neither in the overlay nor the base has no children.
        None => Ok(true),
    }
}

fn kind_from_tag(tag: i64) -> OverlayResult<Kind> {
    match tag {
        1 => Ok(Kind::Directory),
        2 => Ok(Kind::File),
        other => Err(OverlayError::InvalidRecord {
            field: "ov_inode.kind",
            detail: format!("unknown kind {other}"),
        }),
    }
}
