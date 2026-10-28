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

/// **W1** — `checkout <b>` from inside a mount switches it, and the record is never seen as `Gone` while it happens.
#[test]
fn a_checkout_inside_the_mount_switches_it_and_the_record_stays_live() {
    common::strict_gate();
    let (store, _child, root) = mount_at("B:");
    let _ = &root;
    let mount_point = PathBuf::from("B:");
    // A branch the mount is not serving, with one file of its own.
    Command::new(binary())
        .arg("--store")
        .arg(&store)
        .arg("branch")
        .arg("feat")
        .arg("--from")
        .arg("main")
        .output()
        .expect("branch feat");

    let record_path =
        fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&mount_point)));
    // Poll liveness for the whole switch: `Gone` would mean the record was deleted and recreated (W1's mutation), `Stale` that nobody holds it.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readings = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let poller = {
        let stop = std::sync::Arc::clone(&stop);
        let readings = std::sync::Arc::clone(&readings);
        let record_path = record_path.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                // The mount.s own liveness question, asked the way it asks it: open for **write**; a sharing violation (32) means the owner still holds it.
                let reading = match std::fs::OpenOptions::new().write(true).open(&record_path) {
                    Err(error) if error.raw_os_error() == Some(32) => "live",
                    Ok(_) => "stale",
                    Err(_) => "unreadable",
                };
                let exists = record_path.exists();
                readings
                    .lock()
                    .expect("readings")
                    .push(if exists { reading } else { "gone" });
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    let (code, out, err) = cli_code(&store, &mount_point, &["checkout", "feat"]);
    let failed = std::process::Command::new(binary())
        .arg("--store")
        .arg(&store)
        .args(["checkout", "feat"])
        .current_dir(&mount_point)
        .output()
        .expect("checkout from inside the mount");
    let _ = (code, out, err);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    poller.join().expect("the poller finishes");
    let readings = readings.lock().expect("readings");
    let gone = readings
        .iter()
        .filter(|reading| **reading == "gone")
        .count();
    let stale = readings
        .iter()
        .filter(|reading| **reading == "stale")
        .count();
    println!(
        "W1 reading: {} samples, gone={gone}, stale={stale}, checkout exit={:?} stdout={:?} stderr={:?}",
        readings.len(),
        failed.status.code(),
        String::from_utf8_lossy(&failed.stdout).trim(),
        String::from_utf8_lossy(&failed.stderr).trim()
    );
    assert_eq!(gone, 0, "cell W1: the record must never read as gone");
    assert_eq!(stale, 0, "cell W1: the record must stay held by the mount");
    assert!(
        failed.status.success(),
        "cell W1: checkout inside the mount must succeed"
    );
    // The mount now serves `feat`, and the volume is still there. The root path is used as the cwd here: right after a remount `K:` (a bare letter) is briefly not
    // a valid directory to set as a working directory, and that transient is not what this criterion is about.
    let mut attempt = 0;
    let (ok, out, err) = loop {
        // Spawned by hand, not through `cli`: `cli` panics on a failed spawn, and the transient above **is** a failed spawn.
        let answer = match Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("status")
            .current_dir("B:\\")
            .output()
        {
            Ok(output) => (
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ),
            Err(error) => (false, String::new(), error.to_string()),
        };
        attempt += 1;
        if answer.0 || attempt >= 20 {
            break answer;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    println!("W1 status after the switch: {out}");
    assert!(ok, "cell W1: status must answer after the switch: {err}");
    assert!(
        out.contains("branch:    feat") || out.contains("workspace: feat"),
        "cell W1: the mount must serve feat now, got {out:?}"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// Mounts a store at a **directory** mount point (the letter pool is exhausted by the other cases, and a directory is the other supported shape).
fn mount_at_directory(label: &str) -> (PathBuf, MountChild, PathBuf) {
    let store = temp_dir(label);
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let mount_point = store.join("mnt");
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
    // **Ready means the record exists with a pipe**, not that the directory does: a directory mount point is on the host volume and exists before the volume is
    // attached (the A6d lesson), so `metadata` here would let the first command run against a mount that is not there yet.
    let record = fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&mount_point)));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15)
        && !std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe=")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe="),
        "cell setup: the mount must be recorded and listening"
    );
    (store, child, mount_point)
}

