//! A6b / A6d — path discovery against a **real mount**, plus the measurement A6d asks for (a directory mount point).
//!
//! A6b is the promise path discovery makes: standing inside a mount, `status` works without `--store` or `--workspace`, because the volume knows which
//! workspace it is serving. A6d is the question the card says to **measure and record** rather than assume: what does a *directory* mount point answer, and does
//! discovery work there?

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fs_cli::NO_WORKSPACE;

struct MountChild(Child);

impl Drop for MountChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fs-cli"))
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fs-cli-discovery-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

/// Waits until **no volume is attached** at `mount_point`.
///
/// A previous case can leave a letter attached for a moment after its own `umount` returned (`StopDispatcher` returning is not the same as the device being gone),
/// and the next case then mounts onto a volume that is still being torn down — which shows up as one random failure in a different test each run. Waiting for the
/// **state** is what makes the precondition true rather than likely.
fn wait_until_free(mount_point: &Path) {
    // Only a **drive letter** has a volume that can be "present": a directory mount point is a directory on the host volume, so `drive_present` cannot describe it
    // (measured: it answers `None`, and a loop waiting for `Some(false)` would sit there for the whole bound).
    if !mount_point.to_string_lossy().ends_with(':') {
        return;
    }
    let started = Instant::now();
    loop {
        if fs_winfsp::mounted::drive_present(mount_point) == Some(false) {
            return;
        }
        assert!(
            started.elapsed() <= Duration::from_secs(20),
            "cell setup: {} never became free (still present after {:?})",
            mount_point.display(),
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_mount(label: &str, mount_point: &Path) -> (PathBuf, MountChild) {
    let store = temp_dir(label);
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    wait_until_free(mount_point);
    let child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(mount_point)
            .arg("--workspace")
            .arg("main")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    );
    // **"Ready" means the control channel answers, not that a directory exists** (434): for a directory mount point the directory is on the *host* volume and
    // exists before the mount is attached at all, so `metadata` succeeds immediately and the criterion would run against a volume that is not there yet.
    let started = Instant::now();
    let mut ready = false;
    while started.elapsed() < Duration::from_secs(10) && !ready {
        let record = std::fs::read_to_string(
            fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(mount_point))),
        )
        .unwrap_or_default();
        let pipe = record
            .lines()
            .find_map(|line| line.strip_prefix("pipe=").map(str::to_string))
            .unwrap_or_default();
        if !pipe.is_empty() {
            let request = fs_cli::pipe::Request {
                v: fs_cli::pipe::PROTOCOL,
                op: "hello".to_owned(),
                args: serde_json::json!({}),
            };
            ready = fs_cli::pipe_win::call(&pipe, &request, 500).is_ok();
        }
        if !ready {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(
        ready,
        "cell setup: the mount at {} must serve (control channel answered={ready})",
        mount_point.display()
    );
    // **The control channel answering is not the volume serving** (measured: `hello` is answered by the pipe thread, which starts *before* the volume is attached,
    // and the next command then fails with `ERROR_DIRECTORY (267)` when it tries to stand inside the mount). Readiness is a **write through the mount**, with a
    // bound; the reading is printed if it never comes.
    let root = if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.to_path_buf()
    };
    let probe = root.join("probe.txt");
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        match std::fs::write(&probe, b"probe") {
            Ok(()) => break,
            Err(error) if started.elapsed() < Duration::from_secs(15) => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!(
                "cell setup: the volume at {} never became writable after {attempts} attempts: {error:?}",
                mount_point.display()
            ),
        }
    }
    let _ = std::fs::remove_file(&probe);
    (store, child)
}

