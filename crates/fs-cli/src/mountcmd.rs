//! `layerfs mount | umount | list | version` — the interactive entry points (B-24), and the version gate (B-23).
//!
//! # Why the mount keeps running by waiting on a **file**
//!
//! `mount_read_only` mounts, calls a probe, then unmounts — the probe is the only moment the file system is alive. So the probe *is* the command:
//! it blocks until someone asks for the unmount. The request travels as a **stop file** in a runtime directory rather than through IPC, because the
//! alternative (a named pipe or a socket) adds a protocol that has to be tested, while a file is something the OS already knows how to make sure of:
//! if it exists, the request was recorded.
//!
//! What this deliberately does **not** do: unmount another process's mount on demand. `umount --force` can only remove a stale record and say so,
//! because the WinFsp handle lives in the mounting process — reporting "unmounted" when the volume is still attached would be the exact kind of
//! comfortable lie this project keeps refusing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs_api::winfsp_backend::ApiBackend;
use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::loader::default_library;
use fs_winfsp::mounted::mount_serving;
use fs_winfsp::ops::DynOps;
use fs_workspace::Repo;

/// One error type for the subcommands: everything they can fail with is a message a user can act on.
pub type Error = Box<dyn std::error::Error>;

/// How long `umount` waits for the mounting process to notice the request.
const UNMOUNT_WAIT: Duration = Duration::from_secs(10);
/// How often the mount looks for a stop request.
const POLL: Duration = Duration::from_millis(50);
/// `STATUS_OBJECT_NAME_COLLISION` — the drive letter is taken (`ntstatus.h`; the same value `callbacks` uses for a taken *name*).
const OURS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
/// `STATUS_ACCESS_DENIED` — what WinFsp answers when the mount point exists as a real volume (`ntstatus.h:4392`).
const OURS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;

/// `V:` becomes `V`; `C:mntx` becomes `C_mnt_x` (ADR-010 §9.1). The implementation lives in the library so the criteria compute the same name.
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
    // "2.1.25156.ddca7bd" -> 2
    fs_winfsp::abi::measured::DLL_BUILD
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(0)
}

