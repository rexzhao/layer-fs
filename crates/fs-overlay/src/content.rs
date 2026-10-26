//! File content as pages (ADR-006 §5.3, §7.2, §7.3).
//!
//! The overlay does not have extent rows. A file's bytes are a sparse map of
//! [`PAGE_BYTES`](crate::schema::PAGE_BYTES)-sized pages, and everything not covered by
//! a page is read from the base while it lies below `base_limit`, then zeros.
//!
//! Two properties come straight out of that shape:
//!
//! - **truncate cannot lose a byte to a boundary artefact.** Lowering `base_limit` makes the
//!   cut bytes unreadable in one column update; there is no "rewrite the partial block"
//!   step, which is what P1's KI-1 had to fix. `truncate` to a page boundary therefore
//!   creates nothing (K6).
//! - **truncate cannot give a byte back either** (R3-1/KI-9). The boundary page is kept, so
//!   its tail is zeroed in place: *no stored page holds a non-zero byte at or past EOF*, and
//!   a later extension therefore reads zeros. Reads are bounded by `size`, but "unreadable"
//!   is not enough — the bytes must be **gone**, or the next extension resurrects them.
//!   Pages stay exactly `PAGE_BYTES` long; this tree never produces a short page.
//! - **a write touches only its own pages.** Every other page keeps whatever it had —
//!   its overlay row, or its base bytes.
//!
//! These operations are **not** part of [`fs_namespace::TreeStore`]: the overlay's
//! content representation is its own, exactly as the retired table backend's extents were
//! (ADR-006 §7.1). They are atomic because each one runs in a single overlay
//! transaction, which is what makes a concurrent `write_at` never half-visible (§7.4).

use rusqlite::{Connection, OptionalExtension};

use fs_namespace::{FileId, Kind, NamespaceError, NamespaceResult, Path};

use crate::error::{OverlayError, OverlayResult};
use crate::schema;
use crate::store::{OverlayStore, ScopeGuard};
use crate::view::{inode_row, into_namespace};
use crate::write_view::now_ns;

/// What a content read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentState {
    /// The file's logical size.
    pub size: u64,
    /// The first byte that must come from the base; everything at or past it reads as
    /// the overlay's own pages or zeros.
    pub base_limit: u64,
    /// How many pages this workspace overrides.
    pub pages: u64,
}

