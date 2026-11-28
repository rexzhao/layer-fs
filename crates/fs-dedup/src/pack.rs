//! Immutable append-only external `.pack` files with O(1) positional reads and a
//! sharded in-memory LRU decoded chunk cache.

use crate::chunker::{decompress_chunk_arc, Codec, Hash32, Hash32BuildHasher};
use crate::error::{Error, Result};
use crate::parallel_io::ParallelFile;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};

pub const PACK_MAGIC: &[u8; 8] = b"DDFSPCK1";
pub const PACK_HEADER_BYTES: u64 = 16;
pub const RECORD_MAGIC: u32 = 0x4348_4E4B; // "CHNK"
pub const RECORD_HEADER_BYTES: usize = 48;

pub const DEFAULT_PACK_TARGET_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB
pub const DEFAULT_CACHE_BYTES: usize = 256 * 1024 * 1024; // 256 MiB
const MAX_OPEN_PACK_READERS: usize = 1024;
const CACHE_SHARDS: usize = 16;

/// Location of a stored chunk inside a `.pack` file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkLoc {
    pub pack_id: u64,
    pub offset: u64,
    pub raw_len: u32,
    pub stored_len: u32,
    pub codec: Codec,
}

/// Read-at helper using positional OS I/O (lock-free concurrent reads on Windows & Unix).
pub fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> Result<()> {
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;

    while !buf.is_empty() {
        #[cfg(windows)]
        let n = file.seek_read(buf, offset)?;
        #[cfg(unix)]
        let n = file.read_at(buf, offset)?;
        if n == 0 {
            return Err(Error::Corrupt(format!(
                "unexpected EOF reading pack at offset {offset}"
            )));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Returns the filename path of pack `pack_id` inside `packs_dir`.
pub fn pack_path(packs_dir: &Path, pack_id: u64) -> PathBuf {
    packs_dir.join(format!("{pack_id:08}.pack"))
}

struct ActiveWriter {
    pack_id: u64,
    writer: BufWriter<File>,
    offset: u64,
}

/// Append-only pack writer serialized by an internal mutex; automatically rolls to a new
/// pack file when `pack_target_bytes` is reached.
pub struct PackWriter {
    packs_dir: PathBuf,
    pack_target_bytes: u64,
    next_pack_id: u64,
    active: Option<ActiveWriter>,
    /// Newly created or grown packs since the last `take_pack_updates()` call: `(pack_id, size_bytes, sealed)`.
    pending_packs: HashMap<u64, (u64, bool)>,
}

impl PackWriter {
    pub fn new(packs_dir: PathBuf, pack_target_bytes: u64, next_pack_id: u64) -> Self {
        Self {
            packs_dir,
            pack_target_bytes: pack_target_bytes.max(1024 * 1024),
            next_pack_id: next_pack_id.max(1),
            active: None,
            pending_packs: HashMap::new(),
        }
    }

    fn ensure_open(&mut self) -> Result<&mut ActiveWriter> {
        if self.active.is_none() {
            fs::create_dir_all(&self.packs_dir)?;
            // Never truncate an existing pack file: allocate a fresh `pack_id` using `create_new`
            // (O_EXCL / CREATE_NEW) so committed packs can never be overwritten.
            let (pack_id, mut file) = loop {
                let candidate = self.next_pack_id;
                self.next_pack_id += 1;
                let path = pack_path(&self.packs_dir, candidate);
                match OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                {
                    Ok(f) => break (candidate, f),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            };
            let mut hdr = [0u8; 16];
            hdr[..8].copy_from_slice(PACK_MAGIC);
            hdr[8..16].copy_from_slice(&pack_id.to_le_bytes());
            file.write_all(&hdr)?;
            self.pending_packs.insert(pack_id, (PACK_HEADER_BYTES, false));
            self.active = Some(ActiveWriter {
                pack_id,
                writer: BufWriter::with_capacity(256 * 1024, file),
                offset: PACK_HEADER_BYTES,
            });
        }
        Ok(self.active.as_mut().unwrap())
    }

    /// Appends one chunk record and returns its `ChunkLoc`.
    pub fn append(
        &mut self,
        hash: &Hash32,
        codec: Codec,
        raw_len: u32,
        stored: &[u8],
    ) -> Result<ChunkLoc> {
        let stored_len = u32::try_from(stored.len())
            .map_err(|_| Error::InvalidInput("chunk payload exceeds u32".into()))?;
        let record_bytes = RECORD_HEADER_BYTES as u64 + u64::from(stored_len);

        // Roll pack if current active pack is non-empty and adding this record exceeds target.
        if let Some(active) = &self.active {
            if active.offset > PACK_HEADER_BYTES
                && active.offset.saturating_add(record_bytes) > self.pack_target_bytes
            {
                self.seal_active()?;
            }
        }

        let active = self.ensure_open()?;
        let offset = active.offset;
        let pack_id = active.pack_id;

        let mut header = [0u8; RECORD_HEADER_BYTES];
        header[0..4].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
        header[4] = codec as u8;
        header[5] = 0; // flags
        header[6..8].copy_from_slice(&0u16.to_le_bytes());
        header[8..12].copy_from_slice(&raw_len.to_le_bytes());
        header[12..16].copy_from_slice(&stored_len.to_le_bytes());
        header[16..48].copy_from_slice(hash);

        active.writer.write_all(&header)?;
        active.writer.write_all(stored)?;
        active.offset += record_bytes;
        let new_offset = active.offset;
        self.pending_packs.insert(pack_id, (new_offset, false));

        Ok(ChunkLoc {
            pack_id,
            offset,
            raw_len,
            stored_len,
            codec,
        })
    }

    /// Flushes buffered bytes of the active pack so readers immediately observe appended records.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(active) = &mut self.active {
            active.writer.flush()?;
        }
        Ok(())
    }

    /// Flushes and durably syncs (`sync_data`) the active pack to storage so newly written
    /// chunks survive power loss before any old copy (overlay file or compacted pack) is deleted.
    pub fn sync_data(&mut self) -> Result<()> {
        if let Some(active) = &mut self.active {
            active.writer.flush()?;
            active.writer.get_ref().sync_data()?;
        }
        Ok(())
    }

    /// Flushes, syncs to disk, and seals the active pack.
    pub fn seal_active(&mut self) -> Result<()> {
        if let Some(mut active) = self.active.take() {
            active.writer.flush()?;
            active.writer.get_ref().sync_data()?;
            self.pending_packs.insert(active.pack_id, (active.offset, true));
        }
        Ok(())
    }

    /// Drains pack size/seal updates to persist in `meta.sqlite`.
    pub fn take_pack_updates(&mut self) -> Vec<(u64, u64, bool)> {
        self.pending_packs
            .drain()
            .map(|(id, (size, sealed))| (id, size, sealed))
            .collect()
    }

    /// Flushes and closes the active pack handle without marking it sealed, so another
    /// serialized writer can safely resume or allocate the next pack after `pack.lock` releases.
    pub fn close_active(&mut self) -> Result<()> {
        if let Some(mut active) = self.active.take() {
            active.writer.flush()?;
        }
        Ok(())
    }

    /// Synchronizes `next_pack_id` and optionally resumes the latest committed unsealed pack
    /// from SQLite while holding the exclusive cross-process store pack lock.
    pub fn sync_from_db(
        &mut self,
        db_next_pack_id: u64,
        unsealed: Option<(u64, u64)>,
    ) -> Result<()> {
        self.close_active()?;
        self.pending_packs.clear();
        self.next_pack_id = db_next_pack_id.max(1);
        if let Some((pid, sz)) = unsealed {
            if sz < self.pack_target_bytes {
                self.resume_pack(pid, sz)?;
            }
        }
        Ok(())
    }

    /// Resumes writing into an existing unsealed pack if one exists (optional optimization).
    pub fn resume_pack(&mut self, pack_id: u64, size_bytes: u64) -> Result<()> {
        let path = pack_path(&self.packs_dir, pack_id);
        if !path.exists() {
            return Ok(());
        }
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.set_len(size_bytes)?;
        file.seek(SeekFrom::End(0))?;
        self.active = Some(ActiveWriter {
            pack_id,
            writer: BufWriter::with_capacity(256 * 1024, file),
            offset: size_bytes,
        });
        if self.next_pack_id <= pack_id {
            self.next_pack_id = pack_id + 1;
        }
        Ok(())
    }
}

type SingleFlightOutcome = std::result::Result<Arc<[u8]>, String>;
type SingleFlightSlot = Arc<(Mutex<Option<SingleFlightOutcome>>, Condvar)>;

struct CacheShard {
    map: HashMap<Hash32, (Arc<[u8]>, bool), Hash32BuildHasher>,
    in_flight: HashMap<Hash32, SingleFlightSlot, Hash32BuildHasher>,
    order: VecDeque<Hash32>,
    bytes: usize,
    limit: usize,
}

impl CacheShard {
    fn new(limit: usize) -> Self {
        Self {
            map: HashMap::with_hasher(Hash32BuildHasher::default()),
            in_flight: HashMap::with_hasher(Hash32BuildHasher::default()),
            order: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }

    /// CLOCK-LRU lookup: marks the hit entry referenced so hot chunks get second chances.
    fn get(&mut self, hash: &Hash32) -> Option<Arc<[u8]>> {
        if let Some((data, referenced)) = self.map.get_mut(hash) {
            *referenced = true;
            return Some(Arc::clone(data));
        }
        None
    }

    fn insert(&mut self, hash: Hash32, data: Arc<[u8]>) {
        if self.limit == 0 || data.len() > self.limit {
            return;
        }
        if let Some((_, referenced)) = self.map.get_mut(&hash) {
            *referenced = true;
            return;
        }
        while self.bytes.saturating_add(data.len()) > self.limit {
            let Some(cand) = self.order.pop_front() else {
                break;
            };
            if let Some((_, referenced)) = self.map.get_mut(&cand) {
                if *referenced {
                    // Second chance: clear reference bit and re-queue at tail.
                    *referenced = false;
                    self.order.push_back(cand);
                    continue;
                }
            }
            if let Some((old, _)) = self.map.remove(&cand) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }
        self.bytes += data.len();
        self.order.push_back(hash);
        self.map.insert(hash, (data, true));
    }
}

struct ReaderPool {
    handles: HashMap<u64, Arc<ParallelFile>>,
    order: VecDeque<u64>,
}

impl ReaderPool {
    fn new() -> Self {
        Self {
            handles: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_cached(&self, pack_id: u64) -> Option<Arc<ParallelFile>> {
        self.handles.get(&pack_id).cloned()
    }

    fn insert_opened(&mut self, pack_id: u64, file: Arc<ParallelFile>) -> Arc<ParallelFile> {
        if let Some(existing) = self.handles.get(&pack_id) {
            return Arc::clone(existing);
        }
        while self.handles.len() >= MAX_OPEN_PACK_READERS {
            if let Some(old) = self.order.pop_front() {
                self.handles.remove(&old);
            } else {
                break;
            }
        }
        self.handles.insert(pack_id, Arc::clone(&file));
        self.order.push_back(pack_id);
        file
    }

    fn evict(&mut self, pack_id: u64) {
        self.handles.remove(&pack_id);
        self.order.retain(|&id| id != pack_id);
    }
}

/// Thread-safe pack store supporting genuinely parallel `FILE_FLAG_OVERLAPPED` chunk reads,
/// single-flight decompression, zero-copy `Arc<[u8]>` cache insertion, and serialized appends.
pub struct PackStore {
    packs_dir: PathBuf,
    writer: Mutex<PackWriter>,
    readers: RwLock<ReaderPool>,
    cache: Vec<Mutex<CacheShard>>,
}

impl PackStore {
    pub fn open(
        packs_dir: PathBuf,
        pack_target_bytes: u64,
        next_pack_id: u64,
        cache_bytes: usize,
    ) -> Self {
        let per_shard = cache_bytes / CACHE_SHARDS;
        let mut cache = Vec::with_capacity(CACHE_SHARDS);
        for _ in 0..CACHE_SHARDS {
            cache.push(Mutex::new(CacheShard::new(per_shard)));
        }
        Self {
            packs_dir: packs_dir.clone(),
            writer: Mutex::new(PackWriter::new(packs_dir, pack_target_bytes, next_pack_id)),
            readers: RwLock::new(ReaderPool::new()),
            cache,
        }
    }

    fn open_reader(&self, pack_id: u64) -> Result<Arc<ParallelFile>> {
        if let Some(f) = self.readers.read().unwrap().get_cached(pack_id) {
            return Ok(f);
        }
        // Open overlapped pack handle OUTSIDE the pool write lock so concurrent threads never
        // serialize on `CreateFileW` or on synchronous Windows `FILE_OBJECT` I/O locks!
        let opened = Arc::new(ParallelFile::open_read(&pack_path(&self.packs_dir, pack_id))?);
        Ok(self.readers.write().unwrap().insert_opened(pack_id, opened))
    }

    pub fn packs_dir(&self) -> &Path {
        &self.packs_dir
    }

    pub fn writer(&self) -> std::sync::MutexGuard<'_, PackWriter> {
        self.writer.lock().unwrap()
    }

    /// Reads raw uncompressed bytes of `hash` at `loc`, using the sharded CLOCK-LRU cache,
    /// `FILE_FLAG_OVERLAPPED` parallel kernel I/O, single-flight decompression coalescing, and
    /// single-allocation decompression directly into `Arc<[u8]>`.
    pub fn read_chunk(&self, hash: &Hash32, loc: ChunkLoc) -> Result<Arc<[u8]>> {
        let shard_idx = (hash[0] as usize) & (CACHE_SHARDS - 1);
        let leader_slot = {
            let mut shard = self.cache[shard_idx]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(hit) = shard.get(hash) {
                return Ok(hit);
            }
            if let Some(existing) = shard.in_flight.get(hash) {
                let wait_slot = Arc::clone(existing);
                drop(shard);
                let (lock, cvar) = &*wait_slot;
                let mut res = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while res.is_none() {
                    res = cvar
                        .wait(res)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                return match res.as_ref().unwrap() {
                    Ok(arc) => Ok(Arc::clone(arc)),
                    Err(msg) => Err(Error::Other(msg.clone())),
                };
            }
            let slot: SingleFlightSlot = Arc::new((Mutex::new(None), Condvar::new()));
            shard.in_flight.insert(*hash, Arc::clone(&slot));
            slot
        };

        // RAII guard (N6): guarantees `in_flight` removal and waiter wakeup even if the leader
        // panics mid-read/decompress so waiting threads can never hang indefinitely.
        struct InFlightGuard<'a> {
            shard: &'a Mutex<CacheShard>,
            hash: Hash32,
            slot: SingleFlightSlot,
            completed: bool,
        }
        impl InFlightGuard<'_> {
            fn complete(mut self, outcome: &Result<Arc<[u8]>>) {
                self.completed = true;
                let pub_outcome: SingleFlightOutcome = match outcome {
                    Ok(arc) => Ok(Arc::clone(arc)),
                    Err(err) => Err(err.to_string()),
                };
                {
                    let mut shard = self
                        .shard
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Ok(arc) = outcome {
                        shard.insert(self.hash, Arc::clone(arc));
                    }
                    shard.in_flight.remove(&self.hash);
                }
                let (lock, cvar) = &*self.slot;
                *lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pub_outcome);
                cvar.notify_all();
            }
        }
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                if !self.completed {
                    {
                        let mut shard = self
                            .shard
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        shard.in_flight.remove(&self.hash);
                    }
                    let (lock, cvar) = &*self.slot;
                    *lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(Err("single-flight chunk reader panicked".to_owned()));
                    cvar.notify_all();
                }
            }
        }

        let guard = InFlightGuard {
            shard: &self.cache[shard_idx],
            hash: *hash,
            slot: leader_slot,
            completed: false,
        };

        let outcome = self.read_chunk_uncached(hash, loc);
        guard.complete(&outcome);
        outcome
    }

    fn read_chunk_uncached(&self, hash: &Hash32, loc: ChunkLoc) -> Result<Arc<[u8]>> {
        let file = self.open_reader(loc.pack_id)?;
        let total_len = RECORD_HEADER_BYTES + loc.stored_len as usize;
        let mut record = vec![0u8; total_len];
        file.read_exact_at(loc.offset, &mut record)?;

        let magic = u32::from_le_bytes(record[0..4].try_into().unwrap());
        if magic != RECORD_MAGIC {
            return Err(Error::Corrupt(format!(
                "bad chunk record magic in pack {} at offset {}",
                loc.pack_id, loc.offset
            )));
        }
        let codec = Codec::from_u8(record[4])?;
        let raw_len = u32::from_le_bytes(record[8..12].try_into().unwrap());
        let stored_len = u32::from_le_bytes(record[12..16].try_into().unwrap());
        if raw_len != loc.raw_len || stored_len != loc.stored_len || &record[16..48] != hash {
            return Err(Error::Corrupt(format!(
                "chunk header mismatch in pack {} at offset {}",
                loc.pack_id, loc.offset
            )));
        }

        decompress_chunk_arc(codec, &record[RECORD_HEADER_BYTES..], raw_len as usize)
    }

    /// Reads raw on-disk stored bytes (still compressed) for pack GC compaction without re-compressing.
    pub fn read_stored_payload(&self, hash: &Hash32, loc: ChunkLoc) -> Result<Vec<u8>> {
        let file = self.open_reader(loc.pack_id)?;
        let total_len = RECORD_HEADER_BYTES + loc.stored_len as usize;
        let mut record = vec![0u8; total_len];
        file.read_exact_at(loc.offset, &mut record)?;
        let magic = u32::from_le_bytes(record[0..4].try_into().unwrap());
        if magic != RECORD_MAGIC || &record[16..48] != hash {
            return Err(Error::Corrupt(format!(
                "corrupt record during GC in pack {} at offset {}",
                loc.pack_id, loc.offset
            )));
        }
        Ok(record[RECORD_HEADER_BYTES..].to_vec())
    }

    /// Evicts any cached open reader handle and deletes pack `pack_id` from disk.
    pub fn delete_pack_file(&self, pack_id: u64) -> Result<()> {
        self.readers.write().unwrap().evict(pack_id);
        let path = pack_path(&self.packs_dir, pack_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n6_single_flight_waiters_never_hang_when_leader_fails() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dedupfs_n6_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(PackStore::open(dir.clone(), 16 * 1024 * 1024, 1, 1024 * 1024));
        let hash = [0x5Au8; 32];
        let bad_loc = ChunkLoc {
            pack_id: 9999, // missing pack file triggers error on leader
            offset: 16,
            raw_len: 128,
            stored_len: 128,
            codec: Codec::None,
        };

        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let st = Arc::clone(&store);
                let bar = Arc::clone(&barrier);
                s.spawn(move || {
                    bar.wait();
                    let err = st.read_chunk(&hash, bad_loc);
                    assert!(err.is_err(), "missing pack must return Err to leader & all waiters");
                });
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
