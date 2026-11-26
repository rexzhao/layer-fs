//! Loose sparse-file sidecar for overlay file payloads.
//!
//! File content bytes live as per-`(inode, generation)` sparse host files under
//! `<overlay>.blobs/<shard>/i<inode>_g<gen>.dat` instead of SQLite `fs_data` BLOBs.
//! SQLite retains only lightweight extent/validity metadata and generation history,
//! keeping transactions atomic via deferred commit/rollback hooks while giving large
//! files native OS I/O and immediate disk-space reclamation on delete/reclaim.

use crate::{OverlayError, OverlayResult};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

/// Maximum number of open loose payload files retained in the LRU cache.
const MAX_OPEN_HANDLES: usize = 64;

/// Minimum hole size worth issuing an explicit NTFS `FSCTL_SET_ZERO_DATA` punch for.
const PUNCH_HOLE_MIN_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct LayerKey {
    serial: i64,
    gen: i64,
}

struct CachedFile {
    file: File,
    len: u64,
    last_used: u64,
}

enum PendingOp {
    RemoveLayer(LayerKey),
    PunchHole {
        key: LayerKey,
        offset: u64,
        length: u64,
    },
}

enum UndoEntry {
    Truncate {
        key: LayerKey,
        previous_len: u64,
    },
    RestoreBytes {
        key: LayerKey,
        offset: u64,
        previous: Vec<u8>,
    },
    RemoveNewFile(LayerKey),
}

pub(crate) struct PayloadStore {
    root: PathBuf,
    created_root: bool,
    handles: HashMap<LayerKey, CachedFile>,
    clock: u64,
    pending: Vec<PendingOp>,
    undo: Vec<UndoEntry>,
    quota_bytes: Option<u64>,
    committed_bytes: u64,
    staged_delta: i64,
}

impl PayloadStore {
    pub(crate) fn new(db_path: &Path, max_pages: Option<u32>) -> Self {
        let mut root_os = db_path.as_os_str().to_os_string();
        root_os.push(".blobs");
        Self {
            root: PathBuf::from(root_os),
            created_root: false,
            handles: HashMap::new(),
            clock: 0,
            pending: Vec::new(),
            undo: Vec::new(),
            quota_bytes: max_pages.map(|pages| u64::from(pages).saturating_mul(4096)),
            committed_bytes: 0,
            staged_delta: 0,
        }
    }

