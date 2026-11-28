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
    Attributes, DirEntry, DirOutcome, Kind, NodeInfo, OpenOutcome, OpenTarget, OpsError, OpsResult,
    Path, RenameOutcome, Times, WorkspaceOps,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

type CachedUtf16Listing = Arc<Vec<(Vec<u16>, NodeInfo)>>;

#[derive(Default)]
struct CachedDir {
    entries: HashMap<Box<[u8]>, DentryVal>,
    /// `true` when `read_dir` has loaded the complete directory listing into `entries`,
    /// enabling O(1) negative lookups without hitting SQLite.
    complete: bool,
    /// Monotonic generation bumped on every mutation to this directory so concurrent `read_dir`
    /// never overwrites newer mutations or sets `complete = true` from a stale snapshot.
    generation: u64,
    /// Cached final sorted UTF-16 `ReadDirectory` listing `(meta_version, Arc<Vec<...>>)` (N7).
    utf16_listing: Option<(u64, CachedUtf16Listing)>,
    /// CLOCK reference bit so hot directories (`/`, `Assets/`) survive partial shard eviction.
    referenced: AtomicBool,
}

#[derive(Default)]
struct DentryShardMap {
    dirs: HashMap<u64, CachedDir>,
    total: usize,
    /// Shard-wide mutation counter bumped on every create/mkdir/remove/rename in this shard so
    /// `read_dir` detects concurrent mutations even for directories not yet in `dirs`.
    mut_gen: u64,
}

