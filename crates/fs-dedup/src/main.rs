//! `dedupfs` CLI — Unity-focused multi-workspace deduplicating WinFsp filesystem.

use fs_dedup::{
    request_umount, run_mount, Codec, DedupStore, ImportProgress, MountOptions, StoreConfig,
    DEFAULT_CACHE_BYTES, DEFAULT_PACK_TARGET_BYTES,
};
use std::path::PathBuf;
use std::time::Instant;

fn parse_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim().to_ascii_uppercase();
    let (num_str, mult): (&str, u64) = if let Some(v) = s.strip_suffix("GIB").or_else(|| s.strip_suffix('G')) {
        (v, 1024 * 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("MIB").or_else(|| s.strip_suffix('M')) {
        (v, 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("KIB").or_else(|| s.strip_suffix('K')) {
        (v, 1024)
    } else {
        (s.as_str(), 1)
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {raw:?}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("size overflow {raw:?}"))
}

fn fmt_bytes(b: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let f = b as f64;
    if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.1} KiB", f / KIB)
    } else {
        format!("{b} B")
    }
}

fn print_help() {
    println!(
        "dedupfs — streamlined multi-workspace deduplicating WinFsp filesystem\n\n\
         COMMANDS:\n  \
           dedupfs init <store> [--codec zstd1|lz4|none] [--pack-size 128M]\n  \
           dedupfs import <store> <source_dir> [--name <ws>] [--threads N]\n  \
           dedupfs create <store> <ws>\n  \
           dedupfs clone <store> <src_ws> <new_ws>\n  \
           dedupfs rm <store> <ws>\n  \
           dedupfs list <store>\n  \
           dedupfs stats <store>\n  \
           dedupfs dedup <store> [ws] [--idle 0]\n  \
           dedupfs gc <store> [--threshold 30]\n  \
           dedupfs mount <store> <ws> <mount_point> [--cache 256M] [--dedup-interval 60] [--dedup-idle 30]\n  \
           dedupfs umount <mount_point> [--force]"
    );
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "init" => {
            let store_dir = args
                .get(1)
                .ok_or("usage: dedupfs init <store> [--codec zstd1|lz4|none] [--pack-size 128M]")?;
            let mut cfg = StoreConfig::default();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--codec" => {
                        let raw = args.get(i + 1).ok_or("--codec requires a value")?;
                        cfg.codec = Codec::parse(raw)?;
                        i += 2;
                    }
                    "--pack-size" => {
                        let raw = args.get(i + 1).ok_or("--pack-size requires a value")?;
                        cfg.pack_target_bytes = parse_size(raw)?;
                        i += 2;
                    }
                    other => return Err(format!("unknown option {other:?}").into()),
                }
            }
            let store = DedupStore::init(store_dir, cfg)?;
            println!(
                "initialized dedup store at {} (codec={}, pack_size={})",
                store.root().display(),
                store.config().codec.as_str(),
                fmt_bytes(store.config().pack_target_bytes),
            );
        }
        "create" => {
            let store_dir = args.get(1).ok_or("usage: dedupfs create <store> <ws>")?;
            let ws_name = args.get(2).ok_or("usage: dedupfs create <store> <ws>")?;
            let store = DedupStore::open(store_dir)?;
            let ws_id = store.create_workspace(ws_name)?;
            println!("created workspace `{ws_name}` (id={ws_id})");
        }
        "import" => {
            let store_dir = args
                .get(1)
                .ok_or("usage: dedupfs import <store> <source_dir> [--name <ws>] [--threads N]")?;
            let source_dir = args
                .get(2)
                .ok_or("usage: dedupfs import <store> <source_dir> [--name <ws>] [--threads N]")?;
            let mut ws_name: Option<String> = None;
            let mut threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => {
                        ws_name = Some(args.get(i + 1).ok_or("--name requires a value")?.clone());
                        i += 2;
                    }
                    "--threads" => {
                        let raw = args.get(i + 1).ok_or("--threads requires a value")?;
                        threads = raw.parse::<usize>()?.max(1);
                        i += 2;
                    }
                    other => return Err(format!("unknown option {other:?}").into()),
                }
            }
            let src_path = PathBuf::from(source_dir);
            let ws = ws_name.unwrap_or_else(|| {
                src_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "default".to_owned())
            });
            let store = DedupStore::open(store_dir)?;
            let start = Instant::now();
            let cb = Box::new(move |p: ImportProgress| {
                let secs = start.elapsed().as_secs_f64().max(0.001);
                let mb_s = p.logical_bytes as f64 / (1024.0 * 1024.0) / secs;
                eprintln!(
                    "  [{:>5.0}s] {:>9} files, {} logical -> {} new unique ({} stored), {:.0} MB/s",
                    secs,
                    p.files,
                    fmt_bytes(p.logical_bytes),
                    fmt_bytes(p.new_unique_bytes),
                    fmt_bytes(p.stored_bytes),
                    mb_s,
                );
            });
            let sum = store.import_dir(&src_path, &ws, threads, Some(cb))?;
            println!(
                "imported `{}` (ws_id={}): {} files, {} dirs, {} logical -> {} new unique ({} stored, {} hot-DB pinned, {} errors) in {:.1}s ({:.0} MB/s)",
                sum.workspace,
                sum.ws_id,
                sum.files,
                sum.dirs,
                fmt_bytes(sum.logical_bytes),
                fmt_bytes(sum.new_unique_bytes),
                fmt_bytes(sum.stored_bytes),
                sum.overlay_pinned_files,
                sum.errors,
                sum.elapsed_secs,
                sum.logical_bytes as f64 / (1024.0 * 1024.0) / sum.elapsed_secs.max(0.001),
            );
        }
        "clone" => {
            let store_dir = args
                .get(1)
                .ok_or("usage: dedupfs clone <store> <src_ws> <new_ws>")?;
            let src_ws = args
                .get(2)
                .ok_or("usage: dedupfs clone <store> <src_ws> <new_ws>")?;
            let new_ws = args
                .get(3)
                .ok_or("usage: dedupfs clone <store> <src_ws> <new_ws>")?;
            let store = DedupStore::open(store_dir)?;
            let t0 = Instant::now();
            let ws_id = store.clone_workspace(src_ws, new_ws)?;
            println!(
                "cloned `{src_ws}` -> `{new_ws}` (ws_id={ws_id}) in {:.2}s (0 bytes copied)",
                t0.elapsed().as_secs_f64()
            );
        }
        "rm" => {
            let store_dir = args.get(1).ok_or("usage: dedupfs rm <store> <ws>")?;
            let ws = args.get(2).ok_or("usage: dedupfs rm <store> <ws>")?;
            let store = DedupStore::open(store_dir)?;
            store.remove_workspace(ws)?;
            println!("removed workspace `{ws}` (run `dedupfs gc` to reclaim dead pack space)");
        }
        "list" => {
            let store_dir = args.get(1).ok_or("usage: dedupfs list <store>")?;
            let store = DedupStore::open(store_dir)?;
            let list = store.list_workspaces()?;
            println!(
                "{:<4} {:<24} {:>10} {:>8} {:>12} {:>12}",
                "ID", "Workspace", "Files", "Dirs", "Logical", "Overlay"
            );
            for w in list {
                println!(
                    "{:<4} {:<24} {:>10} {:>8} {:>12} {:>12}",
                    w.ws_id,
                    w.name,
                    w.files,
                    w.dirs,
                    fmt_bytes(w.logical_bytes),
                    fmt_bytes(w.overlay_bytes),
                );
            }
        }
        "stats" => {
            let store_dir = args.get(1).ok_or("usage: dedupfs stats <store>")?;
            let store = DedupStore::open(store_dir)?;
            let s = store.stats()?;
            let physical = s.pack_disk_bytes + s.overlay_bytes + s.meta_db_bytes;
            let saved_pct = if s.total_logical_bytes == 0 {
                0.0
            } else {
                (1.0 - (physical as f64 / s.total_logical_bytes as f64)) * 100.0
            };
            println!("store          : {}", store.root().display());
            println!(
                "config         : codec={}, pack_target={}",
                store.config().codec.as_str(),
                fmt_bytes(store.config().pack_target_bytes)
            );
            println!("workspaces     : {}", s.workspaces);
            println!(
                "logical data   : {} ({} files)",
                fmt_bytes(s.total_logical_bytes),
                s.total_files
            );
            println!(
                "unique chunks  : {} chunks, {} raw -> {} stored",
                s.unique_chunks,
                fmt_bytes(s.unique_raw_bytes),
                fmt_bytes(s.stored_chunk_bytes),
            );
            println!(
                "packs on disk  : {} packs, {} total ({} dead)",
                s.pack_count,
                fmt_bytes(s.pack_disk_bytes),
                fmt_bytes(s.pack_dead_bytes)
            );
            println!(
                "overlay files  : {} files ({})",
                s.overlay_files,
                fmt_bytes(s.overlay_bytes)
            );
            println!("meta.sqlite    : {}", fmt_bytes(s.meta_db_bytes));
            println!(
                "total physical : {} (net savings: {:.1}%)",
                fmt_bytes(physical),
                saved_pct
            );
        }
        "dedup" => {
            let store_dir = args
                .get(1)
                .ok_or("usage: dedupfs dedup <store> [ws] [--idle 0]")?;
            let mut ws: Option<String> = None;
            let mut idle_secs = 0u64;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--idle" => {
                        idle_secs = args.get(i + 1).ok_or("--idle requires seconds")?.parse()?;
                        i += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option {other:?}").into());
                    }
                    other => {
                        ws = Some(other.to_owned());
                        i += 1;
                    }
                }
            }
            let store = DedupStore::open(store_dir)?;
            let sum = store.dedup_workspace(ws.as_deref(), idle_secs, |_, _| false)?;
            println!(
                "deduped {} / {} overlay files: {} raw -> {} new stored, reclaimed {} from overlay",
                sum.deduped_files,
                sum.scanned,
                fmt_bytes(sum.raw_bytes),
                fmt_bytes(sum.stored_bytes),
                fmt_bytes(sum.reclaimed_overlay_bytes),
            );
        }
        "gc" => {
            let store_dir = args
                .get(1)
                .ok_or("usage: dedupfs gc <store> [--threshold 30]")?;
            let mut threshold = 30u8;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--threshold" => {
                        threshold = args
                            .get(i + 1)
                            .ok_or("--threshold requires percentage 0..100")?
                            .parse()?;
                        i += 2;
                    }
                    other => return Err(format!("unknown option {other:?}").into()),
                }
            }
            let store = DedupStore::open(store_dir)?;
            let sum = store.gc(threshold)?;
            println!(
                "gc complete: deleted {} empty pack(s), compacted {} pack(s) ({} chunks moved), reclaimed {}",
                sum.deleted_empty_packs,
                sum.compacted_packs,
                sum.moved_chunks,
                fmt_bytes(sum.reclaimed_disk_bytes),
            );
        }
        "mount" => {
            let store_dir = args.get(1).ok_or(
                "usage: dedupfs mount <store> <ws> <mount_point> [--cache 256M] [--dedup-interval 60] [--dedup-idle 30]",
            )?;
            let ws = args.get(2).ok_or("missing workspace name")?;
            let mount_point = args.get(3).ok_or("missing mount point")?;
            let mut opts = MountOptions {
                cache_bytes: DEFAULT_CACHE_BYTES,
                dedup_interval_secs: 60,
                dedup_idle_secs: 30,
            };
            let mut i = 4;
            while i < args.len() {
                match args[i].as_str() {
                    "--cache" => {
                        let raw = args.get(i + 1).ok_or("--cache requires a size")?;
                        opts.cache_bytes = parse_size(raw)? as usize;
                        i += 2;
                    }
                    "--dedup-interval" => {
                        opts.dedup_interval_secs =
                            args.get(i + 1).ok_or("--dedup-interval requires seconds")?.parse()?;
                        i += 2;
                    }
                    "--dedup-idle" => {
                        opts.dedup_idle_secs =
                            args.get(i + 1).ok_or("--dedup-idle requires seconds")?.parse()?;
                        i += 2;
                    }
                    other => return Err(format!("unknown option {other:?}").into()),
                }
            }
            let _ = DEFAULT_PACK_TARGET_BYTES;
            run_mount(
                &PathBuf::from(store_dir),
                ws,
                &PathBuf::from(mount_point),
                opts,
            )?;
        }
        "umount" => {
            let mount_point = args
                .get(1)
                .ok_or("usage: dedupfs umount <mount_point> [--force]")?;
            let force = args.iter().skip(2).any(|a| a == "--force");
            let reply = request_umount(&PathBuf::from(mount_point), force)?;
            println!("{reply}");
        }
        other => return Err(format!("unknown command {other:?}; run `dedupfs --help`").into()),
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("dedupfs: {err}");
        std::process::exit(1);
    }
}
