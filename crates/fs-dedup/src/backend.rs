//! `fs_winfsp::WorkspaceOps` backend over `DedupStore`.
//!
//! - O(log N) case-insensitive lookup with exact case preservation via `dentries(ws_id, parent_id, folded_name)`.
//! - Native-speed writes directly to NTFS sparse overlay files (`STORAGE_OVERLAY`), with automatic
//!   Copy-on-Write materialization on first write/truncate to a deduplicated (`STORAGE_CHUNKS`) file.
//! - Per-inode stripe locking shared with `DedupStore::dedup_workspace` so background cold-file
//!   deduplication never races with live WinFsp I/O.

use crate::error::Error;
use crate::meta::{
    find_workspace_id, is_hot_db_name, load_inode, now_ns, release_manifest_chunks, InodeRow,
    KIND_DIR, KIND_FILE, ROOT_INODE, STORAGE_CHUNKS, STORAGE_OVERLAY,
};
use crate::store::DedupStore;
use fs_semantics::attributes::{report as report_attributes, validate as validate_attributes};
use fs_semantics::disposition::{decide, Action, Disposition, Exists, Failure, Outcome};
use fs_semantics::name::fold_bytes;
use fs_winfsp::ops::{
    Attributes, DirEntry, DirOutcome, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, Path,
    RenameOutcome, Times, WorkspaceOps,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// WinFsp `WorkspaceOps` backend serving one workspace of a `DedupStore`.
pub struct DedupWorkspaceBackend {
    store: Arc<DedupStore>,
    ws_id: u64,
    ws_name: String,
    conn: Mutex<Connection>,
    next_inode: AtomicU64,
}

impl DedupWorkspaceBackend {
    /// Opens `ws_name` in `store` for live WinFsp serving.
    pub fn open(store: Arc<DedupStore>, ws_name: &str) -> crate::Result<Self> {
        let conn = store.open_conn()?;
        let ws_id = find_workspace_id(&conn, ws_name)?;
        let next_inode: i64 = conn.query_row(
            "SELECT next_inode FROM workspaces WHERE ws_id = ?1",
            params![ws_id as i64],
            |r| r.get(0),
        )?;
        Ok(Self {
            store,
            ws_id,
            ws_name: ws_name.to_owned(),
            conn: Mutex::new(conn),
            next_inode: AtomicU64::new(next_inode.max((ROOT_INODE + 1) as i64) as u64),
        })
    }

    pub fn ws_id(&self) -> u64 {
        self.ws_id
    }

    pub fn ws_name(&self) -> &str {
        &self.ws_name
    }

    pub fn store(&self) -> &Arc<DedupStore> {
        &self.store
    }

    fn alloc_inode(&self, tx: &rusqlite::Transaction<'_>) -> OpsResult<u64> {
        let id = self.next_inode.fetch_add(1, Ordering::Relaxed);
        tx.execute(
            "UPDATE workspaces SET next_inode = MAX(next_inode, ?1) WHERE ws_id = ?2",
            params![(id + 1) as i64, self.ws_id as i64],
        )
        .map_err(map_sql)?;
        Ok(id)
    }

    fn lookup_child(
        &self,
        conn: &Connection,
        parent_id: u64,
        folded: &[u8],
    ) -> OpsResult<Option<(u64, u8, Vec<u8>)>> {
        conn.query_row(
            "SELECT child_id, kind, display_name FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
            params![self.ws_id as i64, parent_id as i64, folded],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u8,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql)
    }

    /// Splits a normalized WinFsp `Path` into `(parent_dir_id, leaf_display, leaf_folded)`.
    /// Returns `Ok(None)` when `path` is the root `/`.
    #[allow(clippy::type_complexity)]
    fn resolve_parent(
        &self,
        conn: &Connection,
        path: &Path<'_>,
    ) -> OpsResult<Option<(u64, Vec<u8>, Vec<u8>)>> {
        let display_parts = split_components(path.display())?;
        let folded_parts = split_components(path.key())?;
        if display_parts.is_empty() {
            return Ok(None);
        }
        if display_parts.len() != folded_parts.len() {
            return Err(OpsError::InvalidParameter);
        }
        let mut cur = ROOT_INODE;
        for part in folded_parts.iter().take(display_parts.len() - 1) {
            let Some((child_id, kind, _)) = self.lookup_child(conn, cur, part)? else {
                return Err(OpsError::NotFound);
            };
            if kind != KIND_DIR {
                return Err(OpsError::NotADirectory);
            }
            cur = child_id;
        }
        let last_disp = display_parts[display_parts.len() - 1].to_vec();
        let last_fold = folded_parts[folded_parts.len() - 1].to_vec();
        Ok(Some((cur, last_disp, last_fold)))
    }

    /// Materializes a `STORAGE_CHUNKS` file into its local sparse overlay file (Copy-on-Write)
    /// and flips `inodes.storage_mode = STORAGE_OVERLAY` while decrementing old chunk refcounts.
    fn ensure_overlay_cow(&self, conn: &mut Connection, inode: &mut InodeRow) -> OpsResult<()> {
        if inode.storage_mode == STORAGE_OVERLAY {
            return Ok(());
        }
        // Materialize all existing chunks into the sparse overlay file first.
        let mut file = self
            .store
            .overlay()
            .create_empty(self.ws_id, inode.inode_id)
            .map_err(map_err)?;
        if inode.size > 0 && !inode.manifest.is_empty() {
            let entries = crate::meta::decode_manifest(&inode.manifest).map_err(map_err)?;
            for entry in entries {
                let loc = crate::meta::get_chunk_loc(conn, &entry.hash)
                    .map_err(map_err)?
                    .ok_or_else(|| OpsError::Other("missing chunk during CoW".into()))?;
                let raw = self
                    .store
                    .packs()
                    .read_chunk(&entry.hash, loc)
                    .map_err(map_err)?;
                file.write_all(&raw).map_err(map_io)?;
            }
            file.flush().map_err(map_io)?;
        }
        let ts = now_ns();
        let tx = conn.transaction().map_err(map_sql)?;
        release_manifest_chunks(&tx, &inode.manifest).map_err(map_err)?;
        tx.execute(
            "UPDATE inodes SET storage_mode = ?1, manifest = X'', dirty_since_ns = ?2
             WHERE ws_id = ?3 AND inode_id = ?4",
            params![
                i64::from(STORAGE_OVERLAY),
                ts,
                self.ws_id as i64,
                inode.inode_id as i64
            ],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        inode.storage_mode = STORAGE_OVERLAY;
        inode.manifest.clear();
        inode.dirty_since_ns = ts;
        Ok(())
    }

    fn is_descendant(&self, conn: &Connection, ancestor_id: u64, mut node_id: u64) -> OpsResult<bool> {
        if ancestor_id == node_id {
            return Ok(true);
        }
        let mut depth = 0usize;
        while node_id != ROOT_INODE && depth < 1024 {
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT parent_id FROM dentries WHERE ws_id = ?1 AND child_id = ?2 LIMIT 1",
                    params![self.ws_id as i64, node_id as i64],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_sql)?;
            let Some(p) = parent else {
                break;
            };
            let p = p as u64;
            if p == ancestor_id {
                return Ok(true);
            }
            node_id = p;
            depth += 1;
        }
        Ok(false)
    }
}

fn split_components(raw: &[u8]) -> OpsResult<Vec<&[u8]>> {
    let trimmed = raw
        .strip_prefix(b"/")
        .or_else(|| raw.strip_prefix(b"\\"))
        .unwrap_or(raw);
    let trimmed = trimmed
        .strip_suffix(b"/")
        .or_else(|| trimmed.strip_suffix(b"\\"))
        .unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for seg in trimmed.split(|&b| b == b'/' || b == b'\\') {
        if seg.is_empty() || seg == b"." || seg == b".." {
            return Err(OpsError::InvalidParameter);
        }
        parts.push(seg);
    }
    Ok(parts)
}

fn inode_to_node_info(row: &InodeRow) -> NodeInfo {
    let is_dir = row.kind == KIND_DIR;
    NodeInfo {
        file_id: row.inode_id,
        kind: if is_dir { Kind::Directory } else { Kind::File },
        size: if is_dir { 0 } else { row.size },
        times: Times {
            created: row.created_ns,
            accessed: row.accessed_ns,
            written: row.written_ns,
            changed: row.changed_ns,
        },
        attributes: Attributes(report_attributes(is_dir, row.attributes)),
    }
}

fn map_failure(f: Failure) -> OpsError {
    match f {
        Failure::LeafMissing | Failure::ParentMissing => OpsError::NotFound,
        Failure::NameCollision => OpsError::NameCollision,
        Failure::NotADirectory => OpsError::NotADirectory,
        Failure::IsADirectory => OpsError::IsADirectory,
        Failure::AccessDenied => OpsError::AccessDenied,
        Failure::DirectoryNotEmpty => OpsError::NotEmpty,
        Failure::DeletePending => OpsError::DeletePending,
        Failure::SharingViolation => OpsError::SharingViolation,
        Failure::InvalidName | Failure::InvalidParameter => OpsError::InvalidParameter,
        other => OpsError::Other(format!("{other:?}")),
    }
}

fn map_sql(e: rusqlite::Error) -> OpsError {
    OpsError::Other(format!("sqlite: {e}"))
}

fn map_io(e: std::io::Error) -> OpsError {
    OpsError::Other(format!("io: {e}"))
}

fn map_err(e: Error) -> OpsError {
    match e {
        Error::NotFound(_) => OpsError::NotFound,
        Error::AlreadyExists(_) => OpsError::NameCollision,
        Error::InvalidInput(_) => OpsError::InvalidParameter,
        other => OpsError::Other(other.to_string()),
    }
}

impl WorkspaceOps for DedupWorkspaceBackend {
    fn create_or_open(&self, path: &Path<'_>, disposition: Disposition) -> OpsResult<OpenOutcome> {
        let mut conn = self.conn.lock().unwrap();
        let Some((parent_id, leaf_disp, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            // Root is a directory — evaluate against the exact disposition table.
            return match decide(disposition, Exists::Directory) {
                Outcome::Failed(fail) => Err(map_failure(fail)),
                Outcome::Opened(_) => Err(OpsError::NotADirectory),
            };
        };
        let existing = self.lookup_child(&conn, parent_id, &leaf_fold)?;
        let exists = match &existing {
            None => Exists::Absent,
            Some((_, kind, _)) if *kind == KIND_DIR => Exists::Directory,
            Some(_) => Exists::File,
        };
        let action = match decide(disposition, exists) {
            Outcome::Opened(act) => act,
            Outcome::Failed(fail) => return Err(map_failure(fail)),
        };

        match action {
            Action::CreateNew => {
                let ts = now_ns();
                let pinned = std::str::from_utf8(&leaf_disp)
                    .map(is_hot_db_name)
                    .unwrap_or(false);
                let tx = conn.transaction().map_err(map_sql)?;
                let inode_id = self.alloc_inode(&tx)?;
                tx.execute(
                    "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
                     VALUES (?1, ?2, ?3, 0, 0, ?4, ?4, ?4, ?4, ?5, X'', ?4, ?6)",
                    params![
                        self.ws_id as i64,
                        inode_id as i64,
                        i64::from(KIND_FILE),
                        ts,
                        i64::from(STORAGE_OVERLAY),
                        i64::from(pinned),
                    ],
                )
                .map_err(map_sql)?;
                tx.execute(
                    "INSERT INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        self.ws_id as i64,
                        parent_id as i64,
                        leaf_fold.as_slice(),
                        leaf_disp.as_slice(),
                        inode_id as i64,
                        i64::from(KIND_FILE),
                    ],
                )
                .map_err(map_sql)?;
                tx.commit().map_err(map_sql)?;
                self.store
                    .overlay()
                    .create_empty(self.ws_id, inode_id)
                    .map_err(map_err)?;

                Ok(OpenOutcome {
                    file_id: inode_id,
                    kind: Kind::File,
                    action: Action::CreateNew,
                    size: 0,
                    times: Times {
                        created: ts,
                        accessed: ts,
                        written: ts,
                        changed: ts,
                    },
                    attributes: Attributes(report_attributes(false, 0)),
                    binding: (parent_id, leaf_disp),
                    replaced: None,
                })
            }
            Action::Open => {
                let (child_id, _, stored_name) = existing.unwrap();
                let row = load_inode(&conn, self.ws_id, child_id)
                    .map_err(map_err)?
                    .ok_or(OpsError::NotFound)?;
                let info = inode_to_node_info(&row);
                Ok(OpenOutcome {
                    file_id: info.file_id,
                    kind: info.kind,
                    action: Action::Open,
                    size: info.size,
                    times: info.times,
                    attributes: info.attributes,
                    binding: (parent_id, stored_name),
                    replaced: None,
                })
            }
            Action::TruncateExisting => {
                let (child_id, _, stored_name) = existing.unwrap();
                let _lock = self.store.lock_inode(self.ws_id, child_id);
                let row = load_inode(&conn, self.ws_id, child_id)
                    .map_err(map_err)?
                    .ok_or(OpsError::NotFound)?;
                let ts = now_ns();
                let tx = conn.transaction().map_err(map_sql)?;
                if row.storage_mode == STORAGE_CHUNKS && !row.manifest.is_empty() {
                    release_manifest_chunks(&tx, &row.manifest).map_err(map_err)?;
                }
                let new_attrs = if disposition == Disposition::Supersede {
                    0u32
                } else {
                    row.attributes
                };
                tx.execute(
                    "UPDATE inodes SET size = 0, attributes = ?1, written_ns = ?2, changed_ns = ?2,
                                       storage_mode = ?3, manifest = X'', dirty_since_ns = ?2
                     WHERE ws_id = ?4 AND inode_id = ?5",
                    params![
                        i64::from(new_attrs),
                        ts,
                        i64::from(STORAGE_OVERLAY),
                        self.ws_id as i64,
                        child_id as i64,
                    ],
                )
                .map_err(map_sql)?;
                tx.commit().map_err(map_sql)?;
                self.store
                    .overlay()
                    .create_empty(self.ws_id, child_id)
                    .map_err(map_err)?;

                Ok(OpenOutcome {
                    file_id: child_id,
                    kind: Kind::File,
                    action: Action::TruncateExisting,
                    size: 0,
                    times: Times {
                        created: row.created_ns,
                        accessed: row.accessed_ns,
                        written: ts,
                        changed: ts,
                    },
                    attributes: Attributes(report_attributes(false, new_attrs)),
                    binding: (parent_id, stored_name),
                    replaced: None,
                })
            }
        }
    }

    fn mkdir(&self, path: &Path<'_>) -> OpsResult<u64> {
        let mut conn = self.conn.lock().unwrap();
        let Some((parent_id, leaf_disp, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            return Err(OpsError::NameCollision);
        };
        if self.lookup_child(&conn, parent_id, &leaf_fold)?.is_some() {
            return Err(OpsError::NameCollision);
        }
        let ts = now_ns();
        let tx = conn.transaction().map_err(map_sql)?;
        let inode_id = self.alloc_inode(&tx)?;
        tx.execute(
            "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
             VALUES (?1, ?2, ?3, 0, 0, ?4, ?4, ?4, ?4, ?5, X'', 0, 0)",
            params![
                self.ws_id as i64,
                inode_id as i64,
                i64::from(KIND_DIR),
                ts,
                i64::from(STORAGE_CHUNKS),
            ],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.ws_id as i64,
                parent_id as i64,
                leaf_fold.as_slice(),
                leaf_disp.as_slice(),
                inode_id as i64,
                i64::from(KIND_DIR),
            ],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        Ok(inode_id)
    }

    fn open_dir(&self, path: &Path<'_>) -> OpsResult<u64> {
        let conn = self.conn.lock().unwrap();
        let Some((parent_id, _, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            return Ok(ROOT_INODE);
        };
        let Some((child_id, kind, _)) = self.lookup_child(&conn, parent_id, &leaf_fold)? else {
            return Err(OpsError::NotFound);
        };
        if kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }
        Ok(child_id)
    }

    fn dir_binding(&self, path: &Path<'_>) -> Option<DirOutcome> {
        let conn = self.conn.lock().unwrap();
        let (parent_id, _, leaf_fold) = self.resolve_parent(&conn, path).ok()??;
        let (child_id, kind, stored_name) = self.lookup_child(&conn, parent_id, &leaf_fold).ok()??;
        if kind != KIND_DIR {
            return None;
        }
        Some(DirOutcome {
            id: child_id,
            binding: (parent_id, stored_name),
        })
    }

    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        let conn = self.conn.lock().unwrap();
        let row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut out = if offset >= row.size {
            Vec::new()
        } else if row.storage_mode == STORAGE_OVERLAY {
            self.store
                .overlay()
                .read_at(self.ws_id, file_id, offset, len)
                .map_err(map_err)?
        } else {
            self.store
                .read_chunks_inode(&conn, &row, offset, len)
                .map_err(map_err)?
        };
        // WorkspaceOps::read contract: short reads at EOF are zero-padded to `len`.
        out.resize(len, 0);
        Ok(out)
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        let mut conn = self.conn.lock().unwrap();
        let mut row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        self.ensure_overlay_cow(&mut conn, &mut row)?;
        let new_len = self
            .store
            .overlay()
            .write_at(self.ws_id, file_id, offset, bytes)
            .map_err(map_err)?
            .max(row.size);
        let ts = now_ns();
        conn.execute(
            "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
             WHERE ws_id = ?3 AND inode_id = ?4",
            params![new_len as i64, ts, self.ws_id as i64, file_id as i64],
        )
        .map_err(map_sql)?;
        Ok(bytes.len())
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        let mut conn = self.conn.lock().unwrap();
        let mut row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        if len == 0 && row.storage_mode == STORAGE_CHUNKS {
            // Fast truncate-to-zero without materializing old chunks.
            let ts = now_ns();
            let tx = conn.transaction().map_err(map_sql)?;
            release_manifest_chunks(&tx, &row.manifest).map_err(map_err)?;
            tx.execute(
                "UPDATE inodes SET size = 0, written_ns = ?1, changed_ns = ?1,
                                   storage_mode = ?2, manifest = X'', dirty_since_ns = ?1
                 WHERE ws_id = ?3 AND inode_id = ?4",
                params![
                    ts,
                    i64::from(STORAGE_OVERLAY),
                    self.ws_id as i64,
                    file_id as i64
                ],
            )
            .map_err(map_sql)?;
            tx.commit().map_err(map_sql)?;
            self.store
                .overlay()
                .create_empty(self.ws_id, file_id)
                .map_err(map_err)?;
            return Ok(());
        }
        self.ensure_overlay_cow(&mut conn, &mut row)?;
        self.store
            .overlay()
            .set_len(self.ws_id, file_id, len)
            .map_err(map_err)?;
        let ts = now_ns();
        conn.execute(
            "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
             WHERE ws_id = ?3 AND inode_id = ?4",
            params![len as i64, ts, self.ws_id as i64, file_id as i64],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let conn = self.conn.lock().unwrap();
        let row = load_inode(&conn, self.ws_id, dir_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        if row.kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }
        let mut stmt = conn
            .prepare_cached(
                "SELECT display_name, kind, child_id FROM dentries WHERE ws_id = ?1 AND parent_id = ?2",
            )
            .map_err(map_sql)?;
        let rows = stmt
            .query_map(params![self.ws_id as i64, dir_id as i64], |r| {
                let name: Vec<u8> = r.get(0)?;
                let kind_u8: i64 = r.get(1)?;
                let child_id: i64 = r.get(2)?;
                Ok(DirEntry {
                    name,
                    kind: if kind_u8 as u8 == KIND_DIR {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    file_id: child_id as u64,
                })
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_sql)?);
        }
        Ok(out)
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        let conn = self.conn.lock().unwrap();
        let row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        Ok(inode_to_node_info(&row))
    }

    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()> {
        let conn = self.conn.lock().unwrap();
        let row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        conn.execute(
            "UPDATE inodes SET created_ns = ?1, accessed_ns = ?2, written_ns = ?3, changed_ns = ?4
             WHERE ws_id = ?5 AND inode_id = ?6",
            params![
                created.unwrap_or(row.created_ns),
                accessed.unwrap_or(row.accessed_ns),
                written.unwrap_or(row.written_ns),
                changed.unwrap_or(row.changed_ns),
                self.ws_id as i64,
                file_id as i64,
            ],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        validate_attributes(attributes.0).map_err(|_| OpsError::InvalidParameter)?;
        let conn = self.conn.lock().unwrap();
        let updated = conn
            .execute(
                "UPDATE inodes SET attributes = ?1, changed_ns = ?2 WHERE ws_id = ?3 AND inode_id = ?4",
                params![
                    i64::from(attributes.0),
                    now_ns(),
                    self.ws_id as i64,
                    file_id as i64,
                ],
            )
            .map_err(map_sql)?;
        if updated == 0 {
            return Err(OpsError::NotFound);
        }
        Ok(())
    }

    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        let mut conn = self.conn.lock().unwrap();
        let Some((src_parent, _, src_fold)) = self.resolve_parent(&conn, from)? else {
            return Err(OpsError::InvalidParameter);
        };
        let Some((dst_parent, dst_disp, dst_fold)) = self.resolve_parent(&conn, to)? else {
            return Err(OpsError::InvalidParameter);
        };
        let Some((src_id, src_kind, _)) = self.lookup_child(&conn, src_parent, &src_fold)? else {
            return Err(OpsError::NotFound);
        };

        // Case-only or identical rename within the same parent.
        if src_parent == dst_parent && src_fold == dst_fold {
            conn.execute(
                "UPDATE dentries SET display_name = ?1 WHERE ws_id = ?2 AND parent_id = ?3 AND folded_name = ?4",
                params![
                    dst_disp.as_slice(),
                    self.ws_id as i64,
                    src_parent as i64,
                    src_fold.as_slice(),
                ],
            )
            .map_err(map_sql)?;
            return Ok(RenameOutcome {
                file_id: src_id,
                binding: (dst_parent, dst_disp),
                replaced: None,
            });
        }

        // Prevent moving a directory into itself or one of its descendants.
        if src_kind == KIND_DIR && self.is_descendant(&conn, src_id, dst_parent)? {
            return Err(OpsError::InvalidParameter);
        }

        let dst_existing = self.lookup_child(&conn, dst_parent, &dst_fold)?;
        let mut replaced_id = None;
        let mut replaced_overlay_file = None;

        if let Some((dst_id, dst_kind, _)) = dst_existing {
            if dst_id == src_id {
                return Ok(RenameOutcome {
                    file_id: src_id,
                    binding: (dst_parent, dst_disp),
                    replaced: None,
                });
            }
            if !replace {
                return Err(OpsError::NameCollision);
            }
            if src_kind != dst_kind {
                return Err(OpsError::TypeMismatch);
            }
            if dst_kind == KIND_DIR {
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM dentries WHERE ws_id = ?1 AND parent_id = ?2",
                        params![self.ws_id as i64, dst_id as i64],
                        |r| r.get(0),
                    )
                    .map_err(map_sql)?;
                if count > 0 {
                    return Err(OpsError::NotEmpty);
                }
            }
            replaced_id = Some(dst_id);
        }

        let tx = conn.transaction().map_err(map_sql)?;
        if let Some(dst_id) = replaced_id {
            if let Some(old_row) = load_inode(&tx, self.ws_id, dst_id).map_err(map_err)? {
                if old_row.storage_mode == STORAGE_CHUNKS && !old_row.manifest.is_empty() {
                    release_manifest_chunks(&tx, &old_row.manifest).map_err(map_err)?;
                } else if old_row.storage_mode == STORAGE_OVERLAY {
                    replaced_overlay_file = Some(dst_id);
                }
            }
            tx.execute(
                "DELETE FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
                params![self.ws_id as i64, dst_parent as i64, dst_fold.as_slice()],
            )
            .map_err(map_sql)?;
            tx.execute(
                "DELETE FROM inodes WHERE ws_id = ?1 AND inode_id = ?2",
                params![self.ws_id as i64, dst_id as i64],
            )
            .map_err(map_sql)?;
        }

        tx.execute(
            "DELETE FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
            params![self.ws_id as i64, src_parent as i64, src_fold.as_slice()],
        )
        .map_err(map_sql)?;
        tx.execute(
            "INSERT INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.ws_id as i64,
                dst_parent as i64,
                dst_fold.as_slice(),
                dst_disp.as_slice(),
                src_id as i64,
                i64::from(src_kind),
            ],
        )
        .map_err(map_sql)?;
        // Update pinned_overlay flag if renamed to/from a hot DB name.
        if src_kind == KIND_FILE {
            let pinned = std::str::from_utf8(&dst_disp)
                .map(is_hot_db_name)
                .unwrap_or(false);
            tx.execute(
                "UPDATE inodes SET changed_ns = ?1, pinned_overlay = ?2 WHERE ws_id = ?3 AND inode_id = ?4",
                params![now_ns(), i64::from(pinned), self.ws_id as i64, src_id as i64],
            )
            .map_err(map_sql)?;
        }
        tx.commit().map_err(map_sql)?;

        if let Some(old_id) = replaced_overlay_file {
            let _ = self.store.overlay().remove_file(self.ws_id, old_id);
        }

        Ok(RenameOutcome {
            file_id: src_id,
            binding: (dst_parent, dst_disp),
            replaced: replaced_id,
        })
    }

    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        let _lock = self.store.lock_inode(self.ws_id, expected_id);
        let mut conn = self.conn.lock().unwrap();
        let folded = fold_bytes(name);
        let Some((child_id, kind, _)) = self.lookup_child(&conn, parent, &folded)? else {
            // Idempotent when the expected inode is already retired; `NotFound` when it is still live elsewhere.
            let still_live = load_inode(&conn, self.ws_id, expected_id)
                .map_err(map_err)?
                .is_some();
            return if still_live {
                Err(OpsError::NotFound)
            } else {
                Ok(())
            };
        };
        if child_id != expected_id {
            return Err(OpsError::InvalidRecord(format!(
                "stale identity for remove_at: expected {expected_id}, found {child_id}"
            )));
        }
        if kind == KIND_DIR {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM dentries WHERE ws_id = ?1 AND parent_id = ?2",
                    params![self.ws_id as i64, child_id as i64],
                    |r| r.get(0),
                )
                .map_err(map_sql)?;
            if count > 0 {
                return Err(OpsError::NotEmpty);
            }
        }
        let old_row = load_inode(&conn, self.ws_id, child_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;

        let tx = conn.transaction().map_err(map_sql)?;
        if old_row.storage_mode == STORAGE_CHUNKS && !old_row.manifest.is_empty() {
            release_manifest_chunks(&tx, &old_row.manifest).map_err(map_err)?;
        }
        tx.execute(
            "DELETE FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
            params![self.ws_id as i64, parent as i64, folded.as_slice()],
        )
        .map_err(map_sql)?;
        tx.execute(
            "DELETE FROM inodes WHERE ws_id = ?1 AND inode_id = ?2",
            params![self.ws_id as i64, child_id as i64],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;

        if old_row.storage_mode == STORAGE_OVERLAY {
            let _ = self.store.overlay().remove_file(self.ws_id, child_id);
        }
        Ok(())
    }

    fn flush(&self) -> OpsResult<()> {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        Ok(())
    }
}
