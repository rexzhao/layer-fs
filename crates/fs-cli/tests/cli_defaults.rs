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

/// **A0c + A0b** (ADR-010 §13): the eight file commands are gone, they say so **before** the store is touched, and `init` leaves no `cli/` directory.
///
/// The criterion this replaces (`init` then `put`/`ls` without a checkout) is void: those commands no longer exist — the ADR's own wording for this row.
#[test]
fn the_removed_file_commands_say_so_and_init_leaves_no_current() {
    let store = temp_dir("init");
    let init = Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    assert!(init.status.success(), "init: {init:?}");

    // **A0b**: no `cli/` directory is created any more — the workspace is named on the command line.
    assert!(
        !store.join("cli").exists(),
        "cell init: `init` must not write a current-workspace record"
    );

    // **A0c**: every removed command fails with the constant message and exit code 2, *and* does so against a store that does not exist — so the
    // sentence is about the command rather than about the store (the mutation "open the store first" turns this red with a different message).
    let missing_store = temp_dir("init-missing");
    for command in fs_cli::FILE_COMMANDS {
        let output = Command::new(binary())
            .arg("--store")
            .arg(&missing_store)
            .arg(command)
            .output()
            .expect("run the CLI");
        assert_eq!(
            output.status.code(),
            Some(2),
            "cell removed: `{command}` must exit 2, got {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("file operations go through the mounted file system"),
            "cell removed: `{command}` must name the replacement, got {stderr:?}"
        );
        assert!(
            !stderr.contains("does not exist"),
            "cell removed: `{command}` must not be answered as a missing store: {stderr:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&missing_store);
}

/// **A0**: with no `--workspace`, a lifecycle command fails with the ADR's constant and exit code 2 — and nothing else.
#[test]
fn a_lifecycle_command_without_a_workspace_says_so() {
    let store = temp_dir("no-workspace");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    // Plant a `cli/current` that **would** have made the old code succeed: the point of A0 is that the record is not consulted at all.
    std::fs::create_dir_all(store.join("cli")).expect("cli dir");
    std::fs::write(store.join("cli").join("current"), b"main\n").expect("plant a current");

    for arguments in [
        vec!["commit", "-m", "x"],
        vec!["status"],
        vec!["restore", "main"],
        vec!["snapshot", "create"],
    ] {
        let output = Command::new(binary())
            .arg("--store")
            .arg(&store)
            .args(&arguments)
            .output()
            .expect("run the CLI");
        assert_eq!(
            output.status.code(),
            Some(2),
            "cell workspace: {arguments:?} must exit 2, got {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(fs_cli::NO_WORKSPACE),
            "cell workspace: {arguments:?} must print the constant, got {stderr:?}"
        );
    }
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
            .arg("--workspace")
            .arg("main")
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
        .arg("--workspace")
        .arg("main")
        .output()
        .expect("second mount");
    let stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        !second.status.success(),
        "cell collision: a taken letter must refuse, got {second:?}"
    );
    assert!(
        stderr.contains("umount"),
        "cell collision: the message must name the way out, got {stderr:?}"
    );
    // **Widened once, deliberately (§9.2).** With mount records in place, a taken letter is caught by the **record layer** before WinFsp sees it, so the
    // message is now "`T:` is already mounted (pid N) — `layerfs umount T:` first": it names the same exit (`umount`) *and* the owner, which the
    // `0xC0000035` message could not. The assertion stays "the message names a way out" rather than pinning which layer found it.

    let _ = Command::new(binary())
        .arg("umount")
        .arg(&mount_point)
        .output();
    drop(first);
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&other);
}

/// **A1** (ADR-010 §13): the branch verbs do not need the workspace's overlay.
///
/// The cheapest honest way to prove it without a mount helper: **remove the workspace row entirely** and ask again. If `log main` / `reset` / `revert`
/// still work, they are reading the branch, not the workspace; the mutation the ADR names (branch source back to `workspace_open`) makes this red with
/// "no workspace main".
#[test]
fn the_branch_verbs_work_without_the_workspace() {
    let store = temp_dir("branch-verbs");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    // `init` created the `main` workspace; discard it so nothing can open an overlay under that name.
    let discard = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("workspace")
        .arg("discard")
        .arg("main")
        .output()
        .expect("discard");
    assert!(discard.status.success(), "discard: {discard:?}");

    let log = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("log")
        .arg("main")
        .output()
        .expect("log");
    assert!(
        log.status.success(),
        "cell A1: `log main` must read the branch, not the workspace: {:?}",
        String::from_utf8_lossy(&log.stderr)
    );

    let head = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("branch")
        .output()
        .expect("branch");
    let head = String::from_utf8_lossy(&head.stdout)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .expect("a head id")
        .to_owned();
    let reset = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("reset")
        .arg(&head)
        .arg("--branch")
        .arg("main")
        .output()
        .expect("reset");
    assert!(
        reset.status.success(),
        "cell A1: `reset --branch main` must not need the workspace: {:?}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **A2** (ADR-010 §13): `mount` without `--workspace` refuses, says which option is missing, and mounts nothing.
#[test]
fn mount_without_a_workspace_refuses() {
    let store = temp_dir("mount-no-workspace");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let mount_point = std::env::var_os("LAYER_FS_CLI_MNT_THIRD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("R:"));
    let output = Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("mount")
        .arg(&mount_point)
        .output()
        .expect("mount");
    assert_eq!(
        output.status.code(),
        Some(2),
        "cell A2: a usage error exits 2, got {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--workspace"),
        "cell A2: the message must name the option, got {stderr:?}"
    );
    assert!(
        !mount_point.exists(),
        "cell A2: nothing may be mounted when the command is refused"
    );
    let _ = std::fs::remove_dir_all(&store);
}