impl OverlayStore {
    /// Reads `len` bytes at `offset`, zero-filling past the file's end-of-content.
    ///
    /// # Errors
    ///
    /// `NotFound` when the path is not a live file, `IsADirectory` for a directory, or a
    /// store failure.
    pub fn read_at(&self, path: &Path, offset: u64, len: usize) -> NamespaceResult<Vec<u8>> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        let id = resolve_file(&connection, &self.base, path).map_err(into_namespace)?;
        read_at_impl(&connection, &self.base, id, offset, len).map_err(into_namespace)
    }

    /// Reads by **FileId** instead of path (P3-2 G2).
    ///
    /// The path version resolves the path and calls the same implementation, so this is not a second read path — it is
    /// the same one without the resolution step. That matters for a caller holding a handle: after a rename its path is
    /// stale, its FileId is not.
    ///
    /// # Errors
    ///
    /// `IsADirectory` for a directory, `NotFound` for an id that is not live, or a store failure.
    pub fn read_at_id(
        &self,
        id: fs_namespace::FileId,
        offset: u64,
        len: usize,
    ) -> NamespaceResult<Vec<u8>> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        // The path version got this check for free: `resolve_file` refuses a directory. Going straight to the id
        // skips that, so the check has to be here — otherwise "read a directory" returns its pages as if they were
        // file bytes, which is worse than an error because it looks like data.
        match crate::view::inode_impl(&connection, &self.base, id).map_err(into_namespace)? {
            Some(view) if view.kind == fs_namespace::Kind::File => {}
            Some(_) => {
                return Err(fs_namespace::NamespaceError::IsADirectory {
                    path: String::new(),
                })
            }
            None => {
                return Err(fs_namespace::NamespaceError::NotFound {
                    path: String::new(),
                })
            }
        }
        read_at_impl(&connection, &self.base, id, offset, len).map_err(into_namespace)
    }

    /// Writes `bytes` at `offset`, extending the file when the write runs past its end.
    ///
    /// # Errors
    ///
    /// `NotFound` for a path that is not a live file, `IsADirectory` for a directory.
    pub fn write_at(&self, path: &Path, offset: u64, bytes: &[u8]) -> NamespaceResult<()> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        let id = resolve_file(&transaction, &self.base, path).map_err(into_namespace)?;
        write_at_impl(&transaction, &self.base, id, offset, bytes).map_err(into_namespace)?;
        transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// Sets the file's size, zero-extending or cutting it (ADR-006 §7.3).
    ///
    /// # Errors
    ///
    /// `NotFound` for a path that is not a live file, `IsADirectory` for a directory.
    pub fn truncate(&self, path: &Path, new_size: u64) -> NamespaceResult<()> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        let id = resolve_file(&transaction, &self.base, path).map_err(into_namespace)?;
        truncate_impl(&transaction, &self.base, id, new_size).map_err(into_namespace)?;
        transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// Sets the **stored** attribute bits and moves `changed_ns`.
    ///
    /// # Errors
    ///
    /// `InvalidRecord` for a derived/unknown bit, `NotFound` for an absent inode.
    pub fn set_attributes(&self, id: FileId, attributes: u32) -> NamespaceResult<()> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        set_attributes_impl(&transaction, &self.base, id, attributes).map_err(into_namespace)?;
        transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// The stored attribute bits for `id` (0 when the overlay has no row).
    ///
    /// # Errors
    ///
    /// SQL failures.
    pub fn attributes(&self, id: FileId) -> NamespaceResult<u32> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        attributes_impl(&connection, id).map_err(into_namespace)
    }

    /// Sets whichever of the four timestamps are given (`None` leaves that one alone).
    ///
    /// # Errors
    ///
    /// `NotFound` for an absent inode, SQL failures.
    pub fn set_times(
        &self,
        id: FileId,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> NamespaceResult<()> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        set_times_impl(
            &transaction,
            &self.base,
            id,
            accessed,
            written,
            changed,
            created,
        )
        .map_err(into_namespace)?;
        transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// Sets (or clears, with `None`) the stored security descriptor.
    ///
    /// # Errors
    ///
    /// `InvalidRecord` when the descriptor exceeds 65536 bytes, `NotFound` for an absent inode.
    pub fn set_security(&self, id: FileId, security: Option<&[u8]>) -> NamespaceResult<()> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        set_security_impl(&transaction, &self.base, id, security).map_err(into_namespace)?;
        transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// The stored descriptor, if any (v1 overlays have none).
    ///
    /// # Errors
    ///
    /// SQL failures.
    pub fn security(&self, id: FileId) -> NamespaceResult<Option<Vec<u8>>> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        security_impl(&connection, id).map_err(into_namespace)
    }

    /// How many inode rows the overlay holds, for tests that must prove a refusal created none.
    ///
    /// # Errors
    ///
    /// SQL failures.
    #[cfg(feature = "gc-test-hooks")]
    pub fn inode_row_count(&self) -> NamespaceResult<u64> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        connection
            .query_row("SELECT COUNT(*) FROM ov_inode", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| u64::try_from(count).unwrap_or(0))
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))
    }

    /// Reports how a file's bytes are currently represented.
    ///
    /// # Errors
    ///
    /// `NotFound` when the path is not a live file.
    pub fn content_state(&self, path: &Path) -> NamespaceResult<ContentState> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        let id = resolve_file(&connection, &self.base, path).map_err(into_namespace)?;
        content_state_impl(&connection, &self.base, id).map_err(into_namespace)
    }

    /// Creates an empty regular file.
    ///
    /// `fs_namespace::Rules` has no `create_file`: in the retired table backend creating a file
    /// and creating a directory differ only in the inode's `kind` and in the content
    /// row that follows, so it lived with the content paths there. The overlay has the
    /// same split, so it provides the same shape.
    ///
    /// # Errors
    ///
    /// `AlreadyExists` when the name is taken, `NotFound` for a missing parent,
    /// `NotADirectory` when a parent component is a file.
    pub fn create_file(&self, path: &Path) -> NamespaceResult<()> {
        // `TreeRead` must be imported as well: its methods are callable on a
        // `TreeWrite` only when the supertrait is in scope.
        use fs_namespace::{OpLabel, TreeRead, TreeStore, TreeWrite};
        if path.is_root() {
            return Err(NamespaceError::AlreadyExists {
                path: path.display(),
            });
        }
        self.write(OpLabel::Create, |tree| {
            let parent = fs_namespace::resolve_parent_in(tree, path)?;
            let name = path.name().expect("a non-root path has a name");
            if tree.lookup(parent, name.as_bytes())?.is_some() {
                return Err(NamespaceError::AlreadyExists {
                    path: path.display(),
                });
            }
            let id = tree.create_inode(Kind::File)?;
            tree.bind(parent, name.as_bytes(), id)
                .map_err(|error| match error {
                    NamespaceError::AlreadyExists { .. } => NamespaceError::AlreadyExists {
                        path: path.display(),
                    },
                    other => other,
                })
        })
    }
}

