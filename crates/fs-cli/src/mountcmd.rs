//! `layerfs mount | umount | list | version` — interactive Windows WinFsp entry points.
//!
//! Backed by `fs_layerfs::{Repo, WorkspaceHandle}` with:
//! - Live `RwLock<Option<Arc<DynAdapter>>>` so `switch` updates the control pipe across remounts;
//! - Zero `Box::leak`: `WorkspaceHandle` and `CapacitySource` are reference-counted (`Arc`) and
//!   closed cleanly on switch and unmount.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use fs_layerfs::{Error as LayerError, Repo, WorkspaceHandle};
use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::loader::default_library;
use fs_winfsp::mounted::mount_serving_with_timeout;
use fs_winfsp::ops::DynOps;

pub type Error = Box<dyn std::error::Error>;

/// Default WinFsp kernel metadata/dir/volume cache timeout for `layerfs mount` (1000 ms).
const DEFAULT_INFO_TIMEOUT_MS: u32 = 1000;

const UNMOUNT_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
const MOUNT_READY_ENV: &str = "LAYERFS_MOUNT_READY_FILE";
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(15);
const MOUNT_READY_POLL: Duration = Duration::from_millis(25);
const MOUNT_ACCESS_WAIT: Duration = Duration::from_secs(3);
const OURS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const OURS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;

fn state_path(mount_point: &Path) -> PathBuf {
    mounts_dir().join(format!("{}.mount", record_name(mount_point)))
}

pub use fs_cli::{mounts_dir, record_name};

pub fn version_parts(version: u32) -> (u16, u16) {
    (
        u16::try_from((version >> 16) & 0xFFFF).unwrap_or(0),
        u16::try_from(version & 0xFFFF).unwrap_or(0),
    )
}

#[must_use]
pub fn calibrated_major() -> u16 {
    fs_winfsp::abi::measured::DLL_BUILD
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(0)
}

fn volume_warning(store: &Path, advertised: u64) -> Option<String> {
    let (_host_total, host_free) = fs_winfsp::callbacks::host_capacity(store)?;
    if host_free >= advertised {
        return None;
    }
    Some(format!(
        "warning: {} advertises {} bytes but the host volume has {} bytes free; writes will fail before the volume reports full",
        store.display(),
        advertised,
        host_free
    ))
}

fn volume() -> VolumeInfo {
    VolumeInfo {
        total_size: 1 << 30,
        free_size: 1 << 29,
        // UTF-16 byte length of `"layerfs"` (7 chars * 2 bytes = 14).
        volume_label_length: 2 * 7,
        volume_label: {
            let mut label = [0u16; 32];
            for (index, unit) in "layerfs".encode_utf16().enumerate() {
                label[index] = unit;
            }
            label
        },
    }
}

/// `layerfs version` — WinFsp DLL version vs calibrated layout + upstream core engine info.
pub fn version() -> Result<(), Error> {
    let library = default_library();
    println!("core engine       : upstream layerfs v1.0.0 (SQLite CAS + overlay)");
    let (major, minor) = match fs_winfsp::handshake::read_version(&library) {
        Ok(raw) => version_parts(raw),
        Err(error) => {
            println!(
                "winfsp dll        : not loadable at {} ({error:?})",
                library.display()
            );
            return Ok(());
        }
    };
    let calibrated = calibrated_major();
    println!(
        "winfsp dll        : {} ({major}.{minor})",
        library.display()
    );
    println!(
        "calibrated against: {}",
        fs_winfsp::abi::measured::DLL_BUILD
    );
    println!(
        "measured on       : {}",
        fs_winfsp::abi::measured::MEASURED_ON
    );
    if major == calibrated {
        println!("abi gate          : ok (major {major} matches the measured layout)");
    } else {
        println!(
            "abi gate          : MISMATCH — the slot table was measured on {calibrated}.x, this DLL is {major}.x"
        );
    }
    Ok(())
}

