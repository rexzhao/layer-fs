//! **R1** — `restore` through the mount: the file reads back the **old** bytes, and the workspace's base is the commit that was asked for.
//!
//! The step that makes this more than "the command exited 0" is ③: after the restore, the bytes under the same name must be the ones from `c1`. That is also
//! the step that measures V6 — whether `FileInfoTimeout = 0` really means there is no data cache. If the old content cannot be seen, `restore` depends on
//! change notification (`FspFileSystemNotify`, U9) and this card cannot deliver it; the card says to **stop** in that case, and so does this test.

mod common;

use std::os::windows::fs::OpenOptionsExt;

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
    // A label like `V:` is a **mount point**, not a path segment: it is sanitised here rather than at every call site, because a colon in a directory name is
    // `ERROR_DIRECTORY (267)` and the failure would point at the fixture instead of at the mistake.
    let label = label.replace(':', "_");
    let path = std::env::temp_dir().join(format!("fs-cli-restore-{label}-{}", std::process::id()));
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

/// Starts a store and mounts it, returning the pieces a case needs. Each case uses its **own letter**: the mounts in one session do not always come back
/// cleanly, and a shared letter would make two cases blame each other.
fn mount_at(letter: &str) -> (PathBuf, MountChild, PathBuf) {
    let store = temp_dir(letter);
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let mount_point = PathBuf::from(letter);
    assert_eq!(
        fs_winfsp::mounted::drive_present(&mount_point),
        Some(false),
        "cell precondition: {letter} must be free before mounting"
    );
    let child = MountChild(
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
    let root = PathBuf::from(format!("{letter}\\"));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&root).is_ok(),
        "cell setup: {letter} must serve"
    );
    (store, child, root)
}

