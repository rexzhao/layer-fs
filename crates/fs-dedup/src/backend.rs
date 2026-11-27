//! `fs_winfsp::WorkspaceOps` backend over `DedupStore`.
//!
//! Performance architecture (tuned for 4M+ file Unity repositories):
//! - Concurrent read-only WAL SQLite connection pool (`RoConnPool`, 4 GiB OS mmap) so
//!   `Open`, `GetFileInfo`, `ReadDirectory`, and `Read` run in parallel across all WinFsp
//!   dispatcher threads without serializing behind a single writer mutex.
//! - Single-query `read_dir` JOIN prefetching child `NodeInfo` + chunk manifests into RAM
//!   (`DedupStore` sharded `InodeCache` + `DentryCache`), eliminating N+1 cold B-tree seeks
//!   when VS Code / Explorer / Unity lists large directories.
//! - Completely lock-free `.pack` disk I/O + Zstd/LZ4 decompression during `read()` (zero
//!   SQLite lock held across pack reads).
//! - Native-speed writes directly to NTFS sparse overlay files (`STORAGE_OVERLAY`), with
//!   automatic Copy-on-Write materialization and per-inode stripe locking shared with
//!   background cold-file deduplication.

use crate::error::Error;
use crate::meta::{
    find_workspace_id, is_hot_db_name, load_inode, now_ns, release_manifest_chunks, InodeRow,
    KIND_DIR, KIND_FILE, ROOT_INODE, STORAGE_CHUNKS, STORAGE_OVERLAY,
};
use crate::store::{CachedInode, DedupStore};
use fs_semantics::attributes::{report as report_attributes, validate as validate_attributes};
use fs_semantics::disposition::{decide, Action, Disposition, Exists, Failure, Outcome};
use fs_semantics::name::fold_bytes;
use fs_winfsp::ops::{
    Attributes, DirEntry, DirOutcome, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, Path,
    RenameOutcome, Times, WorkspaceOps,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const MAX_RO_CONNS: usize = 16;
const DENTRY_SHARDS: usize = 16;
const MAX_DENTRY_PER_SHARD: usize = 32_768; // 524k total across 16 shards

/// Pool of read-only WAL SQLite connections returned automatically on drop.
struct RoConnPool {
    store: Arc<DedupStore>,
    idle: Mutex<Vec<Connection>>,
}

struct RoConnGuard<'a> {
    pool: &'a RoConnPool,
    conn: Option<Connection>,
}

impl std::ops::Deref for RoConnGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}

impl Drop for RoConnGuard<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut idle = self.pool.idle.lock().unwrap();
            if idle.len() < MAX_RO_CONNS {
                idle.push(conn);
            }
        }
    }
}

impl RoConnPool {
    fn new(store: Arc<DedupStore>) -> Self {
        Self {
            store,
            idle: Mutex::new(Vec::with_capacity(MAX_RO_CONNS)),
        }
    }

    fn get(&self) -> OpsResult<RoConnGuard<'_>> {
        if let Some(conn) = self.idle.lock().unwrap().pop() {
            return Ok(RoConnGuard {
                pool: self,
                conn: Some(conn),
            });
        }
        let conn = self.store.open_readonly_conn().map_err(map_err)?;
        Ok(RoConnGuard {
            pool: self,
            conn: Some(conn),
        })
    }
}

type DentryVal = (u64, u8, Vec<u8>);
type DentryShard = RwLock<HashMap<(u64, Vec<u8>), DentryVal>>;

