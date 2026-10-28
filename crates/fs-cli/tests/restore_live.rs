//! **R1** — `restore` through the mount: the file reads back the **old** bytes, and the workspace's base is the commit that was asked for.
//!
//! The step that makes this more than "the command exited 0" is ③: after the restore, the bytes under the same name must be the ones from `c1`. That is also
//! the step that measures V6 — whether `FileInfoTimeout = 0` really means there is no data cache. If the old content cannot be seen, `restore` depends on
//! change notification (`FspFileSystemNotify`, U9) and this card cannot deliver it; the card says to **stop** in that case, and so does this test.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    let path = std::env::temp_dir().join(format!("fs-cli-r1-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp");
    path
}

fn cli(
    store: &std::path::Path,
    cwd: &std::path::Path,
    arguments: &[&str],
) -> (bool, String, String) {
    let output = Command::new(binary())
        .arg("--store")
        .arg(store)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .expect("run fs-cli");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_restore_through_the_mount_shows_the_old_bytes() {
    common::strict_gate();
    let store = temp_dir("store");
    let mount_point = std::env::var_os("LAYER_FS_CLI_MNT_R1")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("W:"));
    assert_eq!(
        fs_winfsp_mount_free(&mount_point),
        Some(false),
        "cell precondition: {} must be free before mounting",
        mount_point.display()
    );
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let _child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&mount_point)
            .arg("--workspace")
            .arg("main")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    );
    let root = PathBuf::from(format!("{}\\", mount_point.display()));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&root).is_ok(),
        "cell setup: the mount must serve"
    );

    let file = root.join("f");
    std::fs::write(&file, b"1").expect("write 1");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c1"]);
    assert!(ok, "cell R1: commit c1 must succeed: {out}{err}");
    let c1 = out
        .trim()
        .strip_prefix("commit ")
        .unwrap_or_default()
        .to_owned();
    assert!(
        !c1.is_empty(),
        "cell R1: the commit id is printed, got {out:?}"
    );

    // **c1's root, read while c1 is still the view.** The root is what a `restore` moves — the base *commit* deliberately stays where the branch head was, so
    // the root is the observable that has to come back.
    let root_of_c1 = {
        let (ok, out, err) = cli(&store, &mount_point, &["status"]);
        assert!(ok, "cell R1: status must answer after c1: {err}");
        let root = out
            .lines()
            .find_map(|line| line.strip_prefix("root:      "))
            .unwrap_or_default()
            .trim()
            .to_owned();
        assert!(
            !root.is_empty(),
            "cell R1: status must name the root, got {out:?}"
        );
        root
    };

    std::fs::write(&file, b"2").expect("write 2");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c2"]);
    assert!(ok, "cell R1: commit c2 must succeed: {out}{err}");

    // ① Read the current content once, so that any cache has had its chance to fill with the **new** bytes.
    assert_eq!(
        std::fs::read(&file).expect("read 2"),
        b"2",
        "cell R1: before the restore the file holds the second commit's bytes"
    );

    // ② No file handle is open at this point, so the restore must be allowed.
    let (ok, out, err) = cli(&store, &mount_point, &["restore", &c1]);
    assert!(
        ok,
        "cell R1: `restore {c1}` from inside the mount must succeed, got stdout={out:?} stderr={err:?}"
    );

    // ③ **The step that measures V6**: the bytes under the same name must be the ones from `c1`.
    let after = std::fs::read(&file).expect("read after the restore");
    println!(
        "R1 reading: after restore, /f = {:?}",
        String::from_utf8_lossy(&after)
    );
    assert_eq!(
        after, b"1",
        "cell R1/V6: the restore must be visible — if this reads \"2\" there is a data cache and restore depends on U9"
    );

    // ④ The workspace's **root** is the restored one. (The base *commit* legitimately stays where the branch head was: restore grows history rather than
    // rewinding it, so the observable that must move is the root.)
    let (ok, out, err) = cli(&store, &mount_point, &["status"]);
    assert!(ok, "cell R1: status must answer: {err}");
    println!("R1 status after restore: {out}");
    let root_line = out
        .lines()
        .find_map(|line| line.strip_prefix("root:      "))
        .unwrap_or_default()
        .trim()
        .to_owned();
    assert_eq!(
        root_line, root_of_c1,
        "cell R1: after the restore the view must be c1's root {root_of_c1}, got {root_line}"
    );

    let _ = std::fs::remove_dir_all(&store);
}

/// Whether a drive letter is present, through the frontend that knows how to ask.
fn fs_winfsp_mount_free(letter: &std::path::Path) -> Option<bool> {
    fs_winfsp::mounted::drive_present(letter)
}
