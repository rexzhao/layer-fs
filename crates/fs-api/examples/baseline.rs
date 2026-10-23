//! The P1 performance baseline harness.
//!
//! Run with `cargo run -p fs-api --release --example baseline -- <scenario>`, or
//! `-- <scenario> --repeat N` to spread the measurement over N independent runs.
//! Every result is printed as raw CSV lines so the report can quote them
//! verbatim, and each scenario reports a distribution rather than a best value
//! (spec §8 item 6).
//!
//! Scenarios:
//!   `small`  — 100k small files: create, random read, rename, delete
//!   `large`  — 1 GiB file: sequential write, random write, random read
//!   `dedup`  — 100 paths sharing one content; reports logical/unique/stored
//!
//! The harness deliberately measures the **product** through `Fs`, not internals,
//! so the numbers describe what a caller experiences.

use std::path::PathBuf;
use std::time::Instant;

use fs_api::{Fs, OpenMode, BLOCK_BYTES};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let scenario = arguments.next().unwrap_or_else(|| {
        eprintln!("usage: baseline <small|large|dedup> [--repeat N]");
        std::process::exit(2)
    });
    let mut repeat = 1usize;
    let mut rest: Vec<String> = arguments.collect();
    if let Some(position) = rest.iter().position(|argument| argument == "--repeat") {
        if position + 1 < rest.len() {
            repeat = rest[position + 1].parse().unwrap_or(1).max(1);
        }
        rest.drain(position..(position + 2).min(rest.len()));
    }

    println!("# scenario={scenario} repeat={repeat}");
    match scenario.as_str() {
        "small" => small(repeat)?,
        "large" => large(repeat)?,
        "dedup" => dedup(repeat)?,
        other => {
            eprintln!("unknown scenario {other}");
            std::process::exit(2);
        }
    }
    Ok(())
}

/// A fresh directory per run, so runs cannot benefit from each other's caches.
///
/// Honours `WCAS_TEST_ROOT` like the acceptance tests: the reference machine's C:
/// drive commits at ~6ms and F: at ~0.8ms, and the baseline must be taken on the
/// fast device with the report saying which.
fn fresh_root(label: &str, run: usize) -> PathBuf {
    let base = std::env::var_os("WCAS_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!(
        "fs-baseline-{label}-{}-{run}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create root");
    path
}

fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * percent).round() as usize;
    sorted[index]
}

fn describe(label: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    println!(
        "{label},n={},mean={mean:.4},min={:.4},p50={:.4},p95={:.4},p99={:.4},max={:.4}",
        samples.len(),
        samples.first().copied().unwrap_or(0.0),
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        percentile(&samples, 0.99),
        samples.last().copied().unwrap_or(0.0)
    );
}

fn content_for(index: usize) -> Vec<u8> {
    let length = 16 + (index % 97);
    (0..length)
        .map(|offset| ((index * 31 + offset * 17) % 251) as u8)
        .collect()
}

/// 100,000 small files: create, random read, rename, delete.
fn small(repeat: usize) -> Result<(), Box<dyn std::error::Error>> {
    const FILES: usize = 100_000;
    let mut create_runs = Vec::new();
    let mut read_runs = Vec::new();
    let mut rename_runs = Vec::new();
    let mut delete_runs = Vec::new();

    for run in 0..repeat {
        let root = fresh_root("small", run);
        let fs = Fs::open(&root, "default", OpenMode::Create)?;
        for index in 0..100 {
            fs.mkdir(&format!("/dir{index:02}"))?;
        }

        let started = Instant::now();
        for index in 0..FILES {
            let path = format!("/dir{:02}/f{:06}", index % 100, index);
            fs.create(&path)?;
            fs.write_at(&path, 0, &content_for(index))?;
        }
        let create = started.elapsed().as_secs_f64();
        create_runs.push(create);
        println!(
            "small.create,run={run},files={FILES},seconds={create:.3},files_per_s={:.0}",
            FILES as f64 / create
        );

        // Random reads: a stride coprime with FILES visits distinct files.
        let started = Instant::now();
        let mut read_count = 0usize;
        for step in 0..10_000usize {
            let index = (step * 7919) % FILES;
            let path = format!("/dir{:02}/f{:06}", index % 100, index);
            let expected = content_for(index);
            let mut buffer = vec![0xAAu8; expected.len()];
            let read = fs.read_at(&path, 0, &mut buffer)?;
            assert_eq!(&buffer[..read], &expected[..], "read back differs");
            read_count += 1;
        }
        let read = started.elapsed().as_secs_f64();
        read_runs.push(read);
        println!(
            "small.read,run={run},reads={read_count},seconds={read:.3},ops_per_s={:.0}",
            read_count as f64 / read
        );

        let started = Instant::now();
        let mut renamed = 0usize;
        for index in (0..FILES).step_by(50) {
            let from = format!("/dir{:02}/f{:06}", index % 100, index);
            let to = format!("/dir{:02}/r{:06}", index % 100, index);
            fs.rename(&from, &to)?;
            renamed += 1;
        }
        let rename = started.elapsed().as_secs_f64();
        rename_runs.push(rename);
        println!(
            "small.rename,run={run},ops={renamed},seconds={rename:.3},ops_per_s={:.0}",
            renamed as f64 / rename
        );

        let started = Instant::now();
        let mut deleted = 0usize;
        for index in (1..FILES).step_by(50) {
            let path = format!("/dir{:02}/f{:06}", index % 100, index);
            fs.remove(&path)?;
            deleted += 1;
        }
        let delete = started.elapsed().as_secs_f64();
        delete_runs.push(delete);
        println!(
            "small.delete,run={run},ops={deleted},seconds={delete:.3},ops_per_s={:.0}",
            deleted as f64 / delete
        );

        // Storage accounting for the spec §5.6 indicators.
        let logical: u64 = (0..FILES)
            .step_by(97)
            .map(|index| content_for(index).len() as u64)
            .sum();
        println!(
            "small.storage,run={run},objects={},logical_sampled={logical}",
            fs.stats()?.objects
        );
        drop(fs);
        let _ = std::fs::remove_dir_all(&root);
    }

    describe("small.create_seconds", create_runs);
    describe("small.read_seconds", read_runs);
    describe("small.rename_seconds", rename_runs);
    describe("small.delete_seconds", delete_runs);
    Ok(())
}

