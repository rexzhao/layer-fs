//! High-level `DedupStore`: init, open, parallel import, instant CoW clone, background
//! cold-file deduplication, pack GC, and store statistics.

use crate::chunker::{blake3_hash, chunk_bytes, chunk_reader, compress_chunk, Codec, Hash32};
use crate::error::{Error, Result};
use crate::meta::{
    create_empty_workspace, decode_manifest, encode_manifest, find_workspace_id, get_chunk_loc,
    is_hot_db_name, load_inode, next_pack_id, now_ns, open_meta_connection,
    release_manifest_chunks, upsert_chunk_ref, upsert_pack_updates, InodeRow, ManifestEntry,
    StoreConfig, StoreStats, WorkspaceInfo, KIND_DIR, KIND_FILE, ROOT_INODE, STORAGE_CHUNKS,
    STORAGE_OVERLAY,
};
use crate::overlay::OverlayStore;
use crate::pack::{ChunkLoc, PackStore, DEFAULT_CACHE_BYTES, RECORD_HEADER_BYTES};
use fs_semantics::name::fold_bytes;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

const INODE_LOCK_STRIPES: usize = 1024;
const KNOWN_SHARDS: usize = 64;
const IN_MEMORY_IMPORT_LIMIT: u64 = 16 * 1024 * 1024;
const IMPORT_COMMIT_BATCH: usize = 2048;

/// Live progress snapshot reported during `import_dir`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportProgress {
    pub files: u64,
    pub dirs: u64,
    pub logical_bytes: u64,
    pub new_unique_bytes: u64,
    pub stored_bytes: u64,
    pub overlay_pinned_files: u64,
    pub errors: u64,
}

/// Summary returned by `import_dir`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ImportSummary {
    pub workspace: String,
    pub ws_id: u64,
    pub files: u64,
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

/// Sharded in-memory set of already-stored 32-byte chunk hashes for lock-free fast-path dedup
/// during parallel `import_dir`.
struct KnownChunks {
    shards: Vec<Mutex<HashSet<Hash32>>>,
}

