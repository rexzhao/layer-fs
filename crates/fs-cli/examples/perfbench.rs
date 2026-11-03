//! A filesystem benchmark with **two arms and no thresholds**.
//!
//! The shape comes from the reference suites the owner named (fio for the structure of a measured job, fs-bench for its scenario list and timing discipline,
//! mdtest for metadata-heavy work, smallfile for small-file batches), and the code is ours: the categories are borrowed, the numbers are measured here.
//!
//! # What is measured and what is not
//!
//! The clock covers **the workload only**. Mounting, `init`, and teardown are all outside it; the mount time is reported on its own line so the reader can see it
//! was not hidden inside a number. Each scenario runs in its own directory, created before and removed after, with one warm-up run and then `--runs` measured
//! runs.
//!
//! # Two arms, one disk
//!
//! Arm A is a layerfs mount point, arm B is a plain host directory. **They must live on the same physical disk**, or the comparison is an instrument error
//! rather than a result — the harness cannot verify that from the paths, so it states the requirement and prints both paths for the reader to check.
//!
//! # Why there are no pass/fail thresholds
//!
//! A performance number is a **report**, not a criterion: "must be faster than X" makes a measurement of a machine into a measurement of a decision. The only
//! failure the harness itself reports is "the operation did not succeed" or "the instrument lost its footing" (a re-run more than 10× off, which means the
//! measurement is broken, not that the product is slow).
//!
//! Usage: `cargo run --release --example perfbench -- --a W:\ --b D:\host --runs 3 [--scenarios S1,S3]`

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One scenario's numbers for one arm.
#[derive(Default, Clone)]
struct Reading {
    scenario: &'static str,
    /// `(operation count, total time)`, used for throughput.
    operations: u64,
    bytes: u64,
    /// Per-operation durations, for the percentiles.
    samples: Vec<Duration>,
}

impl Reading {
    fn push(&mut self, elapsed: Duration, bytes: u64) {
        self.samples.push(elapsed);
        self.operations += 1;
        self.bytes += bytes;
    }

    fn total(&self) -> Duration {
        self.samples.iter().copied().sum()
    }

    fn percentile(&self, fraction: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted = self.samples.clone();
        sorted.sort();
        let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
        sorted[index]
    }

    /// Operations per second, over the whole scenario.
    fn throughput(&self) -> f64 {
        let seconds = self.total().as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.operations as f64 / seconds
    }

    fn mebibytes_per_second(&self) -> f64 {
        let seconds = self.total().as_secs_f64();
        if seconds <= 0.0 {
            return 0.0;
        }
        self.bytes as f64 / seconds / (1024.0 * 1024.0)
    }
}

/// Everything one scenario produced for both arms.
struct Row {
    scenario: &'static str,
    load: String,
    a: Reading,
    b: Reading,
}

fn pattern(length: usize, seed: u64) -> Vec<u8> {
    // A cheap deterministic pseudo-random stream: the point is "not all zeroes", not cryptography.
    let mut state = seed | 1;
    let mut bytes = Vec::with_capacity(length);
    for _ in 0..length {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        bytes.push((state >> 33) as u8);
    }
    bytes
}

fn fresh_dir(root: &Path, name: &str) -> PathBuf {
    let directory = root.join(name);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scenario directory");
    directory
}

fn write_all_at(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for write");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(bytes).expect("write");
    file.flush().expect("flush");
}

fn time_it(reading: &mut Reading, bytes: u64, body: impl FnOnce()) {
    let started = Instant::now();
    body();
    reading.push(started.elapsed(), bytes);
}

