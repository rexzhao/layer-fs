//! The 1 GiB **dedup ceiling** run (ADR-004 §6, §12; manager ruling T5 item 3).
//!
//! ## What this test does and does not measure
//!
//! It writes 16,384 blocks that are **all identical**, so content addressing
//! collapses the whole 1 GiB into **one object**. The ~31s it takes therefore
//! describes the deduplicated ceiling, **not** raw write throughput: almost no
//! object writing happens.
//!
//! The real P1 write baseline — 16,384 *distinct* blocks, 192s, ~5 MiB/s — is
//! measured by `acceptance_scale.rs::s3_gi_b_file_random_reads_are_bounded_and_correct`
//! and reported in `docs/reports/p1-baseline.md`.
//!
//! This test kept its value: it verifies that a large file whose blocks are all
//! the same really does deduplicate to a single object, and that a random 4 KiB
//! read into it still touches exactly one block. It is not a throughput test, and
//! the name and this header say so, so the next reader cannot mistake a
//! deduplicated number for a write baseline (that mistake was made once).
//!
//! ```text
//! cargo test -p fs-api --test gb --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, BLOCK_BYTES};

/// The 60s budget from the T4 card: a third of the two-minute red line, leaving
/// room for the rest of the suite.
const WRITE_BUDGET_SECS: u64 = 60;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-gb-{label}-{}-{}-{sequence}",
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

/// A 1 GiB file built from **identical** blocks deduplicates to one object, and a
/// 4 KiB random read into it still fetches one block.
///
/// The name states the dedup claim on purpose: this is the ceiling, not the write
/// throughput (see the module docs).
#[test]
#[ignore = "writes 1 GiB; run explicitly or from T5"]
fn a_one_gibibyte_file_of_identical_blocks_dedups_to_one_object_and_reads_one_block() {
    const BYTES: u64 = 1024 * 1024 * 1024;
    let root = TempRoot::new("read");
    let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");

    let started = std::time::Instant::now();
    fs.create("/big").expect("create");

    // Write in block-sized chunks so the test itself does not hold 1 GiB.
    let block = vec![0x5Au8; BLOCK_BYTES as usize];
    let mut written = 0u64;
    while written < BYTES {
        let remaining = (BYTES - written).min(BLOCK_BYTES);
        fs.write_at("/big", written, &block[..remaining as usize])
            .expect("write block");
        written += remaining;
    }
    let write_elapsed = started.elapsed();

    assert_eq!(fs.stat("/big").expect("stat").size, BYTES);

    let offset = 700 * 1024 * 1024 + 4096;
    let before = fs.read_stats().object_gets;
    let mut buffer = [0u8; 4096];
    let read = fs.read_at("/big", offset, &mut buffer).expect("read");
    let fetches = fs.read_stats().object_gets - before;

    println!(
        "1 GiB write: {:.1}s; random 4 KiB read touched {fetches} block(s)",
        write_elapsed.as_secs_f64()
    );

    assert_eq!(read, 4096);
    assert!(buffer.iter().all(|byte| *byte == 0x5A));
    assert_eq!(
        fetches, 1,
        "a 1 GiB file's random read still touches one block"
    );
    assert!(
        write_elapsed.as_secs() <= WRITE_BUDGET_SECS,
        "the 1 GiB write must stay inside the 60s budget, took {write_elapsed:?}"
    );
}
