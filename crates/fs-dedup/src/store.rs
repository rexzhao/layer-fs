//! High-level `DedupStore`: init, open, parallel import, instant CoW clone, background
//! cold-file deduplication, pack GC, and store statistics.

use crate::chunker::{blake3_hash, chunk_bytes, chunk_reader, compress_chunk, Codec, Hash32};
use crate::error::{Error, Result};
use crate::filter;
use crate::meta::{
    cancel_pending_chunk_release, create_empty_workspace, decode_manifest, encode_manifest,
    enqueue_pending_chunk_release, find_workspace_id, get_chunk_loc, is_hot_db_name, load_inode,
    next_pack_id, now_ns, open_meta_connection, open_readonly_meta_connection,
    release_manifest_chunks, replay_pending_chunk_releases, upsert_chunk_ref, upsert_pack_updates,
    InodeRow, ManifestEntry, StoreConfig, StoreStats, WorkspaceInfo, KIND_DIR, KIND_FILE,
    ROOT_INODE, STORAGE_CHUNKS, STORAGE_OVERLAY,
};
use crate::overlay::OverlayStore;
use crate::pack::{ChunkLoc, PackStore, DEFAULT_CACHE_BYTES, RECORD_HEADER_BYTES};
use fs_semantics::name::fold_bytes;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

const INODE_LOCK_STRIPES: usize = 1024;
const KNOWN_SHARDS: usize = 64;
const META_CACHE_SHARDS: usize = 16;
const CHUNK_LOC_CACHE_PER_SHARD: usize = 32_768; // 524k total (~32 MiB)
const INODE_CACHE_PER_SHARD: usize = 65_536; // 1,048k total (~100 MiB, fits entire Unity Assets/)
const IN_MEMORY_IMPORT_LIMIT: u64 = 256 * 1024;
const IMPORT_COMMIT_BATCH: usize = 2048;
const IMPORT_COMMIT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const IMPORT_INFLIGHT_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const DEDUP_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Cached inode metadata + pre-decoded chunk manifest + chunk start offsets held in RAM for
/// sub-microsecond `get_info` and O(log N) `read` dispatch without hitting SQLite.
#[derive(Clone, Debug)]
pub struct CachedInode {
    pub inode_id: u64,
    pub kind: u8,
    pub size: u64,
    pub attributes: u32,
    pub created_ns: i64,
    pub accessed_ns: i64,
    pub written_ns: i64,
    pub changed_ns: i64,
    pub storage_mode: u8,
    /// Decoded chunk manifest (`None` when prefetched by `read_dir` without reading `manifest` BLOBs).
    pub entries: Option<Arc<[ManifestEntry]>>,
    /// Prefix-sum byte start offsets for each entry in `entries` (enables O(log N) binary search).
    pub chunk_offsets: Option<Arc<[u64]>>,
    /// `true` when this inode has buffered `(size, written_ns)` writes not yet committed to SQLite.
    /// Dirty entries are never evicted by CLOCK and never overwritten by stale `read_dir` snapshots.
    pub dirty: bool,
}

impl CachedInode {
    pub fn build_offsets(entries: &[ManifestEntry]) -> Arc<[u64]> {
        let mut offsets = Vec::with_capacity(entries.len());
        let mut cur = 0u64;
        for e in entries {
            offsets.push(cur);
            cur += u64::from(e.raw_len);
        }
        Arc::from(offsets.into_boxed_slice())
    }

    pub fn from_row(row: &InodeRow) -> Result<Self> {
        let (entries, chunk_offsets) =
            if row.storage_mode == STORAGE_CHUNKS && !row.manifest.is_empty() {
                let decoded = decode_manifest(&row.manifest)?;
                let offsets = Self::build_offsets(&decoded);
                (Arc::from(decoded.into_boxed_slice()), offsets)
            } else {
                (
                    Arc::from(Vec::new().into_boxed_slice()),
                    Arc::from(Vec::new().into_boxed_slice()),
                )
            };
        Ok(Self {
            inode_id: row.inode_id,
            kind: row.kind,
            size: row.size,
            attributes: row.attributes,
            created_ns: row.created_ns,
            accessed_ns: row.accessed_ns,
            written_ns: row.written_ns,
            changed_ns: row.changed_ns,
            storage_mode: row.storage_mode,
            entries: Some(entries),
            chunk_offsets: Some(chunk_offsets),
            dirty: false,
        })
    }
}

/// Live progress snapshot reported during `import_dir` / `import_dir_update`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportProgress {
    pub files: u64,
    pub unchanged_files: u64,
    pub updated_files: u64,
    pub removed_entries: u64,
    pub excluded_entries: u64,
    pub dirs: u64,
    pub logical_bytes: u64,
    pub new_unique_bytes: u64,
    pub stored_bytes: u64,
    pub overlay_pinned_files: u64,
    pub errors: u64,
}

/// Summary returned by `import_dir` / `import_dir_update`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ImportSummary {
    pub workspace: String,
    pub ws_id: u64,
    pub files: u64,
    pub unchanged_files: u64,
    pub updated_files: u64,
    pub removed_entries: u64,
    pub excluded_entries: u64,
    pub dirs: u64,
    pub logical_bytes: u64,
    pub new_unique_chunks: u64,
    pub new_unique_bytes: u64,
    pub stored_bytes: u64,
    pub overlay_pinned_files: u64,
    pub errors: u64,
    pub elapsed_secs: f64,
}

/// Summary returned by `dedup_workspace`.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DedupSummary {
    pub scanned: u64,
    pub deduped_files: u64,
    pub skipped_busy: u64,
    pub skipped_changed: u64,
    pub skipped_filtered: u64,
    pub raw_bytes: u64,
    pub new_unique_bytes: u64,
    pub stored_bytes: u64,
    pub reclaimed_overlay_bytes: u64,
}

/// Summary returned by `gc`.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct GcSummary {
    pub deleted_empty_packs: u64,
    pub compacted_packs: u64,
    pub moved_chunks: u64,
    pub reclaimed_disk_bytes: u64,
}

/// Summary returned by `fsck`.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct FsckSummary {
    pub files_scanned: u64,
    pub chunk_refs_counted: u64,
    pub total_chunks: u64,
    pub missing_chunks: u64,
    pub mismatched_refcounts: u64,
    pub orphan_chunks: u64,
    pub pack_accounting_fixed: u64,
    pub fixed: bool,
}

/// Sharded in-memory set of **already committed** 32-byte chunk hashes for lock-free fast-path
/// dedup during parallel `import_dir`.
///
/// Only the single-threaded committer inserts into `KnownChunks` AFTER a SQLite batch commits,
/// guaranteeing workers never skip a chunk whose writer batch hasn't committed yet (or failed).
struct KnownChunks {
    shards: Vec<RwLock<HashSet<Hash32>>>,
}

impl KnownChunks {
    fn load(conn: &Connection) -> Result<Self> {
        let mut shards: Vec<RwLock<HashSet<Hash32>>> = (0..KNOWN_SHARDS)
            .map(|_| RwLock::new(HashSet::new()))
            .collect();
        let mut stmt = conn.prepare("SELECT hash FROM chunks")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        for row in rows {
            let bytes = row?;
            if bytes.len() == 32 {
                let h: Hash32 = bytes.try_into().unwrap();
                let idx = (h[0] as usize) & (KNOWN_SHARDS - 1);
                shards[idx].get_mut().unwrap().insert(h);
            }
        }
        Ok(Self { shards })
    }

    #[inline]
    fn contains(&self, hash: &Hash32) -> bool {
        let idx = (hash[0] as usize) & (KNOWN_SHARDS - 1);
        self.shards[idx].read().unwrap().contains(hash)
    }

    fn insert_batch(&self, hashes: impl IntoIterator<Item = Hash32>) {
        let mut by_shard: [Vec<Hash32>; KNOWN_SHARDS] = std::array::from_fn(|_| Vec::new());
        for h in hashes {
            by_shard[(h[0] as usize) & (KNOWN_SHARDS - 1)].push(h);
        }
        for (idx, list) in by_shard.into_iter().enumerate() {
            if !list.is_empty() {
                let mut guard = self.shards[idx].write().unwrap();
                for h in list {
                    guard.insert(h);
                }
            }
        }
    }
}

type InodeCacheShard = RwLock<HashMap<(u64, u64), (Arc<CachedInode>, AtomicBool)>>;
type ChunkLocCacheShard = RwLock<HashMap<Hash32, (ChunkLoc, AtomicBool)>>;

/// CLOCK-style partial eviction: removes ~25% of unreferenced entries (giving referenced
/// entries a second chance) instead of wiping the entire shard with `clear()`.
fn clock_evict_quarter<K: Eq + std::hash::Hash + Copy, V>(
    map: &mut HashMap<K, (V, AtomicBool)>,
    limit: usize,
) {
    let target_remove = (limit / 4).max(1);
    let mut to_remove: Vec<K> = Vec::with_capacity(target_remove);
    // Pass 1: evict cold entries (`referenced == false`), clear ref bit on hot entries.
    for (&k, (_, ref_bit)) in map.iter() {
        if ref_bit.swap(false, Ordering::Relaxed) {
            continue;
        }
        to_remove.push(k);
        if to_remove.len() >= target_remove {
            break;
        }
    }
    // Pass 2 (if almost everything was hot): evict remaining needed count.
    if to_remove.len() < target_remove {
        for &k in map.keys() {
            to_remove.push(k);
            if to_remove.len() >= target_remove {
                break;
            }
        }
    }
    for k in to_remove {
        map.remove(&k);
    }
}

/// CLOCK-style partial eviction for `inode_cache`: removes ~25% of clean entries,
/// never evicting `dirty == true` entries that carry authoritative unflushed metadata.
fn clock_evict_quarter_inodes(
    map: &mut HashMap<(u64, u64), (Arc<CachedInode>, AtomicBool)>,
    limit: usize,
) {
    let target_remove = (limit / 4).max(1);
    let mut to_remove: Vec<(u64, u64)> = Vec::with_capacity(target_remove);
    for (&k, (inode, ref_bit)) in map.iter() {
        if inode.dirty {
            continue;
        }
        if ref_bit.swap(false, Ordering::Relaxed) {
            continue;
        }
        to_remove.push(k);
        if to_remove.len() >= target_remove {
            break;
        }
    }
    if to_remove.len() < target_remove {
        for (&k, (inode, _)) in map.iter() {
            if inode.dirty {
                continue;
            }
            to_remove.push(k);
            if to_remove.len() >= target_remove {
                break;
            }
        }
    }
    for k in to_remove {
        map.remove(&k);
    }
}

struct ByteBudgetState {
    used: usize,
    cancelled: bool,
}

/// Byte-level backpressure semaphore bounding total in-flight chunk payload memory between
/// parallel import workers and the sequential pack committer, with RAII permits and cancellation.
struct ByteBudget {
    max: usize,
    state: Mutex<ByteBudgetState>,
    cvar: Condvar,
}

/// RAII permit returning reserved bytes to [`ByteBudget`] automatically on drop (including early
/// `?` returns, channel teardown, or panics).
struct BudgetPermit {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for BudgetPermit {
    fn drop(&mut self) {
        if self.bytes > 0 {
            let mut st = self.budget.state.lock().unwrap();
            st.used = st.used.saturating_sub(self.bytes);
            self.budget.cvar.notify_all();
        }
    }
}

/// RAII guard that cancels a [`ByteBudget`] when the consumer thread exits (on error, panic, or return).
struct CancelOnExit(Arc<ByteBudget>);

impl Drop for CancelOnExit {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl ByteBudget {
    fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            state: Mutex::new(ByteBudgetState {
                used: 0,
                cancelled: false,
            }),
            cvar: Condvar::new(),
        }
    }