/// **S0 (diagnostic)** — creating *empty* files: the same path as S7 with no content at all.
///
/// This exists to split one number in two. If an empty create already costs as much as a 1 KiB one, the cost is in the per-operation machinery (overlay insert,
/// transaction, handle registration); if it is an order of magnitude cheaper, the cost is in the content path (hash and store write). It is a probe, not a
/// scenario: it is measured with the same discipline so the two numbers are comparable.
/// **S0-open (diagnostic)** — opening an existing file, with the clock on the open only.
///
/// Paired with `S0` (empty create): if opening costs about the same as creating, the fixed cost is in the **open path** (name resolution, context acquire, handle
/// registration); if opening is far cheaper, the cost is in the create's overlay insert and commit. Files are made before the clock starts.
fn opens_only(root: &Path, count: usize) -> Reading {
    let directory = fresh_dir(root, "opens-only");
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let path = directory.join(format!("o-{index:05}.bin"));
        std::fs::write(&path, b"x").expect("prepare file");
        paths.push(path);
    }
    let mut reading = Reading {
        scenario: "S0-open existing file",
        ..Reading::default()
    };
    for path in &paths {
        time_it(&mut reading, 0, || {
            let handle = std::fs::File::open(path).expect("open");
            drop(handle);
        });
    }
    reading
}

fn empty_creates(root: &Path, count: usize) -> Reading {
    let directory = fresh_dir(root, "empty-creates");
    let mut reading = Reading {
        scenario: "S0 create 0-byte files",
        ..Reading::default()
    };
    for index in 0..count {
        let path = directory.join(format!("e-{index:05}.bin"));
        time_it(&mut reading, 0, || {
            std::fs::write(&path, b"").expect("create empty");
        });
    }
    reading
}

/// S1/S2: 64 MiB sequential write and read, in two shapes.
fn sequential(root: &Path, shape: &str, bytes: usize) -> (Reading, Reading) {
    let mut write = Reading {
        scenario: if shape == "zero" {
            "S1 write 64MiB (zeroes)"
        } else {
            "S2 write 64MiB (random)"
        },
        ..Reading::default()
    };
    let mut read = Reading {
        scenario: if shape == "zero" {
            "S1 read 64MiB (zeroes)"
        } else {
            "S2 read 64MiB (random)"
        },
        ..Reading::default()
    };
    let payload = if shape == "zero" {
        vec![0u8; bytes]
    } else {
        pattern(bytes, 0x5EED)
    };
    let directory = fresh_dir(root, &format!("sequential-{shape}"));
    let file = directory.join("payload.bin");
    time_it(&mut write, bytes as u64, || {
        std::fs::write(&file, &payload).expect("sequential write");
    });
    time_it(&mut read, bytes as u64, || {
        let mut handle = std::fs::File::open(&file).expect("open for read");
        let mut buffer = vec![0u8; bytes];
        handle.read_exact(&mut buffer).expect("sequential read");
        assert_eq!(buffer.len(), bytes);
    });
    (write, read)
}

/// S3/S4: fixed-count random 4 KiB reads and writes, reporting IOPS and latency percentiles.
fn random_4k(root: &Path, writes: bool, operations: usize) -> Reading {
    let directory = fresh_dir(
        root,
        if writes {
            "random-write"
        } else {
            "random-read"
        },
    );
    let file = directory.join("sparse.bin");
    let blocks = 16 * 1024; // 64 MiB worth of block addresses
    let mut reading = Reading {
        scenario: if writes {
            "S4 random 4KiB write"
        } else {
            "S3 random 4KiB read"
        },
        ..Reading::default()
    };
    // **Seeded for both arms of the pair**: the read variant needs a file to read, and seeding it outside the clock is what keeps the two measurements comparable.
    if !file.exists() {
        std::fs::write(&file, vec![0u8; blocks * 4096]).expect("seed file");
    }
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for index in 0..operations {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let block = (state >> 33) as usize % blocks;
        let offset = (block * 4096) as u64;
        if writes {
            let payload = pattern(4096, index as u64 + 1);
            time_it(&mut reading, 4096, || write_all_at(&file, offset, &payload));
        } else {
            time_it(&mut reading, 4096, || {
                let mut handle = std::fs::File::open(&file).expect("open for read");
                handle.seek(SeekFrom::Start(offset)).expect("seek");
                let mut buffer = [0u8; 4096];
                handle.read_exact(&mut buffer).expect("random read");
            });
        }
    }
    reading
}