pub(crate) fn read_at_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    offset: u64,
    len: usize,
) -> OverlayResult<Vec<u8>> {
    let (size, base_limit) = geometry(connection, base, id)?;
    let mut out = vec![0u8; len];
    if len == 0 || offset >= size {
        return Ok(out);
    }
    let want = ((size - offset) as usize).min(len);

    let first_page = offset / schema::PAGE_BYTES;
    let last_page = (offset + want as u64 - 1) / schema::PAGE_BYTES;
    // The base is only consulted for the part of the range below `base_limit`; a page
    // that starts above it is the overlay's (or zeros), so no base read is wasted.
    let base_record = if offset < base_limit {
        base.inode(id.serial())?
    } else {
        None
    };

    for page in first_page..=last_page {
        let page_start = page * schema::PAGE_BYTES;
        let from = offset.max(page_start);
        let to = (offset + want as u64).min(page_start + schema::PAGE_BYTES);
        if from >= to {
            continue;
        }
        let target = (from - offset) as usize..(to - offset) as usize;
        match page_bytes(connection, id.serial(), page)? {
            Some(data) => {
                let start = (from - page_start) as usize;
                let available = data.len().saturating_sub(start).min(target.len());
                out[target.start..target.start + available]
                    .copy_from_slice(&data[start..start + available]);
            }
            None => {
                if from < base_limit {
                    if let Some(record) = &base_record {
                        let readable_end = to.min(base_limit);
                        let count = (readable_end - from) as usize;
                        let bytes = base.read_at(record, from, count)?;
                        out[target.start..target.start + count].copy_from_slice(&bytes);
                    }
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn write_at_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    offset: u64,
    bytes: &[u8],
) -> OverlayResult<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let (old_size, base_limit) = geometry(connection, base, id)?;
    let end = offset + bytes.len() as u64;
    let new_size = old_size.max(end);
    let base_record = base.inode(id.serial())?;

    let first_page = offset / schema::PAGE_BYTES;
    let last_page = (end - 1) / schema::PAGE_BYTES;
    for page in first_page..=last_page {
        let page_start = page * schema::PAGE_BYTES;
        let from = offset.max(page_start);
        let to = end.min(page_start + schema::PAGE_BYTES);
        if from >= to {
            continue;
        }
        // Materialise the page's current view first: a partial write must keep the
        // bytes it does not cover, whether they came from another overlay page or from
        // the base.
        let mut current = match page_bytes(connection, id.serial(), page)? {
            Some(data) => {
                let mut buffer = vec![0u8; schema::PAGE_BYTES as usize];
                let copy = data.len().min(buffer.len());
                buffer[..copy].copy_from_slice(&data[..copy]);
                buffer
            }
            None => {
                let mut buffer = vec![0u8; schema::PAGE_BYTES as usize];
                if page_start < base_limit {
                    if let Some(record) = &base_record {
                        let readable = (base_limit - page_start).min(schema::PAGE_BYTES);
                        let bytes = base.read_at(record, page_start, readable as usize)?;
                        buffer[..bytes.len()].copy_from_slice(&bytes);
                    }
                }
                buffer
            }
        };
        let start = (from - page_start) as usize;
        let source = (from - offset) as usize;
        let count = (to - from) as usize;
        current[start..start + count].copy_from_slice(&bytes[source..source + count]);
        store_page(connection, id.serial(), page, &current)?;
    }

    touch_inode(connection, base, id, new_size, new_size, base_limit)?;
    Ok(())
}

pub(crate) fn truncate_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    new_size: u64,
) -> OverlayResult<()> {
    let (old_size, base_limit) = geometry(connection, base, id)?;
    // Both directions of `truncate` have to end with one invariant — it *is* the whole rule:
    //
    // > **No stored page holds a non-zero byte at or past the file's EOF.**
    //
    // Shrinking needs it so that a later extension reads zeros instead of the bytes it cut;
    // growing needs it because it has just exposed a range that must read as zeros. The two
    // directions share the single `live_end` below, and the read path relies on it: a page
    // that exists is returned as-is (a missing tail reads as zeros), so "the read path stops
    // at `size`" would only hide cut bytes, not remove them.
    //
    // Zeroing rather than truncating the page blob is this implementation's choice (ADR-006
    // §7.3, KI-9): pages stay exactly `PAGE_BYTES` long, so no short page ever exists and no
    // later stage needs short-page tolerance.
    //
    // The live bytes are below `live_end`, so every page that *starts* at or past it is
    // droppable. Rounding up, not down: for a size that is not a multiple of the page size,
    // the page containing the last surviving byte starts before it and must be kept. Rounding
    // down deleted that page and with it every byte the file still had (KI-6).
    let live_end = old_size.min(new_size);
    let boundary = live_end.div_ceil(schema::PAGE_BYTES);
    connection.execute(
        "DELETE FROM ov_page WHERE serial = ?1 AND page_index >= ?2",
        (id.serial() as i64, boundary as i64),
    )?;
    // The one page that straddles `live_end` cannot be dropped — it still carries live
    // bytes — so its tail is zeroed in place (ADR-006 §7.3: 把边界页的尾部截掉). Skipping
    // this was R3-1's defect: write 0xE9 over [104, 159), truncate to 66, extend to 181, and
    // the cut bytes came back (reference model, seed=1000 step=357).
    let straddling = live_end / schema::PAGE_BYTES;
    let keep_bytes = (live_end % schema::PAGE_BYTES) as usize;
    if keep_bytes != 0 {
        if let Some(mut data) = page_bytes(connection, id.serial(), straddling)? {
            if data.len() > keep_bytes {
                data[keep_bytes..].fill(0);
                store_page(connection, id.serial(), straddling, &data)?;
            }
        }
    }
    // Base bytes above the new size stop being visible by lowering `base_limit`; no
    // copy, no rewrite, and it cannot be raised again by a later extension.
    let new_limit = base_limit.min(new_size);
    touch_inode(connection, base, id, new_size, new_size, new_limit)?;
    Ok(())
}

pub(crate) fn content_state_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
) -> OverlayResult<ContentState> {
    let (size, base_limit) = geometry(connection, base, id)?;
    let pages: i64 = connection.query_row(
        "SELECT COUNT(*) FROM ov_page WHERE serial = ?1",
        [id.serial() as i64],
        |row| row.get(0),
    )?;
    Ok(ContentState {
        size,
        base_limit,
        pages: pages as u64,
    })
}

/// The file's size and base limit, creating an override row when the base provides it.
fn geometry(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
) -> OverlayResult<(u64, u64)> {
    if let Some(row) = inode_row(connection, id.serial())? {
        if row.live == schema::TOMBSTONE {
            return Err(OverlayError::Namespace(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            }));
        }
        return Ok((row.size as u64, row.base_limit as u64));
    }
    match base.inode(id.serial())? {
        Some(record) => Ok((record.size, record.size)),
        None => Err(OverlayError::Namespace(NamespaceError::NotFound {
            path: format!("{}", id.serial()),
        })),
    }
}

