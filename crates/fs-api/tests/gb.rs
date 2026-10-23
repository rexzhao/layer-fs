//! The 1 GiB confirmation run (ADR-004 §6, §12).
//!
//! 256 MiB is covered on every build in `large_file.rs`; this test runs the same
//! claim at the scale the spec names. It is `#[ignore]`d by default because a
//! 1 GiB write costs about 30s, which would dominate a routine `cargo test`;
//! T5 runs it as part of the scale suite:
//!
//! ```text
//! cargo test -p fs-api --test gb --release -- --ignored --nocapture
//! ```
//!
//! Measured on Windows x86_64, release: 30.9s for the 1 GiB write, and the
//! random 4 KiB read touches exactly one block.

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

/// A 1 GiB file, then one 4 KiB random read, which must fetch one block.
#[test]
#[ignore = "writes 1 GiB; run explicitly or from T5"]
fn a_gi_b_file_answers_a_random_read_from_one_block() {
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