/// **W2** — an open **file** handle blocks a switch, the mount keeps serving what it served, and a later attempt succeeds.
#[test]
fn an_open_file_handle_blocks_the_switch() {
    common::strict_gate();
    let (store, _child, root) = mount_at_directory("switch-blocked");
    Command::new(binary())
        .arg("--store")
        .arg(&store)
        .args(["branch", "feat", "--from", "main"])
        .output()
        .expect("branch feat");
    // **A branch is not a workspace** (A3b): the first `checkout` creates the workspace row and does not switch. Doing it here, from outside, is what makes the
    // checkout below the switch rather than a creation.
    let (created, out, err) = cli_code(&store, &store, &["checkout", "feat"]);
    assert_eq!(
        created, 0,
        "cell setup: the feat workspace row must exist: {out}{err}"
    );
    assert!(
        out.contains("on feat"),
        "cell setup: the first checkout creates it, got {out:?}"
    );

    let file = root.join("f");
    std::fs::write(&file, b"main content").expect("write");

    let held = std::fs::File::open(&file).expect("hold the file open");
    let (code, out, err) = cli_code(&store, &root, &["checkout", "feat"]);
    println!("W2 reading: blocked -> code={code} stderr={:?}", err.trim());
    assert_eq!(
        code, 4,
        "cell W2: an open file handle must make the switch busy (4), got {code}: {out}{err}"
    );
    assert!(
        err.contains("1 file"),
        "cell W2: the refusal names the **file** count, got {err:?}"
    );
    drop(held);
    assert_eq!(
        std::fs::read(&file).expect("the mount still reads its file"),
        b"main content",
        "cell W2: the view did not move"
    );

    let (code, out, err) = cli_code(&store, &root, &["checkout", "feat"]);
    println!(
        "W2 reading: after closing -> code={code} stdout={:?}",
        out.trim()
    );
    assert_eq!(
        code, 0,
        "cell W2: with no handle held the switch must succeed: {out}{err}"
    );
    // **Bounded retry, not a retry of the assertion**: right after a remount the mount point is briefly unusable as a working directory (267), which is not what
    // this criterion is about. The assertion stays zero-tolerance once the answer arrives.
    let mut attempt = 0;
    let (code, out, _) = loop {
        attempt += 1;
        match Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("status")
            .current_dir(&root)
            .output()
        {
            Ok(output) => {
                break (
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )
            }
            Err(_) if attempt < 20 => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => break (1, String::new(), error.to_string()),
        }
    };
    println!(
        "W2 reading: status after the switch -> code={code} stdout={:?}",
        out.trim()
    );
    assert_eq!(code, 0, "cell W2: status must answer");
    assert!(
        out.contains("workspace: feat") || out.contains("branch:    feat"),
        "cell W2: the mount must serve feat now, got {out:?}"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **W3** — a switch to a workspace another process holds is **busy** (4), the mount keeps serving what it served, and the control channel **recovers** as soon as
/// the attempt is over.
///
/// The control channel is **serial on purpose** (ruling 丙): one request at a time, so during the attempt `hello` is unanswered. That is not a defect to explain
/// away — the mount really is in transition, and answering "alive" would be the lie. What must hold is the **recovery**, and it is measured here with a bound
/// rather than assumed.
#[test]
fn a_switch_to_a_held_workspace_is_busy_and_the_channel_recovers() {
    common::strict_gate();
    let (store, _child, root) = mount_at_directory("switch-held");
    Command::new(binary())
        .arg("--store")
        .arg(&store)
        .args(["branch", "feat", "--from", "main"])
        .output()
        .expect("branch feat");
    let (created, out, err) = cli_code(&store, &store, &["checkout", "feat"]);
    assert_eq!(
        created, 0,
        "cell setup: the feat workspace row must exist: {out}{err}"
    );

    // Hold `feat` from another process: a second mount of the same workspace, at a mount point of its own.
    let other_point = store.join("other");
    let mut holder = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&other_point)
            .arg("--workspace")
            .arg("feat")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the holder mount"),
    );
    let held_record =
        fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&other_point)));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15)
        && !std::fs::read_to_string(&held_record)
            .unwrap_or_default()
            .contains("pipe=")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::read_to_string(&held_record)
            .unwrap_or_default()
            .contains("pipe="),
        "cell setup: the holder mount must be serving feat"
    );
    let pipe = std::fs::read_to_string(
        fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&root))),
    )
    .unwrap_or_default()
    .lines()
    .find_map(|line| line.strip_prefix("pipe=").map(str::to_string))
    .unwrap_or_default();
    assert!(
        !pipe.is_empty(),
        "cell setup: the mount must have a control pipe"
    );

    let (code, out, err) = cli_code(&store, &root, &["checkout", "feat"]);
    let ask = |pipe: &str| {
        let request = fs_cli::pipe::Request {
            v: fs_cli::pipe::PROTOCOL,
            op: "hello".to_owned(),
            args: serde_json::json!({}),
        };
        fs_cli::pipe_win::call(pipe, &request, 2_000).is_ok()
    };
    // **Recovery, measured**: from the moment the command returns, how long until the channel answers again.
    let recovered_at = Instant::now();
    let mut recovered = ask(&pipe);
    let deadline = recovered_at + Duration::from_millis(2_000);
    while !recovered && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        recovered = ask(&pipe);
    }
    let recovery = recovered_at.elapsed();
    println!(
        "W3 reading: code={code} recovered={recovered} after={recovery:?} stdout={:?} stderr={:?}",
        out.trim(),
        err.trim()
    );
    assert_eq!(
        code, 4,
        "cell W3: a workspace another process holds is busy (4), got {code}: {out}{err}"
    );
    assert!(recovered, "cell W3: the control channel must answer again");
    assert!(
        recovery <= Duration::from_millis(100),
        "cell W3: the channel must recover within 100 ms of the attempt ending, took {recovery:?}"
    );
    // Spawned by hand with a bounded retry: a directory mount point can be briefly unusable as a working directory (267), which is not this criterion's subject.
    let mut attempt = 0;
    let (code, out, _) = loop {
        attempt += 1;
        match Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("status")
            .current_dir(&root)
            .output()
        {
            Ok(output) => {
                break (
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                )
            }
            Err(_) if attempt < 20 => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => break (1, String::new(), error.to_string()),
        }
    };
    assert_eq!(code, 0, "cell W3: status must answer");
    assert!(
        out.contains("workspace: main"),
        "cell W3: the mount still serves main, got {out:?}"
    );
    let _ = holder.0.kill();
    let _ = holder.0.wait();
    let _ = std::fs::remove_dir_all(&store);
}