/// Writes (or creates) the inode row for a file this workspace has just changed.
fn touch_inode(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    size: u64,
    _new_size: u64,
    base_limit: u64,
) -> OverlayResult<()> {
    let now = now_ns();
    if inode_row(connection, id.serial())?.is_some() {
        connection.execute(
            // A content write moves **both** the modification time and the metadata-change time: NTFS reports the latter
            // as `ChangeTime`, and a writer that only bumped `modified_ns` would make "metadata changed" look false after
            // a content change — the two are not synonyms, but a content write is a change to both.
            "UPDATE ov_inode SET size = ?1, base_limit = ?2, modified_ns = ?3, changed_ns = ?3 WHERE serial = ?4",
            (size as i64, base_limit as i64, now, id.serial() as i64),
        )?;
        return Ok(());
    }
    let Some(record) = base.inode(id.serial())? else {
        return Err(OverlayError::Namespace(NamespaceError::NotFound {
            path: format!("{}", id.serial()),
        }));
    };
    connection.execute(
        "INSERT INTO ov_inode
         (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
        (
            id.serial() as i64,
            schema::ORIGIN_BASE,
            schema::LIVE,
            kind_tag(record.kind),
            size as i64,
            record.created_ns,
            now,
            base_limit as i64,
        ),
    )?;
    Ok(())
}

fn kind_tag(kind: fs_snapshot::Kind) -> i64 {
    match kind {
        fs_snapshot::Kind::Directory => 1,
        fs_snapshot::Kind::File => 2,
    }
}

fn page_bytes(connection: &Connection, serial: u64, page: u64) -> OverlayResult<Option<Vec<u8>>> {
    let data: Option<Vec<u8>> = connection
        .query_row(
            "SELECT data FROM ov_page WHERE serial = ?1 AND page_index = ?2",
            (serial as i64, page as i64),
            |row| row.get(0),
        )
        .optional()?;
    Ok(data)
}

fn store_page(connection: &Connection, serial: u64, page: u64, data: &[u8]) -> OverlayResult<()> {
    connection.execute(
        "INSERT INTO ov_page (serial, page_index, data) VALUES (?1, ?2, ?3)
         ON CONFLICT(serial, page_index) DO UPDATE SET data = excluded.data",
        (serial as i64, page as i64, data),
    )?;
    Ok(())
}

/// Resolves a path to a live regular file **inside the caller's scope**.
fn resolve_file(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    path: &Path,
) -> OverlayResult<FileId> {
    let view = crate::view::OverlayRead::new(connection, base);
    let id = fs_namespace::resolve_in(&view, path)?;
    match fs_namespace::TreeRead::inode(&view, id)? {
        Some(view) if view.kind == Kind::File => Ok(id),
        Some(_) => Err(OverlayError::Namespace(NamespaceError::IsADirectory {
            path: path.display(),
        })),
        None => Err(OverlayError::Namespace(NamespaceError::NotFound {
            path: path.display(),
        })),
    }
}

/// Sets the **stored** attribute bits and moves `changed_ns`.
///
/// Validation happens **here** — the layer closest to the disk — with the same `fs_semantics::attributes::validate` the
/// core and the frontend call, so a derived bit cannot be stored by a caller that bypasses the rules layer.
///
/// # Errors
///
/// [`OverlayError::InvalidRecord`] for a derived/unknown bit, `NotFound` when the inode does not exist, SQL failures.
pub(crate) fn set_attributes_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    attributes: u32,
) -> OverlayResult<()> {
    fs_semantics::attributes::validate(attributes).map_err(|failure| {
        OverlayError::InvalidRecord {
            field: "ov_inode.attributes",
            detail: format!("attributes {attributes:#x} rejected: {failure:?}"),
        }
    })?;
    let now = now_ns();
    if inode_row(connection, id.serial())?.is_none() {
        let Some(record) = base.inode(id.serial())? else {
            return Err(OverlayError::Namespace(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            }));
        };
        // A base inode acquires an override row the first time it is touched. Everything except the field being set keeps
        // the base's value — including `base_limit`, which is the whole size because no content was written — so "what the
        // workspace sees" changes in exactly one place.
        connection.execute(
            "INSERT INTO ov_inode
             (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries, changed_ns, attributes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0)",
            (
                id.serial() as i64,
                schema::ORIGIN_BASE,
                schema::LIVE,
                kind_tag(record.kind),
                record.size as i64,
                record.created_ns,
                record.modified_ns,
                record.size as i64,
                now,
            ),
        )?;
    }
    connection.execute(
        "UPDATE ov_inode SET attributes = ?1, changed_ns = ?2 WHERE serial = ?3",
        (i64::from(attributes), now, id.serial() as i64),
    )?;
    Ok(())
}

