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
const MAX_DENTRY_PER_SHARD: usize = 65_536; // 1,048k total across 16 shards (fits Unity Assets/)

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

#[derive(Default)]
struct CachedDir {
    entries: HashMap<Box<[u8]>, DentryVal>,
    /// `true` when `read_dir` has loaded the complete directory listing into `entries`,
    /// enabling O(1) negative lookups without hitting SQLite.
    complete: bool,
}

#[derive(Default)]
struct DentryShardMap {
    dirs: HashMap<u64, CachedDir>,
    total: usize,
}

impl DentryShardMap {
    /// Evicts ~25% of cached entries by removing whole directories (preserving `complete`
    /// invariants on all surviving directories) instead of clearing the entire shard.
    fn evict_quarter(&mut self, protect_dir: u64) {
        let target = (self.total / 4).max(1);
        let mut freed = 0usize;
        let mut to_remove = Vec::new();
        for (&dir_id, dir) in &self.dirs {
            if dir_id == protect_dir {
                continue;
            }
            to_remove.push(dir_id);
            freed += dir.entries.len().max(1);
            if freed >= target {
                break;
            }
        }
        for dir_id in to_remove {
            if let Some(removed) = self.dirs.remove(&dir_id) {
                self.total = self.total.saturating_sub(removed.entries.len());
            }
        }
    }
}

type DentryShard = RwLock<DentryShardMap>;

const DIRTY_FLUSH_THRESHOLD: usize = 256;

/// WinFsp `WorkspaceOps` backend serving one workspace of a `DedupStore`.
pub struct DedupWorkspaceBackend {
    store: Arc<DedupStore>,
    ws_id: u64,
    ws_name: String,
    /// Writer SQLite connection (used ONLY for mutations).
    write_conn: Mutex<Connection>,
    /// Concurrent read-only WAL SQLite connection pool (used for all read/stat/list/open paths).
    ro_pool: RoConnPool,
    /// Sharded positive & negative `(parent_id -> {folded_name -> DentryVal, complete})` cache.
    dentry_cache: Vec<DentryShard>,
    /// Buffered dirty overlay `(size, written_ns)` updates flushed lazily on `flush` / `flush_file` / mutations.
    dirty_inodes: Mutex<HashMap<u64, (u64, i64)>>,
    next_inode: AtomicU64,
}