/// **W5** — while a switch is between "closed the gate" and "unmounted", an open **fails immediately** instead of waiting.
///
/// Waiting there would deadlock: the switch is about to unmount, and the unmount waits for the dispatcher, which is running the very callback that would be
/// waiting. The mount is parked at that point with `WCAS_CLI_PAUSE_AT=switch-after-quiesce=…`.
#[test]
fn an_open_during_a_closing_switch_fails_instead_of_waiting() {
    common::strict_gate();
    let store = temp_dir("switch-closing");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    Command::new(binary())
        .arg("--store")
        .arg(&store)
        .args(["branch", "feat", "--from", "main"])
        .output()
        .expect("branch feat");
    let (created, out, err) = cli_code(&store, &store, &["checkout", "feat"]);
    assert_eq!(
        created, 0,
        "cell setup: the feat workspace row must exist: {out}{err}"
    );

    let mount_point = store.join("mnt");
    let mut child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&mount_point)
            .arg("--workspace")
            .arg("main")
            .env("WCAS_CLI_PAUSE_AT", "switch-after-quiesce=3000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    );
    let record = fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&mount_point)));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15)
        && !std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe=")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe="),
        "cell setup: the mount must be serving"
    );
    let file = mount_point.join("f.txt");
    // **The record appears before the volume is attached**, so readiness here is "a write works", retried with a bound (the A6d lesson, one step further).
    let mut attempt = 0;
    loop {
        attempt += 1;
        match std::fs::write(&file, b"content") {
            Ok(()) => break,
            Err(error) if attempt < 50 => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("write through the mount: {error:?}"),
        }
    }

    // Ask for the switch, then open while it is parked inside the gate.
    let checkout = {
        let store = store.clone();
        let mount_point = mount_point.clone();
        std::thread::spawn(move || cli_code(&store, &mount_point, &["checkout", "feat"]))
    };
    std::thread::sleep(Duration::from_millis(700));
    let opened_at = Instant::now();
    let outcome = std::fs::File::open(&file);
    let waited = opened_at.elapsed();
    let (code, out, err) = checkout.join().expect("the checkout finishes");
    println!(
        "W5 reading: open during closing -> {outcome:?} after {waited:?}; checkout code={code} stdout={:?} stderr={:?}",
        out.trim(),
        err.trim()
    );
    match outcome {
        Err(error) => {
            let raw = error.raw_os_error();
            println!("W5 reading: raw_os_error={raw:?} kind={:?}", error.kind());
            // **The measured value, pinned** (card §5.3: "按实测值写入判据"): 21 is `ERROR_NOT_READY`, which is what Win32 makes of the
            // `STATUS_VOLUME_DISMOUNTED` this frontend answers while the volume is going away.
            assert_eq!(
                raw,
                Some(21),
                "cell W5: the refusal must be the measured one (ERROR_NOT_READY), got {raw:?}"
            );
            assert!(
                waited < Duration::from_secs(1),
                "cell W5: the open must fail immediately, took {waited:?}"
            );
        }
        Ok(_) => panic!("cell W5: an open must not succeed while the volume is being replaced"),
    }
    assert_eq!(
        code, 0,
        "cell W5: the switch itself must still succeed: {out}{err}"
    );
    let _ = child.0.kill();
    let _ = child.0.wait();
    let _ = std::fs::remove_dir_all(&store);
}