impl DentryShardMap {
    /// Two-pass CLOCK eviction: removes ~25% of cold cached entries by whole directory
    /// (giving recently accessed directories a second chance and preserving `complete` invariants).
    /// Runs in linear O(D) time (removes Pass-1 victims before scanning Pass-2).
    fn evict_quarter(&mut self, protect_dir: u64) {
        let target = (self.total / 4).max(1);
        let mut freed = 0usize;
        let mut to_remove = Vec::new();
        // Pass 1: collect cold directories, clearing ref bit on hot ones.
        for (&dir_id, dir) in &self.dirs {
            if dir_id == protect_dir {
                continue;
            }
            if dir.referenced.swap(false, Ordering::Relaxed) {
                continue;
            }
            to_remove.push(dir_id);
            freed += dir.entries.len().max(1);
            if freed >= target {
                break;
            }
        }
        for dir_id in to_remove.drain(..) {
            if let Some(removed) = self.dirs.remove(&dir_id) {
                self.total = self.total.saturating_sub(removed.entries.len());
            }
        }
        // Pass 2: if everything was hot, evict remaining needed directories from surviving keys (O(D)).
        if freed < target {
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
}

type DentryShard = RwLock<DentryShardMap>;

const DIRTY_FLUSH_THRESHOLD: usize = 256;
const INODE_ALLOC_BATCH: u64 = 256;

/// WinFsp `WorkspaceOps` backend serving one workspace of a `DedupStore`.
///
/// All metadata writes share `DedupStore`'s single SQLite write connection (`store.lock_meta()`)
/// so foreground WinFsp mutations never contend on SQLite file write locks with background
/// `dedup_workspace` or `checkpoint_wal`.
pub struct DedupWorkspaceBackend {
    store: Arc<DedupStore>,
    ws_id: u64,
    ws_name: String,
    /// Concurrent read-only WAL SQLite connection pool (used for all read/stat/list/open paths).
    ro_pool: RoConnPool,
    /// Dedicated background WAL checkpoint connection (N1): never acquires `store.lock_meta()` so
    /// periodic `wal_checkpoint(PASSIVE)` / idle `TRUNCATE` fsyncs never stall foreground writes.
    checkpoint_conn: Mutex<Option<Connection>>,
    /// Counter of periodic `checkpoint_wal()` calls for scheduling occasional idle `TRUNCATE`.
    checkpoint_ticks: AtomicU64,
    /// Sharded positive & negative `(parent_id -> {folded_name -> DentryVal, complete, generation})` cache.
    dentry_cache: Vec<DentryShard>,
    /// Authoritative buffered overlay `(size, written_ns)` updates not yet persisted to SQLite.
    dirty_inodes: Mutex<HashMap<u64, (u64, i64)>>,
    /// Lock-free count of entries in `dirty_inodes` so clean read paths avoid locking the mutex.
    dirty_count: AtomicUsize,
    /// Monotonic workspace metadata version bumped on every file/dir mutation to validate O(1)
    /// cached UTF-16 `ReadDirectory` snapshots (N7).
    meta_version: AtomicU64,
    next_inode: AtomicU64,
    /// High-water mark already persisted to `workspaces.next_inode` so `alloc_inode` batches
    /// SQLite updates to 1 per `INODE_ALLOC_BATCH` creations.
    reserved_inode_end: AtomicU64,
}

impl Drop for DedupWorkspaceBackend {
    fn drop(&mut self) {
        // N11: Flush dirty overlay sizes AND checkpoint/truncate WAL into `meta.sqlite` BEFORE
        // writing and `sync_all()`-ing `.clean`, so a power loss can never leave `.clean` on disk
        // while SQLite metadata is still un-synced in the WAL.
        if self.flush_all_dirty().is_err() {
            return;
        }
        let checkpoint_ok = {
            let conn = self.store.lock_meta();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").is_ok()
        };
        if !checkpoint_ok {
            return;
        }
        let marker = self.store.overlay().clean_marker_path(self.ws_id);
        if let Ok(mut file) = std::fs::File::create(&marker) {
            use std::io::Write as _;
            if file.write_all(b"clean").is_ok() {
                let _ = file.sync_all();
            }
        }
    }
}

/// Reconciles on-disk sparse overlay file sizes against SQLite on workspace open so that after
/// a crash/power-loss, any buffered overlay writes whose `size` hadn't reached SQLite are
/// automatically recovered instead of appearing truncated.
fn reconcile_overlay_sizes(conn: &mut Connection, store: &DedupStore, ws_id: u64) -> crate::Result<()> {
    let mut mismatched: Vec<(u64, u64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT inode_id, size FROM inodes WHERE ws_id = ?1 AND kind = ?2 AND storage_mode = ?3",
        )?;
        let rows = stmt.query_map(
            params![ws_id as i64, i64::from(KIND_FILE), i64::from(STORAGE_OVERLAY)],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        for row in rows {
            let (inode_id, db_size) = row?;
            if let Some(disk_len) = store.overlay().file_len_if_exists(ws_id, inode_id) {
                if disk_len != db_size {
                    mismatched.push((inode_id, disk_len));
                }
            }
        }
    }
    if !mismatched.is_empty() {
        let tx = conn.transaction()?;
        {
            let mut upd = tx.prepare_cached(
                "UPDATE inodes SET size = ?1 WHERE ws_id = ?2 AND inode_id = ?3 AND storage_mode = ?4",
            )?;
            for (inode_id, disk_len) in mismatched {
                upd.execute(params![
                    disk_len as i64,
                    ws_id as i64,
                    inode_id as i64,
                    i64::from(STORAGE_OVERLAY)
                ])?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

impl DedupWorkspaceBackend {
    /// Opens `ws_name` in `store` for live WinFsp serving, reconciling any un-flushed overlay
    /// file sizes left over from a prior unclean shutdown and pre-creating the 256 overlay shards.
    pub fn open(store: Arc<DedupStore>, ws_name: &str) -> crate::Result<Self> {
        let (ws_id, start_inode) = {
            let mut conn = store.lock_meta();
            // Disable foreground WAL auto-checkpoint on the shared write connection so dispatcher
            // threads never stall on a multi-hundred-millisecond checkpoint fsync; the background
            // maintenance thread runs `checkpoint_wal()` periodically instead.
            let _ = conn.execute_batch("PRAGMA wal_autocheckpoint = 0;");
            let ws_id = find_workspace_id(&conn, ws_name)?;

            // Check clean-shutdown marker: skip per-overlay stat sweep when previous session exited cleanly.
            let marker = store.overlay().clean_marker_path(ws_id);
            let was_clean = marker.exists();
            let _ = std::fs::remove_file(&marker);
            if !was_clean {
                reconcile_overlay_sizes(&mut conn, &store, ws_id)?;
            }

            let next_inode: i64 = conn.query_row(
                "SELECT next_inode FROM workspaces WHERE ws_id = ?1",
                params![ws_id as i64],
                |r| r.get(0),
            )?;
            let start_inode = next_inode.max((ROOT_INODE + 1) as i64) as u64;
            (ws_id, start_inode)
        };
        // Pre-create all 256 hex shard directories once so file creations avoid per-file `create_dir_all`.
        store.overlay().precreate_shards(ws_id);

        let ro_pool = RoConnPool::new(Arc::clone(&store));
        let dentry_cache = (0..DENTRY_SHARDS)
            .map(|_| RwLock::new(DentryShardMap::default()))
            .collect();
        Ok(Self {
            store,
            ws_id,
            ws_name: ws_name.to_owned(),
            ro_pool,
            checkpoint_conn: Mutex::new(None),
            checkpoint_ticks: AtomicU64::new(0),
            dentry_cache,
            dirty_inodes: Mutex::new(HashMap::new()),
            dirty_count: AtomicUsize::new(0),
            meta_version: AtomicU64::new(1),
            next_inode: AtomicU64::new(start_inode),
            reserved_inode_end: AtomicU64::new(start_inode),
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

    /// Allocates an inode ID, batch-reserving `INODE_ALLOC_BATCH` IDs in SQLite so normal
    /// file/dir creations only execute `UPDATE workspaces` once per 256 allocations.
    fn alloc_inode(&self, tx: &rusqlite::Transaction<'_>) -> OpsResult<u64> {
        let id = self.next_inode.fetch_add(1, Ordering::Relaxed);
        if id >= self.reserved_inode_end.load(Ordering::Relaxed) {
            let new_end = id.saturating_add(INODE_ALLOC_BATCH);
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE workspaces SET next_inode = MAX(next_inode, ?1) WHERE ws_id = ?2",
                )
                .map_err(map_sql)?;
            stmt.execute(params![new_end as i64, self.ws_id as i64])
                .map_err(map_sql)?;
            self.reserved_inode_end.store(new_end, Ordering::Relaxed);
        }
        Ok(id)
    }

    #[inline]
    fn bump_meta_version(&self) {
        self.meta_version.fetch_add(1, Ordering::Release);
    }

    #[inline]
    fn dentry_shard(&self, parent_id: u64) -> &DentryShard {
        &self.dentry_cache[(parent_id as usize) & (DENTRY_SHARDS - 1)]
    }

    /// Inserts/updates a dentry caused by a namespace mutation (`create`, `mkdir`, `rename`)
    /// and bumps both the shard and directory generation counters so any in-flight `read_dir`
    /// cannot clobber this mutation or mark a stale snapshot `complete = true`.
    fn cache_dentry_mutation(&self, parent_id: u64, folded: Vec<u8>, val: DentryVal) {
        self.bump_meta_version();
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        if guard.total >= MAX_DENTRY_PER_SHARD {
            guard.evict_quarter(parent_id);
        }
        guard.mut_gen = guard.mut_gen.wrapping_add(1);
        let new_gen = guard.mut_gen;
        let added = {
            let dir = guard.dirs.entry(parent_id).or_default();
            dir.generation = new_gen;
            dir.utf16_listing = None;
            dir.referenced.store(true, Ordering::Relaxed);
            dir.entries.insert(folded.into_boxed_slice(), val).is_none()
        };
        if added {
            guard.total += 1;
        }
    }

    /// Inserts a dentry discovered by a read-only SQLite lookup, ONLY if no concurrent mutation
    /// occurred in this directory/shard since the lookup started.
    fn cache_dentry_if_unchanged(
        &self,
        parent_id: u64,
        folded: Vec<u8>,
        val: DentryVal,
        start_dir_gen: Option<u64>,
        start_shard_gen: u64,
    ) {
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        let mutated = match start_dir_gen {
            Some(g) => guard.dirs.get(&parent_id).map(|d| d.generation) != Some(g),
            None => guard.mut_gen != start_shard_gen,
        };
        if mutated {
            return;
        }
        if guard.total >= MAX_DENTRY_PER_SHARD {
            guard.evict_quarter(parent_id);
        }
        let gen = start_dir_gen.unwrap_or(start_shard_gen);
        let added = {
            let dir = guard.dirs.entry(parent_id).or_insert_with(|| CachedDir {
                entries: HashMap::new(),
                complete: false,
                generation: gen,
                utf16_listing: None,
                referenced: AtomicBool::new(true),
            });
            dir.referenced.store(true, Ordering::Relaxed);
            dir.entries.insert(folded.into_boxed_slice(), val).is_none()
        };
        if added {
            guard.total += 1;
        }
    }

    fn invalidate_dentry(&self, parent_id: u64, folded: &[u8]) {
        self.bump_meta_version();
        let mut guard = self.dentry_shard(parent_id).write().unwrap();
        guard.mut_gen = guard.mut_gen.wrapping_add(1);
        let new_gen = guard.mut_gen;
        let removed = if let Some(dir) = guard.dirs.get_mut(&parent_id) {
            dir.generation = new_gen;
            dir.utf16_listing = None;
            dir.entries.remove(folded).is_some()
        } else {
            false
        };
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
        let (start_dir_gen, start_shard_gen) = {
            let guard = self.dentry_shard(parent_id).read().unwrap();
            if let Some(dir) = guard.dirs.get(&parent_id) {
                dir.referenced.store(true, Ordering::Relaxed);
                if let Some(hit) = dir.entries.get(folded) {
                    return Ok(Some(hit.clone()));
                }
                if dir.complete {
                    return Ok(None);
                }
                (Some(dir.generation), guard.mut_gen)
            } else {
                (None, guard.mut_gen)
            }
        };
        let found = self.lookup_child_db(conn, parent_id, folded)?;
        if let Some(val) = &found {
            self.cache_dentry_if_unchanged(
                parent_id,
                folded.to_vec(),
                val.clone(),
                start_dir_gen,
                start_shard_gen,
            );
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

    #[inline]
    fn set_dirty(&self, inode_id: u64, size: u64, ts: i64) -> usize {
        let mut guard = self.dirty_inodes.lock().unwrap();
        guard.insert(inode_id, (size, ts));
        let len = guard.len();
        self.dirty_count.store(len, Ordering::Release);
        len
    }

    #[inline]
    fn clear_dirty(&self, inode_id: u64) {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut guard = self.dirty_inodes.lock().unwrap();
        if guard.remove(&inode_id).is_some() {
            self.dirty_count.store(guard.len(), Ordering::Release);
        }
    }

    #[inline]
    fn get_dirty(&self, inode_id: u64) -> Option<(u64, i64)> {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.dirty_inodes.lock().unwrap().get(&inode_id).copied()
    }

    /// Returns `true` when `inode_id` has unflushed buffered write metadata in memory.
    pub fn is_dirty(&self, inode_id: u64) -> bool {
        self.get_dirty(inode_id).is_some()
    }

    /// Flushes a single inode's buffered dirty `(size, written_ns)` to SQLite if pending.
    /// Keeps the entry visible in `dirty_inodes` until AFTER SQLite commits so concurrent
    /// readers never observe stale DB values during the flush window.
    fn flush_dirty_inode(&self, inode_id: u64) -> OpsResult<()> {
        let Some((size, ts)) = self.get_dirty(inode_id) else {
            return Ok(());
        };
        {
            let conn = self.store.lock_meta();
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
                     WHERE ws_id = ?3 AND inode_id = ?4 AND storage_mode = ?5",
                )
                .map_err(map_sql)?;
            stmt.execute(params![
                size as i64,
                ts,
                self.ws_id as i64,
                inode_id as i64,
                i64::from(STORAGE_OVERLAY),
            ])
            .map_err(map_sql)?;
        }
        let cleaned = {
            let mut guard = self.dirty_inodes.lock().unwrap();
            if guard.get(&inode_id) == Some(&(size, ts)) {
                guard.remove(&inode_id);
                self.dirty_count.store(guard.len(), Ordering::Release);
                true
            } else {
                false
            }
        };
        if cleaned {
            self.store.clear_inode_dirty(self.ws_id, inode_id, size, ts);
        }
        Ok(())
    }

    /// Flushes all buffered dirty overlay metadata updates in one batched SQLite transaction.
    /// Entries remain in `dirty_inodes` until AFTER commit; any inode written concurrently during
    /// commit retains its newer dirty entry and stays marked dirty.
    fn flush_all_dirty(&self) -> OpsResult<()> {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let snapshot: Vec<(u64, (u64, i64))> = {
            let guard = self.dirty_inodes.lock().unwrap();
            if guard.is_empty() {
                return Ok(());
            }
            guard.iter().map(|(&id, &v)| (id, v)).collect()
        };
        {
            let mut conn = self.store.lock_meta();
            let tx = conn.transaction().map_err(map_sql)?;
            {
                let mut stmt = tx
                    .prepare_cached(
                        "UPDATE inodes SET size = ?1, written_ns = ?2, changed_ns = ?2, dirty_since_ns = ?2
                         WHERE ws_id = ?3 AND inode_id = ?4 AND storage_mode = ?5",
                    )
                    .map_err(map_sql)?;
                for &(inode_id, (size, ts)) in &snapshot {
                    stmt.execute(params![
                        size as i64,
                        ts,
                        self.ws_id as i64,
                        inode_id as i64,
                        i64::from(STORAGE_OVERLAY),
                    ])
                    .map_err(map_sql)?;
                }
            }
            tx.commit().map_err(map_sql)?;
        }
        let mut cleaned = Vec::new();
        {
            let mut guard = self.dirty_inodes.lock().unwrap();
            for &(inode_id, (size, ts)) in &snapshot {
                if guard.get(&inode_id) == Some(&(size, ts)) {
                    guard.remove(&inode_id);
                    cleaned.push((inode_id, size, ts));
                }
            }
            self.dirty_count.store(guard.len(), Ordering::Release);
        }
        for (inode_id, size, ts) in cleaned {
            self.store.clear_inode_dirty(self.ws_id, inode_id, size, ts);
        }
        Ok(())
    }

    fn dirty_snapshot(&self) -> Option<HashMap<u64, (u64, i64)>> {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return None;
        }
        let guard = self.dirty_inodes.lock().unwrap();
        if guard.is_empty() {
            None
        } else {
            Some(guard.clone())
        }
    }

    /// Loads an inode via `DedupStore`'s sharded RAM cache, falling back to `conn` and overlaying
    /// any authoritative buffered `dirty_inodes` state.
    fn load_inode_cached(&self, conn: &Connection, inode_id: u64) -> OpsResult<Option<Arc<CachedInode>>> {
        if let Some(hit) = self.store.get_cached_inode(self.ws_id, inode_id) {
            return Ok(Some(hit));
        }
        let Some(row) = load_inode(conn, self.ws_id, inode_id).map_err(map_err)? else {
            return Ok(None);
        };
        let mut cached = CachedInode::from_row(&row).map_err(map_err)?;
        if let Some((d_size, d_ts)) = self.get_dirty(inode_id) {
            cached.size = d_size;
            cached.written_ns = d_ts;
            cached.changed_ns = d_ts;
            cached.storage_mode = STORAGE_OVERLAY;
            cached.dirty = true;
        }
        let arc = Arc::new(cached);
        self.store.put_cached_inode(self.ws_id, Arc::clone(&arc));
        Ok(Some(arc))
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
    /// into its local overlay file WITHOUT holding `write_conn`!
    /// Caller MUST hold `self.store.lock_inode(self.ws_id, inode.inode_id)` exclusively.
    fn materialize_overlay_unlocked(
        &self,
        inode: &InodeRow,
        max_bytes: Option<u64>,
    ) -> OpsResult<()> {
        let file = self
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
                file.write_all_at(copied, &raw[..take]).map_err(map_io)?;
                copied += take as u64;
            }
        }
        Ok(())
    }

    /// Commits the `STORAGE_CHUNKS -> STORAGE_OVERLAY` metadata flip inside a brief `lock_meta` txn.
    ///
    /// Power-loss safety (N3): durably `sync_data`s the materialized overlay file BEFORE acquiring
    /// `lock_meta()` and releasing the old chunk references in SQLite, so a crash or subsequent GC
    /// can never lose the sole copy of the file's data.
    fn commit_overlay_flip(&self, inode: &mut InodeRow, new_size: u64, ts: i64) -> OpsResult<()> {
        if new_size > 0 {
            self.store
                .overlay()
                .sync_data(self.ws_id, inode.inode_id)
                .map_err(map_err)?;
        }
        let deleted_chunks = {
            let mut conn = self.store.lock_meta();
            let tx = conn.transaction().map_err(map_sql)?;
            let deleted = release_manifest_chunks(&tx, &inode.manifest).map_err(map_err)?;
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
            deleted
        };
        self.store.invalidate_chunk_locs(&deleted_chunks);
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
        // Fast zero-SQLite path: if `dir_id` is already marked `complete = true` in `dentry_cache`
        // AND every child hits `inode_cache`, serve the entire directory listing directly from RAM
        // without touching SQLite or acquiring any write locks!
        {
            let guard = self.dentry_shard(dir_id).read().unwrap();
            if let Some(dir) = guard.dirs.get(&dir_id) {
                if dir.complete {
                    dir.referenced.store(true, Ordering::Relaxed);
                    let mut keyed = Vec::with_capacity(dir.entries.len());
                    let mut all_hit = true;
                    for (folded, (child_id, kind_u8, display_name)) in &dir.entries {
                        if let Some(cached) = self.store.get_cached_inode(self.ws_id, *child_id) {
                            keyed.push((
                                folded.as_ref(),
                                display_name.clone(),
                                *kind_u8,
                                cached_to_node_info(&cached),
                            ));
                        } else {
                            all_hit = false;
                            break;
                        }
                    }
                    if all_hit {
                        keyed.sort_unstable_by(|a, b| a.0.cmp(b.0));
                        return Ok(keyed
                            .into_iter()
                            .map(|(_, name, kind, info)| (name, kind, info))
                            .collect());
                    }
                }
            }
        }

        let conn = self.ro_pool.get()?;
        let dir_row = self
            .load_inode_cached(&conn, dir_id)?
            .ok_or(OpsError::NotFound)?;
        if dir_row.kind != KIND_DIR {
            return Err(OpsError::NotADirectory);
        }

        // Record directory & shard mutation generations BEFORE running SQLite SELECT so any
        // concurrent create/mkdir/remove/rename invalidates negative-cache promotion.
        let (start_dir_gen, start_shard_gen) = {
            let guard = self.dentry_shard(dir_id).read().unwrap();
            let dg = guard.dirs.get(&dir_id).map(|d| d.generation);
            (dg, guard.mut_gen)
        };

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

        let mut raw_rows = Vec::new();
        for row in rows {
            raw_rows.push(row.map_err(map_sql)?);
        }

        // Take dirty overlay snapshot AFTER SQLite query finishes so it is at least as fresh as DB.
        let dirty = self.dirty_snapshot();

        let mut out = Vec::with_capacity(raw_rows.len());
        let mut new_dentries = Vec::with_capacity(raw_rows.len());
        let mut new_inodes = Vec::with_capacity(raw_rows.len());
        for (
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
            mut storage_mode,
        ) in raw_rows
        {
            let mut is_dirty = false;
            if let Some(ref dirty_map) = dirty {
                if let Some(&(d_size, d_ts)) = dirty_map.get(&child_id) {
                    size = d_size;
                    written_ns = d_ts;
                    changed_ns = d_ts;
                    storage_mode = STORAGE_OVERLAY;
                    is_dirty = true;
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
                dirty: is_dirty,
            };
            let info = cached_to_node_info(&cached_inode);
            new_inodes.push(cached_inode);
            new_dentries.push((folded, (child_id, kind_u8, name.clone())));
            out.push((name, kind_u8, info));
        }
        // Note: `put_cached_inodes_preserve_manifest` skips any `prev.dirty == true` entries,
        // so writes racing after `dirty_snapshot()` can never be overwritten with stale sizes.
        self.store
            .put_cached_inodes_preserve_manifest(self.ws_id, new_inodes);

        // Populate directory cache and mark `complete = true` ONLY if no concurrent mutation
        // occurred in this directory/shard while our SQLite snapshot was running!
        {
            let mut guard = self.dentry_shard(dir_id).write().unwrap();
            let mutated = match start_dir_gen {
                Some(g) => guard.dirs.get(&dir_id).map(|d| d.generation) != Some(g),
                None => guard.mut_gen != start_shard_gen,
            };
            if !mutated {
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
                        generation: start_dir_gen.unwrap_or(start_shard_gen),
                        utf16_listing: None,
                        referenced: AtomicBool::new(true),
                    },
                );
                guard.total = guard.total.saturating_sub(prev_len) + new_len;
            }
        }
        Ok(out)
    }
    /// Shared write implementation supporting both positional writes (`Some(offset)`) and
    /// atomic append writes (`None` -> appends at authoritative EOF under `lock_inode`).
    fn write_inner(&self, file_id: u64, offset_opt: Option<u64>, bytes: &[u8]) -> OpsResult<(u64, usize)> {
        let _lock = self.store.lock_inode(self.ws_id, file_id);

        // Fast path: already-cached `STORAGE_OVERLAY` file -> ZERO `write_conn` lock,
        // ZERO SQLite query/txn, ZERO `file.metadata()` syscall!
        if let Some(hit) = self.store.get_cached_inode(self.ws_id, file_id) {
            if hit.kind == KIND_DIR {
                return Err(OpsError::IsADirectory);
            }
            if hit.storage_mode == STORAGE_OVERLAY {
                let offset = offset_opt.unwrap_or(hit.size);
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
                        dirty: true,
                    }),
                );
                let dirty_len = self.set_dirty(file_id, new_len, ts);
                self.bump_meta_version();
                if dirty_len >= DIRTY_FLUSH_THRESHOLD {
                    let _ = self.flush_all_dirty();
                }
                return Ok((offset, bytes.len()));
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
        if let Some((d_size, d_ts)) = self.get_dirty(file_id) {
            row.size = d_size;
            row.written_ns = d_ts;
            row.changed_ns = d_ts;
            row.storage_mode = STORAGE_OVERLAY;
        }
        let was_chunks = row.storage_mode == STORAGE_CHUNKS;
        if was_chunks {
            self.materialize_overlay_unlocked(&row, None)?;
        }
        let offset = offset_opt.unwrap_or(row.size);
        let end_pos = self
            .store
            .overlay()
            .write_at_fast(self.ws_id, file_id, offset, bytes)
            .map_err(map_err)?;
        let new_len = row.size.max(end_pos);
        let ts = now_ns();
        if was_chunks {
            self.commit_overlay_flip(&mut row, new_len, ts)?;
            self.clear_dirty(file_id);
            self.bump_meta_version();
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
                    dirty: false,
                }),
            );
        } else {
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
                    dirty: true,
                }),
            );
            let dirty_len = self.set_dirty(file_id, new_len, ts);
            if dirty_len >= DIRTY_FLUSH_THRESHOLD {
                let _ = self.flush_all_dirty();
            }
        }
        Ok((offset, bytes.len()))
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