/// `layerfs --store <dir> mount <mount_point> [--branch <name>] [--cache <size>] [-d|--background|--detach] [--no-version-gate]`
pub fn mount(
    store: &Path,
    branch: Option<&str>,
    cache_bytes: Option<usize>,
    arguments: &[String],
) -> Result<(), Error> {
    let mut positional: Vec<&String> = Vec::new();
    let mut inline_branch: Option<&str> = None;
    let mut inline_cache: Option<usize> = cache_bytes;
    let mut background = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--branch" | "--workspace" => {
                inline_branch = arguments.get(index + 1).map(String::as_str);
                index += 2;
            }
            "--cache" => {
                let raw = arguments
                    .get(index + 1)
                    .ok_or_else(|| fs_cli::Failure::usage("--cache requires a size (e.g. 256M)"))?;
                inline_cache = Some(fs_layerfs::parse_cache_size(raw)?);
                index += 2;
            }
            "-d" | "--background" | "--detach" => {
                background = true;
                index += 1;
            }
            "--no-version-gate" => index += 1,
            other if other.starts_with("--branch=") => {
                inline_branch = Some(&other["--branch=".len()..]);
                index += 1;
            }
            other if other.starts_with("--workspace=") => {
                inline_branch = Some(&other["--workspace=".len()..]);
                index += 1;
            }
            other if other.starts_with("--cache=") => {
                inline_cache = Some(fs_layerfs::parse_cache_size(&other["--cache=".len()..])?);
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(fs_cli::Failure::usage(format!("unknown mount option {other:?}")).into());
            }
            _ => {
                positional.push(&arguments[index]);
                index += 1;
            }
        }
    }

    let (store, mount_point) = match (positional.first(), positional.get(1)) {
        (Some(mount_point), None) => (store.to_path_buf(), PathBuf::from(mount_point.as_str())),
        (Some(store_arg), Some(mount_point)) => {
            eprintln!(
                "layerfs: note: prefer `layerfs --store <store> mount <mount_point>` over positional store"
            );
            (
                PathBuf::from(store_arg.as_str()),
                PathBuf::from(mount_point.as_str()),
            )
        }
        _ => {
            return Err(fs_cli::Failure::usage(
                "usage: layerfs --store <store> mount <mount_point> [--branch <name>] [--cache <size>] [-d|--background]",
            )
            .into())
        }
    };

    let requested_branch = inline_branch.or(branch).map(str::to_owned);
    let skip_gate = arguments.iter().any(|a| a == "--no-version-gate");

    if background {
        return spawn_background_mount(
            &store,
            &mount_point,
            requested_branch.as_deref(),
            inline_cache,
            skip_gate,
        );
    }

    let ready_file = std::env::var_os(MOUNT_READY_ENV).map(PathBuf::from);
    let signaled_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = mount_foreground(
        &store,
        &mount_point,
        requested_branch.as_deref(),
        inline_cache,
        skip_gate,
        ready_file.as_deref(),
        &signaled_ready,
    );
    if let Err(ref error) = result {
        if !signaled_ready.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Some(ref ready_path) = ready_file {
                let code = classify_error_code(error.as_ref());
                let sanitized = error.to_string().replace(['\r', '\n'], " ");
                let _ = write_ready_status(ready_path, &format!("ERR\t{code}\t{sanitized}\n"));
            }
        }
    }
    result
}

fn classify_error_code(error: &(dyn std::error::Error + 'static)) -> u8 {
    if let Some(failure) = error.downcast_ref::<fs_cli::Failure>() {
        return failure.code;
    }
    if let Some(err) = error.downcast_ref::<LayerError>() {
        return match err {
            LayerError::InvalidInput(_) => 2,
            LayerError::NotFound(_) => 3,
            LayerError::Busy(_) => 4,
            LayerError::Conflict(_) => 5,
            LayerError::Unsupported(_) | LayerError::Other(_) => 1,
        };
    }
    1
}

fn write_ready_status(path: &Path, content: &str) -> std::io::Result<()> {
    let part = path.with_extension("part");
    std::fs::write(&part, content.as_bytes())?;
    std::fs::rename(&part, path)
}

struct ReadyFileGuard(PathBuf);

impl Drop for ReadyFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("part"));
    }
}

fn parse_ready_status(
    text: &str,
    store: &Path,
    mount_point: &Path,
    fallback_branch: &str,
) -> Result<(), Error> {
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.splitn(5, '\t');
    match parts.next() {
        Some("OK") => {
            let pid = parts.next().unwrap_or("?");
            let profile = parts.next().unwrap_or("unknown");
            let branch = parts.next().unwrap_or(fallback_branch);
            let cache_mib = parts.next().unwrap_or("?");
            let wait_start = Instant::now();
            while wait_start.elapsed() < MOUNT_ACCESS_WAIT {
                if std::fs::read_dir(mount_point).is_ok() {
                    break;
                }
                std::thread::sleep(MOUNT_READY_POLL);
            }
            println!(
                "mounted {} at {} in background (pid {pid}, branch {branch}, profile={profile}, cache={cache_mib}MiB); stop with `layerfs umount {}`",
                store.display(),
                mount_point.display(),
                mount_point.display()
            );
            Ok(())
        }
        Some("ERR") => {
            let code: u8 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let message = parts.next().unwrap_or("background mount failed").to_owned();
            Err(fs_cli::Failure { message, code }.into())
        }
        _ => Err(format!("unexpected background mount handshake payload: {line:?}").into()),
    }
}

