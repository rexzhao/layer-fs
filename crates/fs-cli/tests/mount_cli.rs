//! B-24: the mount CLI end to end — mount in a child process, write through the OS, unmount with the CLI, and check the OS agrees.
//!
//! This is the criterion the card asks for, and it is deliberately written against the **binary**: `mount`/`umount`/`list` are the interface, so a test
//! that called the library directly would prove something else.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs_workspace::Repo;

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fs-cli"))
}

fn mount_point() -> PathBuf {
    std::env::var_os("LAYER_FS_CLI_MNT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("U:"))
}

fn root_of(mount_point: &Path) -> PathBuf {
    if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.to_path_buf()
    }
}

/// A repository with a `main` workspace, created through the library (the CLI's own `workspace` verb only discards).
fn fixture(label: &str) -> (TempRoot, PathBuf) {
    let root = TempRoot(std::env::temp_dir().join(format!(
        "fs-cli-mount-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    )));
    std::fs::create_dir_all(&root.0).expect("temp");
    let store = root.0.join("store");
    let repo = Arc::new(Repo::open(&store, true).expect("repo"));
    repo.init().expect("init");
    repo.workspace_create("main", "main").expect("workspace");
    (root, store)
}

fn wait_until<F: Fn() -> bool>(what: &str, ready: F) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn mount_write_unmount_through_the_cli() {
    let (_root, store) = fixture("rw");
    let mount_point = mount_point();
    let root = root_of(&mount_point);
    assert!(
        !mount_point.exists(),
        "cell setup: {} must be free before mounting",
        mount_point.display()
    );

    let child: Child = Command::new(binary())
        .arg("mount")
        .arg(&store)
        .arg(&mount_point)
        .arg("--workspace")
        .arg("main")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn mount");

    // The volume is served once the OS can write through it — the same question the mount's own probe asks.
    wait_until("the mount to serve", || std::fs::metadata(&root).is_ok());
    std::fs::write(root.join("through-cli.txt"), b"hello layerfs")
        .expect("write through the mount");

    // `list` must report the mount while it is alive.
    let listing = Command::new(binary()).arg("list").output().expect("list");
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    assert!(
        listing.contains(&mount_point.to_string_lossy().into_owned()),
        "cell list: the mount must be listed, got {listing:?}"
    );

    let unmount = Command::new(binary())
        .arg("umount")
        .arg(&mount_point)
        .output()
        .expect("umount");
    assert!(
        unmount.status.success(),
        "cell umount: {unmount:?} (stdout {:?})",
        String::from_utf8_lossy(&unmount.stdout)
    );

    let mut child = child;
    let mut output = String::new();
    if let Some(stdout) = child.stdout.as_mut() {
        let _ = stdout.read_to_string(&mut output);
    }
    let _ = child.wait();

    // **The OS, not the CLI's own word** (452): the drive is gone, and the bytes are in the workspace.
    wait_until("the drive to disappear", || !mount_point.exists());
    let repo = Arc::new(Repo::open(&store, false).expect("reopen"));
    let workspace = repo.workspace_open("main").expect("workspace");
    let bytes = workspace.read_at("/through-cli.txt", 0, 13).expect("read");
    assert_eq!(bytes, b"hello layerfs");
}