/// **A6b**: standing inside a drive-letter mount, `status` needs no options — and standing outside one, it still refuses with the ADR's constant.
#[test]
fn status_inside_a_mount_needs_no_options() {
    common::strict_gate();
    let mount_point = std::env::var_os("LAYER_FS_CLI_MNT_DISC_A")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("J:"));
    let (_store, _child) = start_mount("inside", &mount_point);
    let inside = PathBuf::from(format!("{}\\", mount_point.display()));

    let discovered = Command::new(binary())
        .arg("status")
        .current_dir(&inside)
        .output()
        .expect("status from inside the mount");
    assert!(
        discovered.status.success(),
        "cell A6b: `status` inside the mount must work without options: {:?}",
        String::from_utf8_lossy(&discovered.stderr)
    );
    let printed = String::from_utf8_lossy(&discovered.stdout);
    assert!(
        printed.contains("workspace: main"),
        "cell A6b: discovery must pick the mount's workspace, got {printed:?}"
    );

    // The negative half: the same command somewhere that is not a mount must refuse, and say what to add.
    let outside = temp_dir("outside");
    let refused = Command::new(binary())
        .arg("status")
        .current_dir(&outside)
        .output()
        .expect("status from outside");
    assert_eq!(
        refused.status.code(),
        Some(2),
        "cell A6b negative: a usage error, got {refused:?}"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(NO_WORKSPACE),
        "cell A6b negative: the message is the ADR's constant"
    );
    let _ = std::fs::remove_dir_all(&outside);
}

/// **A6d (V2)** — the measurement the card asks for, recorded rather than assumed.
///
/// A **directory** mount point is not on a volume of its own, so `GetVolumePathNameW` answers the *host* volume — whose label is not `layerfs`. The prediction
/// is therefore that discovery does **not** work there; this test measures it and states which of the two happened, so the conclusion lives in the suite
/// instead of in somebody's memory.
#[test]
fn a_directory_mount_point_is_measured_not_assumed() {
    common::strict_gate();
    let root = temp_dir("dir-mount");
    let mount_point = root.join("mnt");
    let (dir_store, _child) = start_mount("dir", &mount_point);

    // **`bash` cd, not `current_dir`** (579): `SetCurrentDirectory` answers ERROR_DIRECTORY (267) for a directory mount point, so a test driven by it would
    // measure that Win32 boundary rather than the discovery logic. `bash cd` is what a user does here, and it works.
    let discovered = Command::new("C:/Program Files/Git/bin/bash.exe")
        .arg("-c")
        .arg(format!(
            "cd '{}' && '{}' status",
            mount_point.display(),
            binary().display()
        ))
        .output()
        .expect("status from a directory mount");
    let stderr = String::from_utf8_lossy(&discovered.stderr).into_owned();
    let reading = if discovered.status.success() {
        "discovery works at a directory mount point".to_string()
    } else {
        format!(
            "discovery does not work at a directory mount point (exit {:?}): {stderr}",
            discovered.status.code()
        )
    };
    println!("A6d READING: {reading}");

    // **Either answer is allowed, but it must be one of the two known ones.** A third possibility — a crash, or a wrong workspace — is a defect, not a
    // measurement.
    if discovered.status.success() {
        assert!(
            String::from_utf8_lossy(&discovered.stdout).contains("workspace: main"),
            "cell A6d: if discovery works here it must name the right workspace"
        );
    } else {
        assert_eq!(
            discovered.status.code(),
            Some(2),
            "cell A6d: the refusal is a usage error, not a crash"
        );
        assert!(
            stderr.contains(NO_WORKSPACE),
            "cell A6d: and it names the way out"
        );
    }

    // The two-step hint A3b prints is the documented workaround for the red case, so it must exist.
    let checkout = Command::new(binary())
        .arg("--store")
        .arg(&dir_store)
        .arg("checkout")
        .arg("main")
        .current_dir(&mount_point)
        .output()
        .expect("checkout from inside a directory mount");
    assert!(
        checkout.status.success(),
        "cell A6d: `checkout` inside a directory mount must still work: {:?}",
        String::from_utf8_lossy(&checkout.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