/// WinFsp `WorkspaceOps` backend serving one workspace of a `DedupStore`.
pub struct DedupWorkspaceBackend {
    store: Arc<DedupStore>,
    ws_id: u64,
    ws_name: String,
    /// Writer SQLite connection (used ONLY for mutations).
    write_conn: Mutex<Connection>,
    /// Concurrent read-only WAL SQLite connection pool (used for all read/stat/list/open paths).
    ro_pool: RoConnPool,
    /// Sharded positive `(parent_id, folded_name) -> (child_id, kind, display_name)` cache.
    dentry_cache: Vec<DentryShard>,
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
        let ro_pool = RoConnPool::new(Arc::clone(&store));
        let dentry_cache = (0..DENTRY_SHARDS)
            .map(|_| RwLock::new(HashMap::with_capacity(512)))
            .collect();
        Ok(Self {
            store,
            ws_id,
            ws_name: ws_name.to_owned(),
            write_conn: Mutex::new(conn),
            ro_pool,
            dentry_cache,
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

    #[inline]
    fn dentry_shard(&self, parent_id: u64) -> &RwLock<HashMap<(u64, Vec<u8>), DentryVal>> {
        &self.dentry_cache[(parent_id as usize) & (DENTRY_SHARDS - 1)]
    }

    fn cache_dentry(&self, parent_id: u64, folded: Vec<u8>, val: DentryVal) {
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        if guard.len() >= MAX_DENTRY_PER_SHARD {
            guard.clear();
        }
        guard.insert((parent_id, folded), val);
    }

    fn invalidate_dentry(&self, parent_id: u64, folded: &[u8]) {
        self.dentry_shard(parent_id)
            .write()
            .unwrap()
            .remove(&(parent_id, folded.to_vec()));
    }

    /// Looks up a child in `DentryCache` first, falling back to SQLite `conn`.
    fn lookup_child_cached(
        &self,
        conn: &Connection,
        parent_id: u64,
        folded: &[u8],
    ) -> OpsResult<Option<DentryVal>> {
        {
            let guard = self.dentry_shard(parent_id).read().unwrap();
            if let Some(hit) = guard.get(&(parent_id, folded.to_vec())) {
                return Ok(Some(hit.clone()));
            }
        }
        let found = self.lookup_child_db(conn, parent_id, folded)?;
        if let Some(val) = &found {
            self.cache_dentry(parent_id, folded.to_vec(), val.clone());
        }
        Ok(found)
    }

    fn lookup_child_db(
        &self,
        conn: &Connection,
        parent_id: u64,
        folded: &[u8],
    ) -> OpsResult<Option<DentryVal>> {
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

    /// Loads an inode via `DedupStore`'s sharded RAM cache, falling back to `conn`.
    fn load_inode_cached(&self, conn: &Connection, inode_id: u64) -> OpsResult<Option<Arc<CachedInode>>> {
        if let Some(hit) = self.store.get_cached_inode(self.ws_id, inode_id) {
            return Ok(Some(hit));
        }
        let Some(row) = load_inode(conn, self.ws_id, inode_id).map_err(map_err)? else {
            return Ok(None);
        };
        let cached = Arc::new(CachedInode::from_row(&row).map_err(map_err)?);
        self.store.put_cached_inode(self.ws_id, Arc::clone(&cached));
        Ok(Some(cached))
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
            let Some((child_id, kind, _)) = self.lookup_child_cached(conn, cur, part)? else {
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
        let mut file = self
            .store
            .overlay()
            .create_empty(self.ws_id, inode.inode_id)
            .map_err(map_err)?;
        if inode.size > 0 && !inode.manifest.is_empty() {
            let entries = crate::meta::decode_manifest(&inode.manifest).map_err(map_err)?;
            for entry in entries {
                let loc = self
                    .store
                    .resolve_chunk_loc(conn, &entry.hash)
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
        self.store.invalidate_inode(self.ws_id, inode.inode_id);
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

fn cached_to_node_info(row: &CachedInode) -> NodeInfo {
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
        // Fast concurrent read-only path for `Disposition::Open` (99% of VS Code / Unity opens!).
        if disposition == Disposition::Open {
            let conn = self.ro_pool.get()?;
            let Some((parent_id, _, leaf_fold)) = self.resolve_parent(&conn, path)? else {
                return Err(OpsError::NotADirectory);
            };
            let Some((child_id, kind, stored_name)) =
                self.lookup_child_cached(&conn, parent_id, &leaf_fold)?
            else {
                return Err(OpsError::NotFound);
            };
            if kind == KIND_DIR {
                return Err(OpsError::NotADirectory);
            }
            let row = self
                .load_inode_cached(&conn, child_id)?
                .ok_or(OpsError::NotFound)?;
            let info = cached_to_node_info(&row);
            return Ok(OpenOutcome {
                file_id: info.file_id,
                kind: info.kind,
                action: Action::Open,
                size: info.size,
                times: info.times,
                attributes: info.attributes,
                binding: (parent_id, stored_name),
                replaced: None,
            });
        }

        // Mutating or potentially-creating dispositions serialize cleanly on `write_conn`.
        let mut conn = self.write_conn.lock().unwrap();
        let Some((parent_id, leaf_disp, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            return match decide(disposition, Exists::Directory) {
                Outcome::Failed(fail) => Err(map_failure(fail)),
                Outcome::Opened(_) => Err(OpsError::NotADirectory),
            };
        };
        let existing = self.lookup_child_db(&conn, parent_id, &leaf_fold)?;
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

                self.cache_dentry(parent_id, leaf_fold, (inode_id, KIND_FILE, leaf_disp.clone()));
                self.store.put_cached_inode(
                    self.ws_id,
                    Arc::new(CachedInode {
                        inode_id,
                        kind: KIND_FILE,
                        size: 0,
                        attributes: 0,
                        created_ns: ts,
                        accessed_ns: ts,
                        written_ns: ts,
                        changed_ns: ts,
                        storage_mode: STORAGE_OVERLAY,
                        entries: Some(Arc::from(Vec::new().into_boxed_slice())),
                    }),
                );

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
                let row = self
                    .load_inode_cached(&conn, child_id)?
                    .ok_or(OpsError::NotFound)?;
                let info = cached_to_node_info(&row);
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
                self.store.invalidate_inode(self.ws_id, child_id);

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
        let mut conn = self.write_conn.lock().unwrap();
        let Some((parent_id, leaf_disp, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            return Err(OpsError::NameCollision);
        };
        if self.lookup_child_db(&conn, parent_id, &leaf_fold)?.is_some() {
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
        self.cache_dentry(parent_id, leaf_fold, (inode_id, KIND_DIR, leaf_disp));
        self.store.put_cached_inode(
            self.ws_id,
            Arc::new(CachedInode {
                inode_id,
                kind: KIND_DIR,
                size: 0,
                attributes: 0,
                created_ns: ts,
                accessed_ns: ts,
                written_ns: ts,
                changed_ns: ts,
                storage_mode: STORAGE_CHUNKS,
                entries: Some(Arc::from(Vec::new().into_boxed_slice())),
            }),
        );
        Ok(inode_id)
    }

    fn open_dir(&self, path: &Path<'_>) -> OpsResult<u64> {
        let conn = self.ro_pool.get()?;
        let Some((parent_id, _, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            return Ok(ROOT_INODE);
        };
        let Some((child_id, kind, _)) = self.lookup_child_cached(&conn, parent_id, &leaf_fold)? else {
            return Err(OpsError::NotFound);
        };
        if kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }
        Ok(child_id)
    }

    fn dir_binding(&self, path: &Path<'_>) -> Option<DirOutcome> {
        let conn = self.ro_pool.get().ok()?;
        let (parent_id, _, leaf_fold) = self.resolve_parent(&conn, path).ok()??;
        let (child_id, kind, stored_name) =
            self.lookup_child_cached(&conn, parent_id, &leaf_fold).ok()??;
        if kind != KIND_DIR {
            return None;
        }
        Some(DirOutcome {
            id: child_id,
            binding: (parent_id, stored_name),
        })
    }

    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        // Phase 1: Resolve inode metadata and (if chunk-backed) overlapping chunk locations
        // using the concurrent RO SQLite pool + RAM caches. Release ALL DB locks BEFORE any
        // `.pack` disk read or Zstd/LZ4 decompression!
        let plan = {
            let _lock = self.store.lock_inode(self.ws_id, file_id);
            let mut ro_conn: Option<RoConnGuard<'_>> = None;
            let cached = match self.store.get_cached_inode(self.ws_id, file_id) {
                Some(hit) if hit.storage_mode == STORAGE_OVERLAY || hit.entries.is_some() => hit,
                _ => {
                    // Load full row including `manifest` BLOB lazily on first actual file read.
                    let conn = ro_conn.insert(self.ro_pool.get()?);
                    let Some(row) = load_inode(conn, self.ws_id, file_id).map_err(map_err)? else {
                        return Err(OpsError::NotFound);
                    };
                    let full = Arc::new(CachedInode::from_row(&row).map_err(map_err)?);
                    self.store.put_cached_inode(self.ws_id, Arc::clone(&full));
                    full
                }
            };
            if cached.kind == KIND_DIR {
                return Err(OpsError::IsADirectory);
            }
            if offset >= cached.size {
                return Ok(vec![0u8; len]);
            }
            if cached.storage_mode == STORAGE_OVERLAY {
                // Local sparse overlay read holds ONLY the per-inode stripe lock, zero SQLite lock.
                let mut out = self
                    .store
                    .overlay()
                    .read_at(self.ws_id, file_id, offset, len)
                    .map_err(map_err)?;
                out.resize(len, 0);
                return Ok(out);
            }

            let to_read = (cached.size - offset).min(len as u64) as usize;
            let end = offset + to_read as u64;
            let mut cursor = 0u64;
            let mut spans = Vec::new();
            let entries = cached.entries.as_deref().unwrap_or(&[]);

            for entry in entries {
                let chunk_len = u64::from(entry.raw_len);
                let chunk_end = cursor + chunk_len;
                if chunk_end <= offset {
                    cursor = chunk_end;
                    continue;
                }
                if cursor >= end {
                    break;
                }
                let conn_ref = match &ro_conn {
                    Some(c) => c,
                    None => ro_conn.insert(self.ro_pool.get()?),
                };
                let loc = self
                    .store
                    .resolve_chunk_loc(conn_ref, &entry.hash)
                    .map_err(map_err)?
                    .ok_or_else(|| {
                        OpsError::Other(format!("missing chunk in index for inode {file_id}"))
                    })?;
                let slice_start = offset.saturating_sub(cursor) as usize;
                let slice_end = (end.min(chunk_end) - cursor) as usize;
                spans.push((entry.hash, loc, slice_start, slice_end));
                cursor = chunk_end;
            }
            (to_read, spans)
        };

        // Phase 2: Lock-free parallel pack reads + decompression (zero DB or inode lock held!).
        let (to_read, spans) = plan;
        let mut out = Vec::with_capacity(len);
        for (hash, loc, slice_start, slice_end) in spans {
            let raw = self.store.packs().read_chunk(&hash, loc).map_err(map_err)?;
            out.extend_from_slice(&raw[slice_start..slice_end]);
        }
        debug_assert!(out.len() <= to_read);
        // WorkspaceOps::read contract: short reads at EOF are zero-padded to `len`.
        out.resize(len, 0);
        Ok(out)
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        let mut conn = self.write_conn.lock().unwrap();
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
        self.store.invalidate_inode(self.ws_id, file_id);
        Ok(bytes.len())
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        let mut conn = self.write_conn.lock().unwrap();
        let mut row = load_inode(&conn, self.ws_id, file_id)
            .map_err(map_err)?
            .ok_or(OpsError::NotFound)?;
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        if len == 0 && row.storage_mode == STORAGE_CHUNKS {
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
            self.store.invalidate_inode(self.ws_id, file_id);
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
        self.store.invalidate_inode(self.ws_id, file_id);
        Ok(())
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let conn = self.ro_pool.get()?;
        let dir_row = self
            .load_inode_cached(&conn, dir_id)?
            .ok_or(OpsError::NotFound)?;
        if dir_row.kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }

        // Single JOIN query loads ALL children + their metadata WITHOUT reading `manifest` BLOBs
        // (avoiding SQLite overflow pages & manifest decoding during `git status` / Explorer scans!),
        // and warms both `DentryCache` and `InodeCache`.
        let mut stmt = conn
            .prepare_cached(
                "SELECT d.folded_name, d.display_name, d.kind, d.child_id,
                        i.size, i.attributes, i.created_ns, i.accessed_ns, i.written_ns, i.changed_ns,
                        i.storage_mode
                 FROM dentries d
                 JOIN inodes i ON i.ws_id = d.ws_id AND i.inode_id = d.child_id
                 WHERE d.ws_id = ?1 AND d.parent_id = ?2",
            )
            .map_err(map_sql)?;
        let rows = stmt
            .query_map(params![self.ws_id as i64, dir_id as i64], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)? as u8,
                    r.get::<_, i64>(3)? as u64,
                    r.get::<_, i64>(4)? as u64,
                    r.get::<_, i64>(5)? as u32,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, i64>(10)? as u8,
                ))
            })
            .map_err(map_sql)?;

        let mut out = Vec::new();
        let mut new_dentries = Vec::new();
        for row in rows {
            let (
                folded,
                name,
                kind_u8,
                child_id,
                size,
                attributes,
                created_ns,
                accessed_ns,
                written_ns,
                changed_ns,
                storage_mode,
            ) = row.map_err(map_sql)?;

            // Preserve any already-decoded manifest in cache if present.
            let existing_entries = self
                .store
                .get_cached_inode(self.ws_id, child_id)
                .and_then(|c| c.entries.clone());
            self.store.put_cached_inode(
                self.ws_id,
                Arc::new(CachedInode {
                    inode_id: child_id,
                    kind: kind_u8,
                    size,
                    attributes,
                    created_ns,
                    accessed_ns,
                    written_ns,
                    changed_ns,
                    storage_mode,
                    entries: existing_entries,
                }),
            );
            new_dentries.push((folded, (child_id, kind_u8, name.clone())));
            out.push(DirEntry {
                name,
                kind: if kind_u8 == KIND_DIR {
                    Kind::Directory
                } else {
                    Kind::File
                },
                file_id: child_id,
            });
        }
        if !new_dentries.is_empty() {
            let mut guard = self.dentry_shard(dir_id).write().unwrap();
            if guard.len().saturating_add(new_dentries.len()) >= MAX_DENTRY_PER_SHARD {
                guard.clear();
            }
            for (folded, val) in new_dentries {
                guard.insert((dir_id, folded), val);
            }
        }
        Ok(out)
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        if let Some(hit) = self.store.get_cached_inode(self.ws_id, file_id) {
            return Ok(cached_to_node_info(&hit));
        }
        let conn = self.ro_pool.get()?;
        let row = self
            .load_inode_cached(&conn, file_id)?
            .ok_or(OpsError::NotFound)?;
        Ok(cached_to_node_info(&row))
    }

    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()> {
        let conn = self.write_conn.lock().unwrap();
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
        self.store.invalidate_inode(self.ws_id, file_id);
        Ok(())
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        validate_attributes(attributes.0).map_err(|_| OpsError::InvalidParameter)?;
        let conn = self.write_conn.lock().unwrap();
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
        self.store.invalidate_inode(self.ws_id, file_id);
        Ok(())
    }

    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        let mut conn = self.write_conn.lock().unwrap();
        let Some((src_parent, _, src_fold)) = self.resolve_parent(&conn, from)? else {
            return Err(OpsError::InvalidParameter);
        };
        let Some((dst_parent, dst_disp, dst_fold)) = self.resolve_parent(&conn, to)? else {
            return Err(OpsError::InvalidParameter);
        };
        let Some((src_id, src_kind, _)) = self.lookup_child_db(&conn, src_parent, &src_fold)? else {
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
            self.cache_dentry(src_parent, src_fold, (src_id, src_kind, dst_disp.clone()));
            return Ok(RenameOutcome {
                file_id: src_id,
                binding: (dst_parent, dst_disp),
                replaced: None,
            });
        }

        if src_kind == KIND_DIR && self.is_descendant(&conn, src_id, dst_parent)? {
            return Err(OpsError::InvalidParameter);
        }

        let dst_existing = self.lookup_child_db(&conn, dst_parent, &dst_fold)?;
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

        self.invalidate_dentry(src_parent, &src_fold);
        self.cache_dentry(dst_parent, dst_fold, (src_id, src_kind, dst_disp.clone()));
        self.store.invalidate_inode(self.ws_id, src_id);
        if let Some(dst_id) = replaced_id {
            self.store.invalidate_inode(self.ws_id, dst_id);
        }
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
        let mut conn = self.write_conn.lock().unwrap();
        let folded = fold_bytes(name);
        let Some((child_id, kind, _)) = self.lookup_child_db(&conn, parent, &folded)? else {
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

        self.invalidate_dentry(parent, &folded);
        self.store.invalidate_inode(self.ws_id, child_id);
        if old_row.storage_mode == STORAGE_OVERLAY {
            let _ = self.store.overlay().remove_file(self.ws_id, child_id);
        }
        Ok(())
    }

    fn flush(&self) -> OpsResult<()> {
        let conn = self.write_conn.lock().unwrap();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        Ok(())
    }
}