/// A 1 GiB file: sequential write with distinct blocks, random write, random read.
fn large(repeat: usize) -> Result<(), Box<dyn std::error::Error>> {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut seq_runs = Vec::new();
    let mut write_runs = Vec::new();
    let mut read_latencies = Vec::new();

    for run in 0..repeat {
        let root = fresh_root("large", run);
        let fs = Fs::open(&root, "default", OpenMode::Create)?;
        fs.create("/big")?;

        let block_index: Vec<u8> = (0..BLOCK_BYTES)
            .map(|offset| (offset % 251) as u8)
            .collect();
        let started = Instant::now();
        let mut written = 0u64;
        while written < GIB {
            let remaining = (GIB - written).min(BLOCK_BYTES);
            let mut block = block_index.clone();
            block[..8].copy_from_slice(&(written / BLOCK_BYTES).to_le_bytes());
            fs.write_at("/big", written, &block[..remaining as usize])?;
            written += remaining;
        }
        let seq = started.elapsed().as_secs_f64();
        seq_runs.push(seq);
        println!(
            "large.sequential_write,run={run},bytes={GIB},seconds={seq:.3},mib_per_s={:.1}",
            (GIB as f64 / 1_048_576.0) / seq
        );

        // Random 64 KiB writes over the existing file (rewrite, not append).
        let started = Instant::now();
        for step in 0..64u64 {
            let offset = (step * 15_485_863) % (GIB - BLOCK_BYTES);
            let mut block = block_index.clone();
            block[..8].copy_from_slice(&(offset / BLOCK_BYTES).to_le_bytes());
            fs.write_at("/big", offset, &block)?;
        }
        let rw = started.elapsed().as_secs_f64();
        write_runs.push(rw);
        println!(
            "large.random_write,run={run},ops=64,seconds={rw:.3},ops_per_s={:.1}",
            64.0 / rw
        );

        for step in 0..500u64 {
            let offset = (step * 5_000_003) % (GIB - BLOCK_BYTES) + 12345;
            let mut buffer = vec![0xAAu8; 4096];
            let before = fs.read_stats().object_gets;
            let started = Instant::now();
            let read = fs.read_at("/big", offset, &mut buffer)?;
            read_latencies.push(started.elapsed().as_secs_f64() * 1e6);
            assert_eq!(read, 4096);
            let gets = fs.read_stats().object_gets - before;
            assert!(gets <= 2, "a 4 KiB read fetched {gets} objects");
        }
        println!("large.random_read,run={run},ops=500");

        println!("large.storage,run={run},objects={}", fs.stats()?.objects);
        drop(fs);
        let _ = std::fs::remove_dir_all(&root);
    }

    describe("large.sequential_write_seconds", seq_runs);
    describe("large.random_write_seconds", write_runs);
    describe("large.random_read_us", read_latencies);
    Ok(())
}

/// 100 paths sharing one content: the spec §5.6 dedup indicators.
fn dedup(repeat: usize) -> Result<(), Box<dyn std::error::Error>> {
    const PATHS: usize = 100;
    for run in 0..repeat {
        let root = fresh_root("dedup", run);
        let fs = Fs::open(&root, "default", OpenMode::Create)?;
        let payload = vec![0x5Au8; 512 * 1024];

        for index in 0..PATHS {
            let path = format!("/f{index:03}");
            fs.create(&path)?;
            fs.write_at(&path, 0, &payload)?;
        }

        let objects = fs.stats()?.objects;
        let logical = payload.len() as u64 * PATHS as u64;
        let stored = object_files_bytes(root.as_path());
        println!(
            "dedup,run={run},paths={PATHS},logical_bytes={logical},unique_canonical_bytes={},objects={objects},stored_bytes={stored},dedup_saved_bytes={},dedup_ratio={:.2}",
            payload.len(),
            logical.saturating_sub(payload.len() as u64),
            logical as f64 / payload.len() as f64
        );
        drop(fs);
        let _ = std::fs::remove_dir_all(&root);
    }
    Ok(())
}

/// Total size of the object files on disk, i.e. stored bytes.
fn object_files_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let objects = root.join("objects");
    let Ok(level1) = std::fs::read_dir(&objects) else {
        return 0;
    };
    for entry1 in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(entry1.path()) else {
            continue;
        };
        for entry2 in level2.flatten() {
            let Ok(files) = std::fs::read_dir(entry2.path()) else {
                continue;
            };
            for file in files.flatten() {
                if let Ok(metadata) = file.metadata() {
                    total += metadata.len();
                }
            }
        }
    }
    total
}
