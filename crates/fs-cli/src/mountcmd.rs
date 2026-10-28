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
use fs_winfsp::mounted::mount_read_only;
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

/// A drive letter or directory, reduced to something usable as a file name: `V:` -> `V`, `C:\mnt\x` -> `C_mnt_x`.
#[must_use]
pub fn record_name(mount_point: &Path) -> String {
    let text = mount_point.to_string_lossy();
    let trimmed = text.trim_end_matches([':', '/', '\\']);
    // Runs of separators collapse: `C:\mnt` is one boundary, not two, and a double underscore in a file name helps nobody.
    let mut flattened = String::new();
    for character in trimmed.chars() {
        let replacement = if matches!(character, ':' | '/' | '\\') {
            '_'
        } else {
            character
        };
        if replacement == '_' && flattened.ends_with('_') {
            continue;
        }
        flattened.push(replacement);
    }
    // A drive letter is the common case and deserves a short name rather than `V_`.
    if flattened.is_empty() {
        "root".to_string()
    } else {
        flattened.trim_matches('_').to_string()
    }
}

fn runtime_dir() -> PathBuf {
    std::env::temp_dir().join("layerfs-mounts")
}

fn state_path(mount_point: &Path) -> PathBuf {
    runtime_dir().join(format!("{}.mount", record_name(mount_point)))
}

fn stop_path(mount_point: &Path) -> PathBuf {
    runtime_dir().join(format!("{}.stop", record_name(mount_point)))
}

/// `FspVersion`'s two halves. The calibration in `fs_winfsp::abi::measured::DLL_BUILD` is a full build string, so the gate compares the **major
/// version** it starts with — the layout was measured against 2.x, and a 3.x DLL may renumber the slots again.
#[must_use]
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
        volume_label_length: 7,
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
pub fn mount(store: &Path, arguments: &[String]) -> Result<(), Error> {
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
    let name = option(arguments, "--workspace").unwrap_or_else(|| "main".to_string());
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
    let backend: DynOps = Box::new(ApiBackend::new(workspace));

    std::fs::create_dir_all(runtime_dir())?;
    let state = state_path(&mount_point);
    let stop = stop_path(&mount_point);
    let _ = std::fs::remove_file(&stop);
    std::fs::write(
        &state,
        format!(
            "pid={}\nmount_point={}\nworkspace={name}\nstore={}\n",
            std::process::id(),
            mount_point.display(),
            store.display()
        ),
    )?;
    println!(
        "mounting {} at {} (workspace {name}); stop with `layerfs umount {}`",
        store.display(),
        mount_point.display(),
        mount_point.display()
    );

    let library = default_library();
    let report = mount_read_only(&library, &mount_point, backend, volume(), |_path: &Path| {
        // The probe **is** the command's lifetime: everything below happens while the volume is attached.
        let started = Instant::now();
        loop {
            if stop.exists() || !state.exists() {
                println!("unmount requested after {:?}", started.elapsed());
                break;
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    });
    let _ = std::fs::remove_file(&stop);
    let _ = std::fs::remove_file(&state);
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
            "{} is already in use (0x{:#010X}); a previous mount may still hold it — try `layerfs umount {} --force`, or pick another letter",
            mount_point.display(),
            report.mount,
            mount_point.display()
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
    let stop = stop_path(&mount_point);
    if !state.exists() {
        return Err(format!("no mount recorded for {}", mount_point.display()).into());
    }
    std::fs::create_dir_all(runtime_dir())?;
    std::fs::write(&stop, b"stop\n")?;
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
        let _ = std::fs::remove_file(&stop);
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
    let directory = runtime_dir();
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

fn option(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|argument| argument == name)?;
    arguments.get(at + 1).cloned()
}

#[cfg(test)]
mod tests {
    use super::{calibrated_major, record_name, version_parts};
    use std::path::Path;

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