impl KnownChunks {
    fn load(conn: &Connection) -> Result<Self> {
        let mut shards: Vec<Mutex<HashSet<Hash32>>> = (0..KNOWN_SHARDS)
            .map(|_| Mutex::new(HashSet::new()))
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

    /// Returns `true` if `hash` was NOT yet known (caller should compress & store it).
    #[inline]
    fn claim_if_new(&self, hash: Hash32) -> bool {
        let idx = (hash[0] as usize) & (KNOWN_SHARDS - 1);
        self.shards[idx].lock().unwrap().insert(hash)
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
    inode_locks: Vec<Mutex<()>>,
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
        // Resume the latest unsealed pack if it has room so small incremental dedups don't create tiny packs.
        if let Some((pid, sz)) = conn
            .query_row(
                "SELECT pack_id, size_bytes FROM packs WHERE sealed = 0 ORDER BY pack_id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?
        {
            if sz < config.pack_target_bytes {
                packs.writer().resume_pack(pid, sz)?;
            }
        }
        let overlay = OverlayStore::new(root.join("overlay"));
        let inode_locks = (0..INODE_LOCK_STRIPES).map(|_| Mutex::new(())).collect();
        Ok(Arc::new(Self {
            root,
            config,
            meta_path,
            meta: Mutex::new(conn),
            packs,
            overlay,
            inode_locks,
        }))
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

    /// Acquires the per-inode stripe lock shared between live WinFsp ops and background dedup.
    pub fn lock_inode(&self, ws_id: u64, inode_id: u64) -> std::sync::MutexGuard<'_, ()> {
        let idx = ((ws_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ inode_id) as usize)
            & (INODE_LOCK_STRIPES - 1);
        self.inode_locks[idx].lock().unwrap()
    }

    /// Opens an additional WAL SQLite connection for concurrent workspace reads/writes.
    pub fn open_conn(&self) -> Result<Connection> {
        open_meta_connection(&self.meta_path)
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
        for m in manifests {
            release_manifest_chunks(&tx, &m)?;
        }
        tx.execute("DELETE FROM dentries WHERE ws_id = ?1", params![ws_id as i64])?;
        tx.execute("DELETE FROM inodes WHERE ws_id = ?1", params![ws_id as i64])?;
        tx.execute(
            "DELETE FROM workspaces WHERE ws_id = ?1",
            params![ws_id as i64],
        )?;
        tx.commit()?;
        drop(conn);
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
        let start = Instant::now();
        let canon_src = fs::canonicalize(source.as_ref()).map_err(|e| {
            Error::InvalidInput(format!(
                "cannot access source {}: {e}",
                source.as_ref().display()
            ))
        })?;
        if !canon_src.is_dir() {
            return Err(Error::InvalidInput(format!(
                "source is not a directory: {}",
                source.as_ref().display()
            )));
        }

        // Reject importing a source that contains this store directory.
        if self.root.starts_with(&canon_src) {
            return Err(Error::InvalidInput(format!(
                "store {} cannot lie inside source {}",
                self.root.display(),
                source.as_ref().display()
            )));
        }

        let ws_id = self.create_workspace(ws_name)?;
        let known = {
            let conn = self.meta.lock().unwrap();
            Arc::new(KnownChunks::load(&conn)?)
        };

        let result = self.import_dir_inner(ws_id, &canon_src, known, threads.max(1), progress_cb);
        match result {
            Ok((
                files,
                dirs,
                logical_bytes,
                new_unique_chunks,
                new_unique_bytes,
                stored_bytes,
                overlay_pinned_files,
                errors,
            )) => Ok(ImportSummary {
                workspace: ws_name.to_owned(),
                ws_id,
                files,
                dirs,
                logical_bytes,
                new_unique_chunks,
                new_unique_bytes,
                stored_bytes,
                overlay_pinned_files,
                errors,
                elapsed_secs: start.elapsed().as_secs_f64(),
            }),
            Err(err) => {
                // Clean up partial workspace on import failure.
                let _ = self.remove_workspace(ws_name);
                Err(err)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn import_dir_inner(
        self: &Arc<Self>,
        ws_id: u64,
        canon_src: &Path,
        known: Arc<KnownChunks>,
        threads: usize,
        progress_cb: Option<Box<dyn Fn(ImportProgress) + Send + Sync>>,
    ) -> Result<(u64, u64, u64, u64, u64, u64, u64, u64)> {
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
        }

        struct NewChunkBlob {
            hash: Hash32,
            codec: Codec,
            raw_len: u32,
            stored: Vec<u8>,
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
            manifest: Vec<u8>,
            new_chunks: Vec<NewChunkBlob>,
        }

        let files_done = Arc::new(AtomicU64::new(0));
        let dirs_done = Arc::new(AtomicU64::new(1)); // root dir
        let logical_done = Arc::new(AtomicU64::new(0));
        let unique_done = Arc::new(AtomicU64::new(0));
        let unique_chunks_done = Arc::new(AtomicU64::new(0));
        let stored_done = Arc::new(AtomicU64::new(0));
        let pinned_done = Arc::new(AtomicU64::new(0));
        let errors_done = Arc::new(AtomicU64::new(0));

        let next_inode = Arc::new(AtomicU64::new(ROOT_INODE + 1));

        // Channel: walker -> workers
        let (job_tx, job_rx) = mpsc::sync_channel::<ImportFileJob>(4096);
        let job_rx = Arc::new(Mutex::new(job_rx));

        // Channel: workers -> committer
        let (prep_tx, prep_rx) = mpsc::sync_channel::<PreparedFile>(1024);

        // Progress reporter thread (optional).
        let stop_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_handle = progress_cb.map(|cb| {
            let stop = Arc::clone(&stop_progress);
            let f = Arc::clone(&files_done);
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
            let known = Arc::clone(&known);
            let store = Arc::clone(self);
            let errs = Arc::clone(&errors_done);
            workers.push(std::thread::spawn(move || {
                let mut file_buf = Vec::with_capacity(1024 * 1024);
                loop {
                    let job = { rx.lock().unwrap().recv() };
                    let Ok(job) = job else {
                        break;
                    };
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
                                let _ = tx.send(PreparedFile {
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
                                    manifest: Vec::new(),
                                    new_chunks: Vec::new(),
                                });
                            }
                            Err(_) => {
                                errs.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        continue;
                    }

                    let prep_res = (|| -> Result<(u64, Vec<ManifestEntry>, Vec<NewChunkBlob>)> {
                        let mut file = File::open(&job.path)?;
                        let mut entries = Vec::new();
                        let mut new_chunks = Vec::new();

                        let mut handle_chunk = |chunk: &[u8]| -> Result<()> {
                            let hash = blake3_hash(chunk);
                            let raw_len = chunk.len() as u32;
                            entries.push(ManifestEntry { hash, raw_len });
                            if known.claim_if_new(hash) {
                                let (actual_codec, stored) = compress_chunk(codec, chunk)?;
                                new_chunks.push(NewChunkBlob {
                                    hash,
                                    codec: actual_codec,
                                    raw_len,
                                    stored,
                                });
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
                            len
                        } else {
                            chunk_reader(&mut file, handle_chunk)?
                        };
                        Ok((actual_len, entries, new_chunks))
                    })();

                    match prep_res {
                        Ok((actual_len, entries, new_chunks)) => {
                            let _ = tx.send(PreparedFile {
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
                                manifest: encode_manifest(&entries),
                                new_chunks,
                            });
                        }
                        Err(_) => {
                            errs.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        drop(prep_tx);

        // Spawn single sequential pack-writer & SQLite metadata committer thread.
        let store_c = Arc::clone(self);
        let f_c = Arc::clone(&files_done);
        let l_c = Arc::clone(&logical_done);
        let u_c = Arc::clone(&unique_done);
        let uc_c = Arc::clone(&unique_chunks_done);
        let s_c = Arc::clone(&stored_done);
        let p_c = Arc::clone(&pinned_done);
        let committer = std::thread::spawn(move || -> Result<()> {
            let mut batch: Vec<PreparedFile> = Vec::with_capacity(IMPORT_COMMIT_BATCH);
            let flush_batch = |batch: &mut Vec<PreparedFile>| -> Result<()> {
                if batch.is_empty() {
                    return Ok(());
                }
                // 1. Sequentially append all new unique chunks to active `.pack` file(s).
                let mut written_locs: HashMap<Hash32, ChunkLoc> = HashMap::new();
                let mut pack_updates = Vec::new();
                {
                    let mut pw = store_c.packs.writer();
                    for item in batch.iter() {
                        for chunk in &item.new_chunks {
                            let loc = pw.append(
                                &chunk.hash,
                                chunk.codec,
                                chunk.raw_len,
                                &chunk.stored,
                            )?;
                            written_locs.insert(chunk.hash, loc);
                            uc_c.fetch_add(1, Ordering::Relaxed);
                            u_c.fetch_add(u64::from(chunk.raw_len), Ordering::Relaxed);
                            s_c.fetch_add(chunk.stored.len() as u64, Ordering::Relaxed);
                        }
                    }
                    pw.flush()?;
                    pack_updates.extend(pw.take_pack_updates());
                }

                // 2. Batch-commit chunks, pack sizes, inodes, and dentries in one SQLite txn.
                let mut conn = store_c.meta.lock().unwrap();
                let tx = conn.transaction()?;
                upsert_pack_updates(&tx, &pack_updates)?;

                // Aggregate refcount additions across this entire batch so duplicate chunks
                // within the batch execute a single SQL statement per unique hash.
                let mut ref_adds: HashMap<Hash32, u64> = HashMap::new();
                for item in batch.iter() {
                    if item.storage_mode == STORAGE_CHUNKS && !item.manifest.is_empty() {
                        for e in decode_manifest(&item.manifest)? {
                            *ref_adds.entry(e.hash).or_insert(0) += 1;
                        }
                    }
                }
                for (hash, add) in ref_adds {
                    let loc = written_locs.remove(&hash);
                    upsert_chunk_ref(&tx, &hash, loc, add)?;
                }

                {
                    let mut ins_inode = tx.prepare_cached(
                        "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
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
                            item.manifest.as_slice(),
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
                        l_c.fetch_add(item.size, Ordering::Relaxed);
                        if item.pinned_overlay {
                            p_c.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                tx.commit()?;
                batch.clear();
                Ok(())
            };

            while let Ok(item) = prep_rx.recv() {
                batch.push(item);
                if batch.len() >= IMPORT_COMMIT_BATCH {
                    flush_batch(&mut batch)?;
                }
            }
            flush_batch(&mut batch)?;
            Ok(())
        });

        // Walk source directory tree, batch-inserting directories and feeding file jobs to workers.
        let mut dir_stack: Vec<(PathBuf, u64)> = vec![(canon_src.to_path_buf(), ROOT_INODE)];
        let mut pending_dirs: Vec<(u64, u64, Vec<u8>, Vec<u8>, i64, i64)> = Vec::with_capacity(1024);

        let flush_dirs = |pending: &mut Vec<(u64, u64, Vec<u8>, Vec<u8>, i64, i64)>| -> Result<()> {
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

        while let Some((dir_path, parent_id)) = dir_stack.pop() {
            let entries = match fs::read_dir(&dir_path) {
                Ok(rd) => rd,
                Err(_) => {
                    errors_done.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    errors_done.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let child_path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&child_path) else {
                    errors_done.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let ft = meta.file_type();
                if ft.is_symlink() {
                    continue;
                }
                let name_os = entry.file_name();
                let Some(name_str) = name_os.to_str() else {
                    errors_done.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
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

                let inode_id = next_inode.fetch_add(1, Ordering::Relaxed);
                if ft.is_dir() {
                    dirs_done.fetch_add(1, Ordering::Relaxed);
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
                    dir_stack.push((child_path, inode_id));
                } else if ft.is_file() {
                    let attrs = if meta.permissions().readonly() {
                        0x01u32
                    } else {
                        0u32
                    };
                    let pinned = is_hot_db_name(name_str);
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
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        flush_dirs(&mut pending_dirs)?;
        drop(job_tx);

        for w in workers {
            w.join()
                .map_err(|_| Error::Other("import worker panicked".into()))?;
        }
        committer
            .join()
            .map_err(|_| Error::Other("import committer panicked".into()))??;

        // Update workspace's `next_inode` counter to the final allocated value.
        let final_next = next_inode.load(Ordering::Relaxed);
        {
            let conn = self.meta.lock().unwrap();
            conn.execute(
                "UPDATE workspaces SET next_inode = ?1 WHERE ws_id = ?2",
                params![final_next as i64, ws_id as i64],
            )?;
        }

        stop_progress.store(true, Ordering::Relaxed);
        if let Some(h) = progress_handle {
            let _ = h.join();
        }

        Ok((
            files_done.load(Ordering::Relaxed),
            dirs_done.load(Ordering::Relaxed),
            logical_done.load(Ordering::Relaxed),
            unique_chunks_done.load(Ordering::Relaxed),
            unique_done.load(Ordering::Relaxed),
            stored_done.load(Ordering::Relaxed),
            pinned_done.load(Ordering::Relaxed),
            errors_done.load(Ordering::Relaxed),
        ))
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

        for (ws_id, inode_id, expected_size, expected_written_ns) in candidates {
            if is_busy(ws_id, inode_id) {
                summary.skipped_busy += 1;
                continue;
            }
            let loose_path = self.overlay.file_path(ws_id, inode_id);
            let mut entries = Vec::new();
            let mut new_blobs: Vec<(Hash32, Codec, u32, Vec<u8>)> = Vec::new();
            let actual_len: u64;

            if expected_size == 0 || !loose_path.exists() {
                actual_len = 0;
            } else {
                let Ok(mut file) = File::open(&loose_path) else {
                    summary.skipped_changed += 1;
                    continue;
                };
                // Chunk & hash outside the inode lock so foreground ops on other inodes never wait.
                let mut chunk_hashes_seen: HashSet<Hash32> = HashSet::new();
                let read_res = chunk_reader(&mut file, |chunk| {
                    let hash = blake3_hash(chunk);
                    let raw_len = chunk.len() as u32;
                    entries.push(ManifestEntry { hash, raw_len });
                    if chunk_hashes_seen.insert(hash) {
                        let exists_in_db = {
                            let conn = self.meta.lock().unwrap();
                            get_chunk_loc(&conn, &hash)?.is_some()
                        };
                        if !exists_in_db {
                            let (c, stored) = compress_chunk(self.config.codec, chunk)?;
                            new_blobs.push((hash, c, raw_len, stored));
                        }
                    }
                    Ok(())
                });
                match read_res {
                    Ok(len) => actual_len = len,
                    Err(_) => {
                        summary.skipped_changed += 1;
                        continue;
                    }
                }
            }

            // Acquire per-inode stripe lock for atomic check-and-swap.
            let _guard = self.lock_inode(ws_id, inode_id);
            if is_busy(ws_id, inode_id) {
                summary.skipped_busy += 1;
                continue;
            }

            // Verify inode metadata hasn't changed while we were chunking.
            let current = {
                let conn = self.meta.lock().unwrap();
                load_inode(&conn, ws_id, inode_id)?
            };
            let Some(cur) = current else {
                summary.skipped_changed += 1;
                continue;
            };
            if cur.storage_mode != STORAGE_OVERLAY
                || cur.pinned_overlay
                || cur.size != expected_size
                || cur.written_ns != expected_written_ns
                || cur.size != actual_len
            {
                summary.skipped_changed += 1;
                continue;
            }

            // Append any truly new unique chunks to pack store.
            let mut written_locs: HashMap<Hash32, ChunkLoc> = HashMap::new();
            let mut pack_updates = Vec::new();
            let mut added_unique = 0u64;
            let mut added_stored = 0u64;
            if !new_blobs.is_empty() {
                let mut pw = self.packs.writer();
                for (hash, codec, raw_len, stored) in &new_blobs {
                    let loc = pw.append(hash, *codec, *raw_len, stored)?;
                    written_locs.insert(*hash, loc);
                    added_unique += u64::from(*raw_len);
                    added_stored += stored.len() as u64;
                }
                pw.flush()?;
                pack_updates.extend(pw.take_pack_updates());
            }

            // Commit chunk refcounts + flip inode from Overlay -> Chunks atomically.
            let manifest_blob = encode_manifest(&entries);
            {
                let mut conn = self.meta.lock().unwrap();
                let tx = conn.transaction()?;
                upsert_pack_updates(&tx, &pack_updates)?;
                let mut ref_adds: HashMap<Hash32, u64> = HashMap::new();
                for e in &entries {
                    *ref_adds.entry(e.hash).or_insert(0) += 1;
                }
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

            // Delete loose overlay file to immediately free host disk space!
            let _ = self.overlay.remove_file(ws_id, inode_id);

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
            summary.compacted_packs += 1;
            summary.moved_chunks += moved.len() as u64;
            summary.reclaimed_disk_bytes += dead_bytes;
        }

        // Flush WAL checkpoint so meta.sqlite stays tidy after GC.
        {
            let conn = self.meta.lock().unwrap();
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }

        Ok(summary)
    }
}