/// **W6** — when the new mount cannot come up, the switch **rolls back**: exit 1, the old workspace is serving again, and its content is still there.
///
/// The failure is injected (`WCAS_CLI_FAIL_AT=switch-mount-new`), because there is no honest way to make WinFsp refuse one mount and accept the next.
#[test]
fn a_failed_remount_rolls_back_and_says_so() {
    common::strict_gate();
    let store = temp_dir("switch-rollback");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    Command::new(binary())
        .arg("--store")
        .arg(&store)
        .args(["branch", "feat", "--from", "main"])
        .output()
        .expect("branch feat");
    let (created, out, err) = cli_code(&store, &store, &["checkout", "feat"]);
    assert_eq!(
        created, 0,
        "cell setup: the feat workspace row must exist: {out}{err}"
    );

    let mount_point = store.join("mnt");
    let mut child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("mount")
            .arg(&mount_point)
            .arg("--workspace")
            .arg("main")
            .env("WCAS_CLI_FAIL_AT", "switch-mount-new")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    );
    let record = fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(&mount_point)));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15)
        && !std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe=")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let file = mount_point.join("f.txt");
    let mut attempt = 0;
    loop {
        attempt += 1;
        match std::fs::write(&file, b"still here") {
            Ok(()) => break,
            Err(_) if attempt < 50 => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => panic!("cell setup: write through the mount failed: {error:?}"),
        }
    }

    let (code, out, err) = cli_code(&store, &mount_point, &["checkout", "feat"]);
    println!(
        "W6 reading: code={code} stdout={:?} stderr={:?}",
        out.trim(),
        err.trim()
    );
    assert_eq!(
        code, 1,
        "cell W6: a failed remount is not a success, got {code}: {out}{err}"
    );
    assert!(
        err.contains("remount") || err.contains("mount"),
        "cell W6: the refusal says what failed, got {err:?}"
    );

    // **Rolled back**: the mount is there again and still serves `main` with its content.
    // The rollback mount is the loop's next iteration, so it is not instant: poll with a bound. A rollback that never comes back is a failure, and the retry must not
    // hide it — the message at the end says how long it waited.
    let wait_started = Instant::now();
    let mut last_error = String::new();
    let status = loop {
        let attempt = std::process::Command::new(binary())
            .arg("--store")
            .arg(&store)
            .arg("status")
            .current_dir(&mount_point)
            .output();
        if let Ok(output) = &attempt {
            if output.status.success() {
                break Ok(output.stdout.clone());
            }
            last_error = String::from_utf8_lossy(&output.stderr).into_owned();
        } else if let Err(error) = &attempt {
            last_error = error.to_string();
        }
        if wait_started.elapsed() > Duration::from_secs(15) {
            break Err(last_error);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let status = status.map(|stdout| String::from_utf8_lossy(&stdout).into_owned());
    match status {
        Ok(text) => {
            println!("W6 reading: status after the rollback -> {text}");
            assert!(
                text.contains("workspace: main"),
                "cell W6: the rollback must serve main again, got {text:?}"
            );
        }
        Err(error) => panic!("cell W6: the mount must be serving again: {error:?}"),
    }
    assert_eq!(
        std::fs::read(&file).expect("the content survived the rollback"),
        b"still here",
        "cell W6: what was written before the switch is still there"
    );
    let _ = child.0.kill();
    let _ = child.0.wait();
    let _ = std::fs::remove_dir_all(&store);
}