/// **R2** — an open **file** handle blocks the restore, the view does not move, and the mount is **not** left refusing opens.
#[test]
fn an_open_file_handle_blocks_the_restore_and_nothing_moves() {
    common::strict_gate();
    let (store, _child, root) = mount_at("V:");
    let mount_point = PathBuf::from("V:");
    let file = root.join("f");

    std::fs::write(&file, b"1").expect("write 1");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c1"]);
    assert!(ok, "cell R2: commit c1: {out}{err}");
    let c1 = out
        .trim()
        .strip_prefix("commit ")
        .unwrap_or_default()
        .to_owned();
    std::fs::write(&file, b"2").expect("write 2");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c2"]);
    assert!(ok, "cell R2: commit c2: {out}{err}");

    // The handle that must block: a **file**, held open for the whole attempt.
    let held = std::fs::File::open(&file).expect("hold the file open");
    let started = Instant::now();
    let (ok, out, err) = cli(&store, &mount_point, &["restore", &c1]);
    let waited = started.elapsed();
    println!(
        "R2 reading: exit_ok={ok} waited={waited:?} stdout={:?} stderr={:?}",
        out.trim(),
        err.trim()
    );
    assert!(
        !ok,
        "cell R2: an open file handle must make the restore fail"
    );
    assert!(
        err.contains("1 file"),
        "cell R2: the refusal names the **file** count, got {err:?}"
    );
    // Nothing moved: the bytes are still the second commit's.
    drop(held);
    assert_eq!(
        std::fs::read(&file).expect("read after the refusal"),
        b"2",
        "cell R2: a refused restore must leave the view exactly as it was"
    );

    // **The gate came back**: an open still works (a refusal that left it Paused would hang here).
    let reopened = std::fs::File::open(&file).expect("the mount still opens files after a refusal");
    drop(reopened);

    // And with no handle held, the same command succeeds.
    let (ok, out, err) = cli(&store, &mount_point, &["restore", &c1]);
    assert!(
        ok,
        "cell R2: after closing, the restore must succeed: {out}{err}"
    );
    assert_eq!(
        std::fs::read(&file).expect("read after the restore"),
        b"1",
        "cell R2: and it really moved the view"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **R3** — directory handles do **not** block (D8 = B), and the count is reported so the user knows their shell's view just went stale.
#[test]
fn directory_handles_do_not_block_and_are_reported() {
    common::strict_gate();
    let (store, _child, root) = mount_at("R:");
    let mount_point = PathBuf::from("R:");
    let file = root.join("f");

    std::fs::write(&file, b"1").expect("write 1");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c1"]);
    assert!(ok, "cell R3: commit c1: {out}{err}");
    let c1 = out
        .trim()
        .strip_prefix("commit ")
        .unwrap_or_default()
        .to_owned();
    std::fs::write(&file, b"2").expect("write 2");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c2"]);
    assert!(ok, "cell R3: commit c2: {out}{err}");

    // Two directory handles of different kinds, because both are the case the rule is about:
    // 1. a **child shell whose cwd is the mount** — the one that would make a restore impossible if directories counted;
    let mut shell = Command::new("C:/Program Files/Git/bin/bash.exe")
        .arg("-c")
        .arg("cd /r && sleep 6")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a shell with its cwd inside the mount");
    std::thread::sleep(Duration::from_millis(500));
    // 2. an explicit directory handle held by this process (`FILE_FLAG_BACKUP_SEMANTICS` is what lets `OpenOptions` open a directory at all).
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0200_0000)
        .open(&root)
        .expect("hold a directory handle on the mount root");

    let (ok, out, err) = cli(&store, &mount_point, &["restore", &c1]);
    println!(
        "R3 reading: exit_ok={ok} stdout={:?} stderr={:?}",
        out.trim(),
        err.trim()
    );
    assert!(
        ok,
        "cell R3: directory handles must not block the restore (D8 = B): {out}{err}"
    );
    assert!(
        out.contains("directory handle"),
        "cell R3: the report must say the directory views went stale, got {out:?}"
    );
    let reported: usize = out
        .split_whitespace()
        .find_map(|word| word.parse::<usize>().ok())
        .unwrap_or(0);
    println!("R3 reading: dirs_invalidated reported = {reported}");
    assert!(
        reported >= 2,
        "cell R3: at least the shell's cwd and the explicit handle must be counted, got {reported}"
    );
    assert_eq!(
        std::fs::read(&file).expect("read after the restore"),
        b"1",
        "cell R3: and the view really moved"
    );

    drop(directory);
    let _ = shell.kill();
    let _ = shell.wait();
    let _ = std::fs::remove_dir_all(&store);
}

/// The exit code as well as the text: `restore` in a mount has to keep the same codes as everywhere else (ADR-010 §8.3), or a script cannot tell "discard your
/// changes" from "that commit does not exist".
fn cli_code(
    store: &std::path::Path,
    cwd: &std::path::Path,
    arguments: &[&str],
) -> (i32, String, String) {
    let output = Command::new(binary())
        .arg("--store")
        .arg(store)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .expect("run fs-cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// **R5** — the error kinds travel: dirty without `--discard` is a **conflict** (5), an unknown commit is **not found** (3), and `--discard` succeeds.
#[test]
fn a_refused_restore_keeps_its_exit_code() {
    common::strict_gate();
    let (store, _child, root) = mount_at("G:");
    let mount_point = PathBuf::from("G:");
    let file = root.join("f");

    std::fs::write(&file, b"1").expect("write 1");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c1"]);
    assert!(ok, "cell R5: commit c1: {out}{err}");
    let c1 = out
        .trim()
        .strip_prefix("commit ")
        .unwrap_or_default()
        .to_owned();
    // Leave an uncommitted change behind, which is what `WorkspaceDirty` is about.
    std::fs::write(&file, b"2").expect("write 2");

    let (code, out, err) = cli_code(&store, &mount_point, &["restore", &c1]);
    println!("R5 reading: dirty -> code={code} stderr={:?}", err.trim());
    assert_eq!(
        code, 5,
        "cell R5: a dirty workspace is a conflict (5), got {code}: {out}{err}"
    );
    assert_eq!(
        std::fs::read(&file).expect("read after the refusal"),
        b"2",
        "cell R5: the refusal left the change in place"
    );

    let (code, out, err) = cli_code(&store, &mount_point, &["restore", "0000000000000000"]);
    println!("R5 reading: unknown -> code={code} stderr={:?}", err.trim());
    assert_eq!(
        code, 3,
        "cell R5: an unknown commit is not found (3), got {code}: {out}{err}"
    );

    let (code, out, err) = cli_code(&store, &mount_point, &["restore", &c1, "--discard"]);
    println!("R5 reading: discard -> code={code}");
    assert_eq!(
        code, 0,
        "cell R5: with --discard it succeeds, got {code}: {out}{err}"
    );
    assert_eq!(
        std::fs::read(&file).expect("read after the discard restore"),
        b"1",
        "cell R5: and the discarded change is gone"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **R4** — the admission window. The restore is parked **between** closing the gate and moving the view; an open that arrives in that window must wait for the
/// restore and then meet the **new** view.
///
/// The discriminator is the name: `/g` exists only in `c2`. Without an admission ticket the parked open resolves `/g` against the old view and succeeds —
/// handing out a handle to an object the restore is about to remove. With the ticket it waits, and then finds that `/g` is gone.
#[test]
fn an_open_during_a_parked_restore_waits_and_then_finds_the_new_view() {
    common::strict_gate();
    let store = temp_dir("window");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let mount_point = PathBuf::from("H:");
    assert_eq!(
        fs_winfsp::mounted::drive_present(&mount_point),
        Some(false),
        "cell precondition: H: must be free"
    );
    // The pause is set on the **mount's** environment: the restore runs in the mount process, so that is where the point has to fire.
    let pause = std::env::var("LAYER_FS_CLI_MNT_R4_PAUSE").unwrap_or_else(|_| "3000".to_owned());
    let child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&mount_point)
            .arg("--workspace")
            .arg("main")
            .env(
                "WCAS_CLI_PAUSE_AT",
                format!("restore-after-quiesce={pause}"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    );
    let root = PathBuf::from("H:\\");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&root).is_ok(),
        "cell R4: the mount must serve"
    );

    let f = root.join("f");
    let g = root.join("g");
    std::fs::write(&f, b"1").expect("write f=1");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c1"]);
    assert!(ok, "cell R4: commit c1: {out}{err}");
    let c1 = out
        .trim()
        .strip_prefix("commit ")
        .unwrap_or_default()
        .to_owned();
    std::fs::write(&f, b"2").expect("write f=2");
    std::fs::write(&g, b"only in c2").expect("write g");
    let (ok, out, err) = cli(&store, &mount_point, &["commit", "-m", "c2"]);
    assert!(ok, "cell R4: commit c2: {out}{err}");

    // Start the restore and let it reach the pause; then open `/g` from another thread.
    let restore = {
        let store = store.clone();
        let mount_point = mount_point.clone();
        let c1 = c1.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let (ok, out, err) = cli(&store, &mount_point, &["restore", &c1]);
            (ok, out, err, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(700));
    let opener = std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = std::fs::read_to_string(&g);
        (outcome, started.elapsed())
    });

    let (restore_ok, out, err, restore_waited) = restore.join().expect("the restore finishes");
    let (outcome, open_waited) = opener.join().expect("the opener finishes");
    println!(
        "R4 reading: restore ok={restore_ok} waited={restore_waited:?} ; open waited={open_waited:?} outcome={outcome:?}"
    );
    assert!(restore_ok, "cell R4: the restore must succeed: {out}{err}");
    assert!(
        outcome.is_err(),
        "cell R4: `/g` exists only in c2 — an open that met the new view must not find it, got {outcome:?}"
    );
    // **It waited**: the open was parked while the restore was parked, and the restore itself had to wait out the pause.
    assert!(
        open_waited >= Duration::from_millis(1000),
        "cell R4: the open must wait for the restore to finish, waited only {open_waited:?}"
    );

    drop(child);
    let _ = std::fs::remove_dir_all(&store);
}