/// The stored attribute bits for `id`: 0 when the overlay holds no row (a base inode nobody has touched).
///
/// # Errors
///
/// SQL failures.
pub(crate) fn attributes_impl(connection: &Connection, id: FileId) -> OverlayResult<u32> {
    match inode_row(connection, id.serial())? {
        Some(row) => Ok(u32::try_from(row.attributes).unwrap_or(0)),
        None => Ok(0),
    }
}
/// Sets the requested timestamps; `None` leaves a field alone, and `changed_ns` moves unless it was set explicitly.
///
/// An explicit `changed` wins over the implicit "metadata changed" bump: the caller that says what the time is knows more
/// than the clock does (`SetFileTime` can backdate).
///
/// # Errors
///
/// [`OverlayError::Namespace`] with `NotFound` for an absent inode, SQL failures.
pub(crate) fn set_times_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    accessed: Option<i64>,
    written: Option<i64>,
    changed: Option<i64>,
    created: Option<i64>,
) -> OverlayResult<()> {
    let now = now_ns();
    if inode_row(connection, id.serial())?.is_none() {
        let Some(record) = base.inode(id.serial())? else {
            return Err(OverlayError::Namespace(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            }));
        };
        connection.execute(
            "INSERT INTO ov_inode
             (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries, changed_ns, attributes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0)",
            (
                id.serial() as i64,
                schema::ORIGIN_BASE,
                schema::LIVE,
                kind_tag(record.kind),
                record.size as i64,
                record.created_ns,
                record.modified_ns,
                record.size as i64,
                now,
            ),
        )?;
    }
    connection.execute(
        "UPDATE ov_inode SET
             accessed_ns = COALESCE(?1, accessed_ns),
             modified_ns = COALESCE(?2, modified_ns),
             changed_ns  = COALESCE(?3, ?4),
             created_ns  = COALESCE(?5, created_ns)
         WHERE serial = ?6",
        (accessed, written, changed, now, created, id.serial() as i64),
    )?;
    Ok(())
}
/// Stores (or clears) the descriptor and moves `changed_ns`.
///
/// The length limit is enforced **here**, at the disk-facing layer, for the same reason the attribute bits are: a caller
/// that bypasses the rules layer must not be able to write a 1 MiB descriptor into an inode row.
///
/// # Errors
///
/// [`OverlayError::InvalidRecord`] over 65536 bytes, `NotFound` for an absent inode, SQL failures.
pub(crate) fn set_security_impl(
    connection: &Connection,
    base: &crate::base::BaseLayer,
    id: FileId,
    security: Option<&[u8]>,
) -> OverlayResult<()> {
    let bytes = security.unwrap_or(&[]);
    // The limit is the record codec's (`fs_snapshot::MAX_SECURITY_LEN`); this layer only enforces it before the bytes reach
    // SQL, so a caller that bypasses the rules layer still cannot write an oversized row.
    if bytes.len() > usize::try_from(fs_snapshot::MAX_SECURITY_LEN).expect("fits") {
        return Err(OverlayError::InvalidRecord {
            field: "ov_inode.security",
            detail: format!("descriptor is {} bytes, limit 65536", bytes.len()),
        });
    }
    let now = now_ns();
    if inode_row(connection, id.serial())?.is_none() {
        let Some(record) = base.inode(id.serial())? else {
            return Err(OverlayError::Namespace(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            }));
        };
        connection.execute(
            "INSERT INTO ov_inode
             (serial, origin, live, kind, size, created_ns, modified_ns, base_limit, entries, changed_ns, attributes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0)",
            (
                id.serial() as i64,
                schema::ORIGIN_BASE,
                schema::LIVE,
                kind_tag(record.kind),
                record.size as i64,
                record.created_ns,
                record.modified_ns,
                record.size as i64,
                now,
            ),
        )?;
    }
    connection.execute(
        "UPDATE ov_inode SET security = ?1, changed_ns = ?2 WHERE serial = ?3",
        (bytes.to_vec(), now, id.serial() as i64),
    )?;
    Ok(())
}

/// The stored descriptor, if any.
///
/// # Errors
///
/// SQL failures.
pub(crate) fn security_impl(connection: &Connection, id: FileId) -> OverlayResult<Option<Vec<u8>>> {
    match inode_row(connection, id.serial())? {
        Some(row) if !row.security.is_empty() => Ok(Some(row.security)),
        _ => Ok(None),
    }
}
