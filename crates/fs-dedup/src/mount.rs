//! Live Windows WinFsp mount with background periodic cold-file deduplication and
//! clean remote `umount [--force]` / `dedup` control via Windows named pipe.

use crate::backend::DedupWorkspaceBackend;
use crate::error::{Error, Result};
use crate::store::DedupStore;
use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::loader::default_library;
use fs_winfsp::mounted::mount_serving_with_timeout;
use fs_winfsp::ops::DynOps;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Options for mounting a workspace via WinFsp.
#[derive(Clone, Debug)]
pub struct MountOptions {
    pub cache_bytes: usize,
    /// Seconds between background cold-file dedup sweeps (`0` disables background dedup).
    pub dedup_interval_secs: u64,
    /// Minimum idle time in seconds before a closed overlay file is eligible for background dedup.
    pub dedup_idle_secs: u64,
    /// WinFsp kernel `FileInfoTimeout` in milliseconds (`5000` default for fast Unity / `git status` / IDE stat sweeps).
    pub info_timeout_ms: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MountRecord {
    pid: u32,
    store: String,
    workspace: String,
    mount_point: String,
    pipe_name: String,
}

fn mounts_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("dedupfs").join("mounts")
}

fn mount_key(mount_point: &Path) -> String {
    let norm = mount_point
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let hash = blake3::hash(norm.as_bytes());
    let hex: String = hash.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex
}

fn record_path(mount_point: &Path) -> PathBuf {
    mounts_dir().join(format!("{}.json", mount_key(mount_point)))
}