impl Drop for DedupWorkspaceBackend {
    fn drop(&mut self) {
        let _ = self.flush_all_dirty();
    }
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
            .map(|_| RwLock::new(DentryShardMap::default()))
            .collect();
        Ok(Self {
            store,
            ws_id,
            ws_name: ws_name.to_owned(),
            write_conn: Mutex::new(conn),
            ro_pool,
            dentry_cache,
            dirty_inodes: Mutex::new(HashMap::new()),
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
    fn dentry_shard(&self, parent_id: u64) -> &DentryShard {
        &self.dentry_cache[(parent_id as usize) & (DENTRY_SHARDS - 1)]
    }

    fn cache_dentry(&self, parent_id: u64, folded: Vec<u8>, val: DentryVal) {
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        if guard.total >= MAX_DENTRY_PER_SHARD {
            guard.evict_quarter(parent_id);
        }
        let added = {
            let dir = guard.dirs.entry(parent_id).or_default();
            dir.entries.insert(folded.into_boxed_slice(), val).is_none()
        };
        if added {
            guard.total += 1;
        }
    }

    fn invalidate_dentry(&self, parent_id: u64, folded: &[u8]) {
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        let removed = guard
            .dirs
            .get_mut(&parent_id)
            .is_some_and(|dir| dir.entries.remove(folded).is_some());
        if removed {
            guard.total = guard.total.saturating_sub(1);
        }
    }

    /// Looks up a child in `DentryCache` first (zero allocation, with negative-cache hit when
    /// `dir.complete == true`), falling back to SQLite `conn`.
    fn lookup_child_cached(
        &self,
        conn: &Connection,
        parent_id: u64,
        folded: &[u8],
    ) -> OpsResult<Option<DentryVal>> {
        {
            let guard = self.dentry_shard(parent_id).read().unwrap();
            if let Some(dir) = guard.dirs.get(&parent_id) {
                if let Some(hit) = dir.entries.get(folded) {
                    return Ok(Some(hit.clone()));
                }
                if dir.complete {
                    return Ok(None);
                }
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
        let mut stmt = conn
            .prepare_cached(
                "SELECT child_id, kind, display_name FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
            )
            .map_err(map_sql)?;
        stmt.query_row(params![self.ws_id as i64, parent_id as i64, folded], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u8,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .optional()
        .map_err(map_sql)
    }

    /// Flushes a single inode's buffered dirty `(size, written_ns)` to SQLite if pending.
    fn flush_dirty_inode(&self, inode_id: u64) -> OpsResult<()> {
        let pending = self.dirty_inodes.lock().unwrap().remove(&inode_id);
        if let Some((size, ts)) = pending {
            let conn = self.write_conn.lock().unwrap();
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
                     WHERE ws_id = ?3 AND inode_id = ?4",
                )
                .map_err(map_sql)?;
            stmt.execute(params![size as i64, ts, self.ws_id as i64, inode_id as i64])
                .map_err(map_sql)?;
        }
        Ok(())
    }

    /// Flushes all buffered dirty overlay metadata updates in one batched SQLite transaction.
    fn flush_all_dirty(&self) -> OpsResult<()> {
        let drained: Vec<(u64, (u64, i64))> = {
            let mut guard = self.dirty_inodes.lock().unwrap();
            if guard.is_empty() {
                return Ok(());
            }
            guard.drain().collect()
        };
        let mut conn = self.write_conn.lock().unwrap();
        let tx = conn.transaction().map_err(map_sql)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
                     WHERE ws_id = ?3 AND inode_id = ?4",
                )
                .map_err(map_sql)?;
            for (inode_id, (size, ts)) in drained {
                stmt.execute(params![size as i64, ts, self.ws_id as i64, inode_id as i64])
                    .map_err(map_sql)?;
            }
        }
        tx.commit().map_err(map_sql)?;
        Ok(())
    }

    fn dirty_snapshot(&self) -> Option<HashMap<u64, (u64, i64)>> {
        let guard = self.dirty_inodes.lock().unwrap();
        if guard.is_empty() {
            None
        } else {
            Some(guard.clone())
        }
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

    /// Materializes a `STORAGE_CHUNKS` file (or its first `max_bytes` prefix on truncating `set_size`)
    /// into its local sparse overlay file WITHOUT holding `write_conn`!
    /// Caller MUST hold `self.store.lock_inode(self.ws_id, inode.inode_id)` exclusively.
    fn materialize_overlay_unlocked(
        &self,
        inode: &InodeRow,
        max_bytes: Option<u64>,
    ) -> OpsResult<()> {
        let mut file = self
            .store
            .overlay()
            .create_empty(self.ws_id, inode.inode_id)
            .map_err(map_err)?;
        let limit = max_bytes.unwrap_or(inode.size).min(inode.size);
        if limit > 0 && !inode.manifest.is_empty() {
            let ro_conn = self.ro_pool.get()?;
            let entries = crate::meta::decode_manifest(&inode.manifest).map_err(map_err)?;
            let mut copied = 0u64;
            for entry in entries {
                if copied >= limit {
                    break;
                }
                let loc = self
                    .store
                    .resolve_chunk_loc(&ro_conn, &entry.hash)
                    .map_err(map_err)?
                    .ok_or_else(|| OpsError::Other("missing chunk during CoW".into()))?;
                let raw = self
                    .store
                    .packs()
                    .read_chunk(&entry.hash, loc)
                    .map_err(map_err)?;
                let take = ((limit - copied) as usize).min(raw.len());
                file.write_all(&raw[..take]).map_err(map_io)?;
                copied += take as u64;
            }
            file.flush().map_err(map_io)?;
        }
        Ok(())
    }

    /// Commits the `STORAGE_CHUNKS -> STORAGE_OVERLAY` metadata flip inside a brief `write_conn` txn.
    fn commit_overlay_flip(&self, inode: &mut InodeRow, new_size: u64, ts: i64) -> OpsResult<()> {
        let mut conn = self.write_conn.lock().unwrap();
        let tx = conn.transaction().map_err(map_sql)?;
        release_manifest_chunks(&tx, &inode.manifest).map_err(map_err)?;
        tx.execute(
            "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2,
                               storage_mode = ?3, manifest = X'', dirty_since_ns = ?2
             WHERE ws_id = ?4 AND inode_id = ?5",
            params![
                new_size as i64,
                ts,
                i64::from(STORAGE_OVERLAY),
                self.ws_id as i64,
                inode.inode_id as i64
            ],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        inode.size = new_size;
        inode.storage_mode = STORAGE_OVERLAY;
        inode.manifest.clear();
        inode.written_ns = ts;
        inode.changed_ns = ts;
        inode.dirty_since_ns = ts;
        Ok(())
    }

    fn is_descendant(&self, conn: &Connection, ancestor_id: u64, mut node_id: u64) -> OpsResult<bool> {
        if ancestor_id == node_id {
            return Ok(true);
        }
        let mut stmt = conn
            .prepare_cached(
                "SELECT parent_id FROM dentries WHERE ws_id = ?1 AND child_id = ?2 LIMIT 1",
            )
            .map_err(map_sql)?;
        let mut depth = 0usize;
        while node_id != ROOT_INODE && depth < 1024 {
            let parent: Option<i64> = stmt
                .query_row(params![self.ws_id as i64, node_id as i64], |r| r.get(0))
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

    fn read_dir_inner(&self, dir_id: u64) -> OpsResult<Vec<(Vec<u8>, u8, NodeInfo)>> {
        let conn = self.ro_pool.get()?;
        let dir_row = self
            .load_inode_cached(&conn, dir_id)?
            .ok_or(OpsError::NotFound)?;
        if dir_row.kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }

        let dirty = self.dirty_snapshot();
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
        let mut new_inodes = Vec::new();
        for row in rows {
            let (
                folded,
                name,
                kind_u8,
                child_id,
                mut size,
                attributes,
                created_ns,
                accessed_ns,
                mut written_ns,
                mut changed_ns,
                storage_mode,
            ) = row.map_err(map_sql)?;

            if let Some(ref dirty_map) = dirty {
                if let Some(&(d_size, d_ts)) = dirty_map.get(&child_id) {
                    size = d_size;
                    written_ns = d_ts;
                    changed_ns = d_ts;
                }
            }

            let cached_inode = CachedInode {
                inode_id: child_id,
                kind: kind_u8,
                size,
                attributes,
                created_ns,
                accessed_ns,
                written_ns,
                changed_ns,
                storage_mode,
                entries: None,
                chunk_offsets: None,
            };
            let info = cached_to_node_info(&cached_inode);
            new_inodes.push(cached_inode);
            new_dentries.push((folded, (child_id, kind_u8, name.clone())));
            out.push((name, kind_u8, info));
        }
        self.store
            .put_cached_inodes_preserve_manifest(self.ws_id, new_inodes);

        // Populate directory cache and mark `complete = true` for O(1) negative lookups!
        {
            let mut guard = self.dentry_shard(dir_id).write().unwrap();
            if guard.total.saturating_add(new_dentries.len()) >= MAX_DENTRY_PER_SHARD {
                guard.evict_quarter(dir_id);
            }
            let prev_len = guard.dirs.get(&dir_id).map_or(0, |d| d.entries.len());
            let mut map = HashMap::with_capacity(new_dentries.len());
            for (folded, val) in new_dentries {
                map.insert(folded.into_boxed_slice(), val);
            }
            let new_len = map.len();
            guard.dirs.insert(
                dir_id,
                CachedDir {
                    entries: map,
                    complete: true,
                },
            );
            guard.total = guard.total.saturating_sub(prev_len) + new_len;
        }
        Ok(out)
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
                        chunk_offsets: Some(Arc::from(Vec::new().into_boxed_slice())),
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
                self.dirty_inodes.lock().unwrap().remove(&child_id);
                self.store.put_cached_inode(
                    self.ws_id,
                    Arc::new(CachedInode {
                        inode_id: child_id,
                        kind: KIND_FILE,
                        size: 0,
                        attributes: new_attrs,
                        created_ns: row.created_ns,
                        accessed_ns: row.accessed_ns,
                        written_ns: ts,
                        changed_ns: ts,
                        storage_mode: STORAGE_OVERLAY,
                        entries: Some(Arc::from(Vec::new().into_boxed_slice())),
                        chunk_offsets: Some(Arc::from(Vec::new().into_boxed_slice())),
                    }),
                );

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
                chunk_offsets: Some(Arc::from(Vec::new().into_boxed_slice())),
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
        let mut out = vec![0u8; len];
        let _ = self.read_into(file_id, offset, &mut out)?;
        // WorkspaceOps::read contract: short reads at EOF are zero-padded to `len`.
        Ok(out)
    }

    fn read_into(&self, file_id: u64, offset: u64, dst: &mut [u8]) -> OpsResult<usize> {
        if dst.is_empty() {
            return Ok(0);
        }

        // Phase 1: Resolve inode metadata and (if chunk-backed) overlapping chunk locations
        // using the concurrent RO SQLite pool + RAM caches + O(log N) binary search.
        // Release ALL DB & inode locks BEFORE any `.pack` disk read or Zstd/LZ4 decompression!
        let plan = {
            let _lock = self.store.lock_inode_read(self.ws_id, file_id);
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
                return Ok(0);
            }
            if cached.storage_mode == STORAGE_OVERLAY {
                // Local sparse overlay read directly into caller's `dst` buffer (zero-copy!).
                return self
                    .store
                    .overlay()
                    .read_into_known_len(self.ws_id, file_id, Some(cached.size), offset, dst)
                    .map_err(map_err);
            }

            let to_read = (cached.size - offset).min(dst.len() as u64) as usize;
            let end = offset + to_read as u64;
            let entries = cached.entries.as_deref().unwrap_or(&[]);
            let offsets = cached.chunk_offsets.as_deref().unwrap_or(&[]);

            // O(log N) binary search for the first chunk containing `offset`.
            let start_idx = if offsets.len() == entries.len() && !offsets.is_empty() {
                offsets.partition_point(|&start| start <= offset).saturating_sub(1)
            } else {
                0
            };
            let mut cursor = offsets.get(start_idx).copied().unwrap_or(0);
            let mut spans = Vec::new();

            for entry in &entries[start_idx..] {
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

        // Phase 2: Lock-free parallel pack reads + decompression directly into `dst`!
        let (to_read, spans) = plan;
        let mut written = 0usize;
        for (hash, loc, slice_start, slice_end) in spans {
            let raw = self.store.packs().read_chunk(&hash, loc).map_err(map_err)?;
            let slice = &raw[slice_start..slice_end];
            let end_pos = written + slice.len();
            dst[written..end_pos].copy_from_slice(slice);
            written = end_pos;
        }
        debug_assert!(written <= to_read);
        Ok(written)
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);

        // Fast path: already-cached `STORAGE_OVERLAY` file -> ZERO `write_conn` lock,
        // ZERO SQLite query/txn, ZERO `file.metadata()` syscall!
        if let Some(hit) = self.store.get_cached_inode(self.ws_id, file_id) {
            if hit.kind == KIND_DIR {
                return Err(OpsError::IsADirectory);
            }
            if hit.storage_mode == STORAGE_OVERLAY {
                let end_pos = self
                    .store
                    .overlay()
                    .write_at_fast(self.ws_id, file_id, offset, bytes)
                    .map_err(map_err)?;
                let new_len = hit.size.max(end_pos);
                let ts = now_ns();
                self.store.put_cached_inode(
                    self.ws_id,
                    Arc::new(CachedInode {
                        inode_id: file_id,
                        kind: KIND_FILE,
                        size: new_len,
                        attributes: hit.attributes,
                        created_ns: hit.created_ns,
                        accessed_ns: hit.accessed_ns,
                        written_ns: ts,
                        changed_ns: ts,
                        storage_mode: STORAGE_OVERLAY,
                        entries: hit.entries.clone(),
                        chunk_offsets: hit.chunk_offsets.clone(),
                    }),
                );
                let need_flush = {
                    let mut dirty = self.dirty_inodes.lock().unwrap();
                    dirty.insert(file_id, (new_len, ts));
                    dirty.len() >= DIRTY_FLUSH_THRESHOLD
                };
                if need_flush {
                    let _ = self.flush_all_dirty();
                }
                return Ok(bytes.len());
            }
        }

        // Slow path: cold cache or `STORAGE_CHUNKS` -> load via RO pool and materialize CoW
        // WITHOUT holding `write_conn`!
        let mut row = {
            let ro_conn = self.ro_pool.get()?;
            load_inode(&ro_conn, self.ws_id, file_id)
                .map_err(map_err)?
                .ok_or(OpsError::NotFound)?
        };
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        let was_chunks = row.storage_mode == STORAGE_CHUNKS;
        if was_chunks {
            self.materialize_overlay_unlocked(&row, None)?;
        }
        let end_pos = self
            .store
            .overlay()
            .write_at_fast(self.ws_id, file_id, offset, bytes)
            .map_err(map_err)?;
        let new_len = row.size.max(end_pos);
        let ts = now_ns();
        if was_chunks {
            self.commit_overlay_flip(&mut row, new_len, ts)?;
        } else {
            let mut dirty = self.dirty_inodes.lock().unwrap();
            dirty.insert(file_id, (new_len, ts));
        }
        self.store.put_cached_inode(
            self.ws_id,
            Arc::new(CachedInode {
                inode_id: file_id,
                kind: KIND_FILE,
                size: new_len,
                attributes: row.attributes,
                created_ns: row.created_ns,
                accessed_ns: row.accessed_ns,
                written_ns: ts,
                changed_ns: ts,
                storage_mode: STORAGE_OVERLAY,
                entries: Some(Arc::from(Vec::new().into_boxed_slice())),
                chunk_offsets: Some(Arc::from(Vec::new().into_boxed_slice())),
            }),
        );
        Ok(bytes.len())
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        self.flush_dirty_inode(file_id)?;

        // Load via RO pool so CoW materialization runs WITHOUT holding `write_conn`.
        let mut row = {
            let ro_conn = self.ro_pool.get()?;
            load_inode(&ro_conn, self.ws_id, file_id)
                .map_err(map_err)?
                .ok_or(OpsError::NotFound)?
        };
        if row.kind == KIND_DIR {
            return Err(OpsError::IsADirectory);
        }
        let ts = now_ns();
        if len == 0 && row.storage_mode == STORAGE_CHUNKS {
            self.store
                .overlay()
                .create_empty(self.ws_id, file_id)
                .map_err(map_err)?;
            self.commit_overlay_flip(&mut row, 0, ts)?;
        } else {
            if row.storage_mode == STORAGE_CHUNKS {
                // Prefix-only CoW when truncating a chunk-backed file!
                self.materialize_overlay_unlocked(&row, Some(len))?;
                if len > row.size {
                    self.store
                        .overlay()
                        .set_len(self.ws_id, file_id, len)
                        .map_err(map_err)?;
                }
                self.commit_overlay_flip(&mut row, len, ts)?;
            } else {
                self.store
                    .overlay()
                    .set_len(self.ws_id, file_id, len)
                    .map_err(map_err)?;
                let conn = self.write_conn.lock().unwrap();
                let mut stmt = conn
                    .prepare_cached(
                        "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
                         WHERE ws_id = ?3 AND inode_id = ?4",
                    )
                    .map_err(map_sql)?;
                stmt.execute(params![len as i64, ts, self.ws_id as i64, file_id as i64])
                    .map_err(map_sql)?;
            }
        }
        self.store.put_cached_inode(
            self.ws_id,
            Arc::new(CachedInode {
                inode_id: file_id,
                kind: KIND_FILE,
                size: len,
                attributes: row.attributes,
                created_ns: row.created_ns,
                accessed_ns: row.accessed_ns,
                written_ns: ts,
                changed_ns: ts,
                storage_mode: STORAGE_OVERLAY,
                entries: Some(Arc::from(Vec::new().into_boxed_slice())),
                chunk_offsets: Some(Arc::from(Vec::new().into_boxed_slice())),
            }),
        );
        Ok(())
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let items = self.read_dir_inner(dir_id)?;
        Ok(items
            .into_iter()
            .map(|(name, kind_u8, info)| DirEntry {
                name,
                kind: if kind_u8 == KIND_DIR {
                    Kind::Directory
                } else {
                    Kind::File
                },
                file_id: info.file_id,
            })
            .collect())
    }

    fn read_dir_with_info(&self, dir_id: u64) -> OpsResult<Vec<(Vec<u8>, NodeInfo)>> {
        let items = self.read_dir_inner(dir_id)?;
        Ok(items
            .into_iter()
            .map(|(name, _, info)| (name, info))
            .collect())
    }

    fn is_dir_empty(&self, dir_id: u64) -> OpsResult<bool> {
        {
            let guard = self.dentry_shard(dir_id).read().unwrap();
            if let Some(dir) = guard.dirs.get(&dir_id) {
                if !dir.entries.is_empty() {
                    return Ok(false);
                }
                if dir.complete {
                    return Ok(true);
                }
            }
        }
        let conn = self.ro_pool.get()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT 1 FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 LIMIT 1",
            )
            .map_err(map_sql)?;
        let has_child: Option<i64> = stmt
            .query_row(params![self.ws_id as i64, dir_id as i64], |r| r.get(0))
            .optional()
            .map_err(map_sql)?;
        Ok(has_child.is_none())
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
        self.flush_dirty_inode(file_id)?;
        let conn = self.write_conn.lock().unwrap();
        let mut stmt = conn
            .prepare_cached(
                "UPDATE inodes
                 SET created_ns  = COALESCE(?1, created_ns),
                     accessed_ns = COALESCE(?2, accessed_ns),
                     written_ns  = COALESCE(?3, written_ns),
                     changed_ns  = COALESCE(?4, changed_ns)
                 WHERE ws_id = ?5 AND inode_id = ?6",
            )
            .map_err(map_sql)?;
        let updated = stmt
            .execute(params![
                created,
                accessed,
                written,
                changed,
                self.ws_id as i64,
                file_id as i64,
            ])
            .map_err(map_sql)?;
        if updated == 0 {
            return Err(OpsError::NotFound);
        }
        if let Some(prev) = self.store.get_cached_inode(self.ws_id, file_id) {
            self.store.put_cached_inode(
                self.ws_id,
                Arc::new(CachedInode {
                    inode_id: prev.inode_id,
                    kind: prev.kind,
                    size: prev.size,
                    attributes: prev.attributes,
                    created_ns: created.unwrap_or(prev.created_ns),
                    accessed_ns: accessed.unwrap_or(prev.accessed_ns),
                    written_ns: written.unwrap_or(prev.written_ns),
                    changed_ns: changed.unwrap_or(prev.changed_ns),
                    storage_mode: prev.storage_mode,
                    entries: prev.entries.clone(),
                    chunk_offsets: prev.chunk_offsets.clone(),
                }),
            );
        }
        Ok(())
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        validate_attributes(attributes.0).map_err(|_| OpsError::InvalidParameter)?;
        self.flush_dirty_inode(file_id)?;
        let ts = now_ns();
        let conn = self.write_conn.lock().unwrap();
        let mut stmt = conn
            .prepare_cached(
                "UPDATE inodes SET attributes = ?1, changed_ns = ?2 WHERE ws_id = ?3 AND inode_id = ?4",
            )
            .map_err(map_sql)?;
        let updated = stmt
            .execute(params![
                i64::from(attributes.0),
                ts,
                self.ws_id as i64,
                file_id as i64,
            ])
            .map_err(map_sql)?;
        if updated == 0 {
            return Err(OpsError::NotFound);
        }
        if let Some(prev) = self.store.get_cached_inode(self.ws_id, file_id) {
            self.store.put_cached_inode(
                self.ws_id,
                Arc::new(CachedInode {
                    inode_id: prev.inode_id,
                    kind: prev.kind,
                    size: prev.size,
                    attributes: attributes.0,
                    created_ns: prev.created_ns,
                    accessed_ns: prev.accessed_ns,
                    written_ns: prev.written_ns,
                    changed_ns: ts,
                    storage_mode: prev.storage_mode,
                    entries: prev.entries.clone(),
                    chunk_offsets: prev.chunk_offsets.clone(),
                }),
            );
        }
        Ok(())
    }

    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        self.flush_all_dirty()?;
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
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT 1 FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 LIMIT 1",
                    )
                    .map_err(map_sql)?;
                let has_child: Option<i64> = stmt
                    .query_row(params![self.ws_id as i64, dst_id as i64], |r| r.get(0))
                    .optional()
                    .map_err(map_sql)?;
                if has_child.is_some() {
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
            let mut stmt = conn
                .prepare_cached(
                    "SELECT 1 FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 LIMIT 1",
                )
                .map_err(map_sql)?;
            let has_child: Option<i64> = stmt
                .query_row(params![self.ws_id as i64, child_id as i64], |r| r.get(0))
                .optional()
                .map_err(map_sql)?;
            if has_child.is_some() {
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
        self.dirty_inodes.lock().unwrap().remove(&child_id);
        self.store.invalidate_inode(self.ws_id, child_id);
        if old_row.storage_mode == STORAGE_OVERLAY {
            let _ = self.store.overlay().remove_file(self.ws_id, child_id);
        }
        Ok(())
    }

    fn flush(&self) -> OpsResult<()> {
        self.flush_all_dirty()
    }

    fn flush_file(&self, file_id: u64) -> OpsResult<()> {
        self.flush_dirty_inode(file_id)?;
        let _ = self.store.overlay().sync_data(self.ws_id, file_id);
        Ok(())
    }
}