    /// Checks optional `max_pages` byte budget before admitting `added_bytes` of new payload.
    pub(crate) fn check_quota(&self, added_bytes: u64) -> OverlayResult<()> {
        let Some(limit) = self.quota_bytes else {
            return Ok(());
        };
        let current = apply_delta(self.committed_bytes, self.staged_delta);
        if current.saturating_add(added_bytes) > limit {
            return Err(OverlayError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                Some("overlay payload quota exceeded".to_owned()),
            )));
        }
        Ok(())
    }

    pub(crate) fn note_bytes_added(&mut self, bytes: u64) {
        if self.quota_bytes.is_some() {
            self.staged_delta = self.staged_delta.saturating_add(bytes as i64);
        }
    }

    pub(crate) fn note_bytes_dropped(&mut self, bytes: u64) {
        if self.quota_bytes.is_some() {
            self.staged_delta = self.staged_delta.saturating_sub(bytes as i64);
        }
    }

    fn layer_path(&self, key: LayerKey) -> PathBuf {
        let shard = format!("{:02x}", (key.serial as u64) & 0xff);
        let name = if key.gen < 0 {
            format!("i{}_orphan.dat", key.serial)
        } else {
            format!("i{}_g{}.dat", key.serial, key.gen)
        };
        self.root.join(shard).join(name)
    }

    fn evict_if_full(&mut self) {
        if self.handles.len() < MAX_OPEN_HANDLES {
            return;
        }
        if let Some((&oldest_key, _)) = self
            .handles
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
        {
            self.handles.remove(&oldest_key);
        }
    }

    fn open_for_write(&mut self, key: LayerKey, record_undo: bool) -> OverlayResult<&mut CachedFile> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;
        if self.handles.contains_key(&key) {
            let entry = self.handles.get_mut(&key).expect("just checked");
            entry.last_used = tick;
            return Ok(entry);
        }
        self.evict_if_full();
        let path = self.layer_path(key);
        if !self.created_root {
            std::fs::create_dir_all(&self.root)?;
            self.created_root = true;
        }
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if !existed {
            #[cfg(target_os = "windows")]
            {
                let _ = layerfs_win32_host::set_sparse_file(&file);
            }
            if record_undo {
                self.undo.push(UndoEntry::RemoveNewFile(key));
            }
        }
        let len = file.metadata()?.len();
        self.handles.insert(
            key,
            CachedFile {
                file,
                len,
                last_used: tick,
            },
        );
        Ok(self.handles.get_mut(&key).expect("just inserted"))
    }

    fn open_for_read(&mut self, key: LayerKey) -> OverlayResult<Option<&mut CachedFile>> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;
        if self.handles.contains_key(&key) {
            let entry = self.handles.get_mut(&key).expect("just checked");
            entry.last_used = tick;
            return Ok(Some(entry));
        }
        let path = self.layer_path(key);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let len = file.metadata()?.len();
        self.evict_if_full();
        self.handles.insert(
            key,
            CachedFile {
                file,
                len,
                last_used: tick,
            },
        );
        Ok(Some(self.handles.get_mut(&key).expect("just inserted")))
    }

    /// Writes `bytes` at `offset` in `(serial, gen)`'s loose sparse file.
    ///
    /// When `in_tx` is true, records enough undo information so a SQLite transaction
    /// rollback leaves every previously committed byte intact.
    pub(crate) fn write_at(
        &mut self,
        serial: i64,
        gen: i64,
        offset: u64,
        bytes: &[u8],
        in_tx: bool,
    ) -> OverlayResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let key = LayerKey { serial, gen };
        let entry = self.open_for_write(key, in_tx)?;
        let prev_len = entry.len;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(OverlayError::Invalid("payload offset overflow"))?;
        if in_tx {
            if offset < prev_len {
                let overlap = (prev_len.min(end) - offset) as usize;
                let mut previous = vec![0u8; overlap];
                read_all_at(&entry.file, offset, &mut previous)?;
                self.undo.push(UndoEntry::RestoreBytes {
                    key,
                    offset,
                    previous,
                });
            }
            if end > prev_len {
                self.undo.push(UndoEntry::Truncate {
                    key,
                    previous_len: prev_len,
                });
            }
        }
        let entry = self.handles.get_mut(&key).expect("still cached");
        write_all_at(&entry.file, offset, bytes)?;
        if end > entry.len {
            entry.len = end;
        }
        Ok(())
    }

    /// Reads `len` bytes starting at `offset` from `(serial, gen)`'s loose sparse file.
    pub(crate) fn read_exact(
        &mut self,
        serial: i64,
        gen: i64,
        offset: u64,
        len: usize,
    ) -> OverlayResult<Vec<u8>> {
        let mut buf = vec![0u8; len];
        if len == 0 {
            return Ok(buf);
        }
        let key = LayerKey { serial, gen };
        let Some(entry) = self.open_for_read(key)? else {
            return Err(OverlayError::Invalid("missing overlay payload file"));
        };
        read_all_at(&entry.file, offset, &mut buf)?;
        Ok(buf)
    }

    /// Copies `[offset, offset + len)` from `(serial, from_gen)` into `(serial, to_gen)`.
    pub(crate) fn copy_between_layers(
        &mut self,
        serial: i64,
        from_gen: i64,
        to_gen: i64,
        offset: u64,
        len: usize,
        in_tx: bool,
    ) -> OverlayResult<()> {
        let data = self.read_exact(serial, from_gen, offset, len)?;
        self.write_at(serial, to_gen, offset, &data, in_tx)
    }

    /// Schedules removal of `(serial, gen)`'s loose file once the current transaction commits.
    pub(crate) fn defer_remove_layer(&mut self, serial: i64, gen: i64, in_tx: bool) {
        let key = LayerKey { serial, gen };
        if in_tx {
            self.pending.push(PendingOp::RemoveLayer(key));
        } else {
            self.remove_layer_now(key);
        }
    }

    /// Schedules punching a sparse hole in `(serial, gen)` once the current transaction commits.
    pub(crate) fn defer_punch_hole(
        &mut self,
        serial: i64,
        gen: i64,
        offset: u64,
        length: u64,
        in_tx: bool,
    ) {
        if length < PUNCH_HOLE_MIN_BYTES {
            return;
        }
        let key = LayerKey { serial, gen };
        if in_tx {
            self.pending.push(PendingOp::PunchHole {
                key,
                offset,
                length,
            });
        } else {
            self.punch_hole_now(key, offset, length);
        }
    }

    /// Finalizes all deferred loose-file deletions/hole-punches after SQLite `COMMIT`.
    pub(crate) fn commit_pending(&mut self) {
        self.committed_bytes = apply_delta(self.committed_bytes, self.staged_delta);
        self.staged_delta = 0;
        self.undo.clear();
        let ops = std::mem::take(&mut self.pending);
        for op in ops {
            match op {
                PendingOp::RemoveLayer(key) => self.remove_layer_now(key),
                PendingOp::PunchHole {
                    key,
                    offset,
                    length,
                } => self.punch_hole_now(key, offset, length),
            }
        }
    }

    /// Reverts any in-transaction loose-file writes and drops deferred deletions on SQLite `ROLLBACK`.
    pub(crate) fn rollback_pending(&mut self) {
        self.staged_delta = 0;
        self.pending.clear();
        let undo = std::mem::take(&mut self.undo);
        for entry in undo.into_iter().rev() {
            match entry {
                UndoEntry::Truncate { key, previous_len } => {
                    if let Ok(Some(cached)) = self.open_for_read(key) {
                        if cached.file.set_len(previous_len).is_ok() {
                            cached.len = previous_len;
                        }
                    }
                }
                UndoEntry::RestoreBytes {
                    key,
                    offset,
                    previous,
                } => {
                    if let Ok(Some(cached)) = self.open_for_read(key) {
                        let _ = write_all_at(&cached.file, offset, &previous);
                    }
                }
                UndoEntry::RemoveNewFile(key) => {
                    self.remove_layer_now(key);
                }
            }
        }
    }

    /// Closes all cached handles and deletes the entire sidecar `.blobs` directory.
    pub(crate) fn cleanup_all(&mut self) {
        self.pending.clear();
        self.undo.clear();
        self.handles.clear();
        if self.created_root || self.root.exists() {
            let _ = std::fs::remove_dir_all(&self.root);
            self.created_root = false;
        }
    }

    fn remove_layer_now(&mut self, key: LayerKey) {
        self.handles.remove(&key);
        let path = self.layer_path(key);
        let _ = std::fs::remove_file(path);
    }

    fn punch_hole_now(&mut self, key: LayerKey, offset: u64, length: u64) {
        #[cfg(target_os = "windows")]
        {
            if let Ok(Some(cached)) = self.open_for_read(key) {
                let _ = layerfs_win32_host::punch_hole(&cached.file, offset, length);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (key, offset, length);
        }
    }
}

impl Drop for PayloadStore {
    fn drop(&mut self) {
        self.cleanup_all();
    }
}

fn apply_delta(base: u64, delta: i64) -> u64 {
    if delta >= 0 {
        base.saturating_add(delta as u64)
    } else {
        base.saturating_sub(delta.unsigned_abs())
    }
}

#[cfg(windows)]
fn write_all_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "overlay payload write returned 0 bytes",
            ));
        }
        offset = offset.saturating_add(written as u64);
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "overlay payload write returned 0 bytes",
            ));
        }
        offset = offset.saturating_add(written as u64);
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_all_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let read = file.seek_read(buf, offset)?;
        if read == 0 {
            break;
        }
        offset = offset.saturating_add(read as u64);
        buf = &mut buf[read..];
    }
    Ok(())
}

#[cfg(unix)]
fn read_all_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        let read = file.read_at(buf, offset)?;
        if read == 0 {
            break;
        }
        offset = offset.saturating_add(read as u64);
        buf = &mut buf[read..];
    }
    Ok(())
}
