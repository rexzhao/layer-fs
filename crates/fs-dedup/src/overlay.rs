//! Per-workspace local NTFS sparse-file overlay storage.
//!
//! All foreground writes go directly to sharded local NTFS files so Unity / Git / SVN
//! write performance stays near native NTFS and hot databases (LMDB / SQLite) stay compatible.
//!
//! Open overlay handles are pooled in a CLOCK-LRU cache using `ParallelFile` (`FILE_FLAG_OVERLAPPED`
//! on Windows + `FILE_SHARE_READ | WRITE | DELETE`) so concurrent random I/O (such as Unity's LMDB
//! `ArtifactDB` / `SourceAssetDB` page reads) executes genuinely in parallel inside the Windows
//! kernel without per-call `CreateFileW` / `CloseHandle` or synchronous `FILE_OBJECT` serialization.

use crate::error::Result;
use crate::parallel_io::ParallelFile;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_OPEN_OVERLAY_HANDLES: usize = 256;

struct OverlayHandlePool {
    handles: HashMap<(u64, u64), (Arc<ParallelFile>, bool)>,
    order: VecDeque<(u64, u64)>,
}

impl OverlayHandlePool {
    fn new() -> Self {
        Self {
            handles: HashMap::with_capacity(MAX_OPEN_OVERLAY_HANDLES),
            order: VecDeque::with_capacity(MAX_OPEN_OVERLAY_HANDLES),
        }
    }

    fn get(&mut self, key: (u64, u64)) -> Option<Arc<ParallelFile>> {
        let (file, ref_bit) = self.handles.get_mut(&key)?;
        *ref_bit = true;
        Some(Arc::clone(file))
    }

    fn insert(&mut self, key: (u64, u64), file: Arc<ParallelFile>) {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.handles.entry(key) {
            e.insert((file, true));
            return;
        }
        // CLOCK second-chance eviction: hot handles get a second chance before eviction.
        let mut scanned = 0usize;
        let max_scan = self.order.len().saturating_mul(2).max(1);
        while self.handles.len() >= MAX_OPEN_OVERLAY_HANDLES {
            let Some(cand) = self.order.pop_front() else {
                break;
            };
            scanned += 1;
            if let Some((_, ref_bit)) = self.handles.get_mut(&cand) {
                if *ref_bit && scanned <= max_scan {
                    *ref_bit = false;
                    self.order.push_back(cand);
                    continue;
                }
                self.handles.remove(&cand);
            }
        }
        self.handles.insert(key, (file, true));
        self.order.push_back(key);
    }

    fn evict(&mut self, key: (u64, u64)) {
        if self.handles.remove(&key).is_some() {
            self.order.retain(|&k| k != key);
        }
    }

    fn evict_workspace(&mut self, ws_id: u64) {
        self.handles.retain(|&(w, _), _| w != ws_id);
        self.order.retain(|&(w, _)| w != ws_id);
    }
}

/// Manages loose overlay files under `<store>/overlay/<ws_id>/<shard>/i<inode_id>.dat`.
#[derive(Clone)]
pub struct OverlayStore {
    root: PathBuf,
    pool: Arc<Mutex<OverlayHandlePool>>,
}

