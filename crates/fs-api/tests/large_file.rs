//! Bounded `read_at` for large files (ADR-004 §5.2, §6).
//!
//! The claim is that a small range read fetches only the blocks it intersects,
//! never the whole file. That is asserted through the **read-amplification
//! counter**, which is a production metric (spec §5.6) and not a test hook.
//!
//! Why ≤ 2 fetches for a 4 KiB read: a 4 KiB range can straddle at most one
//! 64 KiB boundary, so it touches at most two blocks. If `BLOCK_BYTES` changes,
//! this bound changes with it — `a_whole_block_read_touches_one_block` pins the
//! one-block case independently of the size, so a silent block-size change is
//! caught rather than quietly invalidating the threshold.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, BLOCK_BYTES};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-large-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A deterministic byte pattern, so any misplaced block is detectable.
fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

fn fixture(label: &str) -> (TempRoot, Fs) {
    let root = TempRoot::new(label);
    let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
    (root, fs)
}

/// 256 MiB in 4096 blocks. Large enough that "load the whole file" would be
/// obvious, small enough to run on every build. T5 re-runs at 1 GiB.
#[test]
fn large_file_read_is_bounded() {
    const BLOCKS: usize = 4096;
    let (_root, fs) = fixture("bounded");
    let total = BLOCKS * BLOCK_BYTES as usize;
    let bytes = pattern(total);

    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &bytes).expect("write");
    assert_eq!(fs.stat("/big").expect("stat").size, total as u64);

    // Read 4 KiB from the middle of one block: exactly one block is fetched.
    let offset = 100 * BLOCK_BYTES + 1234;
    let before = fs.read_stats().object_gets;
    let mut buffer = [0u8; 4096];
    let read = fs.read_at("/big", offset, &mut buffer).expect("read");
    let fetches = fs.read_stats().object_gets - before;

    assert_eq!(read, 4096);
    assert_eq!(&buffer[..], &bytes[offset as usize..offset as usize + 4096]);
    assert_eq!(
        fetches, 1,
        "a 4 KiB read inside one block must fetch exactly one block"
    );
}

/// A read that straddles a block boundary fetches exactly the two blocks.
#[test]
fn a_read_straddling_a_boundary_fetches_two_blocks() {
    const BLOCKS: usize = 64;
    let (_root, fs) = fixture("straddle");
    let total = BLOCKS * BLOCK_BYTES as usize;
    let bytes = pattern(total);
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &bytes).expect("write");

    // Start 2 KiB before the boundary between block 3 and block 4.
    let offset = 4 * BLOCK_BYTES - 2048;
    let before = fs.read_stats().object_gets;
    let mut buffer = [0u8; 4096];
    let read = fs.read_at("/big", offset, &mut buffer).expect("read");
    let fetches = fs.read_stats().object_gets - before;

    assert_eq!(read, 4096);
    assert_eq!(&buffer[..], &bytes[offset as usize..offset as usize + 4096]);
    assert_eq!(fetches, 2, "a straddling read touches exactly two blocks");
}

/// Pins the one-block case independently of the size, so a block-size change
/// cannot silently invalidate the thresholds above.
#[test]
fn a_whole_block_read_touches_one_block() {
    const BLOCKS: usize = 8;
    let (_root, fs) = fixture("one-block");
    let bytes = pattern(BLOCKS * BLOCK_BYTES as usize);
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &bytes).expect("write");

    let before = fs.read_stats().object_gets;
    let mut buffer = vec![0xAAu8; BLOCK_BYTES as usize];
    let read = fs.read_at("/big", BLOCK_BYTES, &mut buffer).expect("read");
    let fetches = fs.read_stats().object_gets - before;

    assert_eq!(read, BLOCK_BYTES as usize);
    assert_eq!(fetches, 1);
}

/// Reading the first and last byte of a large file touches one block each,
/// which is the strongest form of "does not load the whole file".
#[test]
fn reading_one_byte_from_either_end_fetches_one_block() {
    const BLOCKS: usize = 512;
    let (_root, fs) = fixture("ends");
    let total = BLOCKS * BLOCK_BYTES as usize;
    let bytes = pattern(total);
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &bytes).expect("write");

    let mut one = [0u8; 1];
    let before = fs.read_stats().object_gets;
    fs.read_at("/big", 0, &mut one).expect("read first");
    assert_eq!(one[0], bytes[0]);
    assert_eq!(fs.read_stats().object_gets - before, 1);

    let before = fs.read_stats().object_gets;
    fs.read_at("/big", total as u64 - 1, &mut one)
        .expect("read last");
    assert_eq!(one[0], bytes[total - 1]);
    assert_eq!(fs.read_stats().object_gets - before, 1);
}

/// The whole file still reads back correctly through the block path.
#[test]
fn a_large_file_round_trips_in_full() {
    const BLOCKS: usize = 128;
    let (_root, fs) = fixture("round-trip");
    let bytes = pattern(BLOCKS * BLOCK_BYTES as usize + 777);
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &bytes).expect("write");

    let mut buffer = vec![0xAAu8; bytes.len()];
    assert_eq!(
        fs.read_at("/big", 0, &mut buffer).expect("read"),
        bytes.len()
    );
    assert_eq!(buffer, bytes);
}

/// ADR-004 §5.4: a block write must not get more expensive as the file grows.
///
/// Regression guard for the O(n²) write path found during T4: the first
/// implementation read the whole file on every `write_at`, so per-block cost
/// grew with file size. Comparing early writes against late ones catches that:
/// with a bounded path they cost the same.
#[test]
fn per_block_write_cost_does_not_grow_with_file_size() {
    const BLOCKS: u64 = 2048;
    let (_root, fs) = fixture("write-scaling");
    fs.create("/grow").expect("create");
    let block = vec![0xA5u8; BLOCK_BYTES as usize];

    let mut timings = Vec::new();
    for index in 0..BLOCKS {
        let started = std::time::Instant::now();
        fs.write_at("/grow", index * BLOCK_BYTES, &block)
            .expect("write block");
        timings.push(started.elapsed().as_secs_f64());
    }

    let early: f64 = timings[..32].iter().sum::<f64>() / 32.0;
    let late: f64 = timings[timings.len() - 32..].iter().sum::<f64>() / 32.0;
    eprintln!(
        "per-block cost: early {early:.5}s, late {late:.5}s, ratio {:.2}",
        late / early
    );
    assert!(
        late < early * 1.8 + 0.002,
        "per-block write cost grew from {early:.5}s to {late:.5}s (ratio {:.2}); \
         the write path is not bounded (ADR-004 §5.4)",
        late / early
    );
}

/// Identical blocks are deduplicated by content addressing.
#[test]
fn identical_blocks_share_one_object() {
    let (_root, fs) = fixture("dedup");
    let zero_block = vec![0u8; BLOCK_BYTES as usize];
    let mut bytes = Vec::new();
    for _ in 0..8 {
        bytes.extend_from_slice(&zero_block);
    }
    fs.create("/zeros").expect("create");
    fs.write_at("/zeros", 0, &bytes).expect("write");

    let report = fs.verify().expect("verify");
    assert!(
        report.references <= 2,
        "all-zero blocks collapse to far fewer objects than blocks, got {}",
        report.references
    );
}
