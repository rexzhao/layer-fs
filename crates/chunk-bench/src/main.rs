//! `chunk-bench`: read-only single-pass multi-strategy chunking, deduplication, and
//! compression benchmark for large directory trees (Windows verbatim-path safe).

mod chunker;
mod compress;
mod dedup;

use chunker::{chunk_slice, hash128, Strategy, StreamChunker};
use compress::{CodecMetrics, CompressionSuite, WorkerCompressor};
use dedup::{DedupSnapshot, ShardedChunkSet};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Files up to this size are read into a reusable per-worker buffer once and sliced zero-copy.
/// Larger files use streaming chunkers so RAM stays strictly bounded even on multi-GB blobs.
const IN_MEMORY_FAST_PATH_BYTES: u64 = 16 * 1024 * 1024;
const STREAM_READ_BUF_BYTES: usize = 4 * 1024 * 1024;
const JOB_QUEUE_CAP: usize = 4096;

struct Cli {
    roots: Vec<PathBuf>,
    strategies: Vec<Strategy>,
    threads: usize,
    compress_sample: u64,
    compress_ref: Strategy,
    breakdown_ref: Strategy,
    json_out: Option<PathBuf>,
}

fn parse_cli() -> Result<Cli, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        std::process::exit(0);
    }
    let mut roots = Vec::new();
    let mut strategies = Strategy::ALL.to_vec();
    let mut threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let mut compress_sample: u64 = 10;
    let mut compress_ref = Strategy::Cdc256K;
    let mut breakdown_ref = Strategy::Cdc256K;
    let mut json_out = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--strategies" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--strategies requires a value".to_owned())?;
                strategies = raw
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(Strategy::parse)
                    .collect::<Result<Vec<_>, _>>()?;
                if strategies.is_empty() {
                    return Err("at least one strategy is required".to_owned());
                }
                i += 2;
            }
            "--threads" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--threads requires a value".to_owned())?;
                threads = raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --threads value {raw:?}"))?
                    .max(1);
                i += 2;
            }
            "--compress-sample" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--compress-sample requires a value".to_owned())?;
                compress_sample = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --compress-sample value {raw:?}"))?;
                i += 2;
            }
            "--compress-ref" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--compress-ref requires a value".to_owned())?;
                compress_ref = Strategy::parse(raw)?;
                i += 2;
            }
            "--breakdown-ref" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--breakdown-ref requires a value".to_owned())?;
                breakdown_ref = Strategy::parse(raw)?;
                i += 2;
            }
            "--json" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--json requires a path".to_owned())?;
                json_out = Some(PathBuf::from(raw));
                i += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other:?}; pass --help for usage"));
            }
            other => {
                roots.push(PathBuf::from(other));
                i += 1;
            }
        }
    }

    if roots.is_empty() {
        return Err("at least one directory path is required".to_owned());
    }
    // Always include Whole + compress_ref + breakdown_ref so tables stay complete.
    for required in [Strategy::Whole, compress_ref, breakdown_ref] {
        if !strategies.contains(&required) {
            strategies.push(required);
        }
    }

    Ok(Cli {
        roots,
        strategies,
        threads,
        compress_sample,
        compress_ref,
        breakdown_ref,
        json_out,
    })
}

fn print_usage() {
    eprintln!(
        "chunk-bench: read-only chunking, deduplication & compression benchmark\n\n\
         USAGE:\n  chunk-bench [OPTIONS] <DIR>...\n\n\
         OPTIONS:\n  \
           --strategies <list>        Comma-separated strategies:\n                             \
                                      whole,fixed-256k,fixed-1m,layerfs-16k,cdc-64k,cdc-256k,cdc-1m\n                             \
                                      (default: all 7)\n  \
           --threads <N>              Worker threads (default: CPU count)\n  \
           --compress-sample <N>      Compress 1-in-N unique chunks of --compress-ref\n                             \
                                      (default: 10 = 10% sample; 1 = 100%; 0 = off)\n  \
           --compress-ref <strategy>  Strategy whose unique chunks are compressed (default: cdc-256k)\n  \
           --breakdown-ref <strategy> Strategy for dir/ext breakdown vs whole-file (default: cdc-256k)\n  \
           --json <path>              Write full JSON report to file"
    );
}

struct FileJob {
    path: PathBuf,
    size: u64,
    top_subdir: String,
    ext: String,
}

