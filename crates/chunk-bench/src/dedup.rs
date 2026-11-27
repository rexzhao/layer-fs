//! Low-contention sharded 128-bit hash set for tracking unique chunks across tens of
//! millions of chunks with minimal RAM (~20 bytes/chunk) and zero SipHash overhead.

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const SHARDS: usize = 64;

/// Identity hasher for already-uniform 128-bit BLAKE3 prefixes.
#[derive(Default)]
pub struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() >= 8 {
            self.0 = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        }
    }

    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.0 = i as u64;
    }
}

type FastU128Set = HashSet<u128, BuildHasherDefault<IdentityHasher>>;

/// Concurrent sharded set tracking unique chunks and totals for one chunking strategy.
pub struct ShardedChunkSet {
    shards: Vec<Mutex<FastU128Set>>,
    total_chunks: AtomicU64,
    total_bytes: AtomicU64,
    unique_chunks: AtomicU64,
    unique_bytes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DedupSnapshot {
    pub total_chunks: u64,
    pub total_bytes: u64,
    pub unique_chunks: u64,
    pub unique_bytes: u64,
}

impl ShardedChunkSet {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(FastU128Set::default()));
        }
        Self {
            shards,
            total_chunks: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            unique_chunks: AtomicU64::new(0),
            unique_bytes: AtomicU64::new(0),
        }
    }

    /// Records a chunk and returns `true` if this chunk was newly inserted (unique).
    #[inline]
    pub fn record(&self, hash: u128, len: u64) -> bool {
        self.total_chunks.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_add(len, Ordering::Relaxed);
        let shard_idx = ((hash >> 122) as usize) & (SHARDS - 1);
        let is_new = {
            let mut guard = self.shards[shard_idx].lock().unwrap();
            guard.insert(hash)
        };
        if is_new {
            self.unique_chunks.fetch_add(1, Ordering::Relaxed);
            self.unique_bytes.fetch_add(len, Ordering::Relaxed);
        }
        is_new
    }

    pub fn snapshot(&self) -> DedupSnapshot {
        DedupSnapshot {
            total_chunks: self.total_chunks.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            unique_chunks: self.unique_chunks.load(Ordering::Relaxed),
            unique_bytes: self.unique_bytes.load(Ordering::Relaxed),
        }
    }
}
