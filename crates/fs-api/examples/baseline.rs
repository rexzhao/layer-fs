//! The P2a performance baseline (ADR-006 §18, `docs/process/p2a8-card.md` §2).
//!
//! This is **data, not a gate**: nothing here asserts a timing. It exists so the report in
//! `docs/reports/p2a-baseline.md` has reproducible numbers, measured under the rules that
//! report states (R1–R5, carried over from `p1-baseline.md`):
//!
//! ```sh
//! cargo run -p fs-api --release --example baseline -- write --repeat 3
//! cargo run -p fs-api --release --example baseline -- flush --repeat 3
//! cargo run -p fs-api --release --example baseline -- snapshot --repeat 3
//! cargo run -p fs-api --release --example baseline -- read --repeat 3
//! cargo run -p fs-api --release --example baseline -- storage --repeat 3
//! cargo run -p fs-api --release --example baseline -- everything --repeat 3
//! ```
//!
//! Every scenario prints one line per repeat, then a summary. The transition and storage
//! scenarios also print their load characteristics, because a number without them cannot be
//! read (rule R3).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use fs_api::{Repo, Workspace};

/// How many repeats a scenario does by default (rule R1).
const REPEATS: usize = 3;

fn main() {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    let repeat = take_option(&mut arguments, "--repeat")
        .and_then(|text| text.parse().ok())
        .unwrap_or(REPEATS);
    let scenario = arguments
        .first()
        .cloned()
        .unwrap_or_else(|| "everything".into());
    let keep = arguments.iter().any(|a| a == "--keep");

    println!(
        "load: release=builtin repeat={repeat} root={}",
        root_dir().display()
    );
    match scenario.as_str() {
        "write" => write_latency(repeat),
        "flush" => flush_cost(repeat),
        "snapshot" => snapshot_cost(repeat),
        "read" => read_latency(repeat),
        "storage" => storage_efficiency(repeat),
        "everything" => {
            write_latency(repeat);
            flush_cost(repeat);
            snapshot_cost(repeat);
            read_latency(repeat);
            storage_efficiency(repeat);
        }
        other => {
            eprintln!("unknown scenario {other}");
            std::process::exit(2);
        }
    }
    if keep {
        println!("kept: {}", root_dir().display());
    }
}

// ------------------------------------------------------------------ scenarios

/// `write_at` on an overlay, sustained long enough to get past the write cache (rule R2).
fn write_latency(repeat: usize) {
    println!("\n== write_at / truncate on an overlay (1000 files, 3 000 ops per repeat)");
    println!(
        "   load: distinct 64 B payloads, files created first, hot overlay (no fsync per write)"
    );
    for run in 0..repeat {
        let (root, repo) = setup("write");
        let workspace = repo.workspace_create("w1", "main").expect("workspace");
        // Create the files first so creation is not part of the write latency.
        for index in 0..1000 {
            workspace
                .create(&format!("/f{index:04}"))
                .expect("create file");
        }
        let mut samples = Vec::with_capacity(3000);
        for round in 0..3 {
            for index in 0..1000 {
                let path = format!("/f{index:04}");
                let payload = [u8::try_from((index + round) % 256).unwrap_or(0); 64];
                let started = Instant::now();
                workspace.write_at(&path, 0, &payload).expect("write");
                samples.push(started.elapsed());
            }
        }
        let summary = Summary::of(&samples);
        println!("   run {run}: {summary}");
        let flush = {
            let started = Instant::now();
            workspace.flush().expect("flush");
            started.elapsed()
        };
        println!("   run {run}: flush after 3 000 writes: {}", ms(flush));
        workspace.close().expect("close");
        drop(repo);
        cleanup(root);
    }
}

/// `flush` cost against the number of dirty pages (rule R3: the load is stated).
fn flush_cost(repeat: usize) {
    println!("\n== flush cost against dirty-page count");
    println!("   load: N files of 64 B each written then flushed once");
    for count in [100usize, 1000, 10_000] {
        let mut samples = Vec::new();
        for run in 0..repeat {
            let (root, repo) = setup("flush");
            let workspace = repo.workspace_create("w1", "main").expect("workspace");
            for index in 0..count {
                let path = format!("/f{index:05}");
                workspace.create(&path).expect("create");
                let payload = [u8::try_from(index % 256).unwrap_or(0); 64];
                workspace.write_at(&path, 0, &payload).expect("write");
            }
            let started = Instant::now();
            workspace.flush().expect("flush");
            let elapsed = started.elapsed();
            println!("   N={count} run {run}: {}", ms(elapsed));
            samples.push(elapsed);
            workspace.close().expect("close");
            drop(repo);
            cleanup(root);
        }
        println!("   N={count} summary: {}", Summary::of(&samples));
    }
}