fn volume() -> VolumeInfo {
    VolumeInfo {
        total_size: 1 << 30,
        free_size: 1 << 29,
        // **In bytes, not characters** — `VolumeLabelLength` counts the UTF-16 encoding. With 7 here the label came back as `lay`: WinFsp read 7 bytes, which is
        // three and a half characters (measured with `vol J:`). Path discovery then refused to recognise the volume, because its label was not `layerfs`.
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

/// `layerfs version` — the DLL's version, the version the slot table was calibrated against, and whether they agree.
pub fn version() -> Result<(), Error> {
    let library = default_library();
    let (major, minor) = match fs_winfsp::handshake::read_version(&library) {
        Ok(raw) => version_parts(raw),
        Err(error) => {
            // A missing DLL is not a failure of this command: it is the answer to "what is installed".
            println!(
                "layerfs: winfsp not loadable at {} ({error:?})",
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
        println!("abi gate          : MISMATCH — the slot table was measured on {calibrated}.x, this DLL is {major}.x");
    }
    Ok(())
}

/// `layerfs mount <workspace> <mount_point> [--workspace <name>] [--no-version-gate]`
pub fn mount(store: &Path, workspace: Option<&str>, arguments: &[String]) -> Result<(), Error> {
    // **Positionals are what is left after the options are removed**, not "the first two arguments": with `mount U: --workspace main`, counting by
    // position made `--workspace` the mount point and the store `U:` (measured: the child answered `store directory U: does not exist`).
    let mut positional: Vec<&String> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--workspace" => index += 2,
            "--no-version-gate" => index += 1,
            other if other.starts_with('-') => index += 1,
            other => {
                positional.push(&arguments[index]);
                let _ = other;
                index += 1;
            }
        }
    }
    // **One positional form**: `layerfs --store <dir> mount <mount_point>`, the same shape as every other verb (the store is a global option).
    // The older `mount <store> <mount_point>` still works, with a warning: removing it silently would break the one command whose arguments a user
    // is most likely to have scripted.
    let (store, mount_point) = match (positional.first(), positional.get(1)) {
        (Some(mount_point), None) => (store.to_path_buf(), PathBuf::from(mount_point.as_str())),
        (Some(store_arg), Some(mount_point)) => {
            eprintln!(
                "layerfs: warning: `mount <store> <mount_point>` is the old form; use `--store <store> mount <mount_point>`"
            );
            (
                PathBuf::from(store_arg.as_str()),
                PathBuf::from(mount_point.as_str()),
            )
        }
        _ => return Err("usage: layerfs --store <store> mount <mount_point>".into()),
    };
    // **The workspace is named, never guessed** (ADR-010 §5 / A2): the old default of `"main"` meant a mount could serve a workspace the caller never
    // mentioned, and that is exactly the kind of implicit choice this ADR removes.
    let Some(name) = workspace else {
        // A **usage** error, not a generic failure: the command line is wrong, and §8.3 gives that exit code 2 so a script can tell it from a busy
        // drive letter (4) or a missing store (3).
        return Err(fs_cli::Failure::usage(format!(
            "mount needs a workspace: add `--workspace <name>` (for example `layerfs --store {} mount {} --workspace main`)",
            store.display(),
            positional.first().map_or_else(|| "<mount_point>".to_string(), |point| point.to_string())
        ))
        .into());
    };
    let name = name.to_string();
    let skip_gate = arguments
        .iter()
        .any(|argument| argument == "--no-version-gate");

    if !skip_gate {
        let library = default_library();
        match fs_winfsp::handshake::read_version(&library) {
            Ok(raw) => {
                let (major, minor) = version_parts(raw);
                if major != calibrated_major() {
                    return Err(format!(
                        "refusing to mount: winfsp {major}.{minor} was not the layout this build was measured against ({})",
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

    let repo = std::sync::Arc::new(Repo::open(&store, false)?);
    let workspace = repo.workspace_open(&name)?;
    // `DynOps` carries no lifetime, and this process exists to serve one mount: leaking the workspace is the honest way to say "this outlives every
    // borrow", rather than pretending with a `Box::leak` hidden behind an abstraction.
    let workspace: &'static fs_workspace::Workspace = Box::leak(Box::new(workspace));

    std::fs::create_dir_all(mounts_dir())?;
    let state = state_path(&mount_point);
    // **The pipe name is part of the record** (ADR-010 §9.1): a reader that has the record can reach the mount without asking anyone, and `umount` is exactly
    // that reader.
    let pipe = fs_cli::pipe_win::pipe_name(&record_name(&mount_point), std::process::id());
    // **A record that is still held means this letter is already served** (§9.2): say who holds it instead of failing with `os error 80` from
    // `create_new`. A **stale** record is the leftover of a dead owner (power cut) and is replaced — `DELETE_ON_CLOSE` already covers every ordinary death,
    // so this branch is rare by construction.
    match record_is_live(&state) {
        Liveness::Live => {
            let owner = std::fs::read_to_string(&state).unwrap_or_default();
            let field = |key: &str| {
                owner
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix(key)?
                            .strip_prefix('=')
                            .map(str::to_string)
                    })
                    .unwrap_or_default()
            };
            return Err(fs_cli::Failure::in_use(format!(
                "{} is already mounted (pid {}) — `layerfs umount {}` first",
                mount_point.display(),
                field("pid"),
                mount_point.display()
            ))
            .into());
        }
        Liveness::Stale => {
            let _ = std::fs::remove_file(&state);
        }
        Liveness::Gone => {}
    }
    // **The record's life is the process's life** (ADR-010 §9.2): created with `DELETE_ON_CLOSE` and held open, so however this process dies —
    // `taskkill /F` included — the kernel closes the handle and removes the file. A leftover record then means a power cut, which is the only case the
    // reader's staleness check has to handle.
    let record = format!(
        "version=1\npid={}\nmount_point={}\nstore={}\nworkspace={name}\nbranch={}\npipe={}\nstarted_at={}\n",
        std::process::id(),
        canonical(&mount_point),
        canonical(&store),
        branch_of(&repo, &name).unwrap_or_default(),
        pipe,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0)
    );
    // The handle is kept: a `switch` rewrites this file **in place** through it (S3 §5.2, and W1 polls liveness across the whole switch).
    let mut record_handle = hold_record(&state, &record)?;
    println!(
        "mounting {} at {} (workspace {name}); stop with `layerfs umount {}`",
        store.display(),
        mount_point.display(),
        mount_point.display()
    );

    // **The control channel starts before the volume does**: a `status` that arrives a millisecond after the drive appears must not race a pipe that is not
    // listening yet, and a serve loop that fails to start is worth knowing about before the mount commits.
    let adapter_slot: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<fs_winfsp::DynAdapter>>> =
        std::sync::Arc::new(std::sync::OnceLock::new());
    let pipe_serving: std::sync::Arc<PipeServing> = std::sync::Arc::new(PipeServing {
        store: canonical(&store),
        current: std::sync::RwLock::new(Current {
            workspace: name.clone(),
            branch: branch_of(&repo, &name).unwrap_or_default(),
            handle: workspace,
        }),
        mount_point: canonical(&mount_point),

        record: state.clone(),
        adapter: std::sync::Arc::clone(&adapter_slot),
    });
    let serving: std::sync::Arc<dyn fs_cli::pipe_win::Serving> =
        std::sync::Arc::clone(&pipe_serving) as _;
    let server = std::sync::Arc::new(fs_cli::pipe_win::Server::new(pipe.clone(), serving));
    if let Ok(started) = std::thread::Builder::new()
        .name("layerfs-control".to_owned())
        .spawn({
            let server = std::sync::Arc::clone(&server);
            move || {
                let _ = server.run();
            }
        })
    {
        // The thread is joined by process exit; keeping the handle would only be a place to forget it.
        std::mem::forget(started);
    }

    let library = default_library();
    // Installed **before** mounting, so a Ctrl-C during the mount itself is already an orderly stop.
    install_console_handler();
    // **The loop is here, not in `mount_serving`** (manager's ruling, S3 §5.2 revised): every iteration mounts through the same path S1 verified, and the
    // remount that a `switch` needs is simply the next iteration. The FFI setup stays single-shot, which is where it belongs.
    let mut current_handle = workspace;
    let mut report;
    loop {
        let backend: DynOps = Box::new(ApiBackend::new(current_handle));
        let mounted_report = mount_serving(
            &library,
            &mount_point,
            backend,
            volume(),
            |mounted: &fs_winfsp::mounted::Mounted| {
                // **`status.handles` reads the adapter at the moment of the ask**, so the control channel needs the adapter — not a number captured now.
                // (Until this line runs, the channel answers 0, which is also the truth: nobody has opened a file yet.)
                let _ = adapter_slot.set(mounted.adapter());
                // The probe **is** the command's lifetime: everything below happens while the volume is attached.
                let started = Instant::now();
                let mut last_reading = Instant::now();
                loop {
                    // **Three ways to be asked to stop**, all answering the same way: the stop file (until the pipe replaces it), the record disappearing, and a
                    // console control event. Ctrl-C lands here as an **orderly unmount** rather than a kill, which is the whole point of the handler.
                    // **Two ways to be asked to stop**, both answering the same way: the control pipe (through `request_stop`) and a console control event. The
                    // stop **file** is gone — A15s asserts that writing one no longer stops a mount, because a signal nobody can see the receipt of was the thing
                    // being replaced.
                    if stop_requested() {
                        // **A console stop does not wait for handles** (ADR-010 §7.2): the user in front of the window asked for it, so the mount unmounts and the
                        // handles die — but it says how many, because "my editor lost the file" is much easier to understand with a number attached.
                        let open = adapter_slot
                            .get()
                            .map_or(0, |adapter| adapter.open_handles());
                        if open > 0 {
                            println!("{open} open handle(s) will be invalidated");
                        }
                        println!("unmount requested after {:?}", started.elapsed());
                        break;
                    }
                    // **A timer reading, not an on-demand one** (`LAYERFS_DEBUG_HANDLES=1`): asking over the pipe adds handles by itself, so the only way to see
                    // whether the table decays **without** new commands is to print from the mount on its own clock.
                    if std::env::var_os("LAYERFS_DEBUG_HANDLES").is_some()
                        && started.elapsed().as_secs() % 5 == 0
                        && last_reading.elapsed().as_secs() >= 5
                    {
                        last_reading = Instant::now();
                        eprintln!(
                            "timer: elapsed={:?} table={:?} contexts={:?} cleanup={} stale={}",
                            started.elapsed(),
                            adapter_slot
                                .get()
                                .map_or((0, 0), |adapter| adapter.open_handle_kinds()),
                            fs_winfsp::callbacks::context_counts(),
                            fs_winfsp::callbacks::cleanup_call_count(),
                            fs_winfsp::callbacks::stale_release_count(),
                        );
                    }
                    std::thread::sleep(POLL);
                }
                Ok(())
            },
        );
        report = mounted_report;
        // **Nothing to switch? Then this was a stop.**
        let Some(next) = take_switch_target() else {
            break;
        };
        match repo.workspace_open(&next) {
            Ok(next_workspace) => {
                let next_handle: &'static fs_workspace::Workspace =
                    Box::leak(Box::new(next_workspace));
                let next_branch = branch_of(&repo, &next).unwrap_or_default();
                let text = switched_record(
                    &canonical(&store),
                    &canonical(&mount_point),
                    &next,
                    &next_branch,
                    &pipe,
                );
                if let Err(error) = fs_cli::rewrite_record(&mut record_handle, &text) {
                    // The record is how the world finds this mount; a switch that cannot describe itself must not look successful.
                    publish_switch(Err(format!("the record could not be rewritten: {error}")));
                    break;
                }
                {
                    let mut current = pipe_serving
                        .current
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *current = Current {
                        workspace: next.clone(),
                        branch: next_branch.clone(),
                        handle: next_handle,
                    };
                }
                current_handle = next_handle;
                // The stop flag was set by the switch request; clearing it is what lets the **new** serve loop run.
                STOP_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
                publish_switch(Ok((next, next_branch)));
            }
            Err(error) => {
                // **6b**: the new workspace could not be opened, so this iteration ends and the loop remounts the **old** one. The client is told the truth
                // *after* the rollback is on its way, not instead of it.
                publish_switch(Err(format!("RemountFailed: {error}")));
                STOP_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    // (the stop file is gone: the control pipe is the only stop signal — A15s)
    let _ = std::fs::remove_file(&state);
    // **The teardown is done; the pending `unmount` answer may leave now** (ADR-010 §5.1). This must happen before the process exits, or the client reads a
    // successful unmount as a dropped connection.
    server.teardown_finished();
    match report {
        Ok(report) if report.looked_like_a_mount => {
            println!(
                "unmounted (create={:#010X} mount={:#010X} start={:#010X})",
                report.create, report.mount, report.start
            );
            Ok(())
        }
        // **Say what to do next**, not only what went wrong. `0xC0000035` is `OBJECT_NAME_COLLISION`: the drive letter is already taken, usually by a
        // mount whose process died. The status alone leaves the user with "mounting failed" and no next step.
        Ok(report) if report.mount == OURS_OBJECT_NAME_COLLISION => Err(format!(
            "{} is already in use ({:#010X}); a previous mount may still hold it — try `layerfs umount {} --force`, or pick another letter",
            mount_point.display(),
            report.mount,
            mount_point.display()
        )
        .into()),
        // `0xC0000022` is `STATUS_ACCESS_DENIED`, which is what WinFsp answers for a mount point that **exists as a real volume** (`F:`, a partition):
        // mounting over an existing drive is not a permission problem a user can fix, it is the wrong slot. Same rule as above — the message has to
        // name the next step, not only the status.
        Ok(report) if report.mount == OURS_ACCESS_DENIED => Err(format!(
            "cannot mount at {} ({:#010X}): the slot is not free — use a drive letter that has no volume, or a directory that does not exist yet",
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

/// `layerfs umount <mount_point> [--force]`
pub fn umount(arguments: &[String]) -> Result<(), Error> {
    let Some(mount_point) = arguments.first() else {
        return Err("usage: layerfs umount <mount_point> [--force]".into());
    };
    let mount_point = PathBuf::from(mount_point);
    let force = arguments.iter().any(|argument| argument == "--force");
    let state = state_path(&mount_point);
    if !state.exists() {
        return Err(format!("no mount recorded for {}", mount_point.display()).into());
    }
    // **Ask the mount, do not write a file it might never read** (ADR-010 §10.2): the answer is the mount's own, which is also what lets `unmount` report
    // handles it refuses on. The stop file is gone — its absence is what A15s asserts.
    let record_text = std::fs::read_to_string(&state).unwrap_or_default();
    let pipe = record_text
        .lines()
        .find_map(|line| line.strip_prefix("pipe=").map(str::to_string))
        .unwrap_or_default();
    if pipe.is_empty() {
        return Err(fs_cli::Failure::in_use(format!(
            "{} has no control pipe recorded (a mount from an older build?); end the mounting process",
            mount_point.display()
        ))
        .into());
    }
    let request = fs_cli::pipe::Request {
        v: fs_cli::pipe::PROTOCOL,
        op: "unmount".to_owned(),
        args: serde_json::json!({ "wait_ms": UNMOUNT_WAIT.as_millis() as u64 }),
    };
    let answer = fs_cli::pipe_win::call(&pipe, &request, fs_cli::pipe_win::CONNECT_WAIT_MS);
    match answer {
        Ok(fs_cli::pipe::Response::Unmounted {}) => {}
        Ok(fs_cli::pipe::Response::HandlesOpen { handles }) => {
            return Err(fs_cli::Failure::in_use(format!(
                "{} still has {handles} open handle(s); close them and try again — the mount was left as it was",
                mount_point.display()
            ))
            .into())
        }
        Ok(other) => {
            return Err(format!("{} refused to unmount: {other:?}", mount_point.display()).into())
        }
        Err(error) => {
            return Err(fs_cli::Failure::in_use(format!(
                "{} is not answering ({error}); the mounting process may be wedged",
                mount_point.display()
            ))
            .into())
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
        // Honest about what this can and cannot do: the WinFsp handle lives in the other process, so a forced unmount here would be a claim, not an
        // action. What *is* removed is the record, so a crashed mount does not block the next one.
        let _ = std::fs::remove_file(&state);
        return Err(format!(
            "{} did not release within {UNMOUNT_WAIT:?}; removed the stale record, the volume may still be attached (end the mounting process)",
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

/// `layerfs list` — every recorded mount, with the pid that is serving it.
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
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "mount")
        })
        .collect();
    records.sort();
    if records.is_empty() {
        println!("no mounts recorded ({})", directory.display());
        return Ok(());
    }
    for record in records {
        let text = std::fs::read_to_string(&record).unwrap_or_default();
        let field = |key: &str| {
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(key)?
                        .strip_prefix('=')
                        .map(str::to_string)
                })
                .unwrap_or_default()
        };
        println!(
            "{}  pid {}  workspace {}  store {}",
            field("mount_point"),
            field("pid"),
            field("workspace"),
            field("store")
        );
    }
    Ok(())
}

/// Set when a console control event asks the mount to stop (Ctrl-C, Ctrl-Break, closing the console).
///
/// A console handler runs on a **separate thread** and must return quickly, so it does the smallest thing possible: set this flag. The mount loop notices
/// it and returns, which unmounts through the normal path — Ctrl-C is an **orderly unmount**, not a crash (ADR-010 §7.1).
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The handler's whole effect, separately callable so a test can prove it flips the flag without sending a real Ctrl-C.
pub fn request_stop() {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[must_use]
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(windows)]
extern "system" fn console_control_handler(event: u32) -> i32 {
    // `CTRL_C_EVENT(0)`, `CTRL_BREAK_EVENT(1)`, `CTRL_CLOSE_EVENT(2)` and the logoff/shutdown pair (5/6) all mean "stop", and all are answered
    // "handled": returning 0 would hand the event to the default handler, which terminates the process **before** the unmount runs.
    match event {
        0 | 1 | 2 | 5 | 6 => {
            request_stop();
            1
        }
        _ => 0,
    }
}

#[cfg(windows)]
fn install_console_handler() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    // SAFETY: a plain Win32 call with a function pointer that outlives the process.
    unsafe {
        SetConsoleCtrlHandler(Some(console_control_handler), 1);
    }
}

#[cfg(not(windows))]
fn install_console_handler() {}

fn canonical(path: &Path) -> String {
    // **`V:` is not `V:\`** (the 317 trap, in the record layer this time): `canonicalize("V:")` asks for "the current directory on V", which a drive mounted by
    // an FSD does not have, so it fails and the fallback stores `V:` — a path that `V:\...` does not start with. Discovery then never matches. Putting the
    // separator back first makes the record say the same thing the shell says.
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

fn branch_of(repo: &Repo, name: &str) -> Option<String> {
    repo.workspace_list()
        .ok()?
        .into_iter()
        .find(|workspace| workspace.name == name)
        .map(|workspace| workspace.branch)
}

/// Whether a record's owner is still alive (**ADR-010 §9.2**).
#[derive(Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The owner still holds the file: opening it for writing hits a sharing violation.
    Live,
    /// Nobody holds it any more: the process is gone and the record is a leftover.
    Stale,
    /// There is no record at all.
    Gone,
}

/// The one implementation of "is this mount still alive".
///
/// The owner holds the record with `share_mode(READ | DELETE)` and **no write share**, so a reader that asks for write access gets
/// `ERROR_SHARING_VIOLATION (32)` while the owner lives — the whole test, with no pid probing and no timestamp guessing.
#[must_use]
pub fn record_is_live(path: &Path) -> Liveness {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // READ | WRITE | DELETE
        match std::fs::OpenOptions::new()
            .write(true)
            .share_mode(SHARE_ALL)
            .open(path)
        {
            Ok(_) => Liveness::Stale,
            Err(error) if error.raw_os_error() == Some(32) => Liveness::Live,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Liveness::Gone,
            // Anything else (a permission problem, a locking tool) is **not** evidence of life: calling it stale is the conservative reading, because the
            // alternative is refusing to work on a filesystem that is actually free.
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

/// Creates the record and **keeps it open** for the process's lifetime.
///
/// `DELETE_ON_CLOSE` is why a crashed mount leaves nothing behind: the kernel closes every handle when the process dies, `taskkill /F` included.
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

/// What the control pipe answers for one mount: the three S1 operations, computed from the mount itself.
///
/// It holds the **leaked** workspace the backend already serves, so `status` reports the same view the filesystem is serving — not a second connection that
/// could disagree with it.
struct PipeServing {
    store: String,
    /// The workspace being served **right now**, behind a lock because `switch` replaces it while the pipe keeps answering (S3 §5.2).
    ///
    /// A single `&'static Workspace` field would have made the switch impossible to implement without tearing the whole serving object down and rebuilding it,
    /// and rebuilding it would have meant a window in which the pipe answers nothing — which is exactly what W3 measures against.
    current: std::sync::RwLock<Current>,
    mount_point: String,
    /// Where this mount's record lives: `teardown_complete` is "the record is gone".
    record: std::path::PathBuf,
    /// The mount's adapter, filled in by the mount closure: `status.handles` must be read at the moment of the ask.
    adapter: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<fs_winfsp::DynAdapter>>>,
}

/// What a `switch` changes, as one value: the name, the branch it publishes to, and the handle.
///
/// One record rather than three fields, for the reason S2 settled (566): a report must come from **one** snapshot, or `status` can print the old name with the new
/// branch and be believed.
struct Current {
    workspace: String,
    branch: String,
    handle: &'static fs_workspace::Workspace,
}

impl PipeServing {
    /// The workspace being served right now, for the duration of one answer.
    fn current(&self) -> std::sync::RwLockReadGuard<'_, Current> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        // A tiny adapter: the protocol module wants a `Context`, and everything it needs is right here.
        struct Ctx<'a>(&'a PipeServing);
        impl fs_cli::pipe::Context for Ctx<'_> {
            fn hello(&self) -> fs_cli::pipe::Response {
                self.0.record_fields()
            }

            fn status(&self) -> fs_cli::pipe::Response {
                // **Counters on every ask** (`LAYERFS_DEBUG_HANDLES=1`): the question is which side grows when a command runs from inside the mount, and only the
                // mount can see its own table.
                if std::env::var_os("LAYERFS_DEBUG_HANDLES").is_some() {
                    if let Some(adapter) = self.0.adapter.get() {
                        eprintln!(
                            "handles-after-status: table={:?} open_entries={} contexts={:?} cleanup={}",
                            adapter.open_handle_kinds(),
                            fs_winfsp::callbacks::open_entry_count(),
                            fs_winfsp::callbacks::context_counts(),
                            fs_winfsp::callbacks::cleanup_call_count(),
                        );
                    }
                }
                match self.0.current().handle.status() {
                    Ok(status) => fs_cli::pipe::Response::Status {
                        workspace: status.name,
                        branch: status.branch,
                        base_commit: status.base_commit,
                        base_root: status.base_root,
                        dirty: status.dirty,
                        handles: self
                            .0
                            .adapter
                            .get()
                            .map_or(0, |adapter| adapter.open_handles()),
                    },
                    Err(error) => fs_cli::pipe::Response::Failed {
                        kind: "status".to_owned(),
                        detail: error.to_string(),
                        fields: serde_json::json!({}),
                    },
                }
            }

            fn commit(&self, message: &str, force: bool) -> fs_cli::pipe::Response {
                // **The mount's own workspace publishes**, so the commit goes through the same gate the filesystem does — not through a second handle that would
                // bypass it (that is what G5 is about, in the CLI's own layer).
                match self.0.current().handle.commit(message, force) {
                    Ok(outcome) => fs_cli::pipe::Response::Failed {
                        // Success is reported through the same shape with `kind: "committed"`, because the protocol's success variants are per-operation and
                        // the caller distinguishes them by kind — one place to look, one place to branch.
                        kind: "committed".to_owned(),
                        detail: outcome.commit.clone().unwrap_or_default(),
                        fields: serde_json::json!({"commit": outcome.commit, "root": outcome.root}),
                    },
                    Err(error) => fs_cli::pipe::Response::Failed {
                        kind: "commit".to_owned(),
                        detail: error.to_string(),
                        fields: serde_json::json!({}),
                    },
                }
            }

            fn snapshot(&self) -> fs_cli::pipe::Response {
                match self.0.current().handle.snapshot() {
                    Ok(outcome) => fs_cli::pipe::Response::Failed {
                        kind: "snapshotted".to_owned(),
                        detail: outcome.commit.clone().unwrap_or_default(),
                        fields: serde_json::json!({"root": outcome.root}),
                    },
                    Err(error) => fs_cli::pipe::Response::Failed {
                        kind: "snapshot".to_owned(),
                        detail: error.to_string(),
                        fields: serde_json::json!({}),
                    },
                }
            }

            fn switch(&self, workspace: &str) -> fs_cli::pipe::Response {
                // Asking for what is already served is not an error: `checkout` of the current branch is a no-op everywhere else too.
                if self.0.current().workspace == workspace {
                    let current = self.0.current();
                    return fs_cli::pipe::Response::Switched {
                        workspace: current.workspace.clone(),
                        branch: current.branch.clone(),
                        dirs_invalidated: 0,
                    };
                }
                const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
                let Some(adapter) = self.0.adapter.get() else {
                    return fs_cli::pipe::Response::Failed {
                        kind: "switch".to_owned(),
                        detail: "the mount is not serving yet".to_owned(),
                        fields: serde_json::json!({}),
                    };
                };
                // **Closing, not Paused** (S3 §3.2): a switch unmounts, and an open that waited would be waiting for a dispatcher that is about to exit.
                // **The count gets the same patience an unmount gives it** (SM3 §3.2, and R2's reading of 48 ms before it): `CloseHandle` returns before the FSD delivers
                // `Close`, so a single check refuses a switch that is about to become legal.
                let deadline = std::time::Instant::now() + WAIT;
                let quiesce = loop {
                    match adapter.quiesce(fs_winfsp::admission::QuiesceKind::Switch, WAIT) {
                        fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                            if std::time::Instant::now() >= deadline {
                                break fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs };
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        other => break other,
                    }
                };
                let dirs = match quiesce {
                    fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                        // **Three counters at the moment of the refusal** (607, the Q2 method): "a file handle is open" can mean our table still holds one, or that the
                        // kernel has not delivered the `Close` yet. The numbers are printed from the **mount**, which is the only process that can see them.
                        if std::env::var_os("LAYERFS_DEBUG_SWITCH").is_some() {
                            eprintln!(
                                "switch-refused: table={:?} open_entries={} contexts={:?} cleanup={} stale_releases={}",
                                adapter.open_handle_kinds(),
                                fs_winfsp::callbacks::open_entry_count(),
                                fs_winfsp::callbacks::context_counts(),
                                fs_winfsp::callbacks::cleanup_call_count(),
                                fs_winfsp::callbacks::stale_release_count(),
                            );
                        }
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
                // **The answer is cleared before the ask**, so a stale one from an earlier attempt cannot be read as this attempt's outcome.
                {
                    let outcome = switch_outcome();
                    *outcome
                        .result
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                }
                set_switch_target(workspace);
                // The serve loop exits on this flag; the mount loop then remounts and clears it before the next iteration's serve loop starts.
                request_stop();
                match await_switch(std::time::Duration::from_secs(120)) {
                    Some(Ok((workspace, branch))) => fs_cli::pipe::Response::Switched {
                        workspace,
                        branch,
                        dirs_invalidated: dirs,
                    },
                    Some(Err(detail)) => fs_cli::pipe::Response::Failed {
                        kind: "remount-failed".to_owned(),
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

            fn restore(&self, commit: &str, discard: bool) -> fs_cli::pipe::Response {
                // **Admission first, then the publication gate** (S3 §3.2): the mount stops new opens, waits for the ones in flight, and only then decides. A
                // restore that ran before this would leave a callback that had just resolved an old path about to register a handle on a vanished object.
                const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
                let Some(adapter) = self.0.adapter.get() else {
                    return fs_cli::pipe::Response::Failed {
                        kind: "restore".to_owned(),
                        detail: "the mount is not serving yet".to_owned(),
                        fields: serde_json::json!({}),
                    };
                };
                // **The handle count gets the same patience as an unmount** (S3 §3.2): an application's `CloseHandle` returns before the FSD delivers `Close` — the
                // cache manager can keep a file object for a moment — so a count taken once would refuse a restore that is about to become legal. Measured
                // before this loop existed: the refusal came in **48 ms**, which is exactly the spurious-refusal shape.
                let deadline = std::time::Instant::now() + WAIT;
                let quiesce = loop {
                    match adapter.quiesce(fs_winfsp::admission::QuiesceKind::Restore, WAIT) {
                        fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                            if std::time::Instant::now() >= deadline {
                                break fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs };
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        other => break other,
                    }
                };
                match quiesce {
                    fs_winfsp::admission::Quiesce::HandlesOpen { files, dirs } => {
                        // **Nothing moved**: the refusal happens before `restore` is called, so the view is exactly what it was.
                        fs_cli::pipe::Response::RestoreRefused { files, dirs }
                    }
                    fs_winfsp::admission::Quiesce::TimedOut { in_flight } => {
                        fs_cli::pipe::Response::Failed {
                            kind: "restore".to_owned(),
                            detail: format!(
                                "{in_flight} open(s) did not finish within {}s",
                                WAIT.as_secs()
                            ),
                            fields: serde_json::json!({"in_flight": in_flight}),
                        }
                    }
                    fs_winfsp::admission::Quiesce::Quiesced(guard) => {
                        let dirs = adapter.open_handle_kinds().1;
                        // **R4's pause point**: the gate is closed and the view has not moved yet. An open arriving now must wait for the restore, not resolve against
                        // the view that is about to be replaced.
                        fs_cli::pause_point("restore-after-quiesce");
                        let outcome = self.0.current().handle.restore(commit, discard);
                        // Released whether the restore worked or not: a guard dropped only on success would leave the mount refusing opens after a failure.
                        drop(guard);
                        match outcome {
                            Ok(()) => fs_cli::pipe::Response::Failed {
                                kind: "restored".to_owned(),
                                detail: commit.to_owned(),
                                fields: serde_json::json!({"commit": commit, "dirs_invalidated": dirs}),
                            },
                            // **The failure kind travels** (S3 card R5): the CLI maps a kind to an exit code, and a single `kind: "restore"` collapses
                            // "discard your changes" (5) and "that commit does not exist" (3) into "something went wrong" (1) — a script cannot tell them
                            // apart, which is the whole point of the codes existing.
                            Err(error) => fs_cli::pipe::Response::Failed {
                                kind: restore_kind(&error),
                                detail: error.to_string(),
                                fields: serde_json::json!({}),
                            },
                        }
                    }
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
        self.adapter
            .get()
            .map_or(0, |adapter| adapter.open_handles())
    }
    fn teardown_complete(&self) -> bool {
        // The record is removed by `DELETE_ON_CLOSE` when the mount's handle drops, which happens **after** the unmount and the flush — so its absence is the
        // moment the whole stop sequence is done.
        !self.record.exists()
    }
}

/// The **kind** of a failed `restore`, as the protocol carries it (S3 card R5): the CLI turns a kind into an exit code, so this has to keep the distinctions
/// the rest of the CLI already makes (2 usage / 3 not found / 4 busy / 5 conflict).
fn restore_kind(error: &fs_workspace::WorkspaceError) -> String {
    match error {
        fs_workspace::WorkspaceError::NotFound { .. } => "not-found".to_owned(),
        fs_workspace::WorkspaceError::WorkspaceDirty => "workspace-dirty".to_owned(),
        fs_workspace::WorkspaceError::StoreInUse { .. } => "in-use".to_owned(),
        _ => "restore".to_owned(),
    }
}
/// Every live mount record, as `discovery::discover` wants them: the records and their liveness in one pass.
///
/// One reader for both the A3 message (`live_mount_of`) and path discovery, so a record that one of them considers live cannot be dead to the other.
#[must_use]
pub fn live_records() -> Vec<(fs_cli::discovery::Record, fs_cli::discovery::Liveness)> {
    let Ok(entries) = std::fs::read_dir(mounts_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "mount")
        {
            continue;
        }
        let liveness = match record_is_live(&path) {
            Liveness::Live => fs_cli::discovery::Liveness::Live,
            Liveness::Stale | Liveness::Gone => fs_cli::discovery::Liveness::Stale,
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let field = |key: &str| {
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(key)?
                        .strip_prefix('=')
                        .map(str::to_string)
                })
                .unwrap_or_default()
        };
        out.push((
            fs_cli::discovery::Record {
                store: PathBuf::from(field("store")),
                workspace: field("workspace"),
                mount_point: PathBuf::from(field("mount_point")),
                pid: field("pid").parse().unwrap_or(0),
                branch: field("branch"),
                pipe: field("pipe"),
            },
            liveness,
        ));
    }
    out
}

/// The mount whose volume contains the current directory, if any (ADR-010 §4.4).
///
/// Same two queries as `run`'s discovery, exposed so a command can ask "am I being run from inside a mount" without re-implementing path discovery — A3b's
/// third case is exactly that question.
#[must_use]
pub fn mount_of_cwd() -> Option<fs_cli::discovery::Record> {
    let cwd = std::env::current_dir().ok()?;
    // **Three values before the verdict** (383/580): this function and `run`'s discovery are two callers of one rule, so when they disagree the disagreement has to be
    // visible rather than argued about.
    if std::env::var_os("LAYERFS_DEBUG_DISCOVERY").is_some() {
        let records = live_records();
        eprintln!(
            "mount_of_cwd-values: cwd={:?} candidates={:?}",
            cwd.display().to_string(),
            records
                .iter()
                .map(|(record, liveness)| format!(
                    "{} [{liveness:?}] prefix={} mount_point_is_mount={}",
                    record.mount_point.display(),
                    cwd.starts_with(&record.mount_point),
                    fs_cli::winpaths::is_volume_mount_point(&record.mount_point)
                ))
                .collect::<Vec<_>>()
        );
    }
    fs_cli::discovery::discover(
        &cwd,
        fs_cli::winpaths::volume_root,
        fs_cli::winpaths::is_volume_mount_point,
        live_records,
    )
}

/// The live mount serving `workspace`, if there is one: `(mount_point, pid)`.
///
/// Read from the records rather than guessed: the point of A3's message is that it names **which** mount and **which** process, and a generic "in use" would
/// leave the user to hunt for the window.
#[must_use]
pub fn live_mount_of(workspace: &str) -> Option<(String, u32)> {
    let entries = std::fs::read_dir(mounts_dir()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "mount")
        {
            continue;
        }
        if record_is_live(&path) != Liveness::Live {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let field = |key: &str| {
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(key)?
                        .strip_prefix('=')
                        .map(str::to_string)
                })
                .unwrap_or_default()
        };
        if field("workspace") == workspace {
            return Some((field("mount_point"), field("pid").parse().unwrap_or(0)));
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
/// Where a `switch` records the workspace it wants, and where the answer is left for the waiting pipe.
///
/// Two slots rather than one channel because the two halves are in different threads and neither may block the other's critical section: the pipe thread
/// **asks** (target slot) and then **waits** (outcome slot + condvar); the mount loop takes the target and publishes the outcome after the remount. A channel
/// would work too, but it would need a lifetime that outlives both, which is what these statics are.
static SWITCH_TARGET: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

struct SwitchOutcome {
    result: std::sync::Mutex<Option<Result<(String, String), String>>>,
    changed: std::sync::Condvar,
}

static SWITCH_OUTCOME: std::sync::OnceLock<SwitchOutcome> = std::sync::OnceLock::new();

fn switch_outcome() -> &'static SwitchOutcome {
    SWITCH_OUTCOME.get_or_init(|| SwitchOutcome {
        result: std::sync::Mutex::new(None),
        changed: std::sync::Condvar::new(),
    })
}

/// Asks the mount loop to serve `workspace` from now on. Returns the workspace that was **already** pending, if any.
fn set_switch_target(workspace: &str) -> Option<String> {
    let mut target = SWITCH_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    target.replace(workspace.to_owned())
}

/// Takes the pending target, if a switch is waiting for one.
fn take_switch_target() -> Option<String> {
    SWITCH_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// Leaves the remount's answer for the waiting pipe thread.
fn publish_switch(result: Result<(String, String), String>) {
    let outcome = switch_outcome();
    *outcome
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
    outcome.changed.notify_all();
}

/// Waits (bounded) for the remount's answer.
fn await_switch(wait: std::time::Duration) -> Option<Result<(String, String), String>> {
    let outcome = switch_outcome();
    let deadline = std::time::Instant::now() + wait;
    let mut slot = outcome
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while slot.is_none() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let (next, _) = outcome
            .changed
            .wait_timeout(slot, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot = next;
    }
    slot.take()
}

#[cfg(test)]
mod tests {
    use super::{
        calibrated_major, hold_record, mounts_dir, record_is_live, record_name, request_stop,
        stop_requested, version_parts, Liveness,
    };
    use std::path::Path;

    /// **A10, the half that can be tested without a console**: the handler's effect is a flag, and the mount loop reads that flag.
    ///
    /// The other half — a real Ctrl-Break delivering the event — needs a process group and a console, so it lives in the integration test; what is pinned here
    /// is that "handled" really means "remembered", because a handler that returned 1 without setting anything would let the default handler kill the process
    /// with the volume still attached.
    #[test]
    fn a_control_event_asks_the_mount_to_stop() {
        assert!(!stop_requested(), "cell start: nothing has asked yet");
        request_stop();
        assert!(stop_requested(), "cell after: the mount loop must see it");
    }

    /// **A7 / §9.2**: a record held by its owner is `Live`, one nobody holds is `Stale`, and — because the owner uses `DELETE_ON_CLOSE` — the file is
    /// **gone** once the handle is dropped, which is what makes a crashed mount leave nothing behind.
    #[test]
    fn a_record_is_live_while_held_and_gone_once_released() {
        let directory = std::env::temp_dir().join(format!("fs-cli-record-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp");

        let held = directory.join("held.mount");
        let handle = hold_record(&held, "version=1\npid=1\n").expect("hold a record");
        assert_eq!(
            record_is_live(&held),
            Liveness::Live,
            "cell live: the owner still holds it"
        );
        drop(handle);
        assert_eq!(
            record_is_live(&held),
            Liveness::Gone,
            "cell gone: DELETE_ON_CLOSE removes the record when the owner dies"
        );

        // A record written the ordinary way is `Stale`: nobody holds it, so its owner is gone.
        let stale = directory.join("stale.mount");
        std::fs::write(&stale, b"version=1\n").expect("write");
        assert_eq!(record_is_live(&stale), Liveness::Stale, "cell stale");

        assert_eq!(
            record_is_live(&directory.join("missing.mount")),
            Liveness::Gone,
            "cell missing"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The mounts directory is one function, and the override is what keeps tests from touching the real one.
    #[test]
    fn the_mounts_directory_can_be_redirected() {
        let before = mounts_dir();
        // SAFETY: single-threaded unit test; nothing else in this binary reads the variable concurrently.
        unsafe { std::env::set_var("LAYERFS_MOUNTS_DIR", "C:\\layerfs-test-mounts") };
        assert_eq!(mounts_dir(), Path::new("C:\\layerfs-test-mounts"));
        // SAFETY: as above.
        unsafe { std::env::remove_var("LAYERFS_MOUNTS_DIR") };
        assert_eq!(
            mounts_dir(),
            before,
            "without the override the default comes back"
        );
    }

    #[test]
    fn a_mount_point_becomes_a_file_name() {
        assert_eq!(record_name(Path::new("V:")), "V");
        assert_eq!(record_name(Path::new("C:\\mnt\\x")), "C_mnt_x");
        assert_eq!(record_name(Path::new("/mnt/x")), "mnt_x");
    }

    #[test]
    fn the_version_is_decoded_into_its_two_halves() {
        assert_eq!(version_parts(0x0002_0001), (2, 1));
        assert_eq!(calibrated_major(), 2);
    }
}