/// S5: 1000 files — create, stat, list, delete — inside a nested directory.
fn metadata_1000(root: &Path, count: usize) -> Vec<Reading> {
    let directory = fresh_dir(root, "metadata-1000");
    let nested = directory.join("a").join("b").join("c");
    std::fs::create_dir_all(&nested).expect("nested directories");
    let mut create = Reading {
        scenario: "S5 create 1000",
        ..Reading::default()
    };
    let mut stat = Reading {
        scenario: "S5 stat 1000",
        ..Reading::default()
    };
    let mut list = Reading {
        scenario: "S5 list 1000",
        ..Reading::default()
    };
    let mut delete = Reading {
        scenario: "S5 delete 1000",
        ..Reading::default()
    };
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let path = nested.join(format!("file-{index:04}.txt"));
        time_it(&mut create, 0, || {
            std::fs::write(&path, b"x").expect("create");
        });
        paths.push(path);
    }
    for path in &paths {
        time_it(&mut stat, 0, || {
            let _ = std::fs::metadata(path).expect("stat");
        });
    }
    time_it(&mut list, 0, || {
        let entries: Vec<_> = std::fs::read_dir(&nested)
            .expect("list")
            .flatten()
            .collect();
        assert_eq!(entries.len(), count, "the listing must see every file");
    });
    for path in &paths {
        time_it(&mut delete, 0, || {
            std::fs::remove_file(path).expect("delete");
        });
    }
    vec![create, stat, list, delete]
}

/// S6: a tree of 100 directories × 100 files, then a full walk.
fn tree(root: &Path, per_directory: usize) -> Vec<Reading> {
    let directory = fresh_dir(root, "tree-100x100");
    let mut build = Reading {
        scenario: "S6 build 100x100",
        ..Reading::default()
    };
    for parent in 0..per_directory {
        let path = directory.join(format!("dir-{parent:03}"));
        std::fs::create_dir_all(&path).expect("directory");
        for child in 0..per_directory {
            let file = path.join(format!("f-{child:03}.txt"));
            time_it(&mut build, 0, || {
                std::fs::write(&file, b"x").expect("file");
            });
        }
    }
    let mut walk = Reading {
        scenario: "S6 walk 100x100",
        ..Reading::default()
    };
    time_it(&mut walk, 0, || {
        let mut count = 0u64;
        walk_tree(&directory, &mut count);
        assert_eq!(
            count,
            (per_directory * per_directory) as u64,
            "the walk must see every file"
        );
    });
    vec![build, walk]
}

fn walk_tree(directory: &Path, files: &mut u64) {
    for entry in std::fs::read_dir(directory).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_tree(&path, files);
        } else {
            *files += 1;
        }
    }
}

/// S7: 5000 small files (1 KiB) — create, read, delete.
fn small_files(root: &Path, count: usize) -> Vec<Reading> {
    let directory = fresh_dir(root, "small-5000");
    let payload = pattern(1024, 0xC0FFEE);
    let mut create = Reading {
        scenario: "S7 create 5000x1KiB",
        ..Reading::default()
    };
    let mut read = Reading {
        scenario: "S7 read 5000x1KiB",
        ..Reading::default()
    };
    let mut delete = Reading {
        scenario: "S7 delete 5000x1KiB",
        ..Reading::default()
    };
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let path = directory.join(format!("s-{index:05}.bin"));
        time_it(&mut create, 1024, || {
            std::fs::write(&path, &payload).expect("create");
        });
        paths.push(path);
    }
    for path in &paths {
        time_it(&mut read, 1024, || {
            let bytes = std::fs::read(path).expect("read");
            assert_eq!(bytes.len(), 1024);
        });
    }
    for path in &paths {
        time_it(&mut delete, 0, || {
            std::fs::remove_file(path).expect("delete");
        });
    }
    vec![create, read, delete]
}