fn norm_path_key(p: &Path) -> String {
    let canon = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// Returns `Some((mount_point, workspace, pipe_name))` if a live `dedupfs mount` process is
/// currently serving `store_root`.
pub fn active_mount_info_for_store(store_root: &Path) -> Option<(String, String, String)> {
    let target = norm_path_key(store_root);
    let entries = fs::read_dir(mounts_dir()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<MountRecord>(&raw) else {
            continue;
        };
        if norm_path_key(Path::new(&rec.store)) != target {
            continue;
        }
        #[cfg(windows)]
        {
            if win_pipe::send_command(&rec.pipe_name, "ping").is_ok() {
                return Some((rec.mount_point, rec.workspace, rec.pipe_name));
            }
        }
        #[cfg(not(windows))]
        {
            return Some((rec.mount_point, rec.workspace, rec.pipe_name));
        }
    }
    None
}

/// Returns `Some(mount_point)` if a live `dedupfs mount` process is currently serving `store_root`.
/// Used by `fsck --fix` and `gc` to refuse modifying packs/metadata concurrently with a mount.
pub fn active_mount_for_store(store_root: &Path) -> Option<String> {
    active_mount_info_for_store(store_root).map(|(mp, _, _)| mp)
}

/// Triggers a safe online `dedup` sweep through a running mount's control pipe so active handles,
/// dirty overlay metadata, and `--idle <idle_secs>` are all respected (N8).
pub fn trigger_live_dedup(pipe_name: &str, idle_secs: u64) -> Result<String> {
    #[cfg(windows)]
    {
        let cmd = format!("dedup {idle_secs}");
        let reply = win_pipe::send_command(pipe_name, &cmd)?;
        if let Some(err_msg) = reply.strip_prefix("ERR ") {
            return Err(Error::Other(err_msg.trim().to_owned()));
        }
        Ok(reply.trim().to_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, idle_secs);
        Err(Error::Other("mount control pipe is Windows-only".into()))
    }
}

fn volume_info(total_size: u64) -> VolumeInfo {
    let label_str = "dedupfs";
    let mut volume_label = [0u16; 32];
    for (i, u) in label_str.encode_utf16().enumerate() {
        volume_label[i] = u;
    }
    VolumeInfo {
        total_size,
        free_size: total_size / 2,
        volume_label_length: (label_str.len() * 2) as u16,
        volume_label,
    }
}

/// Mounts `ws_name` from `store_dir` at `mount_point`, running a background cold-file dedup thread
/// and a named-pipe control server so `dedupfs umount <mount_point> [--force]` can stop it cleanly.
pub fn run_mount(
    store_dir: &Path,
    ws_name: &str,
    mount_point: &Path,
    opts: MountOptions,
) -> Result<()> {
    let store = DedupStore::open_with_cache(store_dir, opts.cache_bytes)?;
    let backend = DedupWorkspaceBackend::open(Arc::clone(&store), ws_name)?;
    let ws_id = backend.ws_id();
    let ws_name_owned = ws_name.to_owned();
    let total_size = store.config().volume_size_bytes;

    let library = default_library();
    let cap_store_path = store.root().to_path_buf();
    let capacity: fs_winfsp::callbacks::CapacitySource = Arc::new(move || {
        Some(
            fs_winfsp::callbacks::host_capacity(&cap_store_path)
                .unwrap_or((total_size, total_size / 2)),
        )
    });

    let pipe_name = format!(
        r"\\.\pipe\dedupfs-{}-{}",
        mount_key(mount_point),
        std::process::id()
    );

    let stop_pair = Arc::new((Mutex::new(false), Condvar::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));

    let dyn_backend: DynOps = Box::new(backend);
    let mp_clone = mount_point.to_path_buf();
    let rec_path = record_path(mount_point);
    let store_path_str = store.root().display().to_string();

    let report = mount_serving_with_timeout(
        &library,
        mount_point,
        dyn_backend,
        volume_info(total_size),
        Some(capacity),
        opts.info_timeout_ms,
        move |mounted| {
            let adapter = mounted.adapter();

            // Write mount record for `umount` discovery.
            let _ = fs::create_dir_all(mounts_dir());
            let record = MountRecord {
                pid: std::process::id(),
                store: store_path_str.clone(),
                workspace: ws_name_owned.clone(),
                mount_point: mp_clone.display().to_string(),
                pipe_name: pipe_name.clone(),
            };
            if let Ok(json) = serde_json::to_string_pretty(&record) {
                let _ = fs::write(&rec_path, json);
            }

            eprintln!(
                "mounted workspace `{}` at {} (store={}, cache={} MiB, dedup={}s/{}s)",
                ws_name_owned,
                mp_clone.display(),
                store_path_str,
                opts.cache_bytes / (1024 * 1024),
                opts.dedup_interval_secs,
                opts.dedup_idle_secs,
            );

            // Spawn background maintenance thread:
            // - Periodically flushes buffered dirty overlay metadata every 2s (caps crash-loss window).
            // - Periodically runs cold-file deduplication when `dedup_interval_secs > 0`.
            let maint_handle = {
                let d_store = Arc::clone(&store);
                let d_adapter = Arc::clone(&adapter);
                let d_stop = Arc::clone(&stop_pair);
                let d_ws = ws_name_owned.clone();
                let dedup_interval = if opts.dedup_interval_secs > 0 {
                    Some(Duration::from_secs(opts.dedup_interval_secs))
                } else {
                    None
                };
                let flush_tick =
                    dedup_interval.map_or(Duration::from_secs(2), |d| d.min(Duration::from_secs(2)));
                let idle = opts.dedup_idle_secs;
                std::thread::spawn(move || {
                    let (lock, cvar) = &*d_stop;
                    let mut last_dedup = std::time::Instant::now();
                    let mut last_checkpoint = std::time::Instant::now();
                    let checkpoint_interval = Duration::from_secs(30);
                    loop {
                        let guard = lock.lock().unwrap();
                        let (guard, _) = cvar.wait_timeout(guard, flush_tick).unwrap();
                        if *guard {
                            break;
                        }
                        drop(guard);

                        // Persist any buffered foreground overlay size/mtime updates promptly.
                        let _ = d_adapter.backend().flush();

                        // Run non-blocking PASSIVE WAL checkpoint off the foreground dispatcher
                        // threads so write commits never stall on auto-checkpoint fsyncs.
                        if last_checkpoint.elapsed() >= checkpoint_interval {
                            last_checkpoint = std::time::Instant::now();
                            let _ = d_adapter.backend().checkpoint_wal();
                        }

                        if let Some(interval) = dedup_interval {
                            if last_dedup.elapsed() >= interval {
                                last_dedup = std::time::Instant::now();
                                if let Ok(sum) =
                                    d_store.dedup_workspace(Some(&d_ws), idle, |w, inode| {
                                        w == ws_id
                                            && (d_adapter.has_open_handles(inode)
                                                || d_adapter.backend().is_file_dirty(inode))
                                    })
                                {
                                    if sum.deduped_files > 0 {
                                        eprintln!(
                                            "[dedup] `{}`: {} files -> {:.1} MiB reclaimed ({:.1} MiB new stored)",
                                            d_ws,
                                            sum.deduped_files,
                                            sum.reclaimed_overlay_bytes as f64 / (1024.0 * 1024.0),
                                            sum.stored_bytes as f64 / (1024.0 * 1024.0),
                                        );
                                    }
                                }
                            }
                        }
                    }
                })
            };

            // Start Windows named-pipe control server on a background thread.
            #[cfg(windows)]
            let pipe_handle = {
                let p_name = pipe_name.clone();
                let p_adapter = Arc::clone(&adapter);
                let p_store = Arc::clone(&store);
                let p_ws = ws_name_owned.clone();
                let p_stop = Arc::clone(&stop_pair);
                let p_flag = Arc::clone(&stop_flag);
                std::thread::spawn(move || {
                    win_pipe::serve_pipe(
                        &p_name,
                        ws_id,
                        &p_ws,
                        &p_store,
                        &p_adapter,
                        &p_stop,
                        &p_flag,
                    );
                })
            };

            // Wait until signaled to stop (via `umount` pipe command or Ctrl-C).
            {
                let (lock, cvar) = &*stop_pair;
                let mut stopped = lock.lock().unwrap();
                while !*stopped {
                    stopped = cvar.wait(stopped).unwrap();
                }
            }

            stop_flag.store(true, Ordering::SeqCst);
            let _ = adapter.backend().flush();
            let _ = fs::remove_file(&rec_path);
            let _ = maint_handle.join();
            #[cfg(windows)]
            {
                // Wake blocked ConnectNamedPipe if needed and join cleanly.
                let _ = win_pipe::send_command(&pipe_name, "ping");
                let _ = pipe_handle.join();
            }
            Ok(())
        },
    )
    .map_err(|e| Error::Other(format!("WinFsp mount failed: {e:?}")))?;

    if report.dispatcher_result != 0 {
        return Err(Error::Other(format!(
            "WinFsp dispatcher exited with NTSTATUS 0x{:08x}",
            report.dispatcher_result as u32
        )));
    }
    Ok(())
}

/// Sends an unmount request to a running `dedupfs mount` instance for `mount_point`.
pub fn request_umount(mount_point: &Path, force: bool) -> Result<String> {
    let rec_path = record_path(mount_point);
    if !rec_path.exists() {
        return Err(Error::NotFound(format!(
            "no active dedupfs mount record for {}",
            mount_point.display()
        )));
    }
    let raw = fs::read_to_string(&rec_path)?;
    let record: MountRecord = serde_json::from_str(&raw)
        .map_err(|e| Error::Corrupt(format!("bad mount record: {e}")))?;

    #[cfg(windows)]
    {
        let cmd = if force { "umount-force" } else { "umount" };
        let reply = win_pipe::send_command(&record.pipe_name, cmd)?;
        if let Some(err_msg) = reply.strip_prefix("ERR ") {
            return Err(Error::Other(err_msg.trim().to_owned()));
        }
        Ok(reply.trim().to_owned())
    }
    #[cfg(not(windows))]
    {
        let _ = (record, force);
        Err(Error::Other("umount control pipe is Windows-only".into()))
    }
}

#[cfg(windows)]
mod win_pipe {
    use super::*;
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const INVALID_HANDLE_VALUE: isize = -1;
    const ERROR_PIPE_CONNECTED: u32 = 535;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buf_size: u32,
            in_buf_size: u32,
            default_timeout: u32,
            security_attrs: *const c_void,
        ) -> *mut c_void;
        fn ConnectNamedPipe(handle: *mut c_void, overlapped: *mut c_void) -> i32;
        fn FlushFileBuffers(handle: *mut c_void) -> i32;
        fn DisconnectNamedPipe(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    pub fn serve_pipe(
        pipe_name: &str,
        ws_id: u64,
        ws_name: &str,
        store: &Arc<DedupStore>,
        adapter: &Arc<fs_winfsp::DynAdapter>,
        stop_pair: &Arc<(Mutex<bool>, Condvar)>,
        stop_flag: &Arc<AtomicBool>,
    ) {
        let wide: Vec<u16> = std::ffi::OsStr::new(pipe_name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        while !stop_flag.load(Ordering::Relaxed) {
            let raw = unsafe {
                CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_WAIT,
                    4,
                    4096,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            if raw as isize == INVALID_HANDLE_VALUE || raw.is_null() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let connected = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;

            // Wrap handle in File so it always closes cleanly on drop.
            let mut pipe_file = unsafe { fs::File::from_raw_handle(raw) };
            if !connected {
                continue;
            }
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&pipe_file);
                let _ = reader.read_line(&mut line);
            }
            let cmd = line.trim();

            // N8: Run `dedup [idle_secs]` on a dedicated thread so the control pipe stays
            // immediately responsive to `umount`, `ping`, and `status` while dedup is running!
            if cmd == "dedup" || cmd.starts_with("dedup ") {
                let idle_secs: u64 = cmd
                    .strip_prefix("dedup")
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                let d_store = Arc::clone(store);
                let d_adapter = Arc::clone(adapter);
                let d_ws = ws_name.to_owned();
                let raw_usize = raw as usize;
                std::thread::spawn(move || {
                    let _ = d_adapter.backend().flush();
                    let response = match d_store.dedup_workspace(
                        Some(&d_ws),
                        idle_secs,
                        |w, inode| {
                            w == ws_id
                                && (d_adapter.has_open_handles(inode)
                                    || d_adapter.backend().is_file_dirty(inode))
                        },
                    ) {
                        Ok(s) => format!(
                            "OK deduped_files={} reclaimed_bytes={} idle_secs={}\n",
                            s.deduped_files, s.reclaimed_overlay_bytes, idle_secs
                        ),
                        Err(e) => format!("ERR {e}\n"),
                    };
                    let _ = pipe_file.write_all(response.as_bytes());
                    let _ = pipe_file.flush();
                    unsafe {
                        let h = raw_usize as *mut c_void;
                        FlushFileBuffers(h);
                        DisconnectNamedPipe(h);
                    }
                });
                continue;
            }

            let response = match cmd {
                "ping" => "OK pong\n".to_owned(),
                "status" => {
                    let (files, dirs) = adapter.open_handle_kinds();
                    format!("OK workspace={ws_name} open_files={files} open_dirs={dirs}\n")
                }
                "umount" | "umount-force" => {
                    let force = cmd == "umount-force";
                    let (open_files, _) = adapter.open_handle_kinds();
                    if open_files > 0 && !force {
                        format!(
                            "ERR {open_files} open file handle(s); close files or pass --force\n"
                        )
                    } else {
                        let (lock, cvar) = &**stop_pair;
                        *lock.lock().unwrap() = true;
                        cvar.notify_all();
                        stop_flag.store(true, Ordering::SeqCst);
                        "OK unmounting\n".to_owned()
                    }
                }
                other => format!("ERR unknown command {other:?}\n"),
            };

            let _ = pipe_file.write_all(response.as_bytes());
            let _ = pipe_file.flush();
            unsafe {
                FlushFileBuffers(raw);
                DisconnectNamedPipe(raw);
            }
        }
    }

    pub fn send_command(pipe_name: &str, cmd: &str) -> Result<String> {
        let mut pipe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name)
            .map_err(|e| Error::Other(format!("connect mount pipe {pipe_name}: {e}")))?;
        pipe.write_all(format!("{cmd}\n").as_bytes())?;
        pipe.flush()?;
        let mut resp = String::new();
        let mut reader = BufReader::new(pipe);
        reader.read_line(&mut resp)?;
        Ok(resp)
    }
}
