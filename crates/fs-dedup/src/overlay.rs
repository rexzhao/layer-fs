//! Per-workspace local NTFS sparse-file overlay storage.
//!
//! All foreground writes go directly to sharded local NTFS sparse files so Unity / Git / SVN
//! write performance stays near native NTFS and hot databases (LMDB / SQLite) stay compatible.
//!
//! Open overlay handles are pooled in an LRU cache (opened with `FILE_SHARE_READ | WRITE | DELETE`
//! on Windows) so high-frequency random I/O (such as Unity's LMDB `ArtifactDB` / `SourceAssetDB`
//! page faults) executes a single positional `seek_read`/`seek_write` syscall without paying
//! per-call `CreateFileW` / `CloseHandle` / AV minifilter overhead.

use crate::error::Result;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_OPEN_OVERLAY_HANDLES: usize = 256;

#[cfg(windows)]
fn apply_share_mode(opts: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ (1) | FILE_SHARE_WRITE (2) | FILE_SHARE_DELETE (4)
    opts.share_mode(0x07);
}

#[cfg(not(windows))]
fn apply_share_mode(_opts: &mut OpenOptions) {}

struct OverlayHandlePool {
    handles: HashMap<(u64, u64), (Arc<File>, bool)>,
    order: VecDeque<(u64, u64)>,
}

impl OverlayHandlePool {
    fn new() -> Self {
        Self {
            handles: HashMap::with_capacity(MAX_OPEN_OVERLAY_HANDLES),
            order: VecDeque::with_capacity(MAX_OPEN_OVERLAY_HANDLES),
        }
    }

    fn get(&mut self, key: (u64, u64)) -> Option<Arc<File>> {
        let (file, ref_bit) = self.handles.get_mut(&key)?;
        *ref_bit = true;
        Some(Arc::clone(file))
    }

    fn insert(&mut self, key: (u64, u64), file: Arc<File>) {
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

    /// Creates or truncates a sparse overlay file for `(ws_id, inode_id)`.
    pub fn create_empty(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        self.evict_handle(ws_id, inode_id);
        let path = self.file_path(ws_id, inode_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).read(true).write(true);
        apply_share_mode(&mut opts);
        let file = opts.open(&path)?;
        let _ = layerfs_win32_host::set_sparse_file(&file);
        Ok(file)
    }

    /// Opens an existing overlay file for read/write (creating if absent).
    /// Fast path tries opening directly in 1 syscall without `create_dir_all` / `exists`.
    pub fn open_rw(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        let path = self.file_path(ws_id, inode_id);
        let mut fast = OpenOptions::new();
        fast.read(true).write(true);
        apply_share_mode(&mut fast);
        match fast.open(&path) {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(false).read(true).write(true);
        apply_share_mode(&mut opts);
        let file = opts.open(&path)?;
        let _ = layerfs_win32_host::set_sparse_file(&file);
        Ok(file)
    }

    /// Returns the on-disk byte length of `(ws_id, inode_id)`'s overlay file if it exists.
    pub fn file_len_if_exists(&self, ws_id: u64, inode_id: u64) -> Option<u64> {
        fs::metadata(self.file_path(ws_id, inode_id))
            .ok()
            .map(|m| m.len())
    }

    /// Opens an existing overlay file read-only.
    pub fn open_ro(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        apply_share_mode(&mut opts);
        Ok(opts.open(self.file_path(ws_id, inode_id))?)
    }

    /// Gets a pooled RW handle for an existing overlay file (or `None` if absent).
    fn get_cached_existing(&self, ws_id: u64, inode_id: u64) -> Result<Option<Arc<File>>> {
        let key = (ws_id, inode_id);
        if let Some(hit) = self.pool.lock().unwrap().get(key) {
            return Ok(Some(hit));
        }
        let path = self.file_path(ws_id, inode_id);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        apply_share_mode(&mut opts);
        let file = match opts.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                // Fallback to read-only open if file has read-only OS attributes (do not
                // place an RO-only handle into the shared RW pool).
                let mut ro = OpenOptions::new();
                ro.read(true);
                apply_share_mode(&mut ro);
                return match ro.open(&path) {
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

    /// Gets a pooled RW handle for `(ws_id, inode_id)`, creating the sparse file if absent.
    fn get_cached_rw(&self, ws_id: u64, inode_id: u64) -> Result<Arc<File>> {
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

    /// Positional read using a pooled file handle. When `known_len` is provided from
    /// `CachedInode`, avoids even the per-call `metadata()` syscall.
    pub fn read_at_known_len(
        &self,
        ws_id: u64,
        inode_id: u64,
        known_len: Option<u64>,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>> {
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;

        if len == 0 {
            return Ok(Vec::new());
        }
        let Some(file) = self.get_cached_existing(ws_id, inode_id)? else {
            return Ok(Vec::new());
        };
        let file_len = match known_len {
            Some(l) => l,
            None => file.metadata()?.len(),
        };
        if offset >= file_len {
            return Ok(Vec::new());
        }
        let to_read = (file_len - offset).min(len as u64) as usize;
        let mut buf = vec![0u8; to_read];
        let mut done = 0usize;
        while done < to_read {
            #[cfg(windows)]
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            #[cfg(unix)]
            let n = file.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        if known_len.is_none() {
            buf.truncate(done);
        }
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
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;

        if dst.is_empty() {
            return Ok(0);
        }
        let Some(file) = self.get_cached_existing(ws_id, inode_id)? else {
            return Ok(0);
        };
        let file_len = match known_len {
            Some(l) => l,
            None => file.metadata()?.len(),
        };
        if offset >= file_len {
            return Ok(0);
        }
        let to_read = (file_len - offset).min(dst.len() as u64) as usize;
        let mut done = 0usize;
        while done < to_read {
            #[cfg(windows)]
            let n = file.seek_read(&mut dst[done..to_read], offset + done as u64)?;
            #[cfg(unix)]
            let n = file.read_at(&mut dst[done..to_read], offset + done as u64)?;
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
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;

        let file = self.get_cached_rw(ws_id, inode_id)?;
        let mut pos = offset;
        let mut rem = data;
        while !rem.is_empty() {
            #[cfg(windows)]
            let n = file.seek_write(rem, pos)?;
            #[cfg(unix)]
            let n = file.write_at(rem, pos)?;
            pos += n as u64;
            rem = &rem[n..];
        }
        Ok(pos)
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
        Ok(file.metadata()?.len())
    }

    /// Resizes an overlay file (truncating or sparse zero-extending).
    pub fn set_len(&self, ws_id: u64, inode_id: u64, new_len: u64) -> Result<()> {
        let file = self.get_cached_rw(ws_id, inode_id)?;
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