/// Status transitions against tree size and change volume.
///
/// `change 0` is the interesting one: it publishes the same tree twice and shows how much
/// of the second publication is `AlreadyPresent` rather than new bytes.
fn snapshot_cost(repeat: usize) {
    println!("\n== status transitions (1000 files, 64 B each)");
    println!("   load: distinct payloads; times are wall clock for the whole call");
    for changes in [0usize, 10, 100] {
        let mut snapshot_samples = Vec::new();
        let mut commit_samples = Vec::new();
        for run in 0..repeat {
            let (root, repo) = setup("snapshot");
            let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
            fill(&workspace, 1000);
            workspace.commit("first", false).expect("first commit");
            // Change `changes` files, then publish.
            for index in 0..changes {
                let path = format!("/f{index:04}");
                let payload = [0xA5u8; 64];
                workspace.write_at(&path, 0, &payload).expect("write");
            }
            let started = Instant::now();
            workspace.snapshot().expect("snapshot");
            let snapshot_elapsed = started.elapsed();
            snapshot_samples.push(snapshot_elapsed);
            // A second change set, then commit.
            for index in 0..changes.max(1) {
                let path = format!("/c{index:04}");
                if workspace.stat(&path).is_err() {
                    workspace.create(&path).expect("create");
                }
                workspace.write_at(&path, 0, b"commit me").expect("write");
            }
            let started = Instant::now();
            workspace.commit("second", false).expect("commit");
            let commit_elapsed = started.elapsed();
            commit_samples.push(commit_elapsed);
            println!(
                "   changes={changes} run {run}: snapshot {} / commit {}",
                ms(snapshot_elapsed),
                ms(commit_elapsed)
            );
            workspace.close().expect("close");
            drop(repo);
            cleanup(root);
        }
        println!(
            "   changes={changes} snapshot summary: {}",
            Summary::of(&snapshot_samples)
        );
        println!(
            "   changes={changes} commit   summary: {}",
            Summary::of(&commit_samples)
        );
    }

    // First vs subsequent publication of an *unchanged* tree: the cost ADR-006 §9.2 calls
    // out (the materialiser re-encodes everything; content addressing makes it free on disk
    // but not in time).
    let mut first = Vec::new();
    let mut later = Vec::new();
    for run in 0..repeat {
        let (root, repo) = setup("snapshot-repeat");
        let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
        fill(&workspace, 1000);
        let started = Instant::now();
        workspace.snapshot().expect("first snapshot");
        first.push(started.elapsed());
        let started = Instant::now();
        workspace.snapshot().expect("second snapshot");
        later.push(started.elapsed());
        println!(
            "   unchanged-tree run {run}: first {} / second {}",
            ms(*first.last().expect("sample")),
            ms(*later.last().expect("sample"))
        );
        workspace.close().expect("close");
        drop(repo);
        cleanup(root);
    }
    println!("   unchanged-tree first  summary: {}", Summary::of(&first));
    println!("   unchanged-tree second summary: {}", Summary::of(&later));
}

/// Three ways to read the same bytes: hot overlay, composed from the base, and history.
fn read_latency(repeat: usize) {
    println!("\n== read_at, three paths (1000 files of 64 B)");
    for run in 0..repeat {
        let (root, repo) = setup("read");
        let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
        fill(&workspace, 1000);
        let commit = workspace.commit("first", false).expect("commit");

        // Hot: the bytes were just written through the overlay.
        let hot = time_reads(|| {
            for index in 0..1000 {
                workspace
                    .read_at(&format!("/f{index:04}"), 0, &mut [0u8; 64])
                    .expect("read");
            }
        });

        // Cold: the overlay is empty, the bytes come from the base snapshot.
        let cold = time_reads(|| {
            for index in 0..1000 {
                workspace
                    .read_at(&format!("/f{index:04}"), 0, &mut [0u8; 64])
                    .expect("read");
            }
        });

        // History: straight from the CAS, no table involved.
        let view = repo.read_view(&commit).expect("read_view");
        let history = time_reads(|| {
            for index in 0..1000 {
                view.read_at(&format!("/f{index:04}"), 0, &mut [0u8; 64])
                    .expect("read");
            }
        });

        println!(
            "   run {run}: hot {} / base {} / history {}",
            ms(hot),
            ms(cold),
            ms(history)
        );
        workspace.close().expect("close");
        drop(repo);
        cleanup(root);
    }
}