#[derive(Default, Clone, serde::Serialize)]
struct BucketStats {
    files: u64,
    logical_bytes: u64,
    whole_unique_bytes: u64,
    ref_unique_bytes: u64,
}

#[derive(serde::Serialize)]
struct RootReport {
    label: String,
    path: String,
    files: u64,
    dirs: u64,
    symlinks_skipped: u64,
    read_errors: u64,
    logical_bytes: u64,
    elapsed_secs: f64,
    /// Per-strategy delta (newly added unique bytes / chunks) when this root was scanned.
    incremental: Vec<StrategyDelta>,
}

#[derive(serde::Serialize)]
struct StrategyDelta {
    strategy: String,
    added_unique_bytes: u64,
    added_unique_chunks: u64,
    cumulative_unique_bytes: u64,
    cumulative_unique_chunks: u64,
}

#[derive(serde::Serialize)]
struct StrategySummary {
    strategy: String,
    total_bytes: u64,
    unique_bytes: u64,
    saved_bytes: u64,
    saved_pct: f64,
    total_chunks: u64,
    unique_chunks: u64,
    avg_chunk_kb: f64,
    /// Rough index overhead estimate: ~48 bytes per unique chunk entry + ~24 bytes per ref.
    est_index_mb: f64,
}

#[derive(serde::Serialize)]
struct NamedBucket {
    name: String,
    stats: BucketStats,
}

#[derive(serde::Serialize)]
struct FullReport {
    roots: Vec<RootReport>,
    strategies: Vec<StrategySummary>,
    breakdown_ref: String,
    compress_ref: String,
    compress_sample: u64,
    compression: CompressionSuite,
    subdirs: Vec<NamedBucket>,
    extensions: Vec<NamedBucket>,
    total_elapsed_secs: f64,
    total_logical_bytes: u64,
    aggregate_mb_s: f64,
}

struct SharedState {
    strategies: Vec<Strategy>,
    sets: Vec<ShardedChunkSet>,
    whole_idx: usize,
    compress_ref_idx: usize,
    breakdown_ref_idx: usize,
    compress_sample: u64,
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    read_errors: AtomicU64,
}

struct WorkerAgg {
    subdirs: HashMap<String, BucketStats>,
    exts: HashMap<String, BucketStats>,
    compressor: WorkerCompressor,
}

impl WorkerAgg {
    fn new() -> Self {
        Self {
            subdirs: HashMap::new(),
            exts: HashMap::new(),
            compressor: WorkerCompressor::new(),
        }
    }
}

fn canonical_verbatim(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|e| format!("cannot access {}: {e}", path.display()))
}

fn extract_ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 16 => {
            format!(".{}", ext.to_ascii_lowercase())
        }
        _ => "(no-ext)".to_owned(),
    }
}

fn walk_root(
    root: &Path,
    root_label: &str,
    tx: &mpsc::SyncSender<FileJob>,
    dirs_count: &AtomicU64,
    symlinks_skipped: &AtomicU64,
    walk_errors: &AtomicU64,
) {
    // Iterative DFS stack: (dir_path, top_level_subdir_label)
    let mut stack: Vec<(PathBuf, Option<String>)> = vec![(root.to_path_buf(), None)];
    while let Some((dir, top_subdir)) = stack.pop() {
        dirs_count.fetch_add(1, Ordering::Relaxed);
        let entries = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                walk_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                walk_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                walk_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                symlinks_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                let next_top = match &top_subdir {
                    Some(existing) => Some(existing.clone()),
                    None => Some(file_name),
                };
                stack.push((path, next_top));
            } else if ft.is_file() {
                let bucket_dir = match &top_subdir {
                    Some(sub) => {
                        if root_label == ".git" {
                            ".git".to_owned()
                        } else {
                            sub.clone()
                        }
                    }
                    None => {
                        if root_label == ".git" {
                            ".git".to_owned()
                        } else {
                            "(root)".to_owned()
                        }
                    }
                };
                let job = FileJob {
                    path,
                    size: meta.len(),
                    top_subdir: bucket_dir,
                    ext: extract_ext(&file_name),
                };
                if tx.send(job).is_err() {
                    return;
                }
            }
        }
    }
}