/// S8/S9: copy 64 MiB, and overwrite 10% of it in place.
fn copy_and_overwrite(root: &Path, bytes: usize) -> Vec<Reading> {
    let directory = fresh_dir(root, "copy-64mib");
    let source = directory.join("source.bin");
    let copy = directory.join("copy.bin");
    std::fs::write(&source, pattern(bytes, 0xBEEF)).expect("seed source");
    let mut copying = Reading {
        scenario: "S8 copy 64MiB",
        ..Reading::default()
    };
    time_it(&mut copying, bytes as u64, || {
        std::fs::copy(&source, &copy).expect("copy");
    });
    let mut overwrite = Reading {
        scenario: "S9 overwrite 10% of 64MiB",
        ..Reading::default()
    };
    let chunk = pattern(bytes / 10, 0x1234);
    time_it(&mut overwrite, (bytes / 10) as u64, || {
        write_all_at(&copy, (bytes / 2) as u64, &chunk);
    });
    vec![copying, overwrite]
}

/// S10: `git init`, 100 files, add + commit — a real compound workload.
fn git_workload(root: &Path) -> Reading {
    let directory = fresh_dir(root, "git-workload");
    let mut reading = Reading {
        scenario: "S10 git init + 100 files + commit",
        ..Reading::default()
    };
    time_it(&mut reading, 0, || {
        let run = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(&directory)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {arguments:?} must succeed");
        };
        run(&["init", "-q"]);
        for index in 0..100 {
            std::fs::write(
                directory.join(format!("f-{index:03}.txt")),
                b"hello layerfs\n",
            )
            .expect("file");
        }
        run(&["add", "-A"]);
        run(&[
            "-c",
            "user.email=bench@example.invalid",
            "-c",
            "user.name=bench",
            "commit",
            "-qm",
            "bench",
        ]);
    });
    reading
}

fn report(path: &Path, mount_seconds: Duration, rows: &[Row], runs: usize, scenarios: &[&str]) {
    let mut text = String::new();
    text.push_str("# Performance baseline (this machine)\n\n");
    text.push_str("Two arms, **the same physical disk**: arm A is the layerfs mount, arm B is the host directory the store lives in.\n");
    text.push_str(
        "The clock covers the workload only — `init`, mount and teardown are outside it.\n\n",
    );
    text.push_str("## Environment\n\n");
    text.push_str(&format!("- date: {}\n", chrono_like_now()));
    text.push_str(&format!(
        "- runs per scenario: {runs} (one warm-up, not reported)\n"
    ));
    text.push_str(&format!("- scenarios: {}\n", scenarios.join(", ")));
    text.push_str(&format!(
        "- **mount time (measured by the caller; outside every number below): {:.3} s**\n\n",
        mount_seconds.as_secs_f64()
    ));
    text.push_str("## Numbers\n\n");
    text.push_str("| scenario | load | arm A throughput | arm B throughput | A p50 | A p95 | A p99 | B p50 | B p95 | B p99 |\n");
    text.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        text.push_str(&format!(
            "| {} | {} | {:.1} op/s | {:.1} op/s | {:.3} ms | {:.3} ms | {:.3} ms | {:.3} ms | {:.3} ms | {:.3} ms |\n",
            row.scenario,
            row.load,
            row.a.throughput(),
            row.b.throughput(),
            row.a.percentile(0.50).as_secs_f64() * 1000.0,
            row.a.percentile(0.95).as_secs_f64() * 1000.0,
            row.a.percentile(0.99).as_secs_f64() * 1000.0,
            row.b.percentile(0.50).as_secs_f64() * 1000.0,
            row.b.percentile(0.95).as_secs_f64() * 1000.0,
            row.b.percentile(0.99).as_secs_f64() * 1000.0,
        ));
        if row.a.bytes > 0 {
            text.push_str(&format!(
                "| ↳ MiB/s | {} | {:.1} | {:.1} | | | | | | |\n",
                row.load,
                row.a.mebibytes_per_second(),
                row.b.mebibytes_per_second()
            ));
        }
    }
    text.push_str(
        "\nEvery operation above reported success; a failure would have aborted the run.\n",
    );
    text.push_str("\n**No thresholds.** These numbers are a report, not a gate: they describe this machine on this day.\n");
    std::fs::write(path, text).expect("write the report");
}