/// Logical vs canonical vs stored bytes, and how much dedup saved.
fn storage_efficiency(repeat: usize) {
    println!("\n== storage (1000 files, 64 B each)");
    for run in 0..repeat {
        let (root, repo) = setup("storage");
        let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
        // Load characteristic (rule R3): every payload is distinct, so nothing dedups
        // except the repeated block structure of the store's own pages.
        fill(&workspace, 1000);
        let first = workspace.commit("first", false).expect("commit");
        let logical = 1000 * 64;
        let stored_after_first = stored_bytes(&repo);

        // Change one file and commit again: the new tree shares almost everything.
        workspace
            .write_at(
                "/f0000",
                0,
                b"different content, same size, 64 bytes long............",
            )
            .expect("write");
        workspace.commit("second", false).expect("commit");
        let stored_after_second = stored_bytes(&repo);

        let info = repo.commit_info(&first).expect("info");
        let view = repo.read_view(&info.id).expect("read_view");
        let mut canonical = 0u64;
        for index in 0..1000 {
            canonical += view.read_all(&format!("/f{index:04}")).expect("read").len() as u64;
        }
        println!(
            "   run {run}: logical {logical} / canonical {canonical} / stored after 1st {} / after 2nd {}",
            stored_after_first, stored_after_second
        );
        println!(
            "   run {run}: second commit stored {} bytes for 1 changed file",
            stored_after_second.saturating_sub(stored_after_first)
        );
        workspace.close().expect("close");
        drop(repo);
        cleanup(root);
    }
}

// ------------------------------------------------------------------ helpers

/// Writes `count` distinct 64-byte files.
fn fill(workspace: &Workspace, count: usize) {
    for index in 0..count {
        let path = format!("/f{index:04}");
        workspace.create(&path).expect("create");
        let mut payload = [0u8; 64];
        payload[..8].copy_from_slice(&(index as u64).to_be_bytes());
        workspace.write_at(&path, 0, &payload).expect("write");
    }
}

/// Times one pass of 1000 reads, returning the per-operation average.
fn time_reads(body: impl FnOnce()) -> Duration {
    let started = Instant::now();
    body();
    started.elapsed() / 1000
}

/// The bytes the store's object files occupy.
fn stored_bytes(root: &Repo) -> u64 {
    fn walk(path: &std::path::Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else if let Ok(meta) = entry.metadata() {
                *total += meta.len();
            }
        }
    }
    let mut total = 0;
    walk(&root.store_dir().join("objects"), &mut total);
    total
}

/// A fresh repository in a fresh directory.
fn setup(label: &str) -> (PathBuf, Repo) {
    let root = root_dir().join(format!("{label}-{}", nanos()));
    std::fs::create_dir_all(&root).expect("root");
    let repo = Repo::create(root.join("store")).expect("create");
    (root, repo)
}

fn cleanup(root: PathBuf) {
    let _ = std::fs::remove_dir_all(root);
}

fn root_dir() -> PathBuf {
    std::env::var_os("WCAS_BENCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wcas-baseline")
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

fn take_option(arguments: &mut Vec<String>, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    if let Some(index) = arguments.iter().position(|a| a.starts_with(&prefix)) {
        let value = arguments[index][prefix.len()..].to_owned();
        arguments.remove(index);
        return Some(value);
    }
    let index = arguments.iter().position(|a| a == name)?;
    arguments.remove(index);
    if index < arguments.len() {
        Some(arguments.remove(index))
    } else {
        None
    }
}

/// Milliseconds with three decimals.
fn ms(value: Duration) -> String {
    format!("{:.3} ms", value.as_secs_f64() * 1000.0)
}

/// min / median / max, the shape rule R1 asks for.
struct Summary {
    min: Duration,
    median: Duration,
    max: Duration,
}

impl Summary {
    fn of(samples: &[Duration]) -> Self {
        let mut sorted = samples.to_vec();
        sorted.sort();
        Self {
            min: *sorted.first().expect("a sample"),
            median: sorted[sorted.len() / 2],
            max: *sorted.last().expect("a sample"),
        }
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "min {} / median {} / max {}",
            ms(self.min),
            ms(self.median),
            ms(self.max)
        )
    }
}
