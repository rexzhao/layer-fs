//! A6b / A6d — path discovery against a **real mount**, plus the measurement A6d asks for (a directory mount point).
//!
//! A6b is the promise path discovery makes: standing inside a mount, `status` works without `--store` or `--workspace`, because the volume knows which
//! workspace it is serving. A6d is the question the card says to **measure and record** rather than assume: what does a *directory* mount point answer, and does
//! discovery work there?

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

fn start_mount(label: &str, mount_point: &Path) -> (PathBuf, MountChild) {
    let store = temp_dir(label);
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
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
    let ready = if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.to_path_buf()
    };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) && std::fs::metadata(&ready).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&ready).is_ok(),
        "cell setup: the mount at {} must serve",
        mount_point.display()
    );
    (store, child)
}

/// **A6b**: standing inside a drive-letter mount, `status` needs no options — and standing outside one, it still refuses with the ADR's constant.
#[test]
fn status_inside_a_mount_needs_no_options() {
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
    let root = temp_dir("dir-mount");
    let mount_point = root.join("mnt");
    let (dir_store, _child) = start_mount("dir", &mount_point);

    let discovered = Command::new(binary())
        .arg("status")
        .current_dir(&mount_point)
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