    /// Acquires an RAII permit for `bytes`, returning `None` immediately if the pipeline was cancelled.
    fn acquire(self: &Arc<Self>, bytes: usize) -> Option<BudgetPermit> {
        let mut st = self.state.lock().unwrap();
        // Allow progress when `used == 0` even if a single chunk exceeds `max`.
        while !st.cancelled && st.used > 0 && st.used.saturating_add(bytes) > self.max {
            st = self.cvar.wait(st).unwrap();
        }
        if st.cancelled {
            return None;
        }
        st.used = st.used.saturating_add(bytes);
        Some(BudgetPermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    fn cancel(&self) {
        let mut st = self.state.lock().unwrap();
        st.cancelled = true;
        self.cvar.notify_all();
    }

    fn is_cancelled(&self) -> bool {
        self.state.lock().unwrap().cancelled
    }
}

/// Exclusive cross-process & intra-process guard serializing pack allocation, appends, GC, and
/// `fsck --fix` across independent `DedupStore` handles on the same store directory.
struct PackWriteGuard<'a> {
    store: &'a DedupStore,
    _lock_file: File,
}

impl Drop for PackWriteGuard<'_> {
    fn drop(&mut self) {
        // Flush and close any open active pack handle BEFORE releasing the OS file lock so
        // the next serialized writer can safely sync from SQLite and resume/roll packs.
        let _ = self.store.packs.writer().close_active();
    }
}

/// Shared deduplicating store handle (metadata SQLite + external pack pool + local sparse overlay).
pub struct DedupStore {
    root: PathBuf,
    config: StoreConfig,
    meta_path: PathBuf,
    meta: Mutex<Connection>,
    packs: PackStore,
    overlay: OverlayStore,
    inode_locks: Vec<RwLock<()>>,
    chunk_loc_cache: Vec<ChunkLocCacheShard>,
    inode_cache: Vec<InodeCacheShard>,
}

impl DedupStore {
    /// Initializes a brand-new dedup store directory.
    pub fn init(dir: impl AsRef<Path>, config: StoreConfig) -> Result<Arc<Self>> {
        let root = dir.as_ref().to_path_buf();
        let config_path = root.join("store.json");
        let meta_path = root.join("meta.sqlite");
        if config_path.exists() || meta_path.exists() {
            return Err(Error::AlreadyExists(format!(
                "dedup store already exists at {}",
                root.display()
            )));
        }
        fs::create_dir_all(root.join("packs"))?;
        fs::create_dir_all(root.join("overlay"))?;
        fs::create_dir_all(root.join("mounts"))?;

        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| Error::Other(format!("serialize store.json: {e}")))?;
        fs::write(&config_path, json)?;