fn process_file(
    job: &FileJob,
    shared: &SharedState,
    agg: &mut WorkerAgg,
    file_buf: &mut Vec<u8>,
    stream_buf: &mut [u8],
) {
    let mut file = match File::open(&job.path) {
        Ok(f) => f,
        Err(_) => {
            shared.read_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut whole_unique_added = 0u64;
    let mut ref_unique_added = 0u64;
    let actual_len: u64;

    if job.size <= IN_MEMORY_FAST_PATH_BYTES {
        file_buf.clear();
        if file_buf.capacity() < job.size as usize {
            file_buf.reserve(job.size as usize - file_buf.capacity());
        }
        if file.read_to_end(file_buf).is_err() {
            shared.read_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        actual_len = file_buf.len() as u64;

        // Compute whole-file BLAKE3 once; every strategy whose single-chunk threshold >= len reuses it!
        let whole_h = hash128(file_buf);

        for (idx, &strategy) in shared.strategies.iter().enumerate() {
            let set = &shared.sets[idx];
            if actual_len <= strategy.single_chunk_threshold() {
                let is_new = set.record(whole_h, actual_len);
                if is_new {
                    if idx == shared.whole_idx {
                        whole_unique_added += actual_len;
                    }
                    if idx == shared.breakdown_ref_idx {
                        ref_unique_added += actual_len;
                    }
                    if idx == shared.compress_ref_idx
                        && shared.compress_sample > 0
                        && (whole_h % shared.compress_sample as u128 == 0)
                    {
                        agg.compressor.bench_chunk(file_buf);
                    }
                }
            } else {
                chunk_slice(strategy, file_buf, |chunk| {
                    let h = hash128(chunk);
                    let len = chunk.len() as u64;
                    let is_new = set.record(h, len);
                    if is_new {
                        if idx == shared.whole_idx {
                            whole_unique_added += len;
                        }
                        if idx == shared.breakdown_ref_idx {
                            ref_unique_added += len;
                        }
                        if idx == shared.compress_ref_idx
                            && shared.compress_sample > 0
                            && (h % shared.compress_sample as u128 == 0)
                        {
                            agg.compressor.bench_chunk(chunk);
                        }
                    }
                });
            }
        }
    } else {
        // Streaming path for giant files (> 16 MiB) — reads disk ONCE, bounds RAM to a few MiB.
        let mut chunkers: Vec<StreamChunker> = shared
            .strategies
            .iter()
            .map(|&s| StreamChunker::new(s))
            .collect();
        let mut total_read = 0u64;
        loop {
            let n = match file.read(stream_buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => {
                    shared.read_errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            total_read += n as u64;
            let piece = &stream_buf[..n];
            for (idx, chunker) in chunkers.iter_mut().enumerate() {
                let set = &shared.sets[idx];
                chunker.push(piece, |h, raw| {
                    let len = raw.map_or(0, |r| r.len() as u64);
                    let is_new = set.record(h, len);
                    if is_new {
                        if idx == shared.breakdown_ref_idx {
                            ref_unique_added += len;
                        }
                        if idx == shared.compress_ref_idx
                            && shared.compress_sample > 0
                            && (h % shared.compress_sample as u128 == 0)
                        {
                            if let Some(bytes) = raw {
                                agg.compressor.bench_chunk(bytes);
                            }
                        }
                    }
                });
            }
        }
        actual_len = total_read;
        for (idx, chunker) in chunkers.into_iter().enumerate() {
            let set = &shared.sets[idx];
            chunker.finish(|h, len, raw| {
                let is_new = set.record(h, len);
                if is_new {
                    if idx == shared.whole_idx {
                        whole_unique_added += len;
                    }
                    if idx == shared.breakdown_ref_idx {
                        ref_unique_added += len;
                    }
                    if idx == shared.compress_ref_idx
                        && shared.compress_sample > 0
                        && (h % shared.compress_sample as u128 == 0)
                    {
                        if let Some(bytes) = raw {
                            agg.compressor.bench_chunk(bytes);
                        }
                    }
                }
            });
        }
    }

    let sub = agg.subdirs.entry(job.top_subdir.clone()).or_default();
    sub.files += 1;
    sub.logical_bytes += actual_len;
    sub.whole_unique_bytes += whole_unique_added;
    sub.ref_unique_bytes += ref_unique_added;

    let ext = agg.exts.entry(job.ext.clone()).or_default();
    ext.files += 1;
    ext.logical_bytes += actual_len;
    ext.whole_unique_bytes += whole_unique_added;
    ext.ref_unique_bytes += ref_unique_added;

    shared.files_done.fetch_add(1, Ordering::Relaxed);
    shared.bytes_done.fetch_add(actual_len, Ordering::Relaxed);
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn fmt_mb(bytes: f64) -> String {
    format!("{:.1} MB", bytes / (1024.0 * 1024.0))
}

fn merge_buckets(dst: &mut HashMap<String, BucketStats>, src: HashMap<String, BucketStats>) {
    for (k, v) in src {
        let e = dst.entry(k).or_default();
        e.files += v.files;
        e.logical_bytes += v.logical_bytes;
        e.whole_unique_bytes += v.whole_unique_bytes;
        e.ref_unique_bytes += v.ref_unique_bytes;
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let mut canon_roots = Vec::with_capacity(cli.roots.len());
    for root in &cli.roots {
        let c = canonical_verbatim(root)?;
        if !c.is_dir() {
            return Err(format!("not a directory: {}", root.display()));
        }
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        canon_roots.push((label, root.clone(), c));
    }

    let whole_idx = cli
        .strategies
        .iter()
        .position(|&s| s == Strategy::Whole)
        .unwrap();
    let compress_ref_idx = cli
        .strategies
        .iter()
        .position(|&s| s == cli.compress_ref)
        .unwrap();
    let breakdown_ref_idx = cli
        .strategies
        .iter()
        .position(|&s| s == cli.breakdown_ref)
        .unwrap();

    let shared = Arc::new(SharedState {
        sets: cli.strategies.iter().map(|_| ShardedChunkSet::new()).collect(),
        strategies: cli.strategies.clone(),
        whole_idx,
        compress_ref_idx,
        breakdown_ref_idx,
        compress_sample: cli.compress_sample,
        files_done: AtomicU64::new(0),
        bytes_done: AtomicU64::new(0),
        read_errors: AtomicU64::new(0),
    });

    eprintln!(
        "chunk-bench: {} root(s), {} threads, strategies=[{}], compress_ref={} (sample=1/{})",
        canon_roots.len(),
        cli.threads,
        cli.strategies
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
            .join(","),
        cli.compress_ref.name(),
        cli.compress_sample
    );

    let overall_start = Instant::now();
    let mut root_reports = Vec::with_capacity(canon_roots.len());
    let mut all_subdirs: HashMap<String, BucketStats> = HashMap::new();
    let mut all_exts: HashMap<String, BucketStats> = HashMap::new();
    let mut total_compression = CompressionSuite::default();

    for (root_idx, (label, orig_path, canon_path)) in canon_roots.iter().enumerate() {
        let before_snaps: Vec<DedupSnapshot> = shared.sets.iter().map(|s| s.snapshot()).collect();
        let before_files = shared.files_done.load(Ordering::Relaxed);
        let before_bytes = shared.bytes_done.load(Ordering::Relaxed);
        let before_errs = shared.read_errors.load(Ordering::Relaxed);
        let root_start = Instant::now();

        let dirs_count = Arc::new(AtomicU64::new(0));
        let symlinks_skipped = Arc::new(AtomicU64::new(0));
        let done_flag = Arc::new(AtomicBool::new(false));

        // Progress reporter thread.
        let p_shared = Arc::clone(&shared);
        let p_done = Arc::clone(&done_flag);
        let p_label = label.clone();
        let p_total_roots = canon_roots.len();
        let progress_handle = std::thread::spawn(move || {
            let mut last_bytes = before_bytes;
            let mut last_t = Instant::now();
            while !p_done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                if p_done.load(Ordering::Relaxed) {
                    break;
                }
                let now = Instant::now();
                let cur_files = p_shared.files_done.load(Ordering::Relaxed) - before_files;
                let cur_bytes = p_shared.bytes_done.load(Ordering::Relaxed);
                let root_bytes = cur_bytes - before_bytes;
                let dt = now.duration_since(last_t).as_secs_f64().max(0.001);
                let inst_mb_s = (cur_bytes - last_bytes) as f64 / (1024.0 * 1024.0) / dt;
                let avg_mb_s = root_bytes as f64
                    / (1024.0 * 1024.0)
                    / root_start.elapsed().as_secs_f64().max(0.001);
                eprintln!(
                    "  [{:>5.0}s] root {}/{} ({}): {:>9} files, {:>8} (cur {:>6.0} MB/s, avg {:>6.0} MB/s)",
                    overall_start.elapsed().as_secs_f64(),
                    root_idx + 1,
                    p_total_roots,
                    p_label,
                    cur_files,
                    fmt_gb(root_bytes),
                    inst_mb_s,
                    avg_mb_s,
                );
                last_bytes = cur_bytes;
                last_t = now;
            }
        });

        let (tx, rx) = mpsc::sync_channel::<FileJob>(JOB_QUEUE_CAP);
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::with_capacity(cli.threads);
        for _ in 0..cli.threads {
            let rx = Arc::clone(&rx);
            let shared = Arc::clone(&shared);
            workers.push(std::thread::spawn(move || {
                let mut agg = WorkerAgg::new();
                let mut file_buf = Vec::with_capacity(1024 * 1024);
                let mut stream_buf = vec![0u8; STREAM_READ_BUF_BYTES];
                loop {
                    let job = {
                        let guard = rx.lock().unwrap();
                        guard.recv()
                    };
                    let Ok(job) = job else {
                        break;
                    };
                    process_file(&job, &shared, &mut agg, &mut file_buf, &mut stream_buf);
                }
                (agg.subdirs, agg.exts, agg.compressor.suite)
            }));
        }

        walk_root(
            canon_path,
            label,
            &tx,
            &dirs_count,
            &symlinks_skipped,
            &shared.read_errors,
        );
        drop(tx);

        for w in workers {
            let (subdirs, exts, suite) = w.join().map_err(|_| "worker panicked".to_owned())?;
            merge_buckets(&mut all_subdirs, subdirs);
            merge_buckets(&mut all_exts, exts);
            total_compression.merge_from(&suite);
        }

        done_flag.store(true, Ordering::Relaxed);
        let _ = progress_handle.join();

        let root_elapsed = root_start.elapsed().as_secs_f64();
        let after_snaps: Vec<DedupSnapshot> = shared.sets.iter().map(|s| s.snapshot()).collect();
        let root_files = shared.files_done.load(Ordering::Relaxed) - before_files;
        let root_bytes = shared.bytes_done.load(Ordering::Relaxed) - before_bytes;
        let root_errs = shared.read_errors.load(Ordering::Relaxed) - before_errs;

        let incremental: Vec<StrategyDelta> = shared
            .strategies
            .iter()
            .enumerate()
            .map(|(i, s)| StrategyDelta {
                strategy: s.name().to_owned(),
                added_unique_bytes: after_snaps[i]
                    .unique_bytes
                    .saturating_sub(before_snaps[i].unique_bytes),
                added_unique_chunks: after_snaps[i]
                    .unique_chunks
                    .saturating_sub(before_snaps[i].unique_chunks),
                cumulative_unique_bytes: after_snaps[i].unique_bytes,
                cumulative_unique_chunks: after_snaps[i].unique_chunks,
            })
            .collect();

        eprintln!(
            "  DONE root {}/{} ({}): {} files, {}, {:.1}s ({:.0} MB/s, errors={})",
            root_idx + 1,
            canon_roots.len(),
            label,
            root_files,
            fmt_gb(root_bytes),
            root_elapsed,
            root_bytes as f64 / (1024.0 * 1024.0) / root_elapsed.max(0.001),
            root_errs,
        );

        root_reports.push(RootReport {
            label: label.clone(),
            path: orig_path.display().to_string(),
            files: root_files,
            dirs: dirs_count.load(Ordering::Relaxed),
            symlinks_skipped: symlinks_skipped.load(Ordering::Relaxed),
            read_errors: root_errs,
            logical_bytes: root_bytes,
            elapsed_secs: root_elapsed,
            incremental,
        });
    }

    let total_elapsed = overall_start.elapsed().as_secs_f64();
    let total_logical = shared.bytes_done.load(Ordering::Relaxed);
    let agg_mb_s = total_logical as f64 / (1024.0 * 1024.0) / total_elapsed.max(0.001);

    let strategy_summaries: Vec<StrategySummary> = shared
        .strategies
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let snap = shared.sets[i].snapshot();
            let saved = snap.total_bytes.saturating_sub(snap.unique_bytes);
            let saved_pct = if snap.total_bytes == 0 {
                0.0
            } else {
                saved as f64 * 100.0 / snap.total_bytes as f64
            };
            let avg_kb = if snap.unique_chunks == 0 {
                0.0
            } else {
                (snap.unique_bytes as f64 / snap.unique_chunks as f64) / 1024.0
            };
            let est_index_bytes =
                snap.unique_chunks as f64 * 48.0 + snap.total_chunks as f64 * 24.0;
            StrategySummary {
                strategy: s.name().to_owned(),
                total_bytes: snap.total_bytes,
                unique_bytes: snap.unique_bytes,
                saved_bytes: saved,
                saved_pct,
                total_chunks: snap.total_chunks,
                unique_chunks: snap.unique_chunks,
                avg_chunk_kb: avg_kb,
                est_index_mb: est_index_bytes / (1024.0 * 1024.0),
            }
        })
        .collect();

    let mut subdirs: Vec<NamedBucket> = all_subdirs
        .into_iter()
        .map(|(name, stats)| NamedBucket { name, stats })
        .collect();
    subdirs.sort_by_key(|a| std::cmp::Reverse(a.stats.logical_bytes));

    let mut extensions: Vec<NamedBucket> = all_exts
        .into_iter()
        .map(|(name, stats)| NamedBucket { name, stats })
        .collect();
    extensions.sort_by_key(|a| std::cmp::Reverse(a.stats.logical_bytes));

    let report = FullReport {
        roots: root_reports,
        strategies: strategy_summaries,
        breakdown_ref: cli.breakdown_ref.name().to_owned(),
        compress_ref: cli.compress_ref.name().to_owned(),
        compress_sample: cli.compress_sample,
        compression: total_compression,
        subdirs,
        extensions,
        total_elapsed_secs: total_elapsed,
        total_logical_bytes: total_logical,
        aggregate_mb_s: agg_mb_s,
    };

    print_report(&report);

    if let Some(json_path) = &cli.json_out {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("serialize report: {e}"))?;
        fs::write(json_path, json)
            .map_err(|e| format!("write {}: {e}", json_path.display()))?;
        eprintln!("Wrote JSON report to {}", json_path.display());
    }

    Ok(())
}

fn print_report(r: &FullReport) {
    println!();
    println!("================================================================================");
    println!(
        "CHUNK-BENCH REPORT  |  Total: {} across {} root(s)  |  Time: {:.1}s ({:.0} MB/s)",
        fmt_gb(r.total_logical_bytes),
        r.roots.len(),
        r.total_elapsed_secs,
        r.aggregate_mb_s
    );
    println!("================================================================================");

    println!("\n[1] Input Roots");
    println!(
        "{:<24} {:>10} {:>8} {:>11} {:>8} {:>8}",
        "Root", "Files", "Dirs", "Logical", "Time(s)", "Errors"
    );
    for root in &r.roots {
        println!(
            "{:<24} {:>10} {:>8} {:>11} {:>8.1} {:>8}",
            root.label,
            root.files,
            root.dirs,
            fmt_gb(root.logical_bytes),
            root.elapsed_secs,
            root.read_errors
        );
    }

    println!("\n[2] Chunking Strategy Comparison (Combined Across All Roots)");
    println!(
        "{:<13} {:>11} {:>11} {:>8} {:>12} {:>9} {:>10}",
        "Strategy", "Logical", "Unique", "Saved%", "UniqChunks", "Avg(KB)", "EstIdx"
    );
    for s in &r.strategies {
        println!(
            "{:<13} {:>11} {:>11} {:>7.2}% {:>12} {:>9.1} {:>10}",
            s.strategy,
            fmt_gb(s.total_bytes),
            fmt_gb(s.unique_bytes),
            s.saved_pct,
            s.unique_chunks,
            s.avg_chunk_kb,
            fmt_mb(s.est_index_mb * 1024.0 * 1024.0)
        );
    }

    println!("\n[3] Per-Root Incremental Unique Bytes Added");
    print!("{:<13}", "Strategy");
    for root in &r.roots {
        print!(" {:>20}", root.label);
    }
    println!();
    for (s_idx, s) in r.strategies.iter().enumerate() {
        print!("{:<13}", s.strategy);
        for root in &r.roots {
            let d = &root.incremental[s_idx];
            let pct = if root.logical_bytes == 0 {
                0.0
            } else {
                d.added_unique_bytes as f64 * 100.0 / root.logical_bytes as f64
            };
            print!(
                " {:>11} ({:>5.1}%)",
                fmt_gb(d.added_unique_bytes),
                pct
            );
        }
        println!();
    }

    println!(
        "\n[4] Top-Level Subdirectory Breakdown (Whole vs {})",
        r.breakdown_ref
    );
    println!(
        "{:<22} {:>9} {:>10} {:>10} {:>10} {:>10}",
        "Subdir", "Files", "Logical", "WholeUniq", "RefUniq", "CDC Extra"
    );
    for b in r.subdirs.iter().take(20) {
        let extra = b
            .stats
            .whole_unique_bytes
            .saturating_sub(b.stats.ref_unique_bytes);
        println!(
            "{:<22} {:>9} {:>10} {:>10} {:>10} {:>10}",
            b.name,
            b.stats.files,
            fmt_gb(b.stats.logical_bytes),
            fmt_gb(b.stats.whole_unique_bytes),
            fmt_gb(b.stats.ref_unique_bytes),
            fmt_gb(extra)
        );
    }

    println!(
        "\n[5] Top 20 File Extensions (Whole vs {})",
        r.breakdown_ref
    );
    println!(
        "{:<16} {:>9} {:>10} {:>10} {:>10} {:>10}",
        "Extension", "Files", "Logical", "WholeUniq", "RefUniq", "CDC Extra"
    );
    for b in r.extensions.iter().take(20) {
        let extra = b
            .stats
            .whole_unique_bytes
            .saturating_sub(b.stats.ref_unique_bytes);
        println!(
            "{:<16} {:>9} {:>10} {:>10} {:>10} {:>10}",
            b.name,
            b.stats.files,
            fmt_gb(b.stats.logical_bytes),
            fmt_gb(b.stats.whole_unique_bytes),
            fmt_gb(b.stats.ref_unique_bytes),
            fmt_gb(extra)
        );
    }

    if r.compress_sample > 0 {
        let ref_unique = r
            .strategies
            .iter()
            .find(|s| s.strategy == r.compress_ref)
            .map(|s| s.unique_bytes)
            .unwrap_or(0);
        println!(
            "\n[6] Compression Benchmark on Unique Chunks of `{}` (sample 1/{}, 8 KiB probe)",
            r.compress_ref, r.compress_sample
        );
        println!(
            "{:<10} {:>11} {:>11} {:>8} {:>11} {:>9} {:>10} {:>10}",
            "Codec",
            "SampleRaw",
            "SampleOut",
            "Ratio",
            "ProjTotal",
            "Skip%",
            "CompMB/s",
            "DecMB/s"
        );
        let print_codec = |name: &str, m: &CodecMetrics| {
            let ratio = m.ratio();
            let proj = (ref_unique as f64 * ratio) as u64;
            let skip_pct = if m.sampled_raw_bytes == 0 {
                0.0
            } else {
                m.probe_skipped_bytes as f64 * 100.0 / m.sampled_raw_bytes as f64
            };
            println!(
                "{:<10} {:>11} {:>11} {:>7.1}% {:>11} {:>8.1}% {:>10.0} {:>10.0}",
                name,
                fmt_gb(m.sampled_raw_bytes),
                fmt_gb(m.stored_bytes),
                ratio * 100.0,
                fmt_gb(proj),
                skip_pct,
                m.compress_mb_s(),
                m.decompress_mb_s()
            );
        };
        println!(
            "{:<10} {:>11} {:>11} {:>7.1}% {:>11} {:>9} {:>10} {:>10}",
            "none",
            fmt_gb(r.compression.lz4.sampled_raw_bytes),
            fmt_gb(r.compression.lz4.sampled_raw_bytes),
            100.0,
            fmt_gb(ref_unique),
            "-",
            "-",
            "-"
        );
        print_codec("lz4", &r.compression.lz4);
        print_codec("zstd-1", &r.compression.zstd1);
        print_codec("zstd-3", &r.compression.zstd3);
    }
}

fn main() {
    let cli = match parse_cli() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("chunk-bench: {msg}");
            std::process::exit(2);
        }
    };
    if let Err(err) = run(cli) {
        eprintln!("chunk-bench: {err}");
        std::process::exit(1);
    }
}
