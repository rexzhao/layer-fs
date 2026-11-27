//! `dedupfs` CLI — Unity-focused multi-workspace deduplicating WinFsp filesystem.
//!
//! Store discovery order (for all commands other than `init`):
//!   1. Explicit `--store <dir>` / `--store=<dir>` flag (anywhere on the command line)
//!   2. Current directory or nearest ancestor containing `store.json` + `meta.sqlite`
//!   3. `DEDUPFS_STORE` environment variable

use fs_dedup::{
    request_umount, run_mount, Codec, DedupStore, ImportProgress, MountOptions, StoreConfig,
    DEFAULT_CACHE_BYTES, DEFAULT_PACK_TARGET_BYTES, UNITY_IMPORT_EXCLUDE_PRESET,
    UNITY_NO_DEDUP_PRESET,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn is_store_dir(path: &Path) -> bool {
    path.join("store.json").is_file() && path.join("meta.sqlite").is_file()
}

fn discover_store(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(mut cur) = std::env::current_dir() {
        loop {
            if is_store_dir(&cur) {
                return Ok(cur);
            }
            if !cur.pop() {
                break;
            }
        }
    }
    if let Some(env) = std::env::var_os("DEDUPFS_STORE") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    Err("no dedup store found; run from inside a store directory, pass `--store <dir>`, or set DEDUPFS_STORE".to_owned())
}

fn parse_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim().to_ascii_uppercase();
    let (num_str, mult): (&str, u64) =
        if let Some(v) = s.strip_suffix("GIB").or_else(|| s.strip_suffix('G')) {
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
         STORE DISCOVERY (all commands except `init`):\n  \
           1. `--store <dir>` (anywhere on the command line)\n  \
           2. Current directory or nearest ancestor store\n  \
           3. `DEDUPFS_STORE` environment variable\n\n\
         COMMANDS:\n  \
           dedupfs init [dir] [--codec zstd1|lz4|none] [--pack-size 128M] [--exclude <pat>] [--no-dedup <pat>] [--unity-preset]\n  \
           dedupfs config [--add-exclude <pat>] [--rm-exclude <pat>] [--add-no-dedup <pat>] [--rm-no-dedup <pat>] [--unity-preset]\n  \
           dedupfs import <source_dir> [--name <ws>] [--update] [--exclude <pat>] [--threads N]\n  \
           dedupfs create <ws>\n  \
           dedupfs clone <src_ws> <new_ws>\n  \
           dedupfs rm <ws>\n  \
           dedupfs list\n  \
           dedupfs stats\n  \
           dedupfs dedup [ws] [--idle 0]\n  \
           dedupfs gc [--threshold 30]\n  \
           dedupfs mount <ws> <mount_point> [--cache 256M] [--dedup-interval 60] [--dedup-idle 30]\n  \
           dedupfs umount <mount_point> [--force]"
    );
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    // Extract global `--store <dir>` / `--store=<dir>` anywhere on the command line.
    let mut explicit_store: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::with_capacity(raw_args.len());
    let mut k = 0;
    while k < raw_args.len() {
        if raw_args[k] == "--store" {
            let val = raw_args
                .get(k + 1)
                .ok_or("--store requires a directory path")?;
            explicit_store = Some(PathBuf::from(val));
            k += 2;
        } else if let Some(val) = raw_args[k].strip_prefix("--store=") {
            explicit_store = Some(PathBuf::from(val));
            k += 1;
        } else {
            args.push(raw_args[k].clone());
            k += 1;
        }
    }

    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "init" => {
            let mut cfg = StoreConfig::default();
            let mut pos: Vec<String> = Vec::new();
            let mut i = 1;
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
                    "--exclude" => {
                        let raw = args.get(i + 1).ok_or("--exclude requires a pattern")?;
                        push_unique(&mut cfg.import_exclude, raw);
                        i += 2;
                    }
                    "--no-dedup" => {
                        let raw = args.get(i + 1).ok_or("--no-dedup requires a pattern")?;
                        push_unique(&mut cfg.no_dedup, raw);
                        i += 2;
                    }
                    "--unity-preset" => {
                        for &p in UNITY_IMPORT_EXCLUDE_PRESET {
                            push_unique(&mut cfg.import_exclude, p);
                        }
                        for &p in UNITY_NO_DEDUP_PRESET {
                            push_unique(&mut cfg.no_dedup, p);
                        }
                        i += 1;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option {other:?}").into());
                    }
                    other => {
                        pos.push(other.to_owned());
                        i += 1;
                    }
                }
            }
            if pos.len() > 1 {
                return Err(
                    "usage: dedupfs init [dir] [--codec zstd1|lz4|none] [--pack-size 128M] [--exclude <pat>] [--no-dedup <pat>] [--unity-preset]"
                        .into(),
                );
            }
            let store_dir = pos
                .first()
                .map(PathBuf::from)
                .or(explicit_store)
                .unwrap_or_else(|| PathBuf::from("."));
            let store = DedupStore::init(&store_dir, cfg)?;
            println!(
                "initialized dedup store at {} (codec={}, pack_size={}, import_exclude={}, no_dedup={})",
                store.root().display(),
                store.config().codec.as_str(),
                fmt_bytes(store.config().pack_target_bytes),
                store.config().import_exclude.len(),
                store.config().no_dedup.len(),
            );
        }
        "config" => {
            let mut add_exclude: Vec<String> = Vec::new();
            let mut rm_exclude: Vec<String> = Vec::new();
            let mut add_no_dedup: Vec<String> = Vec::new();
            let mut rm_no_dedup: Vec<String> = Vec::new();
            let mut unity_preset = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--show" => {
                        i += 1;
                    }
                    "--add-exclude" => {
                        add_exclude.push(
                            args.get(i + 1)
                                .ok_or("--add-exclude requires a pattern")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--rm-exclude" => {
                        rm_exclude.push(
                            args.get(i + 1)
                                .ok_or("--rm-exclude requires a pattern")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--add-no-dedup" => {
                        add_no_dedup.push(
                            args.get(i + 1)
                                .ok_or("--add-no-dedup requires a pattern")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--rm-no-dedup" => {
                        rm_no_dedup.push(
                            args.get(i + 1)
                                .ok_or("--rm-no-dedup requires a pattern")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--unity-preset" => {
                        unity_preset = true;
                        i += 1;
                    }
                    other => {
                        return Err(format!(
                            "unknown option {other:?}; usage: dedupfs [--store <dir>] config [--add-exclude <pat>] [--rm-exclude <pat>] [--add-no-dedup <pat>] [--rm-no-dedup <pat>] [--unity-preset]"
                        )
                        .into());
                    }
                }
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let modified = !add_exclude.is_empty()
                || !rm_exclude.is_empty()
                || !add_no_dedup.is_empty()
                || !rm_no_dedup.is_empty()
                || unity_preset;
            let cfg = if modified {
                DedupStore::update_config(&store_dir, |cfg| {
                    if unity_preset {
                        for &p in UNITY_IMPORT_EXCLUDE_PRESET {
                            push_unique(&mut cfg.import_exclude, p);
                        }
                        for &p in UNITY_NO_DEDUP_PRESET {
                            push_unique(&mut cfg.no_dedup, p);
                        }
                    }
                    for p in &add_exclude {
                        push_unique(&mut cfg.import_exclude, p);
                    }
                    for p in &rm_exclude {
                        cfg.import_exclude.retain(|x| !x.eq_ignore_ascii_case(p));
                    }
                    for p in &add_no_dedup {
                        push_unique(&mut cfg.no_dedup, p);
                    }
                    for p in &rm_no_dedup {
                        cfg.no_dedup.retain(|x| !x.eq_ignore_ascii_case(p));
                    }
                })?
            } else {
                DedupStore::open(&store_dir)?.config().clone()
            };
            println!("store          : {}", store_dir.display());
            println!("codec          : {}", cfg.codec.as_str());
            println!("pack_target    : {}", fmt_bytes(cfg.pack_target_bytes));
            println!("import_exclude : {:?}", cfg.import_exclude);
            println!("no_dedup       : {:?}", cfg.no_dedup);
        }
        "create" => {
            if args.len() != 2 {
                return Err("usage: dedupfs [--store <dir>] create <ws>".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let ws_name = args[1].as_str();
            let store = DedupStore::open(&store_dir)?;
            let ws_id = store.create_workspace(ws_name)?;
            println!("created workspace `{ws_name}` (id={ws_id})");
        }
        "import" => {
            let mut ws_name: Option<String> = None;
            let mut update = false;
            let mut extra_excludes: Vec<String> = Vec::new();
            let mut threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            let mut pos: Vec<String> = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => {
                        ws_name = Some(args.get(i + 1).ok_or("--name requires a value")?.clone());
                        i += 2;
                    }
                    "--update" | "-u" => {
                        update = true;
                        i += 1;
                    }
                    "--exclude" => {
                        extra_excludes.push(
                            args.get(i + 1)
                                .ok_or("--exclude requires a pattern")?
                                .clone(),
                        );
                        i += 2;
                    }
                    "--threads" => {
                        let raw = args.get(i + 1).ok_or("--threads requires a value")?;
                        threads = raw.parse::<usize>()?.max(1);
                        i += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option {other:?}").into());
                    }
                    other => {
                        pos.push(other.to_owned());
                        i += 1;
                    }
                }
            }
            if pos.len() != 1 {
                return Err(
                    "usage: dedupfs [--store <dir>] import <source_dir> [--name <ws>] [--update] [--exclude <pat>] [--threads N]"
                        .into(),
                );
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let src_path = PathBuf::from(&pos[0]);
            let ws = ws_name.unwrap_or_else(|| {
                src_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "default".to_owned())
            });
            let store = DedupStore::open(&store_dir)?;
            let start = Instant::now();
            let cb = Box::new(move |p: ImportProgress| {
                let secs = start.elapsed().as_secs_f64().max(0.001);
                let mb_s = p.logical_bytes as f64 / (1024.0 * 1024.0) / secs;
                if update {
                    eprintln!(
                        "  [{:>5.0}s] {:>9} files (unchanged={}, updated={}, removed={}, excluded={}), {} logical -> {} new unique ({} stored), {:.0} MB/s",
                        secs,
                        p.files,
                        p.unchanged_files,
                        p.updated_files,
                        p.removed_entries,
                        p.excluded_entries,
                        fmt_bytes(p.logical_bytes),
                        fmt_bytes(p.new_unique_bytes),
                        fmt_bytes(p.stored_bytes),
                        mb_s,
                    );
                } else {
                    eprintln!(
                        "  [{:>5.0}s] {:>9} files (excluded={}), {} logical -> {} new unique ({} stored), {:.0} MB/s",
                        secs,
                        p.files,
                        p.excluded_entries,
                        fmt_bytes(p.logical_bytes),
                        fmt_bytes(p.new_unique_bytes),
                        fmt_bytes(p.stored_bytes),
                        mb_s,
                    );
                }
            });
            let sum = store.import_dir_with_options(
                &src_path,
                &ws,
                threads,
                &extra_excludes,
                update,
                Some(cb),
            )?;
            if update {
                println!(
                    "updated `{}` (ws_id={}): {} files (unchanged={}, updated={}, added={}, removed={}, excluded={}), {} dirs, {} logical -> {} new unique ({} stored, {} hot-DB pinned, {} errors) in {:.1}s",
                    sum.workspace,
                    sum.ws_id,
                    sum.files,
                    sum.unchanged_files,
                    sum.updated_files,
                    sum.files.saturating_sub(sum.unchanged_files + sum.updated_files),
                    sum.removed_entries,
                    sum.excluded_entries,
                    sum.dirs,
                    fmt_bytes(sum.logical_bytes),
                    fmt_bytes(sum.new_unique_bytes),
                    fmt_bytes(sum.stored_bytes),
                    sum.overlay_pinned_files,
                    sum.errors,
                    sum.elapsed_secs,
                );
            } else {
                println!(
                    "imported `{}` (ws_id={}): {} files (excluded={}), {} dirs, {} logical -> {} new unique ({} stored, {} hot-DB pinned, {} errors) in {:.1}s ({:.0} MB/s)",
                    sum.workspace,
                    sum.ws_id,
                    sum.files,
                    sum.excluded_entries,
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
        }
        "clone" => {
            if args.len() != 3 {
                return Err("usage: dedupfs [--store <dir>] clone <src_ws> <new_ws>".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let (src_ws, new_ws) = (args[1].as_str(), args[2].as_str());
            let store = DedupStore::open(&store_dir)?;
            let t0 = Instant::now();
            let ws_id = store.clone_workspace(src_ws, new_ws)?;
            println!(
                "cloned `{src_ws}` -> `{new_ws}` (ws_id={ws_id}) in {:.2}s (0 bytes copied)",
                t0.elapsed().as_secs_f64()
            );
        }
        "rm" => {
            if args.len() != 2 {
                return Err("usage: dedupfs [--store <dir>] rm <ws>".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let ws = args[1].as_str();
            let store = DedupStore::open(&store_dir)?;
            store.remove_workspace(ws)?;
            println!("removed workspace `{ws}` (run `dedupfs gc` to reclaim dead pack space)");
        }
        "list" => {
            if args.len() != 1 {
                return Err("usage: dedupfs [--store <dir>] list".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let store = DedupStore::open(&store_dir)?;
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
            if args.len() != 1 {
                return Err("usage: dedupfs [--store <dir>] stats".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let store = DedupStore::open(&store_dir)?;
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
            let mut idle_secs = 0u64;
            let mut pos: Vec<String> = Vec::new();
            let mut i = 1;
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
                        pos.push(other.to_owned());
                        i += 1;
                    }
                }
            }
            if pos.len() > 1 {
                return Err("usage: dedupfs [--store <dir>] dedup [ws] [--idle 0]".into());
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let ws = pos.first().map(String::as_str);
            let store = DedupStore::open(&store_dir)?;
            let sum = store.dedup_workspace(ws, idle_secs, |_, _| false)?;
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
            let mut threshold = 30u8;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--threshold" => {
                        threshold = args
                            .get(i + 1)
                            .ok_or("--threshold requires percentage 0..100")?
                            .parse()?;
                        i += 2;
                    }
                    other => {
                        return Err(format!(
                            "unexpected argument {other:?}; usage: dedupfs [--store <dir>] gc [--threshold 30]"
                        )
                        .into());
                    }
                }
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let store = DedupStore::open(&store_dir)?;
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
            let mut opts = MountOptions {
                cache_bytes: DEFAULT_CACHE_BYTES,
                dedup_interval_secs: 60,
                dedup_idle_secs: 30,
            };
            let mut pos: Vec<String> = Vec::new();
            let mut i = 1;
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
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option {other:?}").into());
                    }
                    other => {
                        pos.push(other.to_owned());
                        i += 1;
                    }
                }
            }
            if pos.len() != 2 {
                return Err(
                    "usage: dedupfs [--store <dir>] mount <ws> <mount_point> [--cache 256M] [--dedup-interval 60] [--dedup-idle 30]"
                        .into(),
                );
            }
            let store_dir = discover_store(explicit_store.as_deref())?;
            let (ws, mount_point) = (pos[0].as_str(), PathBuf::from(&pos[1]));
            let _ = DEFAULT_PACK_TARGET_BYTES;
            run_mount(&store_dir, ws, &mount_point, opts)?;
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

fn push_unique(list: &mut Vec<String>, pat: &str) {
    if !list.iter().any(|x| x.eq_ignore_ascii_case(pat)) {
        list.push(pat.to_owned());
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("dedupfs: {err}");
        std::process::exit(1);
    }
}