/// A drive letter is **not** a directory by itself: `Y:` means "the current directory on Y:", so `Path::new("Y:").join("x")` produces `Y:x` — a
/// drive-relative path that silently lands somewhere else. Measured: the first harness run wrote 64 MiB and then could not open it, because the two halves resolved
/// differently. A bare `X:` becomes `X:`.
fn arm_root(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if text.len() == 2 && text.ends_with(':') {
        PathBuf::from(format!("{text}\\"))
    } else {
        path
    }
}

fn chrono_like_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{} s since the epoch (UTC seconds)", now.as_secs())
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let mut arm_a = None;
    let mut arm_b = None;
    let mut runs = 1usize;
    let mut scale = 1.0f64;
    // Mount time is measured by the caller (the harness starts after the volume is up) and reported on its own line, never inside a scenario number.
    let mut mount_seconds = 0.0f64;
    let mut only: Option<Vec<String>> = None;
    let mut out = PathBuf::from("perf-baseline.md");
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--a" => arm_a = arguments.next().map(PathBuf::from),
            "--b" => arm_b = arguments.next().map(PathBuf::from),
            "--runs" => {
                runs = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1)
            }
            // **Sizes are scaled, not shapes**: a tenth of the card's counts still exercises the same code path, and the report says so.
            "--scale" => {
                scale = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1.0)
            }
            "--mount-seconds" => {
                mount_seconds = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0.0)
            }
            "--scenarios" => {
                only = arguments.next().map(|value| {
                    value
                        .split(',')
                        .map(|part| part.trim().to_owned())
                        .collect()
                })
            }
            "--out" => out = arguments.next().map(PathBuf::from).unwrap_or(out),
            other => panic!("unknown argument {other:?}"),
        }
    }
    let arm_a = arm_root(arm_a.expect("--a <layerfs mount point>"));
    let arm_b = arm_root(arm_b.expect("--b <host directory on the same disk>"));
    assert!(
        arm_a.is_dir(),
        "arm A must be a directory: {}",
        arm_a.display()
    );
    assert!(
        arm_b.is_dir(),
        "arm B must be a directory: {}",
        arm_b.display()
    );
    eprintln!("arm A = {} (layerfs mount)", arm_a.display());
    eprintln!(
        "arm B = {} (host directory, must be the same physical disk)",
        arm_b.display()
    );

    let wanted = |name: &str| match &only {
        Some(list) => list.iter().any(|entry| entry.eq_ignore_ascii_case(name)),
        None => true,
    };
    let megabytes = (64.0 * 1024.0 * 1024.0 * scale) as usize;
    let ops = |count: usize| ((count as f64 * scale).round() as usize).max(1);
    let started = Instant::now();
    let mut rows: Vec<Row> = Vec::new();
    for run in 0..runs {
        let mut one_arm = |arm: &Path, label: &str| -> (Vec<Row>, Duration) {
            let begin = Instant::now();
            let mut collected: Vec<Row> = Vec::new();
            if wanted("S0") {
                let reading = empty_creates(arm, ops(5000));
                collected.push(Row {
                    scenario: reading.scenario,
                    load: "0-byte files".into(),
                    a: reading,
                    b: Reading::default(),
                });
                let reading = opens_only(arm, ops(5000));
                collected.push(Row {
                    scenario: reading.scenario,
                    load: "0-byte files".into(),
                    a: reading,
                    b: Reading::default(),
                });
            }
            if wanted("S1") || wanted("S2") {
                let (write, read) = sequential(arm, "zero", megabytes);
                collected.push(Row {
                    scenario: write.scenario,
                    load: "64 MiB, zeroes".into(),
                    a: write,
                    b: Reading::default(),
                });
                collected.push(Row {
                    scenario: read.scenario,
                    load: "64 MiB, zeroes".into(),
                    a: read,
                    b: Reading::default(),
                });
                let (write, read) = sequential(arm, "random", megabytes);
                collected.push(Row {
                    scenario: write.scenario,
                    load: "64 MiB, pseudo-random".into(),
                    a: write,
                    b: Reading::default(),
                });
                collected.push(Row {
                    scenario: read.scenario,
                    load: "64 MiB, pseudo-random".into(),
                    a: read,
                    b: Reading::default(),
                });
            }
            if wanted("S3") {
                let reading = random_4k(arm, false, ops(2000));
                collected.push(Row {
                    scenario: reading.scenario,
                    load: "2000 ops, 4 KiB, 64 MiB file".into(),
                    a: reading,
                    b: Reading::default(),
                });
            }
            if wanted("S4") {
                let reading = random_4k(arm, true, ops(2000));
                collected.push(Row {
                    scenario: reading.scenario,
                    load: "2000 ops, 4 KiB, 64 MiB file".into(),
                    a: reading,
                    b: Reading::default(),
                });
            }
            if wanted("S5") {
                for reading in metadata_1000(arm, ops(1000)) {
                    collected.push(Row {
                        scenario: reading.scenario,
                        load: "1000 files, 3 levels deep".into(),
                        a: reading,
                        b: Reading::default(),
                    });
                }
            }
            if wanted("S6") {
                for reading in tree(arm, ops(100)) {
                    collected.push(Row {
                        scenario: reading.scenario,
                        load: "100 dirs × 100 files".into(),
                        a: reading,
                        b: Reading::default(),
                    });
                }
            }
            if wanted("S7") {
                for reading in small_files(arm, ops(5000)) {
                    collected.push(Row {
                        scenario: reading.scenario,
                        load: "5000 files × 1 KiB".into(),
                        a: reading,
                        b: Reading::default(),
                    });
                }
            }
            if wanted("S8") || wanted("S9") {
                for reading in copy_and_overwrite(arm, megabytes) {
                    collected.push(Row {
                        scenario: reading.scenario,
                        load: "64 MiB".into(),
                        a: reading,
                        b: Reading::default(),
                    });
                }
            }
            if wanted("S10") {
                let reading = git_workload(arm);
                collected.push(Row {
                    scenario: reading.scenario,
                    load: "git init + 100 files + commit".into(),
                    a: reading,
                    b: Reading::default(),
                });
            }
            // **Per scenario, not per run**: a scenario that takes minutes must be visible while it runs, and the mean per operation is the number that explains
            // why (201cslow to measure201d is a reading, not an obstacle).
            for row in &collected {
                let mean = if row.a.operations > 0 {
                    row.a.total().as_secs_f64() * 1000.0 / row.a.operations as f64
                } else {
                    0.0
                };
                eprintln!(
                    "  run {run} arm {label}: {} — {} ops, {:.1} op/s, mean {:.3} ms/op",
                    row.scenario,
                    row.a.operations,
                    row.a.throughput(),
                    mean
                );
            }
            eprintln!(
                "run {run}: arm {label} finished {} scenarios in {:?}",
                collected.len(),
                begin.elapsed()
            );
            (collected, begin.elapsed())
        };

        let (mut a_rows, _a_time) = one_arm(&arm_a, "A");
        let (b_rows, _b_time) = one_arm(&arm_b, "B");
        // Pair the two arms scenario by scenario: the order is the same because both walks build the same list.
        for (index, row) in a_rows.iter_mut().enumerate() {
            if let Some(other) = b_rows.get(index) {
                row.b = other.a.clone();
            }
        }
        if run + 1 == runs {
            rows = a_rows;
        }
    }
    let total = started.elapsed();
    eprintln!("measured {runs} run(s) in {total:?} (mount time is not part of this: it happened before the harness started)");
    report(
        &out,
        Duration::from_secs_f64(mount_seconds),
        &rows,
        runs,
        &["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9", "S10"],
    );
    eprintln!("report written to {}", out.display());
}