impl OverlayStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            pool: Arc::new(Mutex::new(OverlayHandlePool::new())),
        }
    }

    pub fn workspace_dir(&self, ws_id: u64) -> PathBuf {
        self.root.join(ws_id.to_string())
    }

    /// Path of the clean-shutdown marker written on graceful backend drop and removed on open.
    pub fn clean_marker_path(&self, ws_id: u64) -> PathBuf {
        self.workspace_dir(ws_id).join(".clean")
    }

    /// Pre-creates all 256 hex shard subdirectories (`00`..`ff`) for `ws_id` so subsequent file
    /// creations never pay per-file `create_dir_all` syscalls.
    pub fn precreate_shards(&self, ws_id: u64) {
        let ws_dir = self.workspace_dir(ws_id);
        let _ = fs::create_dir_all(&ws_dir);
        for shard in 0u8..=255 {
            let dir = ws_dir.join(format!("{shard:02x}"));
            let _ = fs::create_dir(&dir);
        }
    }

    pub fn file_path(&self, ws_id: u64, inode_id: u64) -> PathBuf {
        let shard = format!("{:02x}", (inode_id & 0xff) as u8);
        self.workspace_dir(ws_id)
            .join(shard)
            .join(format!("i{inode_id}.dat"))
    }

    /// Evicts any cached open file handle for `(ws_id, inode_id)`.
    pub fn evict_handle(&self, ws_id: u64, inode_id: u64) {
        self.pool.lock().unwrap().evict((ws_id, inode_id));
    }

    /// Creates or truncates an overlay file for `(ws_id, inode_id)` and caches its parallel RW
    /// handle directly in the handle pool so the immediately following `write()` avoids a second
    /// `CreateFileW` syscall.
    pub fn create_empty(&self, ws_id: u64, inode_id: u64) -> Result<Arc<ParallelFile>> {
        self.evict_handle(ws_id, inode_id);
        let path = self.file_path(ws_id, inode_id);
        let file = match ParallelFile::create_empty_rw(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                ParallelFile::create_empty_rw(&path)?
            }
            Err(e) => return Err(e.into()),
        };
        let arc = Arc::new(file);
        self.pool
            .lock()
            .unwrap()
            .insert((ws_id, inode_id), Arc::clone(&arc));
        Ok(arc)
    }

    /// Opens an existing overlay file for parallel read/write (creating if absent).
    /// Fast path tries opening directly in 1 syscall without `create_dir_all` / `exists`.
    fn open_rw(&self, ws_id: u64, inode_id: u64) -> Result<ParallelFile> {
        let path = self.file_path(ws_id, inode_id);
        match ParallelFile::open_existing_rw(&path) {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(ParallelFile::open_or_create_rw(&path)?)
    }

    /// Returns the on-disk byte length of `(ws_id, inode_id)`'s overlay file if it exists.
    pub fn file_len_if_exists(&self, ws_id: u64, inode_id: u64) -> Option<u64> {
        fs::metadata(self.file_path(ws_id, inode_id))
            .ok()
            .map(|m| m.len())
    }

    /// Gets a pooled parallel RW handle for an existing overlay file (or `None` if absent).
    fn get_cached_existing(&self, ws_id: u64, inode_id: u64) -> Result<Option<Arc<ParallelFile>>> {
        let key = (ws_id, inode_id);
        if let Some(hit) = self.pool.lock().unwrap().get(key) {
            return Ok(Some(hit));
        }
        let path = self.file_path(ws_id, inode_id);
        let file = match ParallelFile::open_existing_rw(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                // Fallback to read-only open if file has read-only OS attributes (do not
                // place an RO-only handle into the shared RW pool).
                return match ParallelFile::open_read(&path) {
                    Ok(f) => Ok(Some(Arc::new(f))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                };
            }
        };
        let arc = Arc::new(file);
        self.pool.lock().unwrap().insert(key, Arc::clone(&arc));
        Ok(Some(arc))
    }

    /// Gets a pooled parallel RW handle for `(ws_id, inode_id)`, creating the file if absent.
    fn get_cached_rw(&self, ws_id: u64, inode_id: u64) -> Result<Arc<ParallelFile>> {
        let key = (ws_id, inode_id);
        if let Some(hit) = self.pool.lock().unwrap().get(key) {
            return Ok(hit);
        }
        let file = self.open_rw(ws_id, inode_id)?;
        let arc = Arc::new(file);
        self.pool.lock().unwrap().insert(key, Arc::clone(&arc));
        Ok(arc)
    }

    /// Positional read from an overlay file (returns short slice at EOF).
    pub fn read_at(&self, ws_id: u64, inode_id: u64, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.read_at_known_len(ws_id, inode_id, None, offset, len)
    }

    /// Positional read using a pooled parallel file handle. When `known_len` is provided from
    /// `CachedInode`, avoids even the per-call `metadata()` syscall.
    pub fn read_at_known_len(
        &self,
        ws_id: u64,
        inode_id: u64,
        known_len: Option<u64>,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; len];
        let n = self.read_into_known_len(ws_id, inode_id, known_len, offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Positional read directly into caller's `dst` buffer (zero intermediate allocation).
    pub fn read_into_known_len(
        &self,
        ws_id: u64,
        inode_id: u64,
        known_len: Option<u64>,
        offset: u64,
        dst: &mut [u8],
    ) -> Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        let Some(file) = self.get_cached_existing(ws_id, inode_id)? else {
            return Ok(0);
        };
        let file_len = match known_len {
            Some(l) => l,
            None => file.len()?,
        };
        if offset >= file_len {
            return Ok(0);
        }
        let to_read = (file_len - offset).min(dst.len() as u64) as usize;
        let mut done = 0usize;
        while done < to_read {
            let n = file.read_at(offset + done as u64, &mut dst[done..to_read])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        if known_len.is_some() && done < to_read {
            // Sparse tail past physical EOF reads as zeros up to `to_read`.
            dst[done..to_read].fill(0);
            return Ok(to_read);
        }
        Ok(done)
    }

    /// Positional write to an overlay file, returning `offset + data.len()` without an extra `metadata()` syscall.
    pub fn write_at_fast(
        &self,
        ws_id: u64,
        inode_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64> {
        let file = self.get_cached_rw(ws_id, inode_id)?;
        file.write_all_at(offset, data)?;
        Ok(offset.saturating_add(data.len() as u64))
    }

    /// Flushes OS buffers for an overlay file to durable storage (`FlushFileBuffers` / `fdatasync`).
    /// Re-opens the file if its handle was previously evicted from the pool so `FlushFileBuffers`
    /// semantics are always honored.
    pub fn sync_data(&self, ws_id: u64, inode_id: u64) -> Result<()> {
        if let Some(file) = self.get_cached_existing(ws_id, inode_id)? {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Positional write to an overlay file, returning the new file length.
    pub fn write_at(
        &self,
        ws_id: u64,
        inode_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64> {
        let _ = self.write_at_fast(ws_id, inode_id, offset, data)?;
        let file = self.get_cached_rw(ws_id, inode_id)?;
        Ok(file.len()?)
    }

    /// Resizes an overlay file (truncating or sparse zero-extending).
    /// Enables NTFS sparse attribute at most once per handle and only for >= 64 KiB extensions (N9).
    pub fn set_len(&self, ws_id: u64, inode_id: u64, new_len: u64) -> Result<()> {
        let file = self.get_cached_rw(ws_id, inode_id)?;
        if new_len >= 64 * 1024 {
            file.set_sparse();
        }
        file.set_len(new_len)?;
        Ok(())
    }

    /// Deletes the loose overlay file for `(ws_id, inode_id)` if present.
    pub fn remove_file(&self, ws_id: u64, inode_id: u64) -> Result<()> {
        self.evict_handle(ws_id, inode_id);
        let path = self.file_path(ws_id, inode_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Deletes all overlay files for a removed workspace.
    pub fn remove_workspace(&self, ws_id: u64) -> Result<()> {
        self.pool.lock().unwrap().evict_workspace(ws_id);
        let dir = self.workspace_dir(ws_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