        // For dispositions that may truncate an existing file, probe `child_id` via `ro_pool`
        // BEFORE acquiring `write_conn` so we ALWAYS obey the global lock order:
        // `store.lock_inode(ws_id, id)` -> `write_conn`.
        let mut pre_lock_id: Option<u64> = if matches!(
            disposition,
            Disposition::Overwrite | Disposition::Supersede | Disposition::OverwriteIf
        ) {
            let ro_conn = self.ro_pool.get()?;
            match self.resolve_parent(&ro_conn, path) {
                Ok(Some((parent_id, _, leaf_fold))) => self
                    .lookup_child_cached(&ro_conn, parent_id, &leaf_fold)?
                    .and_then(|(cid, kind, _)| (kind == KIND_FILE).then_some(cid)),
                _ => None,
            }
        } else {
            None
        };

        loop {
            let _inode_lock = pre_lock_id.map(|id| self.store.lock_inode(self.ws_id, id));
            let mut conn = self.store.lock_meta();
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

                    self.cache_dentry_mutation(
                        parent_id,
                        leaf_fold,
                        (inode_id, KIND_FILE, leaf_disp.clone()),
                    );
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
                            dirty: false,
                        }),
                    );

                    return Ok(OpenOutcome {
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
                    });
                }
                Action::Open => {
                    let (child_id, _, stored_name) = existing.unwrap();
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
                Action::TruncateExisting => {
                    let (child_id, _, stored_name) = existing.unwrap();
                    // If `child_id` changed (or wasn't pre-locked), drop `write_conn` FIRST and
                    // retry so `lock_inode(child_id)` is ALWAYS acquired before `write_conn`!
                    if pre_lock_id != Some(child_id) {
                        drop(conn);
                        drop(_inode_lock);
                        pre_lock_id = Some(child_id);
                        continue;
                    }
                    let row = load_inode(&conn, self.ws_id, child_id)
                        .map_err(map_err)?
                        .ok_or(OpsError::NotFound)?;
                    let ts = now_ns();
                    let tx = conn.transaction().map_err(map_sql)?;
                    let deleted_chunks = if row.storage_mode == STORAGE_CHUNKS && !row.manifest.is_empty() {
                        release_manifest_chunks(&tx, &row.manifest).map_err(map_err)?
                    } else {
                        Vec::new()
                    };
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
                    self.store.invalidate_chunk_locs(&deleted_chunks);
                    self.store
                        .overlay()
                        .create_empty(self.ws_id, child_id)
                        .map_err(map_err)?;
                    self.clear_dirty(child_id);
                    self.bump_meta_version();
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
                            dirty: false,
                        }),
                    );

                    return Ok(OpenOutcome {
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
                    });
                }
            }
        }
    }

    fn mkdir(&self, path: &Path<'_>) -> OpsResult<u64> {
        Ok(self.mkdir_outcome(path, false)?.file_id)
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
                    let mut full_val = CachedInode::from_row(&row).map_err(map_err)?;
                    if let Some((d_size, d_ts)) = self.get_dirty(file_id) {
                        full_val.size = d_size;
                        full_val.written_ns = d_ts;
                        full_val.changed_ns = d_ts;
                        full_val.storage_mode = STORAGE_OVERLAY;
                        full_val.dirty = true;
                    }
                    let full = Arc::new(full_val);
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
        let (_, written) = self.write_inner(file_id, Some(offset), bytes)?;
        Ok(written)
    }

    fn write_append(&self, file_id: u64, bytes: &[u8]) -> OpsResult<(u64, usize)> {
        self.write_inner(file_id, None, bytes)
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
                let conn = self.store.lock_meta();
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
                dirty: false,
            }),
        );
        self.clear_dirty(file_id);
        self.bump_meta_version();
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
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        if let Some(prev) = self.store.get_cached_inode(self.ws_id, file_id) {
            if !prev.dirty
                && created.is_none_or(|t| t == prev.created_ns)
                && accessed.is_none_or(|t| t == prev.accessed_ns)
                && written.is_none_or(|t| t == prev.written_ns)
                && changed.is_none_or(|t| t == prev.changed_ns)
            {
                return Ok(());
            }
        }
        self.flush_dirty_inode(file_id)?;
        let conn = self.store.lock_meta();
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
        self.bump_meta_version();
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
                    dirty: false,
                }),
            );
        }
        Ok(())
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        validate_attributes(attributes.0).map_err(|_| OpsError::InvalidParameter)?;
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        if let Some(prev) = self.store.get_cached_inode(self.ws_id, file_id) {
            if !prev.dirty && prev.attributes == attributes.0 {
                return Ok(());
            }
        }
        self.flush_dirty_inode(file_id)?;
        let ts = now_ns();
        let conn = self.store.lock_meta();
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
        self.bump_meta_version();
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
                    dirty: false,
                }),
            );
        }
        Ok(())
    }

    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        self.flush_all_dirty()?;
        let mut conn = self.store.lock_meta();
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
            self.cache_dentry_mutation(src_parent, src_fold, (src_id, src_kind, dst_disp.clone()));
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
        let mut deleted_chunks = Vec::new();
        if let Some(dst_id) = replaced_id {
            if let Some(old_row) = load_inode(&tx, self.ws_id, dst_id).map_err(map_err)? {
                if old_row.storage_mode == STORAGE_CHUNKS && !old_row.manifest.is_empty() {
                    deleted_chunks = release_manifest_chunks(&tx, &old_row.manifest).map_err(map_err)?;
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
        self.store.invalidate_chunk_locs(&deleted_chunks);

        self.invalidate_dentry(src_parent, &src_fold);
        self.cache_dentry_mutation(dst_parent, dst_fold, (src_id, src_kind, dst_disp.clone()));
        self.store.invalidate_inode(self.ws_id, src_id);
        if let Some(dst_id) = replaced_id {
            self.clear_dirty(dst_id);
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
        let mut conn = self.store.lock_meta();
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
        let deleted_chunks = if old_row.storage_mode == STORAGE_CHUNKS && !old_row.manifest.is_empty() {
            release_manifest_chunks(&tx, &old_row.manifest).map_err(map_err)?
        } else {
            Vec::new()
        };
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
        self.store.invalidate_chunk_locs(&deleted_chunks);

        self.invalidate_dentry(parent, &folded);
        self.clear_dirty(child_id);
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
        let _lock = self.store.lock_inode(self.ws_id, file_id);
        self.flush_dirty_inode(file_id)?;
        let _ = self.store.overlay().sync_data(self.ws_id, file_id);
        Ok(())
    }

    fn is_file_dirty(&self, file_id: u64) -> bool {
        self.is_dirty(file_id)
    }

    fn on_cleanup(&self, _file_id: u64) -> OpsResult<()> {
        // Avoid triggering a synchronous SQLite write transaction on every handle close during
        // small-file storms: dirty overlay metadata is batched and flushed at `DIRTY_FLUSH_THRESHOLD`
        // (256 files), every 2s by the background maintenance thread, on explicit `FlushFileBuffers`
        // (`flush_file`), and on unmount (`Drop`). `is_file_dirty` keeps unflushed files protected
        // from background dedup in the meantime.
        if self.dirty_count.load(Ordering::Acquire) >= DIRTY_FLUSH_THRESHOLD {
            self.flush_all_dirty()?;
        }
        Ok(())
    }

    fn checkpoint_wal(&self) -> OpsResult<()> {
        // N1: Run WAL checkpoint on a dedicated connection OUTSIDE `store.lock_meta()` so
        // foreground metadata mutations never stall behind checkpoint fsyncs.
        let mut slot = self.checkpoint_conn.lock().unwrap();
        if slot.is_none() {
            let conn = self.store.open_conn().map_err(map_err)?;
            let _ = conn.busy_timeout(std::time::Duration::from_millis(200));
            let _ = conn.execute_batch(
                "PRAGMA wal_autocheckpoint = 0;\
                 PRAGMA cache_size = -4096;\
                 PRAGMA journal_size_limit = 67108864;",
            );
            *slot = Some(conn);
        }
        let conn = slot.as_ref().unwrap();
        let tick = self.checkpoint_ticks.fetch_add(1, Ordering::Relaxed) + 1;
        // Periodically reclaim WAL file size with `TRUNCATE` when idle; fall back to `PASSIVE` if busy.
        if tick % 10 == 0 && self.dirty_count.load(Ordering::Relaxed) == 0 {
            if conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").is_err() {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
            }
        } else {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
        Ok(())
    }

    fn lookup_info(&self, path: &Path<'_>) -> OpsResult<NodeInfo> {
        let conn = self.ro_pool.get()?;
        let child_id = match self.resolve_parent(&conn, path)? {
            None => ROOT_INODE,
            Some((parent_id, _, leaf_fold)) => {
                let Some((cid, _, _)) = self.lookup_child_cached(&conn, parent_id, &leaf_fold)?
                else {
                    return Err(OpsError::NotFound);
                };
                cid
            }
        };
        let row = self
            .load_inode_cached(&conn, child_id)?
            .ok_or(OpsError::NotFound)?;
        Ok(cached_to_node_info(&row))
    }

    fn lookup_child_info(&self, parent_id: u64, name: &[u8]) -> OpsResult<(Vec<u8>, NodeInfo)> {
        let conn = self.ro_pool.get()?;
        let folded = fold_bytes(name);
        let Some((child_id, _, stored_name)) =
            self.lookup_child_cached(&conn, parent_id, &folded)?
        else {
            return Err(OpsError::NotFound);
        };
        let row = self
            .load_inode_cached(&conn, child_id)?
            .ok_or(OpsError::NotFound)?;
        Ok((stored_name, cached_to_node_info(&row)))
    }

    fn open_entry(
        &self,
        path: &Path<'_>,
        disposition: Disposition,
        target: OpenTarget,
    ) -> OpsResult<OpenOutcome> {
        // Fast concurrent RO path for `Open` and already-existing `OpenIf` (handles files, dirs,
        // and root in ONE path walk with ZERO write-lock contention).
        if matches!(disposition, Disposition::Open | Disposition::OpenIf) {
            let conn = self.ro_pool.get()?;
            let resolved = match self.resolve_parent(&conn, path) {
                Ok(None) => Some((ROOT_INODE, KIND_DIR, ROOT_INODE, Vec::new())),
                Ok(Some((parent_id, _, leaf_fold))) => self
                    .lookup_child_cached(&conn, parent_id, &leaf_fold)?
                    .map(|(cid, kind, stored)| (cid, kind, parent_id, stored)),
                Err(OpsError::NotFound) if disposition == Disposition::OpenIf => {
                    return Err(OpsError::NotFound);
                }
                Err(err) => return Err(err),
            };
            if let Some((child_id, kind, parent_id, stored_name)) = resolved {
                if target == OpenTarget::FileOnly && kind == KIND_DIR {
                    return Err(OpsError::IsADirectory);
                }
                if target == OpenTarget::DirOnly && kind != KIND_DIR {
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
            if disposition == Disposition::Open {
                return Err(OpsError::NotFound);
            }
        }

        match target {
            OpenTarget::DirOnly => match disposition {
                Disposition::Create => self.mkdir_outcome(path, false),
                Disposition::OpenIf => self.mkdir_outcome(path, true),
                other => match self.create_or_open(path, other) {
                    Ok(out) if out.kind == Kind::Directory => Ok(out),
                    Ok(_) => Err(OpsError::NotADirectory),
                    Err(err) => Err(err),
                },
            },
            OpenTarget::FileOnly => match self.create_or_open(path, disposition) {
                Ok(out) if out.kind == Kind::Directory => Err(OpsError::IsADirectory),
                Ok(out) => Ok(out),
                // N10: `FileOnly` targeting an existing directory (including `Supersede` which
                // maps to `AccessDenied` in `decide`) must report `IsADirectory`.
                Err(OpsError::NotADirectory) => Err(OpsError::IsADirectory),
                Err(OpsError::AccessDenied)
                    if disposition == Disposition::Supersede
                        && self.open_dir(path).is_ok() =>
                {
                    Err(OpsError::IsADirectory)
                }
                Err(err) => Err(err),
            },
            OpenTarget::Any => match self.create_or_open(path, disposition) {
                Ok(out) => Ok(out),
                // N10: `Overwrite` / `OverwriteIf` / `Supersede` targeting a directory without an
                // explicit file/dir flag must report `IsADirectory` rather than `NotADirectory`.
                Err(OpsError::NotADirectory)
                    if matches!(
                        disposition,
                        Disposition::Overwrite | Disposition::OverwriteIf | Disposition::Supersede
                    ) =>
                {
                    Err(OpsError::IsADirectory)
                }
                Err(OpsError::AccessDenied)
                    if disposition == Disposition::Supersede
                        && self.open_dir(path).is_ok() =>
                {
                    Err(OpsError::IsADirectory)
                }
                Err(err) => Err(err),
            },
        }
    }

    fn read_dir_utf16_cached(&self, dir_id: u64) -> OpsResult<fs_winfsp::ops::DirListingUtf16> {
        // N7: O(1) cached UTF-16 sorted directory listing validated by `meta_version`.
        let start_ver = self.meta_version.load(Ordering::Acquire);
        let start_gen = {
            let guard = self.dentry_shard(dir_id).read().unwrap();
            if let Some(dir) = guard.dirs.get(&dir_id) {
                if dir.complete {
                    if let Some((cached_ver, ref listing)) = dir.utf16_listing {
                        if cached_ver == start_ver {
                            dir.referenced.store(true, Ordering::Relaxed);
                            return Ok(Arc::clone(listing));
                        }
                    }
                }
                Some(dir.generation)
            } else {
                None
            }
        };

        let items = self.read_dir_inner(dir_id)?;
        let mut children: Vec<(Vec<u16>, NodeInfo)> = items
            .into_iter()
            .map(|(name, _, info)| {
                (
                    String::from_utf8_lossy(&name)
                        .encode_utf16()
                        .collect::<Vec<u16>>(),
                    info,
                )
            })
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));

        let entries = if dir_id != ROOT_INODE {
            if let Ok(self_info) = self.get_info(dir_id) {
                let mut out = Vec::with_capacity(children.len() + 2);
                out.push((vec![u16::from(b'.')], self_info.clone()));
                out.push((vec![u16::from(b'.'), u16::from(b'.')], self_info));
                out.extend(children);
                out
            } else {
                children
            }
        } else {
            children
        };
        let arc = Arc::new(entries);

        // Publish cached listing only if no metadata or directory mutations raced while building.
        if self.meta_version.load(Ordering::Acquire) == start_ver {
            let mut guard = self.dentry_shard(dir_id).write().unwrap();
            if let Some(dir) = guard.dirs.get_mut(&dir_id) {
                if dir.complete && (start_gen.is_none() || Some(dir.generation) == start_gen) {
                    dir.utf16_listing = Some((start_ver, Arc::clone(&arc)));
                }
            }
        }
        Ok(arc)
    }
}

impl DedupWorkspaceBackend {
    /// Single-transaction directory creation returning the full `OpenOutcome` (id, binding, and
    /// `NodeInfo`), avoiding follow-up `get_info` + `dir_binding` walks.
    fn mkdir_outcome(&self, path: &Path<'_>, allow_existing_dir: bool) -> OpsResult<OpenOutcome> {
        let mut conn = self.store.lock_meta();
        let Some((parent_id, leaf_disp, leaf_fold)) = self.resolve_parent(&conn, path)? else {
            if allow_existing_dir {
                let row = self
                    .load_inode_cached(&conn, ROOT_INODE)?
                    .ok_or(OpsError::NotFound)?;
                let info = cached_to_node_info(&row);
                return Ok(OpenOutcome {
                    file_id: info.file_id,
                    kind: info.kind,
                    action: Action::Open,
                    size: info.size,
                    times: info.times,
                    attributes: info.attributes,
                    binding: (ROOT_INODE, Vec::new()),
                    replaced: None,
                });
            }
            return Err(OpsError::NameCollision);
        };
        if let Some((child_id, kind, stored_name)) =
            self.lookup_child_db(&conn, parent_id, &leaf_fold)?
        {
            if allow_existing_dir {
                if kind != KIND_DIR {
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
        self.cache_dentry_mutation(parent_id, leaf_fold, (inode_id, KIND_DIR, leaf_disp.clone()));
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
                dirty: false,
            }),
        );
        Ok(OpenOutcome {
            file_id: inode_id,
            kind: Kind::Directory,
            action: Action::CreateNew,
            size: 0,
            times: Times {
                created: ts,
                accessed: ts,
                written: ts,
                changed: ts,
            },
            attributes: Attributes(report_attributes(true, 0)),
            binding: (parent_id, leaf_disp),
            replaced: None,
        })
    }
}
