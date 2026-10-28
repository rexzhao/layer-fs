//! The three conveniences B-24 needed to be usable: `init` leaves a workspace behind, `mount` takes the store as a global option, and a taken drive
//! letter says what to do about it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A spawned mount that is **killed on drop** — a failed test must not leave a volume attached, nor a child holding the runner's pipes open.
struct MountChild(std::process::Child);

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
        "fs-cli-defaults-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

/// Section 1: `init` then `put`/`ls` **without** a `checkout` in between.
#[test]
fn init_leaves_a_workspace_ready_to_use() {
    let store = temp_dir("init");
    let init = Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    assert!(init.status.success(), "init: {init:?}");

    let local = store.join("payload.txt");
    std::fs::write(&local, b"hello").expect("local file");
    let put = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("put")
        .arg(&local)
        .arg("/payload.txt")
        .output()
        .expect("put");
    assert!(
        put.status.success(),
        "cell init: `put` must work straight after `init`, got {put:?} (stderr {:?})",
        String::from_utf8_lossy(&put.stderr)
    );

    let ls = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("ls")
        .output()
        .expect("ls");
    assert!(ls.status.success(), "cell init: `ls`: {ls:?}");
    let listing = String::from_utf8_lossy(&ls.stdout);
    assert!(
        listing.contains("payload.txt"),
        "cell init: the file put a moment ago must be listed, got {listing:?}"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// Section 3: a drive letter that is already mounted must produce a message that names the way out.
#[test]
fn a_taken_drive_letter_says_what_to_do() {
    let store = temp_dir("collision");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    // **A second repository**, because the first one's database is held by the running mount (ADR-008: one connection per database), so the second
    // attempt would be refused with "database is locked" long before it reached the drive letter. The scenario this criterion is about is a *taken
    // letter*, and a second store is the cheapest way to reach it without pretending the lock is the letter.
    let other = temp_dir("collision-other");
    Command::new(binary())
        .arg("init")
        .arg(&other)
        .output()
        .expect("init other");
    // **Its own letter**, not the one `mount_cli` uses: two mount tests running as separate processes must not fight over one drive letter (the same
    // structural rule the mount tests already follow). `LAYER_FS_CLI_MNT` belongs to `mount_cli`; this one answers to `LAYER_FS_CLI_MNT_SECOND`.
    let mount_point = std::env::var_os("LAYER_FS_CLI_MNT_SECOND")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("T:"));

    let first = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&mount_point)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("first mount"),
    );
    // Wait for the volume, so the second attempt genuinely collides.
    let root = if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.clone()
    };
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(10)
        && std::fs::metadata(&root).is_err()
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let second = Command::new(binary())
        .arg("--store")
        .arg(&other)
        .arg("mount")
        .arg(&mount_point)
        .output()
        .expect("second mount");
    let stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        !second.status.success(),
        "cell collision: a taken letter must refuse, got {second:?}"
    );
    assert!(
        stderr.contains("--force"),
        "cell collision: the message must name the way out, got {stderr:?}"
    );

    let _ = Command::new(binary())
        .arg("umount")
        .arg(&mount_point)
        .output();
    drop(first);
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&other);
}