fn spawn_background_mount(
    store: &Path,
    mount_point: &Path,
    branch: Option<&str>,
    cache_bytes: Option<usize>,
    skip_gate: bool,
) -> Result<(), Error> {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    static READY_SEQ: AtomicU64 = AtomicU64::new(1);

    std::fs::create_dir_all(mounts_dir())?;
    let seq = READY_SEQ.fetch_add(1, Ordering::Relaxed);
    let ready_path = mounts_dir().join(format!(".ready-{}-{seq}.tmp", std::process::id()));
    let _ready_guard = ReadyFileGuard(ready_path.clone());

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--store").arg(store).arg("mount").arg(mount_point);
    if let Some(name) = branch {
        cmd.arg("--branch").arg(name);
    }
    if let Some(bytes) = cache_bytes {
        cmd.arg("--cache").arg(bytes.to_string());
    }
    if skip_gate {
        cmd.arg("--no-version-gate");
    }
    cmd.env(MOUNT_READY_ENV, &ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        clear_std_handle_inheritance();
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd.spawn()?;
    let started = Instant::now();
    let fallback_branch = branch.unwrap_or("?");
    while started.elapsed() < MOUNT_READY_TIMEOUT {
        if let Ok(text) = std::fs::read_to_string(&ready_path) {
            if text.starts_with("ERR\t") {
                let _ = child.wait();
            }
            return parse_ready_status(&text, store, mount_point, fallback_branch);
        }
        if let Some(status) = child.try_wait()? {
            if let Ok(text) = std::fs::read_to_string(&ready_path) {
                return parse_ready_status(&text, store, mount_point, fallback_branch);
            }
            return Err(format!("background mount process exited prematurely ({status})").into());
        }
        std::thread::sleep(MOUNT_READY_POLL);
    }

    let _ = child.kill();
    let _ = child.wait();
    Err(format!(
        "timed out waiting for background mount at {} after {:?}",
        mount_point.display(),
        MOUNT_READY_TIMEOUT
    )
    .into())
}

fn mount_foreground(
    store: &Path,
    mount_point: &Path,
    requested_branch: Option<&str>,
    cache_bytes: Option<usize>,
    skip_gate: bool,
    ready_file: Option<&Path>,
    signaled_ready: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Error> {
    if !skip_gate {
        let library = default_library();
        match fs_winfsp::handshake::read_version(&library) {
            Ok(raw) => {
                let (major, minor) = version_parts(raw);
                if major != calibrated_major() {
                    return Err(format!(
                        "refusing to mount: winfsp {major}.{minor} does not match calibrated layout ({})",
                        fs_winfsp::abi::measured::DLL_BUILD
                    )
                    .into());
                }
            }
            Err(error) => {
                return Err(format!(
                    "winfsp is not loadable at {} ({error:?})",
                    library.display()
                )
                .into());
            }
        }
    }

    let repo = Repo::open_with_cache(store, cache_bytes)?;
    let cache_mib = repo.cache_bytes() / (1024 * 1024);
    let name = repo.resolve_default_branch(requested_branch)?;
    let profile_str = repo.profile().as_str().to_owned();
    let volume_total = repo.volume_size_bytes();
    if let Some(warning) = volume_warning(store, volume_total) {
        eprintln!("{warning}");
    }

    let initial_ws = Arc::new(repo.open_workspace(&name)?);

    std::fs::create_dir_all(mounts_dir())?;
    let state = state_path(mount_point);
    let pipe = fs_cli::pipe_win::pipe_name(&record_name(mount_point), std::process::id());

    match record_is_live(&state) {
        Liveness::Live => {
            let owner = std::fs::read_to_string(&state).unwrap_or_default();
            let pid = record_field(&owner, "pid");
            return Err(fs_cli::Failure::in_use(format!(
                "{} is already mounted (pid {pid}) — `layerfs umount {}` first",
                mount_point.display(),
                mount_point.display()
            ))
            .into());
        }
        Liveness::Stale => {
            let _ = std::fs::remove_file(&state);
        }
        Liveness::Gone => {}
    }

    let record = switched_record(
        &canonical(store),
        &canonical(mount_point),
        &name,
        &name,
        &pipe,
    );
    let mut record_handle = hold_record(&state, &record)?;
    println!(
        "mounting {} at {} (branch {name}, profile={profile_str}, cache={cache_mib}MiB); stop with `layerfs umount {}`",
        store.display(),
        mount_point.display(),
        mount_point.display()
    );

    let adapter_slot: AdapterSlot = Arc::new(RwLock::new(None));
    let pipe_serving = Arc::new(PipeServing {
        store: canonical(store),
        current: RwLock::new(Current {
            workspace: name.clone(),
            branch: name.clone(),
            handle: Arc::clone(&initial_ws),
        }),
        mount_point: canonical(mount_point),
        record: state.clone(),
        adapter: Arc::clone(&adapter_slot),
    });
    let serving: Arc<dyn fs_cli::pipe_win::Serving> = Arc::clone(&pipe_serving) as _;
    let server = Arc::new(fs_cli::pipe_win::Server::new(pipe.clone(), serving));
    if let Ok(started) = std::thread::Builder::new()
        .name("layerfs-control".to_owned())
        .spawn({
            let server = Arc::clone(&server);
            move || {
                let _ = server.run();
            }
        })
    {
        std::mem::forget(started);
    }

    let library = default_library();
    install_console_handler();

    let mut current_handle = initial_ws;
    let mut previous_workspace = name.clone();
    let mut previous_handle: Option<Arc<WorkspaceHandle>> = None;
    let pending_switch: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let mut report;

    loop {
        let backend: DynOps = Box::new(current_handle.backend());
        let capacity: fs_winfsp::callbacks::CapacitySource = {
            let total = volume_total;
            let ws = Arc::clone(&current_handle);
            Arc::new(move || {
                let used = ws.logical_bytes_cached();
                Some((total, total.saturating_sub(used)))
            })
        };

        report = mount_serving_with_timeout(
            &library,
            mount_point,
            backend,
            volume(),
            Some(capacity),
            DEFAULT_INFO_TIMEOUT_MS,
            |mounted: &fs_winfsp::mounted::Mounted| {
                // Replace the active adapter on EVERY mount/remount (fixes hot-switch OnceLock bug).
                *adapter_slot
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mounted.adapter());

                if !signaled_ready.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    if let Some(ready_path) = ready_file {
                        let _ = write_ready_status(
                            ready_path,
                            &format!(
                                "OK\t{}\t{profile_str}\t{name}\t{cache_mib}\n",
                                std::process::id()
                            ),
                        );
                    }
                }

                if let Some(done) = pending_switch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    publish_switch(Ok(done));
                }

                let started = Instant::now();
                loop {
                    if stop_requested() {
                        let open = adapter_slot
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                            .map_or(0, |adapter| adapter.open_handles());
                        if open > 0 {
                            println!("{open} open handle(s) will be invalidated");
                        }
                        println!("unmount requested after {:?}", started.elapsed());
                        break;
                    }
                    std::thread::sleep(POLL);
                }
                Ok(())
            },
        );

        // Clear adapter slot while unmounted between iterations.
        *adapter_slot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        // If a switch was pending and mount_serving failed before publishing it, roll back.
        if let Some(pending) = pending_switch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let why = match &report {
                Ok(report) => format!(
                    "the volume for {} never looked mounted (create={:#010X} mount={:#010X})",
                    pending.0, report.create, report.mount
                ),
                Err(error) => format!("the mount for {} failed: {error:?}", pending.0),
            };
            if let Some(prev) = previous_handle.take() {
                let text = switched_record(
                    &canonical(store),
                    &canonical(mount_point),
                    &previous_workspace,
                    &previous_workspace,
                    &pipe,
                );
                let _ = fs_cli::rewrite_record(&mut record_handle, &text);
                {
                    let mut current = pipe_serving
                        .current
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *current = Current {
                        workspace: previous_workspace.clone(),
                        branch: previous_workspace.clone(),
                        handle: Arc::clone(&prev),
                    };
                }
                let failed_ws = std::mem::replace(&mut current_handle, prev);
                if let Ok(ws) = Arc::try_unwrap(failed_ws) {
                    let _ = ws.close();
                }
                STOP_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
                publish_switch(Err(("remount-failed".to_owned(), why)));
                continue;
            }
            publish_switch(Err(("remount-failed".to_owned(), why)));
            break;
        }

        // Previous workspace is no longer needed once the current mount finished cleanly.
        if let Some(old_ws) = previous_handle.take() {
            if let Ok(ws) = Arc::try_unwrap(old_ws) {
                let _ = ws.close();
            }
        }

        let Some(next) = take_switch_target() else {
            break;
        };

        match repo.open_workspace(&next) {
            Ok(next_ws) => {
                let next_handle = Arc::new(next_ws);
                let text = switched_record(
                    &canonical(store),
                    &canonical(mount_point),
                    &next,
                    &next,
                    &pipe,
                );
                if let Err(error) = fs_cli::rewrite_record(&mut record_handle, &text) {
                    if let Ok(ws) = Arc::try_unwrap(next_handle) {
                        let _ = ws.close();
                    }
                    publish_switch(Err((
                        "switch".to_owned(),
                        format!("the record could not be rewritten: {error}"),
                    )));
                    break;
                }
                {
                    let mut current = pipe_serving
                        .current
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    previous_workspace = current.workspace.clone();
                    previous_handle = Some(Arc::clone(&current.handle));
                    *current = Current {
                        workspace: next.clone(),
                        branch: next.clone(),
                        handle: Arc::clone(&next_handle),
                    };
                }
                current_handle = next_handle;
                STOP_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
                *pending_switch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((next.clone(), next));
            }
            Err(error) => {
                let kind = match &error {
                    LayerError::NotFound(_) => "not-found",
                    LayerError::Busy(_) => "in-use",
                    LayerError::Conflict(_) => "conflict",
                    _ => "remount-failed",
                };
                publish_switch(Err((kind.to_owned(), error.to_string())));
                STOP_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    // Release mount record (`DELETE_ON_CLOSE`) and close active workspace cleanly.
    drop(record_handle);
    let _ = std::fs::remove_file(&state);
    server.teardown_finished();

    // Release our strong refs to the workspace before `repo` drops its overlay owner.
    if let Some(old_ws) = previous_handle.take() {
        if let Ok(ws) = Arc::try_unwrap(old_ws) {
            let _ = ws.close();
        }
    }
    drop(pipe_serving);
    if let Ok(ws) = Arc::try_unwrap(current_handle) {
        let _ = ws.close();
    }

    match report {
        Ok(report) if report.looked_like_a_mount => {
            println!(
                "unmounted (create={:#010X} mount={:#010X} start={:#010X})",
                report.create, report.mount, report.start
            );
            Ok(())
        }
        Ok(report) if report.mount == OURS_OBJECT_NAME_COLLISION => Err(format!(
            "{} is already in use ({:#010X}); try `layerfs umount {} --force`, or pick another mount point",
            mount_point.display(),
            report.mount,
            mount_point.display()
        )
        .into()),
        Ok(report) if report.mount == OURS_ACCESS_DENIED => Err(format!(
            "cannot mount at {} ({:#010X}): mount point is not free — use an unused drive letter or a non-existent directory path",
            mount_point.display(),
            report.mount
        )
        .into()),
        Ok(report) => Err(format!(
            "the volume at {} never looked mounted (create={:#010X} mount={:#010X} start={:#010X} dispatcher={:#010X})",
            mount_point.display(),
            report.create,
            report.mount,
            report.start,
            report.dispatcher_result
        )
        .into()),
        Err(error) => Err(format!("mount failed: {error:?}").into()),
    }
}

/// `layerfs umount [<mount_point>] [--force]`
pub fn umount(
    arguments: &[String],
    discovered_mount: Option<&Path>,
    store_hint: Option<&Path>,
) -> Result<(), Error> {
    let mut force = false;
    let mut positional: Option<&str> = None;
    for arg in arguments {
        match arg.as_str() {
            "--force" => force = true,
            other if other.starts_with('-') => {
                return Err(
                    fs_cli::Failure::usage(format!("unknown umount option {other:?}")).into(),
                );
            }
            other => {
                if positional.is_some() {
                    return Err(
                        fs_cli::Failure::usage("usage: layerfs umount [<mount_point>] [--force]")
                            .into(),
                    );
                }
                positional = Some(other);
            }
        }
    }

    let mount_point = if let Some(explicit) = positional {
        PathBuf::from(explicit)
    } else if let Some(mounted) = discovered_mount {
        mounted.to_path_buf()
    } else if let Some(store) = store_hint {
        let mut live: Vec<PathBuf> = live_records()
            .into_iter()
            .filter(|(rec, liveness)| {
                *liveness == fs_cli::discovery::Liveness::Live
                    && fs_cli::discovery::same_path(&rec.store, store)
            })
            .map(|(rec, _)| rec.mount_point)
            .collect();
        match live.len() {
            0 => {
                return Err(fs_cli::Failure::not_found(format!(
                    "no live mount recorded for store {}",
                    store.display()
                ))
                .into());
            }
            1 => live.remove(0),
            _ => {
                let list = live
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(fs_cli::Failure::usage(format!(
                    "multiple live mounts exist for {} ({list}); specify `<mount_point>`",
                    store.display()
                ))
                .into());
            }
        }
    } else {
        return Err(fs_cli::Failure::usage(
            "usage: layerfs umount [<mount_point>] [--force] (omit `<mount_point>` inside a store directory with one live mount)",
        )
        .into());
    };

    let state = state_path(&mount_point);
    if !state.exists() {
        return Err(
            fs_cli::Failure::not_found(format!("no mount recorded for {}", mount_point.display()))
                .into(),
        );
    }

    if force && record_is_live(&state) == Liveness::Stale {
        let _ = std::fs::remove_file(&state);
        println!("removed stale mount record for {}", mount_point.display());
        return Ok(());
    }

    let record_text = std::fs::read_to_string(&state).unwrap_or_default();
    let pipe = record_field(&record_text, "pipe");
    if pipe.is_empty() {
        if force {
            let _ = std::fs::remove_file(&state);
            println!("removed stale mount record for {}", mount_point.display());
            return Ok(());
        }
        return Err(fs_cli::Failure::in_use(format!(
            "{} has no control pipe recorded; end the mounting process",
            mount_point.display()
        ))
        .into());
    }

    let request = fs_cli::pipe::Request {
        v: fs_cli::pipe::PROTOCOL,
        op: "unmount".to_owned(),
        args: serde_json::json!({
            "wait_ms": UNMOUNT_WAIT.as_millis() as u64,
            "force": force,
        }),
    };
    let answer = fs_cli::pipe_win::call(&pipe, &request, fs_cli::pipe_win::CONNECT_WAIT_MS);
    match answer {
        Ok(fs_cli::pipe::Response::Unmounted {}) => {}
        Ok(fs_cli::pipe::Response::HandlesOpen { handles }) => {
            return Err(fs_cli::Failure::in_use(format!(
                "{} still has {handles} open file handle(s); close them or pass `--force`",
                mount_point.display()
            ))
            .into())
        }
        Ok(other) => {
            return Err(format!("{} refused to unmount: {other:?}", mount_point.display()).into())
        }
        Err(error) => {
            if force {
                let _ = std::fs::remove_file(&state);
                return Err(fs_cli::Failure::in_use(format!(
                    "{} is not answering ({error}); removed stale record",
                    mount_point.display()
                ))
                .into());
            }
            return Err(fs_cli::Failure::in_use(format!(
                "{} is not answering ({error}); the mounting process may be gone (use --force to clean record)",
                mount_point.display()
            ))
            .into());
        }
    }

    let started = Instant::now();
    while started.elapsed() < UNMOUNT_WAIT {
        if !state.exists() {
            println!("{} unmounted", mount_point.display());
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
    if force {
        let _ = std::fs::remove_file(&state);
        return Err(format!(
            "{} did not release within {UNMOUNT_WAIT:?}; removed record",
            mount_point.display()
        )
        .into());
    }
    Err(format!(
        "{} did not unmount within {UNMOUNT_WAIT:?}",
        mount_point.display()
    )
    .into())
}

/// `layerfs list` — every recorded mount with live/stale status.
pub fn list() -> Result<(), Error> {
    let directory = mounts_dir();
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no mounts recorded ({})", directory.display());
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let mut records: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mount"))
        .collect();
    records.sort();
    if records.is_empty() {
        println!("no mounts recorded ({})", directory.display());
        return Ok(());
    }
    for record in records {
        let liveness = match record_is_live(&record) {
            Liveness::Live => "live",
            Liveness::Stale => "stale",
            Liveness::Gone => "gone",
        };
        let text = std::fs::read_to_string(&record).unwrap_or_default();
        println!(
            "{}  [{liveness}]  pid {}  branch {}  store {}",
            record_field(&text, "mount_point"),
            record_field(&text, "pid"),
            record_field(&text, "branch"),
            record_field(&text, "store")
        );
    }
    Ok(())
}

// ------------------------------------------------------------------ console & liveness

static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_stop() {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[must_use]
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(windows)]
extern "system" fn console_control_handler(event: u32) -> i32 {
    match event {
        0 | 1 | 2 | 5 | 6 => {
            request_stop();
            1
        }
        _ => 0,
    }
}

#[cfg(windows)]
fn clear_std_handle_inheritance() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn SetHandleInformation(h_object: isize, dw_mask: u32, dw_flags: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: isize = -1;

    for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` returns a standard process handle, 0, or `INVALID_HANDLE_VALUE`.
        // Clearing `HANDLE_FLAG_INHERIT` keeps the handles usable in this process while preventing
        // detached child processes from keeping parent stdout/stderr pipes open.
        unsafe {
            let handle = GetStdHandle(std_id);
            if handle != 0 && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(windows)]
fn install_console_handler() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    // SAFETY: static extern "system" handler pointer outlives the process.
    unsafe {
        SetConsoleCtrlHandler(Some(console_control_handler), 1);
    }
}

#[cfg(not(windows))]
fn install_console_handler() {}

fn canonical(path: &Path) -> String {
    let target = {
        let text = path.to_string_lossy();
        if text.len() == 2 && text.ends_with(':') {
            PathBuf::from(format!("{text}\\"))
        } else {
            path.to_path_buf()
        }
    };
    let resolved = std::fs::canonicalize(&target).unwrap_or(target);
    let text = resolved.display().to_string();
    text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string()
}

fn record_field(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|line| {
            line.strip_prefix(key)?
                .strip_prefix('=')
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Stale,
    Gone,
}

#[must_use]
pub fn record_is_live(path: &Path) -> Liveness {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
        match std::fs::OpenOptions::new()
            .write(true)
            .share_mode(SHARE_ALL)
            .open(path)
        {
            Ok(_) => Liveness::Stale,
            Err(error) if error.raw_os_error() == Some(32) => Liveness::Live,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Liveness::Gone,
            Err(_) => Liveness::Stale,
        }
    }
    #[cfg(not(windows))]
    {
        if path.exists() {
            Liveness::Stale
        } else {
            Liveness::Gone
        }
    }
}

#[cfg(windows)]
fn hold_record(path: &Path, contents: &str) -> Result<std::fs::File, Error> {
    use std::io::Write as _;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    Ok(file)
}

#[cfg(not(windows))]
fn hold_record(path: &Path, contents: &str) -> Result<std::fs::File, Error> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    Ok(file)
}

// ------------------------------------------------------------------ control pipe serving

type AdapterSlot = Arc<RwLock<Option<Arc<fs_winfsp::DynAdapter>>>>;

struct Current {
    workspace: String,
    branch: String,
    handle: Arc<WorkspaceHandle>,
}

struct PipeServing {
    store: String,
    current: RwLock<Current>,
    mount_point: String,
    record: PathBuf,
    adapter: AdapterSlot,
}

impl PipeServing {
    fn current(&self) -> std::sync::RwLockReadGuard<'_, Current> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn active_adapter(&self) -> Option<Arc<fs_winfsp::DynAdapter>> {
        self.adapter
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record_fields(&self) -> fs_cli::pipe::Response {
        let current = self.current();
        fs_cli::pipe::Response::Hello {
            store: self.store.clone(),
            workspace: current.workspace.clone(),
            branch: current.branch.clone(),
            pid: std::process::id(),
            mount_point: self.mount_point.clone(),
            protocol: fs_cli::pipe::PROTOCOL,
        }
    }
}

impl fs_cli::pipe_win::Serving for PipeServing {
    fn answer(&self, frame: &[u8]) -> fs_cli::pipe::Response {
        struct Ctx<'a>(&'a PipeServing);
        impl fs_cli::pipe::Context for Ctx<'_> {
            fn hello(&self) -> fs_cli::pipe::Response {
                self.0.record_fields()
            }

            fn status(&self) -> fs_cli::pipe::Response {
                let handle = Arc::clone(&self.0.current().handle);
                let handles = self
                    .0
                    .active_adapter()
                    .map_or(0, |adapter| adapter.open_handle_kinds().0);
                match handle.status() {
                    Ok(status) => fs_cli::pipe::Response::Status {
                        workspace: status.branch.clone(),
                        branch: status.branch,
                        base_commit: status.head_commit.unwrap_or(status.base_layer),
                        base_root: status.effective_root,
                        dirty: status.dirty,
                        handles,
                    },
                    Err(error) => fs_cli::pipe::Response::Failed {
                        kind: "status".to_owned(),
                        detail: error.to_string(),
                        fields: serde_json::json!({}),
                    },
                }
            }

            fn commit(&self, message: &str, force: bool) -> fs_cli::pipe::Response {
                let handle = Arc::clone(&self.0.current().handle);
                let msg = if message.is_empty() {
                    None
                } else {
                    Some(message)
                };
                match handle.commit(msg, force) {
                    Ok(outcome) => fs_cli::pipe::Response::Failed {
                        kind: "committed".to_owned(),
                        detail: outcome.commit.clone().unwrap_or_default(),
                        fields: serde_json::json!({
                            "commit": outcome.commit,
                            "root": outcome.root,
                        }),
                    },
                    Err(error) => {
                        let kind = match &error {
                            LayerError::Conflict(_) => "workspace-dirty",
                            LayerError::Busy(_) => "in-use",
                            LayerError::NotFound(_) => "not-found",
                            _ => "commit",
                        };
                        fs_cli::pipe::Response::Failed {
                            kind: kind.to_owned(),
                            detail: error.to_string(),
                            fields: serde_json::json!({}),
                        }
                    }
                }
            }

            fn snapshot(&self) -> fs_cli::pipe::Response {
                fs_cli::pipe::Response::Failed {
                    kind: "unsupported".to_owned(),
                    detail: "use `layerfs commit` and `layerfs publish` on upstream LayerFS"
                        .to_owned(),
                    fields: serde_json::json!({}),
                }
            }

            fn switch(&self, workspace: &str) -> fs_cli::pipe::Response {
                {
                    let current = self.0.current();
                    if current.workspace == workspace {
                        return fs_cli::pipe::Response::Switched {
                            workspace: current.workspace.clone(),
                            branch: current.branch.clone(),
                            dirs_invalidated: 0,
                        };
                    }
                }
                const WAIT: Duration = Duration::from_secs(10);
                let Some(adapter) = self.0.active_adapter() else {
                    return fs_cli::pipe::Response::Failed {
                        kind: "switch".to_owned(),
                        detail: "the mount is not serving yet".to_owned(),
                        fields: serde_json::json!({}),
                    };
                };
                let deadline = Instant::now() + WAIT;
                let quiesce = loop {
                    match adapter.quiesce(fs_winfsp::admission::QuiesceKind::Switch, WAIT) {
                        fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                            if Instant::now() >= deadline {
                                break fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs };
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        other => break other,
                    }
                };
                let dirs = match quiesce {
                    fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                        return fs_cli::pipe::Response::RestoreRefused { files, dirs };
                    }
                    fs_winfsp::admission::Quiesce::TimedOut { in_flight } => {
                        return fs_cli::pipe::Response::Failed {
                            kind: "switch".to_owned(),
                            detail: format!(
                                "{in_flight} open(s) did not finish within {}s",
                                WAIT.as_secs()
                            ),
                            fields: serde_json::json!({"in_flight": in_flight}),
                        }
                    }
                    fs_winfsp::admission::Quiesce::Quiesced(guard) => {
                        let dirs = adapter.open_handle_kinds().1;
                        drop(guard);
                        dirs
                    }
                };
                {
                    let outcome = switch_outcome();
                    *outcome
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                set_switch_target(workspace);
                request_stop();
                match await_switch(Duration::from_secs(120)) {
                    Some(Ok((workspace, branch))) => fs_cli::pipe::Response::Switched {
                        workspace,
                        branch,
                        dirs_invalidated: dirs,
                    },
                    Some(Err((kind, detail))) => fs_cli::pipe::Response::Failed {
                        kind,
                        detail,
                        fields: serde_json::json!({}),
                    },
                    None => fs_cli::pipe::Response::Failed {
                        kind: "switch".to_owned(),
                        detail: "the mount did not finish switching".to_owned(),
                        fields: serde_json::json!({}),
                    },
                }
            }

            fn restore(&self, _commit: &str, _discard: bool) -> fs_cli::pipe::Response {
                fs_cli::pipe::Response::Failed {
                    kind: "unsupported".to_owned(),
                    detail: "fork a branch from a commit or layer instead (`layerfs fork`)"
                        .to_owned(),
                    fields: serde_json::json!({}),
                }
            }

            fn shutting_down(&self) -> bool {
                stop_requested()
            }
        }
        fs_cli::pipe::dispatch(frame, &Ctx(self))
    }

    fn request_stop(&self) -> bool {
        request_stop();
        true
    }

    fn shutting_down(&self) -> bool {
        stop_requested()
    }

    fn open_handles(&self) -> usize {
        // Count open regular **file** handles only (matching D8 = B / `Gatekeeper::quiesce`):
        // directory handles (shell CWD, Explorer, mount-point root) do not hold uncommitted data
        // and must never block orderly unmount.
        self.active_adapter()
            .map_or(0, |adapter| adapter.open_handle_kinds().0)
    }

    fn teardown_complete(&self) -> bool {
        !self.record.exists()
    }
}

// ------------------------------------------------------------------ discovery helpers

#[must_use]
pub fn live_records() -> Vec<(fs_cli::discovery::Record, fs_cli::discovery::Liveness)> {
    let Ok(entries) = std::fs::read_dir(mounts_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "mount") {
            continue;
        }
        let liveness = match record_is_live(&path) {
            Liveness::Live => fs_cli::discovery::Liveness::Live,
            Liveness::Stale | Liveness::Gone => fs_cli::discovery::Liveness::Stale,
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        out.push((
            fs_cli::discovery::Record {
                store: PathBuf::from(record_field(&text, "store")),
                workspace: record_field(&text, "workspace"),
                mount_point: PathBuf::from(record_field(&text, "mount_point")),
                pid: record_field(&text, "pid").parse().unwrap_or(0),
                branch: record_field(&text, "branch"),
                pipe: record_field(&text, "pipe"),
            },
            liveness,
        ));
    }
    out
}

#[must_use]
pub fn live_mount_of(branch: &str) -> Option<(String, u32)> {
    let entries = std::fs::read_dir(mounts_dir()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "mount") {
            continue;
        }
        if record_is_live(&path) != Liveness::Live {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if record_field(&text, "branch") == branch || record_field(&text, "workspace") == branch {
            return Some((
                record_field(&text, "mount_point"),
                record_field(&text, "pid").parse().unwrap_or(0),
            ));
        }
    }
    None
}

fn switched_record(
    store: &str,
    mount_point: &str,
    workspace: &str,
    branch: &str,
    pipe: &str,
) -> String {
    format!(
        "version=1\npid={}\nmount_point={mount_point}\nstore={store}\nworkspace={workspace}\nbranch={branch}\npipe={pipe}\nstarted_at={}\n",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0)
    )
}

// ------------------------------------------------------------------ switch rendezvous

static SWITCH_TARGET: Mutex<Option<String>> = Mutex::new(None);
type SwitchAnswer = Result<(String, String), (String, String)>;

struct SwitchOutcome {
    result: Mutex<Option<SwitchAnswer>>,
    changed: Condvar,
}

static SWITCH_OUTCOME: OnceLock<SwitchOutcome> = OnceLock::new();

fn switch_outcome() -> &'static SwitchOutcome {
    SWITCH_OUTCOME.get_or_init(|| SwitchOutcome {
        result: Mutex::new(None),
        changed: Condvar::new(),
    })
}

fn set_switch_target(workspace: &str) -> Option<String> {
    SWITCH_TARGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(workspace.to_owned())
}

fn take_switch_target() -> Option<String> {
    SWITCH_TARGET
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn publish_switch(result: SwitchAnswer) {
    let outcome = switch_outcome();
    *outcome
        .result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    outcome.changed.notify_all();
}

fn await_switch(wait: Duration) -> Option<SwitchAnswer> {
    let outcome = switch_outcome();
    let deadline = Instant::now() + wait;
    let mut slot = outcome
        .result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while slot.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let (next, _) = outcome
            .changed
            .wait_timeout(slot, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot = next;
    }
    slot.take()
}

#[cfg(test)]
mod tests {
    use super::{
        calibrated_major, hold_record, record_is_live, record_name, request_stop, stop_requested,
        version_parts, Liveness,
    };
    use std::path::Path;

    #[test]
    fn a_control_event_asks_the_mount_to_stop() {
        assert!(!stop_requested());
        request_stop();
        assert!(stop_requested());
    }

    #[test]
    fn a_record_is_live_while_held_and_gone_once_released() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|e| e.as_nanos())
            .unwrap_or(0);
        let directory = std::env::temp_dir().join(format!(
            "layerfs-record-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("temp");

        let held = directory.join("held.mount");
        let handle = hold_record(&held, "version=1\npid=1\n").expect("hold");
        assert_eq!(record_is_live(&held), Liveness::Live);
        drop(handle);
        assert_eq!(record_is_live(&held), Liveness::Gone);

        let stale = directory.join("stale.mount");
        std::fs::write(&stale, b"version=1\n").expect("write");
        assert_eq!(record_is_live(&stale), Liveness::Stale);
        assert_eq!(record_is_live(&directory.join("missing.mount")), Liveness::Gone);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn mount_point_and_version_helpers() {
        assert_eq!(record_name(Path::new("V:")), "V");
        assert_eq!(record_name(Path::new("C:\\mnt\\x")), "C_mnt_x");
        assert_eq!(version_parts(0x0002_0001), (2, 1));
        assert_eq!(calibrated_major(), 2);
    }
}