        let conn = open_meta_connection(&meta_path)?;
        drop(conn);
        Self::open_with_cache(root, DEFAULT_CACHE_BYTES)
    }

    /// Opens an existing dedup store with the default decoded chunk cache (256 MiB).
    pub fn open(dir: impl AsRef<Path>) -> Result<Arc<Self>> {
        Self::open_with_cache(dir, DEFAULT_CACHE_BYTES)
    }

    /// Opens an existing dedup store with a custom decoded chunk cache size.
    pub fn open_with_cache(dir: impl AsRef<Path>, cache_bytes: usize) -> Result<Arc<Self>> {
        let root = fs::canonicalize(dir.as_ref()).unwrap_or_else(|_| dir.as_ref().to_path_buf());
        let config_path = root.join("store.json");
        let meta_path = root.join("meta.sqlite");
        if !config_path.exists() || !meta_path.exists() {
            return Err(Error::NotFound(format!(
                "no dedup store found at {}",
                root.display()
            )));
        }
        let raw_cfg = fs::read_to_string(&config_path)?;
        let config: StoreConfig = serde_json::from_str(&raw_cfg)
            .map_err(|e| Error::Corrupt(format!("invalid store.json: {e}")))?;
        let conn = open_meta_connection(&meta_path)?;
        let next_id = next_pack_id(&conn)?;
        let packs = PackStore::open(
            root.join("packs"),
            config.pack_target_bytes,
            next_id,
            cache_bytes,
        );
        let overlay = OverlayStore::new(root.join("overlay"));
        let inode_locks = (0..INODE_LOCK_STRIPES).map(|_| RwLock::new(())).collect();
        let chunk_loc_cache = (0..META_CACHE_SHARDS)
            .map(|_| RwLock::new(HashMap::new()))
            .collect();
        let inode_cache = (0..META_CACHE_SHARDS)
            .map(|_| RwLock::new(HashMap::new()))
            .collect();
        let store = Arc::new(Self {
            root,
            config,
            meta_path,
            meta: Mutex::new(conn),
            packs,
            overlay,
            inode_locks,
            chunk_loc_cache,
            inode_cache,
        });
        // Recover any durable `pending_chunk_releases` left over from an interrupted/failed update.
        store.recover_pending_releases_if_idle()?;
        Ok(store)
    }

    /// Updates `<store>/store.json` atomically via `mutate` and returns the saved config.
    pub fn update_config(
        dir: impl AsRef<Path>,
        mutate: impl FnOnce(&mut StoreConfig),
    ) -> Result<StoreConfig> {
        let root = fs::canonicalize(dir.as_ref()).unwrap_or_else(|_| dir.as_ref().to_path_buf());
        let config_path = root.join("store.json");
        if !config_path.exists() {
            return Err(Error::NotFound(format!(
                "no dedup store found at {}",
                root.display()
            )));
        }
        let raw_cfg = fs::read_to_string(&config_path)?;
        let mut config: StoreConfig = serde_json::from_str(&raw_cfg)
            .map_err(|e| Error::Corrupt(format!("invalid store.json: {e}")))?;
        mutate(&mut config);
        let json = serde_json::to_string_pretty(&config)
            .map_err(|e| Error::Other(format!("serialize store.json: {e}")))?;
        let tmp_path = root.join("store.json.tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &config_path)?;
        Ok(config)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }

    pub fn packs(&self) -> &PackStore {
        &self.packs
    }

    pub fn overlay(&self) -> &OverlayStore {
        &self.overlay
    }

    /// Acquires an exclusive per-inode stripe lock (mutations, CoW, background dedup).
    pub fn lock_inode(&self, ws_id: u64, inode_id: u64) -> std::sync::RwLockWriteGuard<'_, ()> {
        let idx = ((ws_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ inode_id) as usize)
            & (INODE_LOCK_STRIPES - 1);
        self.inode_locks[idx].write().unwrap()
    }

    /// Acquires a shared per-inode stripe lock allowing concurrent reads of the same hot file
    /// (e.g. parallel WinFsp page-fault reads on Unity's `ArtifactDB` / `SourceAssetDB`).
    pub fn lock_inode_read(&self, ws_id: u64, inode_id: u64) -> std::sync::RwLockReadGuard<'_, ()> {
        let idx = ((ws_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ inode_id) as usize)
            & (INODE_LOCK_STRIPES - 1);
        self.inode_locks[idx].read().unwrap()
    }

    /// Opens an additional WAL SQLite connection for concurrent workspace reads/writes.
    pub fn open_conn(&self) -> Result<Connection> {
        open_meta_connection(&self.meta_path)
    }

    /// Opens a fast read-only WAL SQLite connection (`PRAGMA query_only = ON`, 4 GiB mmap).
    pub fn open_readonly_conn(&self) -> Result<Connection> {
        open_readonly_meta_connection(&self.meta_path)
    }

    /// Looks up a chunk's pack location using the sharded CLOCK RAM cache before falling back to SQLite.
    pub fn resolve_chunk_loc(&self, conn: &Connection, hash: &Hash32) -> Result<Option<ChunkLoc>> {
        let shard = (hash[0] as usize) & (META_CACHE_SHARDS - 1);
        if let Some((loc, ref_bit)) = self.chunk_loc_cache[shard].read().unwrap().get(hash) {
            ref_bit.store(true, Ordering::Relaxed);
            return Ok(Some(*loc));
        }
        let loc = get_chunk_loc(conn, hash)?;
        if let Some(found) = loc {
            let mut guard = self.chunk_loc_cache[shard].write().unwrap();
            if guard.len() >= CHUNK_LOC_CACHE_PER_SHARD {
                clock_evict_quarter(&mut guard, CHUNK_LOC_CACHE_PER_SHARD);
            }
            guard.insert(*hash, (found, AtomicBool::new(true)));
        }
        Ok(loc)
    }

    /// Reads a cached inode snapshot if present (shared read lock + lock-free CLOCK ref bit).
    pub fn get_cached_inode(&self, ws_id: u64, inode_id: u64) -> Option<Arc<CachedInode>> {
        let shard = ((ws_id ^ inode_id) as usize) & (META_CACHE_SHARDS - 1);
        let guard = self.inode_cache[shard].read().unwrap();
        let (inode, ref_bit) = guard.get(&(ws_id, inode_id))?;
        ref_bit.store(true, Ordering::Relaxed);
        Some(Arc::clone(inode))
    }

    /// Stores an inode snapshot in the sharded CLOCK RAM cache.
    pub fn put_cached_inode(&self, ws_id: u64, inode: Arc<CachedInode>) {
        let shard = ((ws_id ^ inode.inode_id) as usize) & (META_CACHE_SHARDS - 1);
        let mut guard = self.inode_cache[shard].write().unwrap();
        if guard.len() >= INODE_CACHE_PER_SHARD {
            clock_evict_quarter_inodes(&mut guard, INODE_CACHE_PER_SHARD);
        }
        guard.insert((ws_id, inode.inode_id), (inode, AtomicBool::new(true)));
    }

    /// Marks a cached inode clean (`dirty = false`) once its `(size, written_ns)` has been
    /// committed to SQLite, leaving it dirty if a newer concurrent write updated it.
    pub fn clear_inode_dirty(&self, ws_id: u64, inode_id: u64, flushed_size: u64, flushed_ts: i64) {
        let shard = ((ws_id ^ inode_id) as usize) & (META_CACHE_SHARDS - 1);
        let mut guard = self.inode_cache[shard].write().unwrap();
        if let Some((prev, _)) = guard.get_mut(&(ws_id, inode_id)) {
            if prev.dirty && prev.size == flushed_size && prev.written_ns == flushed_ts {
                let mut clean = (**prev).clone();
                clean.dirty = false;
                *prev = Arc::new(clean);
            }
        }
    }

    /// Batch-populates inode metadata prefetched by `read_dir`, preserving any already-decoded
    /// chunk manifests & offsets in cache and NEVER overwriting `dirty == true` entries.
    pub fn put_cached_inodes_preserve_manifest(&self, ws_id: u64, batch: Vec<CachedInode>) {
        if batch.is_empty() {
            return;
        }
        let mut by_shard: [Vec<CachedInode>; META_CACHE_SHARDS] =
            std::array::from_fn(|_| Vec::new());
        for item in batch {
            let shard = ((ws_id ^ item.inode_id) as usize) & (META_CACHE_SHARDS - 1);
            by_shard[shard].push(item);
        }
        for (shard_idx, items) in by_shard.into_iter().enumerate() {
            if items.is_empty() {
                continue;
            }
            let mut guard = self.inode_cache[shard_idx].write().unwrap();
            if guard.len().saturating_add(items.len()) >= INODE_CACHE_PER_SHARD {
                clock_evict_quarter_inodes(&mut guard, INODE_CACHE_PER_SHARD);
            }
            for mut item in items {
                if let Some((prev, _)) = guard.get(&(ws_id, item.inode_id)) {
                    // Authoritative dirty write metadata must never be clobbered by a `read_dir` snapshot.
                    if prev.dirty {
                        continue;
                    }
                    if item.entries.is_none()
                        && prev.storage_mode == item.storage_mode
                        && prev.size == item.size
                        && prev.written_ns == item.written_ns
                    {
                        item.entries = prev.entries.clone();
                        item.chunk_offsets = prev.chunk_offsets.clone();
                    }
                }
                guard.insert(
                    (ws_id, item.inode_id),
                    (Arc::new(item), AtomicBool::new(true)),
                );
            }
        }
    }

    /// Evicts one inode from the sharded RAM cache (called on deletes and dedup swaps).
    pub fn invalidate_inode(&self, ws_id: u64, inode_id: u64) {
        let shard = ((ws_id ^ inode_id) as usize) & (META_CACHE_SHARDS - 1);
        self.inode_cache[shard]
            .write()
            .unwrap()
            .remove(&(ws_id, inode_id));
    }

    /// Non-blocking attempt to acquire `pack.lock` (returns `Ok(None)` if another writer holds it).
    fn try_acquire_pack_write_lock(&self) -> Result<Option<PackWriteGuard<'_>>> {
        let lock_path = self.root.join("pack.lock");
        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(0);
        }
        let lock_file = match opts.open(&lock_path) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        self.sync_pack_writer_from_db()?;
        Ok(Some(PackWriteGuard {
            store: self,
            _lock_file: lock_file,
        }))
    }

    fn sync_pack_writer_from_db(&self) -> Result<()> {
        let (db_next, unsealed) = {
            let conn = self.meta.lock().unwrap();
            let next_id = next_pack_id(&conn)?;
            let unsealed: Option<(u64, u64)> = conn
                .query_row(
                    "SELECT pack_id, size_bytes FROM packs WHERE sealed = 0 ORDER BY pack_id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
                )
                .optional()?;
            (next_id, unsealed)
        };
        self.packs.writer().sync_from_db(db_next, unsealed)
    }

    /// Replays any durably queued `pending_chunk_releases` when no active writer holds `pack.lock`.
    fn recover_pending_releases_if_idle(&self) -> Result<()> {
        let has_pending: bool = {
            let conn = self.meta.lock().unwrap();
            conn.query_row("SELECT 1 FROM pending_chunk_releases LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some()
        };
        if !has_pending {
            return Ok(());
        }
        let Some(_guard) = self.try_acquire_pack_write_lock()? else {
            return Ok(());
        };
        self.replay_pending_releases_locked()
    }

    /// Applies and clears all rows in `pending_chunk_releases` (caller MUST hold `pack.lock`).
    fn replay_pending_releases_locked(&self) -> Result<()> {
        let deleted = {
            let mut conn = self.meta.lock().unwrap();
            let tx = conn.transaction()?;
            let deleted = replay_pending_chunk_releases(&tx)?;
            tx.commit()?;
            deleted
        };
        self.invalidate_chunk_locs(&deleted);
        Ok(())
    }

    /// Acquires the exclusive `<store>/pack.lock` file and synchronizes `PackWriter`'s
    /// `next_pack_id` and active unsealed pack directly from `meta.sqlite`.
    /// Prevents independent `DedupStore` handles / processes from colliding on pack IDs.
    fn acquire_pack_write_lock(&self) -> Result<PackWriteGuard<'_>> {
        let lock_path = self.root.join("pack.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        let lock_file = loop {
            let mut opts = OpenOptions::new();
            opts.create(true).read(true).write(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // Exclusive OS file lock (`share_mode(0)`) enforced both cross-process and intra-process.
                opts.share_mode(0);
            }
            match opts.open(&lock_path) {
                Ok(f) => break f,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(Error::Other(format!(
                        "another writer holds store pack lock ({}): {e}",
                        lock_path.display()
                    )));
                }
            }
        };

        self.sync_pack_writer_from_db()?;
        Ok(PackWriteGuard {
            store: self,
            _lock_file: lock_file,
        })
    }

    /// Evicts deleted chunk hashes from `chunk_loc_cache` so removed chunks are never served as stale hits.
    pub fn invalidate_chunk_locs(&self, hashes: &[Hash32]) {
        if hashes.is_empty() {
            return;
        }
        let mut by_shard: [Vec<Hash32>; META_CACHE_SHARDS] = std::array::from_fn(|_| Vec::new());
        for &h in hashes {
            let shard = (h[0] as usize) & (META_CACHE_SHARDS - 1);
            by_shard[shard].push(h);
        }
        for (shard_idx, list) in by_shard.into_iter().enumerate() {
            if !list.is_empty() {
                let mut guard = self.chunk_loc_cache[shard_idx].write().unwrap();
                for h in list {
                    guard.remove(&h);
                }
            }
        }
    }

    fn clear_chunk_loc_cache(&self) {
        for shard in &self.chunk_loc_cache {
            shard.write().unwrap().clear();
        }
    }

    /// Creates a brand-new empty workspace.
    pub fn create_workspace(&self, name: &str) -> Result<u64> {
        let mut conn = self.meta.lock().unwrap();
        let tx = conn.transaction()?;
        let ws_id = create_empty_workspace(&tx, name)?;
        tx.commit()?;
        Ok(ws_id)
    }

    /// Instantly clones an existing workspace's directory tree & manifests into `new_name`
    /// without copying any chunk payload bytes.
    pub fn clone_workspace(&self, src_name: &str, new_name: &str) -> Result<u64> {
        if new_name.is_empty() || new_name.contains('/') || new_name.contains('\\') {
            return Err(Error::InvalidInput(format!(
                "invalid workspace name {new_name:?}"
            )));
        }
        let mut conn = self.meta.lock().unwrap();
        let src_ws_id = find_workspace_id(&conn, src_name)?;
        let tx = conn.transaction()?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT ws_id FROM workspaces WHERE name = ?1",
                params![new_name],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            return Err(Error::AlreadyExists(format!(
                "workspace {new_name:?} already exists"
            )));
        }
        let next_inode: i64 = tx.query_row(
            "SELECT next_inode FROM workspaces WHERE ws_id = ?1",
            params![src_ws_id as i64],
            |r| r.get(0),
        )?;
        let ts = now_ns();
        tx.execute(
            "INSERT INTO workspaces (name, root_inode, next_inode, created_ns) VALUES (?1, ?2, ?3, ?4)",
            params![new_name, ROOT_INODE as i64, next_inode, ts],
        )?;
        let dst_ws_id = tx.last_insert_rowid() as u64;

        // Bulk copy inodes and dentries inside SQLite.
        tx.execute(
            "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
             SELECT ?1, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay
             FROM inodes WHERE ws_id = ?2",
            params![dst_ws_id as i64, src_ws_id as i64],
        )?;
        tx.execute(
            "INSERT INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
             SELECT ?1, parent_id, folded_name, display_name, child_id, kind
             FROM dentries WHERE ws_id = ?2",
            params![dst_ws_id as i64, src_ws_id as i64],
        )?;

        // Copy any overlay loose files from src_ws_id to dst_ws_id, and increment refcounts for all Chunks manifests.
        let mut overlay_inodes = Vec::new();
        let mut chunk_ref_adds: HashMap<Hash32, u64> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT inode_id, storage_mode, manifest FROM inodes WHERE ws_id = ?1 AND kind = ?2",
            )?;
            let rows = stmt.query_map(params![dst_ws_id as i64, i64::from(KIND_FILE)], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u8,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for row in rows {
                let (inode_id, mode, manifest) = row?;
                if mode == STORAGE_OVERLAY {
                    overlay_inodes.push(inode_id);
                } else if !manifest.is_empty() {
                    for entry in decode_manifest(&manifest)? {
                        *chunk_ref_adds.entry(entry.hash).or_insert(0) += 1;
                    }
                }
            }
        }
        {
            let mut inc =
                tx.prepare_cached("UPDATE chunks SET refcount = refcount + ?1 WHERE hash = ?2")?;
            for (hash, add) in chunk_ref_adds {
                inc.execute(params![add as i64, hash.as_slice()])?;
            }
        }
        for inode_id in overlay_inodes {
            let src_file = self.overlay.file_path(src_ws_id, inode_id);
            if src_file.exists() {
                let dst_file = self.overlay.file_path(dst_ws_id, inode_id);
                if let Some(p) = dst_file.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(&src_file, &dst_file)?;
            }
        }

        tx.commit()?;
        Ok(dst_ws_id)
    }

    /// Deletes a workspace, decrementing chunk refcounts and removing its local overlay files.
    pub fn remove_workspace(&self, name: &str) -> Result<()> {
        let mut conn = self.meta.lock().unwrap();
        let ws_id = find_workspace_id(&conn, name)?;
        let tx = conn.transaction()?;

        let mut manifests = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT manifest FROM inodes WHERE ws_id = ?1 AND kind = ?2 AND storage_mode = ?3",
            )?;
            let rows = stmt.query_map(
                params![ws_id as i64, i64::from(KIND_FILE), i64::from(STORAGE_CHUNKS)],
                |r| r.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                let m = row?;
                if !m.is_empty() {
                    manifests.push(m);
                }
            }
        }
        let mut deleted_chunks = Vec::new();
        for m in manifests {
            deleted_chunks.extend(release_manifest_chunks(&tx, &m)?);
        }
        tx.execute("DELETE FROM dentries WHERE ws_id = ?1", params![ws_id as i64])?;
        tx.execute("DELETE FROM inodes WHERE ws_id = ?1", params![ws_id as i64])?;
        tx.execute(
            "DELETE FROM workspaces WHERE ws_id = ?1",
            params![ws_id as i64],
        )?;
        tx.commit()?;
        drop(conn);
        self.invalidate_chunk_locs(&deleted_chunks);
        self.overlay.remove_workspace(ws_id)?;
        Ok(())
    }

    /// Lists all workspaces with file/directory counts and logical/overlay byte totals.
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let conn = self.meta.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT w.ws_id, w.name, w.root_inode, w.created_ns,
                    COALESCE(SUM(CASE WHEN i.kind = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN i.kind = 2 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN i.kind = 1 THEN i.size ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN i.kind = 1 AND i.storage_mode = 1 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN i.kind = 1 AND i.storage_mode = 1 THEN i.size ELSE 0 END), 0)
             FROM workspaces w
             LEFT JOIN inodes i ON i.ws_id = w.ws_id
             GROUP BY w.ws_id
             ORDER BY w.ws_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkspaceInfo {
                ws_id: r.get::<_, i64>(0)? as u64,
                name: r.get(1)?,
                root_inode: r.get::<_, i64>(2)? as u64,
                created_ns: r.get(3)?,
                files: r.get::<_, i64>(4)? as u64,
                dirs: r.get::<_, i64>(5)? as u64,
                logical_bytes: r.get::<_, i64>(6)? as u64,
                overlay_files: r.get::<_, i64>(7)? as u64,
                overlay_bytes: r.get::<_, i64>(8)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Computes aggregate store & deduplication statistics.
    pub fn stats(&self) -> Result<StoreStats> {
        let conn = self.meta.lock().unwrap();
        let workspaces: i64 = conn.query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))?;
        let (total_files, total_logical, overlay_files, overlay_bytes): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN kind = 1 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN kind = 1 THEN size ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN kind = 1 AND storage_mode = 1 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN kind = 1 AND storage_mode = 1 THEN size ELSE 0 END), 0)
                 FROM inodes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let (unique_chunks, unique_raw, stored_chunk): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(raw_len), 0), COALESCE(SUM(stored_len), 0) FROM chunks",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (pack_count, pack_disk, pack_dead): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0), COALESCE(SUM(dead_bytes), 0) FROM packs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let meta_db_bytes = fs::metadata(&self.meta_path).map_or(0, |m| m.len());
        Ok(StoreStats {
            workspaces: workspaces as u64,
            total_files: total_files as u64,
            total_logical_bytes: total_logical as u64,
            overlay_files: overlay_files as u64,
            overlay_bytes: overlay_bytes as u64,
            unique_chunks: unique_chunks as u64,
            unique_raw_bytes: unique_raw as u64,
            stored_chunk_bytes: stored_chunk as u64,
            pack_count: pack_count as u64,
            pack_disk_bytes: pack_disk as u64,
            pack_dead_bytes: pack_dead as u64,
            meta_db_bytes,
        })
    }

    /// Reads `[offset, offset + len)` from a `Chunks`-backed inode row.
    pub fn read_chunks_inode(
        &self,
        conn: &Connection,
        inode: &InodeRow,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        if len == 0 || offset >= inode.size || inode.manifest.is_empty() {
            return Ok(Vec::new());
        }
        let entries = decode_manifest(&inode.manifest)?;
        let to_read = (inode.size - offset).min(len as u64) as usize;
        let mut out = Vec::with_capacity(to_read);
        let mut cursor = 0u64;
        let end = offset + to_read as u64;

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
            let loc = get_chunk_loc(conn, &entry.hash)?.ok_or_else(|| {
                Error::Corrupt(format!(
                    "missing chunk in index for inode {}",
                    inode.inode_id
                ))
            })?;
            let raw = self.packs.read_chunk(&entry.hash, loc)?;
            let slice_start = offset.saturating_sub(cursor) as usize;
            let slice_end = (end.min(chunk_end) - cursor) as usize;
            out.extend_from_slice(&raw[slice_start..slice_end]);
            cursor = chunk_end;
        }
        Ok(out)
    }

    /// Parallel multi-threaded host directory import into a new workspace.
    ///
    /// Uses Windows verbatim `\\?\...` paths (safe for reserved device names like `nul` and
    /// paths exceeding 260 characters), parallel FastCDC-256K + BLAKE3 + Zstd-1/LZ4 compression
    /// across all CPU cores, sequential 128 MiB `.pack` appends, and batched SQLite metadata commits.
    pub fn import_dir(
        self: &Arc<Self>,
        source: impl AsRef<Path>,
        ws_name: &str,
        threads: usize,
        progress_cb: Option<Box<dyn Fn(ImportProgress) + Send + Sync>>,
    ) -> Result<ImportSummary> {
        self.import_dir_with_options(source, ws_name, threads, &[], false, progress_cb)
    }

    /// Incrementally updates an existing workspace from `source` (or creates it if absent).
    ///
    /// Files whose `(size, mtime, attributes)` match the workspace's recorded metadata are
    /// skipped without opening or hashing; added/modified files are chunked and deduplicated
    /// in parallel; files/directories removed from `source` are removed from the workspace and
    /// their chunk refcounts are decremented.
    pub fn import_dir_update(
        self: &Arc<Self>,
        source: impl AsRef<Path>,
        ws_name: &str,
        threads: usize,
        progress_cb: Option<Box<dyn Fn(ImportProgress) + Send + Sync>>,
    ) -> Result<ImportSummary> {
        self.import_dir_with_options(source, ws_name, threads, &[], true, progress_cb)
    }

    /// Full-featured import/update entry point supporting additional one-shot `--exclude` patterns
    /// merged with `store.json`'s `import_exclude` rules.
    pub fn import_dir_with_options(
        self: &Arc<Self>,
        source: impl AsRef<Path>,
        ws_name: &str,
        threads: usize,
        extra_excludes: &[String],
        update: bool,
        progress_cb: Option<Box<dyn Fn(ImportProgress) + Send + Sync>>,
    ) -> Result<ImportSummary> {
        let mut excludes = self.config.import_exclude.clone();
        for pat in extra_excludes {
            if !excludes.iter().any(|e| e.eq_ignore_ascii_case(pat)) {
                excludes.push(pat.clone());
            }
        }
        let start = Instant::now();
        let canon_src = self.validate_import_source(source.as_ref())?;

        if update {
            let existing = {
                let conn = self.meta.lock().unwrap();
                conn.query_row(
                    "SELECT ws_id, next_inode FROM workspaces WHERE name = ?1",
                    params![ws_name],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
                )
                .optional()?
            };
            if let Some((ws_id, initial_next_inode)) = existing {
                let mut summary = self.import_dir_inner(
                    ws_id,
                    initial_next_inode.max(ROOT_INODE + 1),
                    true,
                    &canon_src,
                    &excludes,
                    threads.max(1),
                    progress_cb,
                )?;
                summary.workspace = ws_name.to_owned();
                summary.ws_id = ws_id;
                summary.elapsed_secs = start.elapsed().as_secs_f64();
                return Ok(summary);
            }
        }

        let ws_id = self.create_workspace(ws_name)?;
        let result = self.import_dir_inner(
            ws_id,
            ROOT_INODE + 1,
            false,
            &canon_src,
            &excludes,
            threads.max(1),
            progress_cb,
        );
        match result {
            Ok(mut summary) => {
                summary.workspace = ws_name.to_owned();
                summary.ws_id = ws_id;
                summary.elapsed_secs = start.elapsed().as_secs_f64();
                Ok(summary)
            }
            Err(err) => {
                let _ = self.remove_workspace(ws_name);
                Err(err)
            }
        }
    }

    fn validate_import_source(&self, source: &Path) -> Result<PathBuf> {
        let canon_src = fs::canonicalize(source)
            .map_err(|e| Error::InvalidInput(format!("cannot access source {}: {e}", source.display())))?;
        if !canon_src.is_dir() {
            return Err(Error::InvalidInput(format!(
                "source is not a directory: {}",
                source.display()
            )));
        }
        if self.root.starts_with(&canon_src) {
            return Err(Error::InvalidInput(format!(
                "store {} cannot lie inside source {}",
                self.root.display(),
                source.display()
            )));
        }
        Ok(canon_src)
    }

    fn delete_subtree(
        tx: &rusqlite::Transaction<'_>,
        ws_id: u64,
        parent_id: u64,
        folded_name: &[u8],
        root_child_id: u64,
        root_kind: u8,
        overlay_to_delete: &mut Vec<u64>,
    ) -> Result<u64> {
        let mut removed = 0u64;
        // Remove the entry binding in parent first.
        tx.execute(
            "DELETE FROM dentries WHERE ws_id = ?1 AND parent_id = ?2 AND folded_name = ?3",
            params![ws_id as i64, parent_id as i64, folded_name],
        )?;

        let mut stack: Vec<(u64, u8)> = vec![(root_child_id, root_kind)];
        while let Some((node_id, kind)) = stack.pop() {
            removed += 1;
            if kind == KIND_DIR {
                let mut children = Vec::new();
                {
                    let mut stmt = tx.prepare_cached(
                        "SELECT child_id, kind FROM dentries WHERE ws_id = ?1 AND parent_id = ?2",
                    )?;
                    let rows = stmt.query_map(params![ws_id as i64, node_id as i64], |r| {
                        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u8))
                    })?;
                    for r in rows {
                        children.push(r?);
                    }
                }
                tx.execute(
                    "DELETE FROM dentries WHERE ws_id = ?1 AND parent_id = ?2",
                    params![ws_id as i64, node_id as i64],
                )?;
                stack.extend(children);
            }
            if let Some(row) = load_inode(tx, ws_id, node_id)? {
                if row.kind == KIND_FILE {
                    if row.storage_mode == STORAGE_CHUNKS && !row.manifest.is_empty() {
                        // Persist deferred chunk releases atomically inside the same transaction
                        // as the inode delete so a crash/failure never drifts refcounts.
                        for e in decode_manifest(&row.manifest)? {
                            enqueue_pending_chunk_release(tx, &e.hash, 1)?;
                        }
                    } else if row.storage_mode == STORAGE_OVERLAY {
                        overlay_to_delete.push(node_id);
                    }
                }
            }
            tx.execute(
                "DELETE FROM inodes WHERE ws_id = ?1 AND inode_id = ?2",
                params![ws_id as i64, node_id as i64],
            )?;
        }
        Ok(removed)
    }

    #[allow(clippy::too_many_arguments)]
    fn import_dir_inner(
        self: &Arc<Self>,
        ws_id: u64,
        initial_next_inode: u64,
        is_update: bool,
        canon_src: &Path,
        excludes: &[String],
        threads: usize,
        progress_cb: Option<Box<dyn Fn(ImportProgress) + Send + Sync>>,
    ) -> Result<ImportSummary> {
        let _pack_guard = self.acquire_pack_write_lock()?;
        // Complete any pending releases from a previously interrupted import before starting.
        self.replay_pending_releases_locked()?;

        struct ExistingChild {
            display_name: Vec<u8>,
            inode_id: u64,
            kind: u8,
            size: u64,
            written_ns: i64,
            attributes: u32,
            storage_mode: u8,
        }

        struct ImportFileJob {
            parent_id: u64,
            inode_id: u64,
            display_name: Vec<u8>,
            folded_name: Vec<u8>,
            path: PathBuf,
            size: u64,
            attributes: u32,
            created_ns: i64,
            written_ns: i64,
            pinned_overlay: bool,
            /// `Some(old_storage_mode)` when replacing an existing file inode in place.
            replacing_mode: Option<u8>,
        }

        struct NewChunkBlob {
            hash: Hash32,
            codec: Codec,
            raw_len: u32,
            stored: Vec<u8>,
            _permit: BudgetPermit,
        }

        struct PreparedFile {
            parent_id: u64,
            inode_id: u64,
            display_name: Vec<u8>,
            folded_name: Vec<u8>,
            size: u64,
            attributes: u32,
            created_ns: i64,
            written_ns: i64,
            storage_mode: u8,
            pinned_overlay: bool,
            replacing_mode: Option<u8>,
            entries: Vec<ManifestEntry>,
        }

        enum ImportMsg {
            Chunk(NewChunkBlob),
            File(PreparedFile),
        }

        // Lazy-load `KnownChunks` on first file that actually needs chunking so a 0-change
        // `import --update` finishes without even scanning the `chunks` table!
        let lazy_known: Arc<Mutex<Option<Arc<KnownChunks>>>> = Arc::new(Mutex::new(None));
        if !is_update {
            let conn = self.meta.lock().unwrap();
            *lazy_known.lock().unwrap() = Some(Arc::new(KnownChunks::load(&conn)?));
        }

        let files_done = Arc::new(AtomicU64::new(0));
        let unchanged_done = Arc::new(AtomicU64::new(0));
        let updated_done = Arc::new(AtomicU64::new(0));
        let removed_done = Arc::new(AtomicU64::new(0));
        let excluded_done = Arc::new(AtomicU64::new(0));
        let dirs_done = Arc::new(AtomicU64::new(1)); // root dir
        let logical_done = Arc::new(AtomicU64::new(0));
        let unique_done = Arc::new(AtomicU64::new(0));
        let unique_chunks_done = Arc::new(AtomicU64::new(0));
        let stored_done = Arc::new(AtomicU64::new(0));
        let pinned_done = Arc::new(AtomicU64::new(0));
        let errors_done = Arc::new(AtomicU64::new(0));

        let next_inode = Arc::new(AtomicU64::new(initial_next_inode));

        // Channel: walker -> workers
        let (job_tx, job_rx) = mpsc::sync_channel::<ImportFileJob>(256);
        let job_rx = Arc::new(Mutex::new(job_rx));

        // Streaming chunk/file channel: workers -> committer, strictly byte-bounded by `budget`.
        let (prep_tx, prep_rx) = mpsc::sync_channel::<ImportMsg>(256);
        let budget = Arc::new(ByteBudget::new(IMPORT_INFLIGHT_BYTE_BUDGET));

        // Progress reporter thread (optional).
        let stop_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_handle = progress_cb.map(|cb| {
            let stop = Arc::clone(&stop_progress);
            let f = Arc::clone(&files_done);
            let unc = Arc::clone(&unchanged_done);
            let upd = Arc::clone(&updated_done);
            let rem = Arc::clone(&removed_done);
            let exc = Arc::clone(&excluded_done);
            let d = Arc::clone(&dirs_done);
            let l = Arc::clone(&logical_done);
            let u = Arc::clone(&unique_done);
            let s = Arc::clone(&stored_done);
            let p = Arc::clone(&pinned_done);
            let e = Arc::clone(&errors_done);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(2));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    cb(ImportProgress {
                        files: f.load(Ordering::Relaxed),
                        unchanged_files: unc.load(Ordering::Relaxed),
                        updated_files: upd.load(Ordering::Relaxed),
                        removed_entries: rem.load(Ordering::Relaxed),
                        excluded_entries: exc.load(Ordering::Relaxed),
                        dirs: d.load(Ordering::Relaxed),
                        logical_bytes: l.load(Ordering::Relaxed),
                        new_unique_bytes: u.load(Ordering::Relaxed),
                        stored_bytes: s.load(Ordering::Relaxed),
                        overlay_pinned_files: p.load(Ordering::Relaxed),
                        errors: e.load(Ordering::Relaxed),
                    });
                }
            })
        });

        // Spawn chunking + compression workers.
        let codec = self.config.codec;
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let rx = Arc::clone(&job_rx);
            let tx = prep_tx.clone();
            let lazy_known = Arc::clone(&lazy_known);
            let store = Arc::clone(self);
            let errs = Arc::clone(&errors_done);
            let budget_w = Arc::clone(&budget);
            workers.push(std::thread::spawn(move || {
                let mut file_buf = Vec::new();
                let mut cached_known: Option<Arc<KnownChunks>> = None;
                loop {
                    if budget_w.is_cancelled() {
                        break;
                    }
                    let job = { rx.lock().unwrap().recv() };
                    let Ok(job) = job else {
                        break;
                    };
                    if budget_w.is_cancelled() {
                        break;
                    }
                    if job.pinned_overlay {
                        // Copy hot DB file directly into the workspace's local sparse overlay.
                        let copy_res = (|| -> Result<u64> {
                            let dst = store.overlay.file_path(ws_id, job.inode_id);
                            if let Some(p) = dst.parent() {
                                fs::create_dir_all(p)?;
                            }
                            let copied = fs::copy(&job.path, &dst)?;
                            if let Ok(f) = fs::OpenOptions::new().write(true).open(&dst) {
                                let _ = layerfs_win32_host::set_sparse_file(&f);
                            }
                            Ok(copied)
                        })();
                        match copy_res {
                            Ok(actual_len) => {
                                let _ = tx.send(ImportMsg::File(PreparedFile {
                                    parent_id: job.parent_id,
                                    inode_id: job.inode_id,
                                    display_name: job.display_name,
                                    folded_name: job.folded_name,
                                    size: actual_len,
                                    attributes: job.attributes,
                                    created_ns: job.created_ns,
                                    written_ns: job.written_ns,
                                    storage_mode: STORAGE_OVERLAY,
                                    pinned_overlay: true,
                                    replacing_mode: job.replacing_mode,
                                    entries: Vec::new(),
                                }));
                            }
                            Err(err) => {
                                let n = errs.fetch_add(1, Ordering::Relaxed);
                                if n < 50 {
                                    eprintln!(
                                        "  [import error] copy pinned {}: {err}",
                                        job.path.display()
                                    );
                                }
                            }
                        }
                        continue;
                    }

                    // Ensure `KnownChunks` is loaded on first chunked file.
                    let known = match &cached_known {
                        Some(k) => Arc::clone(k),
                        None => {
                            let mut guard = lazy_known.lock().unwrap();
                            if guard.is_none() {
                                let conn = store.meta.lock().unwrap();
                                match KnownChunks::load(&conn) {
                                    Ok(loaded) => *guard = Some(Arc::new(loaded)),
                                    Err(err) => {
                                        let n = errs.fetch_add(1, Ordering::Relaxed);
                                        if n < 50 {
                                            eprintln!("  [import error] load chunk index: {err}");
                                        }
                                        continue;
                                    }
                                }
                            }
                            let k = Arc::clone(guard.as_ref().unwrap());
                            cached_known = Some(Arc::clone(&k));
                            k
                        }
                    };

                    let prep_res = (|| -> Result<(u64, Vec<ManifestEntry>)> {
                        let mut file = File::open(&job.path)?;
                        let mut entries = Vec::new();
                        let mut file_seen: HashSet<Hash32> = HashSet::new();

                        // Stream each newly compressed chunk immediately to the committer under
                        // RAII `ByteBudget` backpressure so peak RAM stays O(1) and failures wake
                        // all waiters immediately without deadlocking!
                        let mut handle_chunk = |chunk: &[u8]| -> Result<()> {
                            if budget_w.is_cancelled() {
                                return Err(Error::Other("import cancelled".into()));
                            }
                            let hash = blake3_hash(chunk);
                            let raw_len = chunk.len() as u32;
                            entries.push(ManifestEntry { hash, raw_len });
                            if !known.contains(&hash) && file_seen.insert(hash) {
                                let (actual_codec, stored) = compress_chunk(codec, chunk)?;
                                let charge = stored.len().max(1);
                                let Some(permit) = budget_w.acquire(charge) else {
                                    return Err(Error::Other("import cancelled".into()));
                                };
                                if tx
                                    .send(ImportMsg::Chunk(NewChunkBlob {
                                        hash,
                                        codec: actual_codec,
                                        raw_len,
                                        stored,
                                        _permit: permit,
                                    }))
                                    .is_err()
                                {
                                    return Err(Error::Other("import committer disconnected".into()));
                                }
                            }
                            Ok(())
                        };

                        let actual_len = if job.size <= IN_MEMORY_IMPORT_LIMIT {
                            file_buf.clear();
                            file.read_to_end(&mut file_buf)?;
                            let len = file_buf.len() as u64;
                            if len > 0 {
                                let mut chunk_err: Option<Error> = None;
                                chunk_bytes(&file_buf, |c| {
                                    if chunk_err.is_none() {
                                        if let Err(e) = handle_chunk(c) {
                                            chunk_err = Some(e);
                                        }
                                    }
                                });
                                if let Some(e) = chunk_err {
                                    return Err(e);
                                }
                            }
                            if file_buf.capacity() > IN_MEMORY_IMPORT_LIMIT as usize {
                                file_buf.shrink_to(IN_MEMORY_IMPORT_LIMIT as usize);
                            }
                            len
                        } else {
                            chunk_reader(&mut file, handle_chunk)?
                        };
                        Ok((actual_len, entries))
                    })();

                    match prep_res {
                        Ok((actual_len, entries)) => {
                            let _ = tx.send(ImportMsg::File(PreparedFile {
                                parent_id: job.parent_id,
                                inode_id: job.inode_id,
                                display_name: job.display_name,
                                folded_name: job.folded_name,
                                size: actual_len,
                                attributes: job.attributes,
                                created_ns: job.created_ns,
                                written_ns: job.written_ns,
                                storage_mode: STORAGE_CHUNKS,
                                pinned_overlay: false,
                                replacing_mode: job.replacing_mode,
                                entries,
                            }));
                        }
                        Err(err) => {
                            if budget_w.is_cancelled() {
                                break;
                            }
                            let n = errs.fetch_add(1, Ordering::Relaxed);
                            if n < 50 {
                                eprintln!("  [import error] read {}: {err}", job.path.display());
                            }
                        }
                    }
                }
            }));
        }
        drop(prep_tx);

        // Spawn single sequential pack-writer & SQLite metadata committer thread.
        let store_c = Arc::clone(self);
        let known_c = Arc::clone(&lazy_known);
        let budget_c = Arc::clone(&budget);
        let f_c = Arc::clone(&files_done);
        let upd_c = Arc::clone(&updated_done);
        let l_c = Arc::clone(&logical_done);
        let u_c = Arc::clone(&unique_done);
        let uc_c = Arc::clone(&unique_chunks_done);
        let s_c = Arc::clone(&stored_done);
        let p_c = Arc::clone(&pinned_done);
        let committer = std::thread::spawn(move || -> Result<()> {
            // Ensure ANY exit from committer (including `?` I/O errors or panics) immediately
            // cancels `ByteBudget` and wakes all blocked workers so `import_dir` never hangs!
            let _cancel_on_exit = CancelOnExit(Arc::clone(&budget_c));
            let mut batch: Vec<PreparedFile> = Vec::with_capacity(256);
            let mut pending_written_locs: HashMap<Hash32, ChunkLoc> = HashMap::new();
            let mut batch_pack_bytes: u64 = 0;

            let flush_batch = |batch: &mut Vec<PreparedFile>,
                               pending_written_locs: &mut HashMap<Hash32, ChunkLoc>,
                               batch_pack_bytes: &mut u64|
             -> Result<()> {
                if batch.is_empty() {
                    return Ok(());
                }
                let known_opt = known_c.lock().unwrap().clone();
                let pack_updates = {
                    let mut pw = store_c.packs.writer();
                    pw.flush()?;
                    pw.take_pack_updates()
                };

                // Batch-commit chunks, pack sizes, inodes, dentries, and durable
                // `pending_chunk_releases` in ONE atomic SQLite transaction!
                let mut overlays_to_remove: Vec<u64> = Vec::new();
                let mut committed_hashes: Vec<Hash32> = Vec::new();
                {
                    let mut conn = store_c.meta.lock().unwrap();
                    let tx = conn.transaction()?;
                    upsert_pack_updates(&tx, &pack_updates)?;

                    // Collect old chunk refs from modified files being replaced in place.
                    let mut old_refs: HashMap<Hash32, u64> = HashMap::new();
                    for item in batch.iter() {
                        if let Some(old_mode) = item.replacing_mode {
                            if old_mode == STORAGE_CHUNKS {
                                if let Some(old_row) = load_inode(&tx, ws_id, item.inode_id)? {
                                    if !old_row.manifest.is_empty() {
                                        for e in decode_manifest(&old_row.manifest)? {
                                            *old_refs.entry(e.hash).or_insert(0) += 1;
                                        }
                                    }
                                }
                            } else if old_mode == STORAGE_OVERLAY
                                && item.storage_mode == STORAGE_CHUNKS
                            {
                                overlays_to_remove.push(item.inode_id);
                            }
                        }
                    }

                    // Aggregate new refcount additions directly from `item.entries`.
                    let mut new_refs: HashMap<Hash32, u64> = HashMap::new();
                    for item in batch.iter() {
                        if item.storage_mode == STORAGE_CHUNKS {
                            for e in &item.entries {
                                *new_refs.entry(e.hash).or_insert(0) += 1;
                            }
                        }
                    }

                    // Compute NET refcount deltas: reused chunks cancel out before touching SQLite,
                    // and any net decrements are recorded DURABLY in `pending_chunk_releases` inside
                    // this same transaction so crash/failure recovery is always exact!
                    for (hash, mut add) in new_refs {
                        if let Some(old_cnt) = old_refs.get_mut(&hash) {
                            let cancel = add.min(*old_cnt);
                            add -= cancel;
                            *old_cnt -= cancel;
                        }
                        if add > 0 {
                            let cancelled = cancel_pending_chunk_release(&tx, &hash, add)?;
                            add -= cancelled;
                        }
                        if add > 0 {
                            let loc = pending_written_locs.remove(&hash);
                            if loc.is_some() {
                                committed_hashes.push(hash);
                            }
                            upsert_chunk_ref(&tx, &hash, loc, add)?;
                        }
                    }
                    for (hash, dec) in old_refs {
                        if dec > 0 {
                            enqueue_pending_chunk_release(&tx, &hash, dec)?;
                        }
                    }

                    {
                        let mut ins_inode = tx.prepare_cached(
                            "INSERT OR REPLACE INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                        )?;
                        let mut ins_dentry = tx.prepare_cached(
                            "INSERT OR REPLACE INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        )?;
                        for item in batch.iter() {
                            let dirty_since = if item.storage_mode == STORAGE_OVERLAY {
                                item.written_ns
                            } else {
                                0
                            };
                            let manifest_blob = if item.entries.is_empty() {
                                Vec::new()
                            } else {
                                encode_manifest(&item.entries)
                            };
                            ins_inode.execute(params![
                                ws_id as i64,
                                item.inode_id as i64,
                                i64::from(KIND_FILE),
                                item.size as i64,
                                i64::from(item.attributes),
                                item.created_ns,
                                item.written_ns,
                                item.written_ns,
                                item.written_ns,
                                i64::from(item.storage_mode),
                                manifest_blob.as_slice(),
                                dirty_since,
                                i64::from(item.pinned_overlay),
                            ])?;
                            ins_dentry.execute(params![
                                ws_id as i64,
                                item.parent_id as i64,
                                item.folded_name.as_slice(),
                                item.display_name.as_slice(),
                                item.inode_id as i64,
                                i64::from(KIND_FILE),
                            ])?;
                            f_c.fetch_add(1, Ordering::Relaxed);
                            if item.replacing_mode.is_some() {
                                upd_c.fetch_add(1, Ordering::Relaxed);
                            }
                            l_c.fetch_add(item.size, Ordering::Relaxed);
                            if item.pinned_overlay {
                                p_c.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    tx.commit()?;
                }
                // Publish committed chunk hashes to `KnownChunks` only AFTER SQLite commit succeeds.
                if !committed_hashes.is_empty() {
                    if let Some(known) = known_opt {
                        known.insert_batch(committed_hashes);
                    }
                }
                for old_id in overlays_to_remove {
                    let _ = store_c.overlay.remove_file(ws_id, old_id);
                }
                batch.clear();
                *batch_pack_bytes = 0;
                Ok(())
            };

            while let Ok(msg) = prep_rx.recv() {
                match msg {
                    ImportMsg::Chunk(chunk) => {
                        let known_opt = known_c.lock().unwrap().clone();
                        let already_present = pending_written_locs.contains_key(&chunk.hash)
                            || known_opt.as_ref().is_some_and(|k| k.contains(&chunk.hash));
                        if !already_present {
                            let loc = store_c.packs.writer().append(
                                &chunk.hash,
                                chunk.codec,
                                chunk.raw_len,
                                &chunk.stored,
                            )?;
                            pending_written_locs.insert(chunk.hash, loc);
                            uc_c.fetch_add(1, Ordering::Relaxed);
                            u_c.fetch_add(u64::from(chunk.raw_len), Ordering::Relaxed);
                            s_c.fetch_add(chunk.stored.len() as u64, Ordering::Relaxed);
                            batch_pack_bytes =
                                batch_pack_bytes.saturating_add(chunk.stored.len() as u64);
                        }
                        // Dropping `chunk` automatically releases its RAII `BudgetPermit`.
                        drop(chunk);
                    }
                    ImportMsg::File(item) => {
                        batch.push(item);
                        if batch.len() >= IMPORT_COMMIT_BATCH
                            || batch_pack_bytes >= IMPORT_COMMIT_MAX_BYTES
                        {
                            flush_batch(
                                &mut batch,
                                &mut pending_written_locs,
                                &mut batch_pack_bytes,
                            )?;
                        }
                    }
                }
            }
            flush_batch(
                &mut batch,
                &mut pending_written_locs,
                &mut batch_pack_bytes,
            )?;

            // All workers have exited: apply and clear all durably queued `pending_chunk_releases`.
            store_c.replay_pending_releases_locked()?;
            Ok(())
        });

        type PendingDir = (u64, u64, Vec<u8>, Vec<u8>, i64, i64);
        // Walk source directory tree, batch-inserting directories and feeding file jobs to workers.
        let mut dir_stack: Vec<(PathBuf, u64, String)> =
            vec![(canon_src.to_path_buf(), ROOT_INODE, String::new())];
        let mut pending_dirs: Vec<PendingDir> = Vec::with_capacity(1024);

        let flush_dirs = |pending: &mut Vec<PendingDir>| -> Result<()> {
            if pending.is_empty() {
                return Ok(());
            }
            let mut conn = self.meta.lock().unwrap();
            let tx = conn.transaction()?;
            {
                let mut ins_inode = tx.prepare_cached(
                    "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
                     VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, ?5, ?5, ?6, X'', 0, 0)",
                )?;
                let mut ins_dentry = tx.prepare_cached(
                    "INSERT OR REPLACE INTO dentries (ws_id, parent_id, folded_name, display_name, child_id, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for &(parent_id, inode_id, ref disp, ref folded, c_ns, w_ns) in pending.iter() {
                    ins_inode.execute(params![
                        ws_id as i64,
                        inode_id as i64,
                        i64::from(KIND_DIR),
                        c_ns,
                        w_ns,
                        i64::from(STORAGE_CHUNKS),
                    ])?;
                    ins_dentry.execute(params![
                        ws_id as i64,
                        parent_id as i64,
                        folded.as_slice(),
                        disp.as_slice(),
                        inode_id as i64,
                        i64::from(KIND_DIR),
                    ])?;
                }
            }
            tx.commit()?;
            pending.clear();
            Ok(())
        };

        while let Some((dir_path, parent_id, rel_dir)) = dir_stack.pop() {
            if budget.is_cancelled() {
                break;
            }
            // In `--update` mode, load existing children of this directory once so unchanged files
            // are skipped with zero file opens and deleted entries are detected cleanly.
            let mut existing_children: HashMap<Vec<u8>, ExistingChild> = if is_update {
                flush_dirs(&mut pending_dirs)?;
                let conn = self.meta.lock().unwrap();
                let mut stmt = conn.prepare_cached(
                    "SELECT d.folded_name, d.display_name, d.child_id, d.kind, i.size, i.written_ns, i.attributes, i.storage_mode
                     FROM dentries d
                     JOIN inodes i ON i.ws_id = d.ws_id AND i.inode_id = d.child_id
                     WHERE d.ws_id = ?1 AND d.parent_id = ?2",
                )?;
                let rows = stmt.query_map(params![ws_id as i64, parent_id as i64], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        ExistingChild {
                            display_name: r.get(1)?,
                            inode_id: r.get::<_, i64>(2)? as u64,
                            kind: r.get::<_, i64>(3)? as u8,
                            size: r.get::<_, i64>(4)? as u64,
                            written_ns: r.get(5)?,
                            attributes: r.get::<_, i64>(6)? as u32,
                            storage_mode: r.get::<_, i64>(7)? as u8,
                        },
                    ))
                })?;
                let mut map = HashMap::new();
                for r in rows {
                    let (k, v) = r?;
                    map.insert(k, v);
                }
                map
            } else {
                HashMap::new()
            };

            let entries = match fs::read_dir(&dir_path) {
                Ok(rd) => rd,
                Err(err) => {
                    let n = errors_done.fetch_add(1, Ordering::Relaxed);
                    if n < 50 {
                        eprintln!("  [import error] read_dir {}: {err}", dir_path.display());
                    }
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        let n = errors_done.fetch_add(1, Ordering::Relaxed);
                        if n < 50 {
                            eprintln!(
                                "  [import error] read_dir entry in {}: {err}",
                                dir_path.display()
                            );
                        }
                        continue;
                    }
                };
                let child_path = entry.path();
                // On Windows, `entry.file_type()` and `entry.metadata()` reuse `WIN32_FIND_DATAW`
                // from `FindNextFileW` with zero extra `CreateFileW` syscalls!
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(err) => {
                        let n = errors_done.fetch_add(1, Ordering::Relaxed);
                        if n < 50 {
                            eprintln!(
                                "  [import error] file_type {}: {err}",
                                child_path.display()
                            );
                        }
                        continue;
                    }
                };
                if ft.is_symlink() {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(err) => {
                        let n = errors_done.fetch_add(1, Ordering::Relaxed);
                        if n < 50 {
                            eprintln!(
                                "  [import error] metadata {}: {err}",
                                child_path.display()
                            );
                        }
                        continue;
                    }
                };
                let name_os = entry.file_name();
                let Some(name_str) = name_os.to_str() else {
                    let n = errors_done.fetch_add(1, Ordering::Relaxed);
                    if n < 50 {
                        eprintln!(
                            "  [import error] non-UTF-8 filename: {}",
                            child_path.display()
                        );
                    }
                    continue;
                };
                let rel_path = if rel_dir.is_empty() {
                    name_str.to_owned()
                } else {
                    format!("{rel_dir}/{name_str}")
                };
                if !excludes.is_empty() && filter::matches_any(&rel_path, excludes) {
                    excluded_done.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let display_name = name_str.as_bytes().to_vec();
                let folded_name = fold_bytes(&display_name);
                let created_ns = meta
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or_else(now_ns, |d| d.as_nanos().min(i64::MAX as u128) as i64);
                let written_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or_else(now_ns, |d| d.as_nanos().min(i64::MAX as u128) as i64);

                let prev = if is_update {
                    existing_children.remove(&folded_name)
                } else {
                    None
                };

                if ft.is_dir() {
                    dirs_done.fetch_add(1, Ordering::Relaxed);
                    if let Some(p) = prev {
                        if p.kind == KIND_DIR {
                            if p.display_name != display_name {
                                let conn = self.meta.lock().unwrap();
                                conn.execute(
                                    "UPDATE dentries SET display_name = ?1 WHERE ws_id = ?2 AND parent_id = ?3 AND folded_name = ?4",
                                    params![display_name.as_slice(), ws_id as i64, parent_id as i64, folded_name.as_slice()],
                                )?;
                            }
                            dir_stack.push((child_path, p.inode_id, rel_path));
                            continue;
                        }
                        // Previously a file, now a directory: remove the old file first.
                        let mut ov = Vec::new();
                        {
                            let mut conn = self.meta.lock().unwrap();
                            let tx = conn.transaction()?;
                            let n = Self::delete_subtree(
                                &tx,
                                ws_id,
                                parent_id,
                                &folded_name,
                                p.inode_id,
                                p.kind,
                                &mut ov,
                            )?;
                            removed_done.fetch_add(n, Ordering::Relaxed);
                            tx.commit()?;
                        }
                        for id in ov {
                            let _ = self.overlay.remove_file(ws_id, id);
                        }
                    }

                    let inode_id = next_inode.fetch_add(1, Ordering::Relaxed);
                    pending_dirs.push((
                        parent_id,
                        inode_id,
                        display_name,
                        folded_name,
                        created_ns,
                        written_ns,
                    ));
                    if pending_dirs.len() >= 1024 {
                        flush_dirs(&mut pending_dirs)?;
                    }
                    dir_stack.push((child_path, inode_id, rel_path));
                } else if ft.is_file() {
                    let attrs = if meta.permissions().readonly() {
                        0x01u32
                    } else {
                        0u32
                    };
                    let pinned = is_hot_db_name(name_str)
                        || (!self.config.no_dedup.is_empty()
                            && filter::matches_any(&rel_path, &self.config.no_dedup));
                    let mut replacing_mode = None;
                    let inode_id = if let Some(p) = prev {
                        if p.kind == KIND_FILE {
                            if p.size == meta.len()
                                && p.written_ns == written_ns
                                && p.attributes == attrs
                            {
                                // Unchanged file: skip open/read/hash completely!
                                if p.display_name != display_name {
                                    let conn = self.meta.lock().unwrap();
                                    conn.execute(
                                        "UPDATE dentries SET display_name = ?1 WHERE ws_id = ?2 AND parent_id = ?3 AND folded_name = ?4",
                                        params![display_name.as_slice(), ws_id as i64, parent_id as i64, folded_name.as_slice()],
                                    )?;
                                }
                                files_done.fetch_add(1, Ordering::Relaxed);
                                unchanged_done.fetch_add(1, Ordering::Relaxed);
                                logical_done.fetch_add(p.size, Ordering::Relaxed);
                                continue;
                            }
                            replacing_mode = Some(p.storage_mode);
                            p.inode_id
                        } else {
                            // Previously a directory, now a file: recursively remove old dir.
                            let mut ov = Vec::new();
                            {
                                let mut conn = self.meta.lock().unwrap();
                                let tx = conn.transaction()?;
                                let n = Self::delete_subtree(
                                    &tx,
                                    ws_id,
                                    parent_id,
                                    &folded_name,
                                    p.inode_id,
                                    p.kind,
                                    &mut ov,
                                )?;
                                removed_done.fetch_add(n, Ordering::Relaxed);
                                tx.commit()?;
                            }
                            for id in ov {
                                let _ = self.overlay.remove_file(ws_id, id);
                            }
                            next_inode.fetch_add(1, Ordering::Relaxed)
                        }
                    } else {
                        next_inode.fetch_add(1, Ordering::Relaxed)
                    };

                    if budget.is_cancelled() {
                        break;
                    }
                    if job_tx
                        .send(ImportFileJob {
                            parent_id,
                            inode_id,
                            display_name,
                            folded_name,
                            path: child_path,
                            size: meta.len(),
                            attributes: attrs,
                            created_ns,
                            written_ns,
                            pinned_overlay: pinned,
                            replacing_mode,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }

            // In `--update` mode, remove any children that no longer exist on the host.
            if is_update && !budget.is_cancelled() && !existing_children.is_empty() {
                let mut ov = Vec::new();
                {
                    let mut conn = self.meta.lock().unwrap();
                    let tx = conn.transaction()?;
                    for (folded, child) in existing_children {
                        let n = Self::delete_subtree(
                            &tx,
                            ws_id,
                            parent_id,
                            &folded,
                            child.inode_id,
                            child.kind,
                            &mut ov,
                        )?;
                        removed_done.fetch_add(n, Ordering::Relaxed);
                    }
                    tx.commit()?;
                }
                for id in ov {
                    let _ = self.overlay.remove_file(ws_id, id);
                }
            }
        }
        if !budget.is_cancelled() {
            if let Err(err) = flush_dirs(&mut pending_dirs) {
                budget.cancel();
                drop(job_tx);
                for w in workers {
                    let _ = w.join();
                }
                let _ = committer.join();
                stop_progress.store(true, Ordering::Relaxed);
                if let Some(h) = progress_handle {
                    let _ = h.join();
                }
                return Err(err);
            }
        }
        drop(job_tx);

        for w in workers {
            w.join()
                .map_err(|_| Error::Other("import worker panicked".into()))?;
        }
        let commit_res = committer
            .join()
            .map_err(|_| Error::Other("import committer panicked".into()))?;
        stop_progress.store(true, Ordering::Relaxed);
        if let Some(h) = progress_handle {
            let _ = h.join();
        }
        commit_res?;

        // Update workspace's `next_inode` counter to the final allocated value.
        let final_next = next_inode.load(Ordering::Relaxed);
        {
            let conn = self.meta.lock().unwrap();
            conn.execute(
                "UPDATE workspaces SET next_inode = MAX(next_inode, ?1) WHERE ws_id = ?2",
                params![final_next as i64, ws_id as i64],
            )?;
        }

        Ok(ImportSummary {
            workspace: String::new(),
            ws_id,
            files: files_done.load(Ordering::Relaxed),
            unchanged_files: unchanged_done.load(Ordering::Relaxed),
            updated_files: updated_done.load(Ordering::Relaxed),
            removed_entries: removed_done.load(Ordering::Relaxed),
            excluded_entries: excluded_done.load(Ordering::Relaxed),
            dirs: dirs_done.load(Ordering::Relaxed),
            logical_bytes: logical_done.load(Ordering::Relaxed),
            new_unique_chunks: unique_chunks_done.load(Ordering::Relaxed),
            new_unique_bytes: unique_done.load(Ordering::Relaxed),
            stored_bytes: stored_done.load(Ordering::Relaxed),
            overlay_pinned_files: pinned_done.load(Ordering::Relaxed),
            errors: errors_done.load(Ordering::Relaxed),
            elapsed_secs: 0.0,
        })
    }

    /// Deduplicates idle, unpinned overlay files in `ws_name` (or all workspaces if `None`)
    /// into the shared `.pack` CAS pool, deleting the local loose overlay files to reclaim space.
    ///
    /// `is_busy(ws_id, inode_id)` should return `true` when any live WinFsp handle is currently
    /// open on that inode.
    pub fn dedup_workspace<F>(
        &self,
        ws_name: Option<&str>,
        idle_secs: u64,
        is_busy: F,
    ) -> Result<DedupSummary>
    where
        F: Fn(u64, u64) -> bool,
    {
        let cutoff_ns = now_ns().saturating_sub((idle_secs as i64).saturating_mul(1_000_000_000));
        let candidates: Vec<(u64, u64, u64, i64)> = {
            let conn = self.meta.lock().unwrap();
            let ws_filter = match ws_name {
                Some(name) => Some(find_workspace_id(&conn, name)?),
                None => None,
            };
            let mut out = Vec::new();
            if let Some(ws_id) = ws_filter {
                let mut stmt = conn.prepare(
                    "SELECT ws_id, inode_id, size, written_ns FROM inodes
                     WHERE ws_id = ?1 AND kind = ?2 AND storage_mode = ?3 AND pinned_overlay = 0 AND dirty_since_ns <= ?4",
                )?;
                let rows = stmt.query_map(
                    params![
                        ws_id as i64,
                        i64::from(KIND_FILE),
                        i64::from(STORAGE_OVERLAY),
                        cutoff_ns
                    ],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? as u64,
                            r.get::<_, i64>(1)? as u64,
                            r.get::<_, i64>(2)? as u64,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )?;
                for row in rows {
                    out.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT ws_id, inode_id, size, written_ns FROM inodes
                     WHERE kind = ?1 AND storage_mode = ?2 AND pinned_overlay = 0 AND dirty_since_ns <= ?3",
                )?;
                let rows = stmt.query_map(
                    params![
                        i64::from(KIND_FILE),
                        i64::from(STORAGE_OVERLAY),
                        cutoff_ns
                    ],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? as u64,
                            r.get::<_, i64>(1)? as u64,
                            r.get::<_, i64>(2)? as u64,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )?;
                for row in rows {
                    out.push(row?);
                }
            }
            out
        };

        let mut summary = DedupSummary {
            scanned: candidates.len() as u64,
            ..Default::default()
        };
        if candidates.is_empty() {
            return Ok(summary);
        }

        let _pack_guard = self.acquire_pack_write_lock()?;

        // Open a dedicated read-only connection so path resolution and chunk existence checks
        // never contend with `self.meta` write locks during background dedup.
        let ro_conn = self.open_readonly_conn()?;

        // Reconstruct relative path for each candidate when `no_dedup` directory rules are configured.
        // Uses `idx_dentries_child` index + `prepare_cached` (O(depth * log N) instead of full scan!).
        let resolve_rel_path = |ws_id: u64, mut node_id: u64| -> Option<String> {
            let mut stmt = ro_conn
                .prepare_cached(
                    "SELECT parent_id, display_name FROM dentries WHERE ws_id = ?1 AND child_id = ?2 LIMIT 1",
                )
                .ok()?;
            let mut parts: Vec<String> = Vec::new();
            let mut depth = 0usize;
            while node_id != ROOT_INODE && depth < 512 {
                let row: Option<(i64, Vec<u8>)> = stmt
                    .query_row(params![ws_id as i64, node_id as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()
                    .ok()
                    .flatten();
                let (parent_id, name_bytes) = row?;
                parts.push(String::from_utf8_lossy(&name_bytes).into_owned());
                node_id = parent_id as u64;
                depth += 1;
            }
            parts.reverse();
            Some(parts.join("/"))
        };

        // Persists any speculative pack spills from a skipped/aborted dedup candidate into
        // `packs` with their exact `dead_bytes` so `gc()` can always reclaim them cleanly.
        let record_aborted_spills = |written_locs: &HashMap<Hash32, ChunkLoc>,
                                     pack_updates: &mut Vec<(u64, u64, bool)>|
         -> Result<()> {
            if written_locs.is_empty() && pack_updates.is_empty() {
                return Ok(());
            }
            {
                let mut pw = self.packs.writer();
                pw.flush()?;
                pack_updates.extend(pw.take_pack_updates());
            }
            let mut dead_by_pack: HashMap<u64, u64> = HashMap::new();
            for loc in written_locs.values() {
                let rec = RECORD_HEADER_BYTES as u64 + u64::from(loc.stored_len);
                *dead_by_pack.entry(loc.pack_id).or_insert(0) += rec;
            }
            let mut conn = self.meta.lock().unwrap();
            let tx = conn.transaction()?;
            upsert_pack_updates(&tx, pack_updates)?;
            {
                let mut add_dead = tx.prepare_cached(
                    "UPDATE packs SET dead_bytes = dead_bytes + ?1 WHERE pack_id = ?2",
                )?;
                for (pid, dead) in dead_by_pack {
                    add_dead.execute(params![dead as i64, pid as i64])?;
                }
            }
            tx.commit()?;
            pack_updates.clear();
            Ok(())
        };

        for (ws_id, inode_id, expected_size, expected_written_ns) in candidates {
            if !self.config.no_dedup.is_empty() {
                if let Some(rel) = resolve_rel_path(ws_id, inode_id) {
                    if filter::matches_any(&rel, &self.config.no_dedup) {
                        summary.skipped_filtered += 1;
                        continue;
                    }
                }
            }
            if is_busy(ws_id, inode_id) {
                summary.skipped_busy += 1;
                continue;
            }
            let loose_path = self.overlay.file_path(ws_id, inode_id);
            let mut entries = Vec::new();
            let mut entry_first_span: HashMap<Hash32, (u64, u32)> = HashMap::new();
            let mut new_blobs: Vec<(Hash32, Codec, u32, Vec<u8>)> = Vec::new();
            let mut new_blobs_bytes: usize = 0;
            let mut written_locs: HashMap<Hash32, ChunkLoc> = HashMap::new();
            let mut pack_updates: Vec<(u64, u64, bool)> = Vec::new();
            let mut added_unique = 0u64;
            let mut added_stored = 0u64;
            let actual_len: u64;

            if expected_size == 0 || !loose_path.exists() {
                actual_len = 0;
            } else {
                let Ok(mut file) = File::open(&loose_path) else {
                    summary.skipped_changed += 1;
                    continue;
                };
                // Chunk & hash outside both inode and meta write locks so foreground ops never stall.
                // Check authoritative SQLite `get_chunk_loc` (NEVER `chunk_loc_cache`) and spill
                // buffered compressed chunks to `PackWriter` at `DEDUP_MAX_BUFFER_BYTES` (4 MiB)
                // so deduping multi-GiB files stays strictly memory-bounded.
                let mut chunk_hashes_seen: HashSet<Hash32> = HashSet::new();
                let mut file_cursor = 0u64;
                let read_res = chunk_reader(&mut file, |chunk| {
                    let hash = blake3_hash(chunk);
                    let raw_len = chunk.len() as u32;
                    entries.push(ManifestEntry { hash, raw_len });
                    entry_first_span
                        .entry(hash)
                        .or_insert((file_cursor, raw_len));
                    file_cursor += u64::from(raw_len);
                    if chunk_hashes_seen.insert(hash) && get_chunk_loc(&ro_conn, &hash)?.is_none() {
                        let (c, stored) = compress_chunk(self.config.codec, chunk)?;
                        new_blobs_bytes = new_blobs_bytes.saturating_add(stored.len());
                        new_blobs.push((hash, c, raw_len, stored));
                        if new_blobs_bytes >= DEDUP_MAX_BUFFER_BYTES {
                            let mut pw = self.packs.writer();
                            for (h, codec, rlen, bytes) in new_blobs.drain(..) {
                                let loc = pw.append(&h, codec, rlen, &bytes)?;
                                written_locs.insert(h, loc);
                                added_unique += u64::from(rlen);
                                added_stored += bytes.len() as u64;
                            }
                            pw.flush()?;
                            pack_updates.extend(pw.take_pack_updates());
                            new_blobs_bytes = 0;
                        }
                    }
                    Ok(())
                });
                match read_res {
                    Ok(len) => actual_len = len,
                    Err(_) => {
                        record_aborted_spills(&written_locs, &mut pack_updates)?;
                        summary.skipped_changed += 1;
                        continue;
                    }
                }
            }

            // Acquire per-inode stripe lock for atomic check-and-swap.
            let _guard = self.lock_inode(ws_id, inode_id);
            if is_busy(ws_id, inode_id) {
                record_aborted_spills(&written_locs, &mut pack_updates)?;
                summary.skipped_busy += 1;
                continue;
            }

            // Verify inode metadata hasn't changed while we were chunking.
            let current = {
                let conn = self.meta.lock().unwrap();
                load_inode(&conn, ws_id, inode_id)?
            };
            let Some(cur) = current else {
                record_aborted_spills(&written_locs, &mut pack_updates)?;
                summary.skipped_changed += 1;
                continue;
            };
            if cur.storage_mode != STORAGE_OVERLAY
                || cur.pinned_overlay
                || cur.size != expected_size
                || cur.written_ns != expected_written_ns
                || cur.size != actual_len
            {
                record_aborted_spills(&written_locs, &mut pack_updates)?;
                summary.skipped_changed += 1;
                continue;
            }

            // Append any remaining buffered new unique chunks to pack store.
            if !new_blobs.is_empty() {
                let mut pw = self.packs.writer();
                for (hash, codec, raw_len, stored) in new_blobs.drain(..) {
                    let loc = pw.append(&hash, codec, raw_len, &stored)?;
                    written_locs.insert(hash, loc);
                    added_unique += u64::from(raw_len);
                    added_stored += stored.len() as u64;
                }
                pw.flush()?;
                pack_updates.extend(pw.take_pack_updates());
            }

            // Commit chunk refcounts + flip inode from Overlay -> Chunks atomically.
            // Re-verify inside the write transaction that any skipped chunk still exists in
            // SQLite `chunks`; if another thread concurrently deleted its last ref, re-read its
            // slice from the still-locked overlay file and write it to pack before inserting!
            let manifest_blob = encode_manifest(&entries);
            {
                let mut conn = self.meta.lock().unwrap();
                let tx = conn.transaction()?;
                let mut ref_adds: HashMap<Hash32, u64> = HashMap::new();
                for e in &entries {
                    *ref_adds.entry(e.hash).or_insert(0) += 1;
                }

                // Check for any chunk concurrently deleted between phase 1 and commit tx.
                let mut missing_hashes: Vec<Hash32> = Vec::new();
                for hash in ref_adds.keys() {
                    if !written_locs.contains_key(hash) && get_chunk_loc(&tx, hash)?.is_none() {
                        missing_hashes.push(*hash);
                    }
                }
                if !missing_hashes.is_empty() {
                    let mut ov_file = File::open(&loose_path)?;
                    let mut pw = self.packs.writer();
                    for hash in missing_hashes {
                        let (off, rlen) = entry_first_span[&hash];
                        let mut raw = vec![0u8; rlen as usize];
                        crate::pack::read_exact_at(&ov_file, off, &mut raw)?;
                        let (codec, stored) = compress_chunk(self.config.codec, &raw)?;
                        let loc = pw.append(&hash, codec, rlen, &stored)?;
                        written_locs.insert(hash, loc);
                        added_unique += u64::from(rlen);
                        added_stored += stored.len() as u64;
                    }
                    pw.flush()?;
                    pack_updates.extend(pw.take_pack_updates());
                    let _ = &mut ov_file;
                }

                upsert_pack_updates(&tx, &pack_updates)?;
                for (hash, add) in ref_adds {
                    let loc = written_locs.remove(&hash);
                    upsert_chunk_ref(&tx, &hash, loc, add)?;
                }
                tx.execute(
                    "UPDATE inodes SET storage_mode = ?1, manifest = ?2, dirty_since_ns = 0
                     WHERE ws_id = ?3 AND inode_id = ?4",
                    params![
                        i64::from(STORAGE_CHUNKS),
                        manifest_blob.as_slice(),
                        ws_id as i64,
                        inode_id as i64
                    ],
                )?;
                tx.commit()?;
            }

            // Delete loose overlay file to immediately free host disk space, and evict cached inode!
            let _ = self.overlay.remove_file(ws_id, inode_id);
            self.invalidate_inode(ws_id, inode_id);

            summary.deduped_files += 1;
            summary.raw_bytes += actual_len;
            summary.new_unique_bytes += added_unique;
            summary.stored_bytes += added_stored;
            summary.reclaimed_overlay_bytes += actual_len;
        }

        Ok(summary)
    }

    /// Reclaims dead space in `.pack` files:
    /// - Packs with 0 live bytes are deleted immediately with zero rewrite.
    /// - Sealed packs whose `dead_bytes / size_bytes >= dead_threshold_pct%` are compacted into
    ///   new pack(s) without re-compressing, then the old pack file is deleted.
    pub fn gc(&self, dead_threshold_pct: u8) -> Result<GcSummary> {
        let _pack_guard = self.acquire_pack_write_lock()?;
        // Apply any pending deferred releases before scanning live/dead pack accounting.
        self.replay_pending_releases_locked()?;
        // Seal any active writer pack first so pack boundaries stay clean.
        {
            let mut pw = self.packs.writer();
            pw.seal_active()?;
            let updates = pw.take_pack_updates();
            if !updates.is_empty() {
                let mut conn = self.meta.lock().unwrap();
                let tx = conn.transaction()?;
                upsert_pack_updates(&tx, &updates)?;
                tx.commit()?;
            }
        }

        let pack_rows: Vec<(u64, u64, u64, u64)> = {
            let conn = self.meta.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT pack_id, size_bytes, live_bytes, dead_bytes FROM packs ORDER BY pack_id ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        let mut summary = GcSummary::default();
        for (pack_id, size_bytes, live_bytes, dead_bytes) in pack_rows {
            if live_bytes == 0 {
                // Entire pack is dead — delete immediately.
                let conn = self.meta.lock().unwrap();
                let live_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE pack_id = ?1",
                    params![pack_id as i64],
                    |r| r.get(0),
                )?;
                if live_count == 0 {
                    conn.execute("DELETE FROM packs WHERE pack_id = ?1", params![pack_id as i64])?;
                    drop(conn);
                    self.packs.delete_pack_file(pack_id)?;
                    summary.deleted_empty_packs += 1;
                    summary.reclaimed_disk_bytes += size_bytes;
                    continue;
                }
            }

            if dead_bytes == 0 || size_bytes == 0 {
                continue;
            }
            let dead_pct = (dead_bytes.saturating_mul(100)) / size_bytes;
            if dead_pct < u64::from(dead_threshold_pct) {
                continue;
            }

            // Compact live chunks out of `pack_id` into fresh pack(s) without re-compressing.
            let live_chunks: Vec<(Hash32, ChunkLoc)> = {
                let conn = self.meta.lock().unwrap();
                let mut stmt = conn.prepare(
                    "SELECT hash, offset, raw_len, stored_len, codec FROM chunks WHERE pack_id = ?1 ORDER BY offset ASC",
                )?;
                let rows = stmt.query_map(params![pack_id as i64], |r| {
                    let h: Vec<u8> = r.get(0)?;
                    let hash: Hash32 = h.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((
                        hash,
                        ChunkLoc {
                            pack_id,
                            offset: r.get::<_, i64>(1)? as u64,
                            raw_len: r.get::<_, i64>(2)? as u32,
                            stored_len: r.get::<_, i64>(3)? as u32,
                            codec: Codec::from_u8(r.get::<_, i64>(4)? as u8)
                                .unwrap_or(Codec::None),
                        },
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };

            let mut moved = Vec::with_capacity(live_chunks.len());
            let mut pack_updates = Vec::new();
            {
                let mut pw = self.packs.writer();
                for (hash, old_loc) in &live_chunks {
                    let payload = self.packs.read_stored_payload(hash, *old_loc)?;
                    let new_loc = pw.append(hash, old_loc.codec, old_loc.raw_len, &payload)?;
                    moved.push((*hash, new_loc));
                }
                pw.flush()?;
                pack_updates.extend(pw.take_pack_updates());
            }

            {
                let mut conn = self.meta.lock().unwrap();
                let tx = conn.transaction()?;
                upsert_pack_updates(&tx, &pack_updates)?;
                {
                    let mut upd = tx.prepare_cached(
                        "UPDATE chunks SET pack_id = ?1, offset = ?2 WHERE hash = ?3",
                    )?;
                    let mut add_live = tx.prepare_cached(
                        "UPDATE packs SET live_bytes = live_bytes + ?1 WHERE pack_id = ?2",
                    )?;
                    for (hash, new_loc) in &moved {
                        upd.execute(params![
                            new_loc.pack_id as i64,
                            new_loc.offset as i64,
                            hash.as_slice()
                        ])?;
                        let rec_bytes = RECORD_HEADER_BYTES as i64 + i64::from(new_loc.stored_len);
                        add_live.execute(params![rec_bytes, new_loc.pack_id as i64])?;
                    }
                }
                tx.execute("DELETE FROM packs WHERE pack_id = ?1", params![pack_id as i64])?;
                tx.commit()?;
            }

            self.packs.delete_pack_file(pack_id)?;
            self.clear_chunk_loc_cache();
            summary.compacted_packs += 1;
            summary.moved_chunks += moved.len() as u64;
            summary.reclaimed_disk_bytes += dead_bytes;
        }

        // Sweep any untracked orphan `.pack` files on disk (e.g. from a hard crash mid-write
        // before SQLite commit) while holding the exclusive `pack.lock`.
        {
            let tracked_ids: HashSet<u64> = {
                let conn = self.meta.lock().unwrap();
                let mut stmt = conn.prepare("SELECT pack_id FROM packs")?;
                let rows = stmt.query_map([], |r| Ok(r.get::<_, i64>(0)? as u64))?;
                let mut set = HashSet::new();
                for r in rows {
                    set.insert(r?);
                }
                set
            };
            if let Ok(rd) = fs::read_dir(self.root.join("packs")) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("pack") {
                        continue;
                    }
                    if let Some(pid) = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        if !tracked_ids.contains(&pid) {
                            let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                            if self.packs.delete_pack_file(pid).is_ok() {
                                summary.deleted_empty_packs += 1;
                                summary.reclaimed_disk_bytes += sz;
                            }
                        }
                    }
                }
            }
        }

        // Flush WAL checkpoint so meta.sqlite stays tidy after GC.
        {
            let conn = self.meta.lock().unwrap();
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }

        Ok(summary)
    }

    /// Audits chunk reference counts and pack live/dead byte accounting across all workspaces
    /// inside a single consistent SQLite transaction.
    /// When `fix == true`, refuses to run if a live mount is active on this store, acquires an
    /// `IMMEDIATE` write transaction across scan + repair so no concurrent writer can interleave,
    /// repairs mismatched `chunks.refcount`, deletes orphan chunks (0 live refs), and recomputes
    /// exact `packs.live_bytes` / `packs.dead_bytes`.
    pub fn fsck(&self, fix: bool) -> Result<FsckSummary> {
        let _pack_guard = if fix {
            if let Some(mp) = crate::mount::active_mount_for_store(&self.root) {
                return Err(Error::Other(format!(
                    "refusing `fsck --fix` while store is actively mounted at {mp}; unmount first"
                )));
            }
            let guard = self.acquire_pack_write_lock()?;
            self.replay_pending_releases_locked()?;
            Some(guard)
        } else {
            None
        };
        let mut conn = self.meta.lock().unwrap();
        let behavior = if fix {
            rusqlite::TransactionBehavior::Immediate
        } else {
            rusqlite::TransactionBehavior::Deferred
        };
        let tx = conn.transaction_with_behavior(behavior)?;
        let mut summary = FsckSummary {
            fixed: fix,
            ..FsckSummary::default()
        };

        // 1. Scan all chunk-backed file manifests within one consistent transaction snapshot.
        let mut actual_refs: HashMap<Hash32, u64> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT manifest FROM inodes WHERE kind = ?1 AND storage_mode = ?2 AND length(manifest) > 0",
            )?;
            let rows = stmt.query_map(
                params![i64::from(KIND_FILE), i64::from(STORAGE_CHUNKS)],
                |r| r.get::<_, Vec<u8>>(0),
            )?;
            for row in rows {
                let blob = row?;
                summary.files_scanned += 1;
                for entry in decode_manifest(&blob)? {
                    *actual_refs.entry(entry.hash).or_insert(0) += 1;
                    summary.chunk_refs_counted += 1;
                }
            }
        }

        // 2. Compare `chunks` table rows against `actual_refs` and recompute per-pack live bytes.
        let mut ref_fixes: Vec<(Hash32, u64)> = Vec::new();
        let mut orphans_to_delete: Vec<Hash32> = Vec::new();
        let mut pack_live: HashMap<u64, u64> = HashMap::new();
        let mut present_hashes: HashSet<Hash32> = HashSet::new();
        {
            let mut stmt =
                tx.prepare("SELECT hash, pack_id, stored_len, refcount FROM chunks")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?;
            for row in rows {
                let (h_bytes, pack_id, stored_len, db_refs) = row?;
                if h_bytes.len() != 32 {
                    continue;
                }
                let hash: Hash32 = h_bytes.try_into().unwrap();
                summary.total_chunks += 1;
                present_hashes.insert(hash);
                let real_refs = actual_refs.get(&hash).copied().unwrap_or(0);
                if real_refs == 0 {
                    summary.orphan_chunks += 1;
                    orphans_to_delete.push(hash);
                } else {
                    let rec_bytes = RECORD_HEADER_BYTES as u64 + stored_len;
                    *pack_live.entry(pack_id).or_insert(0) += rec_bytes;
                    if real_refs != db_refs {
                        summary.mismatched_refcounts += 1;
                        ref_fixes.push((hash, real_refs));
                    }
                }
            }
        }

        for hash in actual_refs.keys() {
            if !present_hashes.contains(hash) {
                summary.missing_chunks += 1;
            }
        }

        // 3. Compare `packs` live/dead accounting against recomputed `pack_live`.
        let mut pack_fixes: Vec<(u64, u64, u64)> = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT pack_id, size_bytes, live_bytes, dead_bytes FROM packs")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                ))
            })?;
            for row in rows {
                let (pack_id, size_bytes, db_live, db_dead) = row?;
                let real_live = pack_live.get(&pack_id).copied().unwrap_or(0);
                let data_bytes = size_bytes.saturating_sub(crate::pack::PACK_HEADER_BYTES);
                let real_dead = data_bytes.saturating_sub(real_live);
                if real_live != db_live || real_dead != db_dead {
                    summary.pack_accounting_fixed += 1;
                    pack_fixes.push((pack_id, real_live, real_dead));
                }
            }
        }

        let mutated = fix
            && (!ref_fixes.is_empty() || !orphans_to_delete.is_empty() || !pack_fixes.is_empty());
        if mutated {
            {
                let mut upd =
                    tx.prepare_cached("UPDATE chunks SET refcount = ?1 WHERE hash = ?2")?;
                for (hash, refs) in &ref_fixes {
                    upd.execute(params![*refs as i64, hash.as_slice()])?;
                }
                let mut del = tx.prepare_cached("DELETE FROM chunks WHERE hash = ?1")?;
                for hash in &orphans_to_delete {
                    del.execute(params![hash.as_slice()])?;
                }
                let mut upd_pack = tx.prepare_cached(
                    "UPDATE packs SET live_bytes = ?1, dead_bytes = ?2 WHERE pack_id = ?3",
                )?;
                for &(pack_id, live, dead) in &pack_fixes {
                    upd_pack.execute(params![live as i64, dead as i64, pack_id as i64])?;
                }
            }
            tx.commit()?;
            self.clear_chunk_loc_cache();
        } else {
            tx.finish()?;
        }

        Ok(summary)
    }
}
