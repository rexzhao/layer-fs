//! A11 / A12 / A14 / A15s — the control channel as a **client** sees it, against a real mount.
//!
//! Every case talks to the mount over the pipe its record names, which is what `umount` and `status` do. Using the client rather than calling the server's
//! functions means the criteria cover the wire format *and* the record's `pipe` field together.

mod common;

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fs_cli::pipe::{Request, Response, PROTOCOL};

/// A spawned mount that is killed on drop, so a failed test leaves no volume behind and no child holding the runner's pipes open.
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
        "fs-cli-pipe-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

/// The record path, computed by the **library** function the binary uses (never re-derived here).
fn record_path(mount_point: &Path) -> PathBuf {
    fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(mount_point)))
}

fn read_record(mount_point: &Path) -> String {
    std::fs::read_to_string(record_path(mount_point)).unwrap_or_default()
}

fn field(record: &str, key: &str) -> String {
    record
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)?
                .strip_prefix('=')
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn root_of(mount_point: &Path) -> PathBuf {
    if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.to_path_buf()
    }
}

/// Starts a mount in another process and waits until the volume really serves.
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

fn start_mount(label: &str, mount_point: &Path) -> MountChild {
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
    let root = root_of(mount_point);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    // **A visible mount point is not a serving volume**, and a **write** probe is worse than useless here: it leaves the workspace dirty, so a criterion that starts
    // from "a fresh mount is clean" would fail for a reason the fixture created. Readiness is **opening the root as a directory** — Q2's lesson (hold a handle, do
    // not enumerate) — which proves the volume answers and changes nothing.
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000)
            .open(&root)
        {
            Ok(handle) => {
                drop(handle);
                break;
            }
            Err(error) if started.elapsed() < Duration::from_secs(15) => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!(
                "cell setup: the volume at {} never answered after {attempts} attempts: {error:?}",
                mount_point.display()
            ),
        }
    }
    child
}

/// Sends one request over the pipe the record names.
fn ask(mount_point: &Path, op: &str) -> Response {
    let record = read_record(mount_point);
    let pipe = field(&record, "pipe");
    assert!(
        !pipe.is_empty(),
        "cell setup: the record must name its control pipe"
    );
    let request = Request {
        v: PROTOCOL,
        op: op.to_owned(),
        args: serde_json::json!({}),
    };
    fs_cli::pipe_win::call(&pipe, &request, 2_000).expect("the mount must answer")
}

/// A case's own drive letter: two cases may not share one (the structural rule from 434).
fn mount_point(label: &str, default: &str) -> PathBuf {
    std::env::var_os(format!("LAYER_FS_CLI_MNT_{label}"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

/// **A11**: `hello` agrees with the record field by field, and a frame over the limit is refused **without taking the mount down**.
#[test]
fn hello_matches_the_record_and_an_oversized_frame_does_not_kill_the_mount() {
    common::strict_gate();
    let mount_point = mount_point("PIPE_A", "N:");
    let _child = start_mount("hello", &mount_point);
    let record = read_record(&mount_point);

    match ask(&mount_point, "hello") {
        Response::Hello {
            store,
            workspace,
            branch,
            pid,
            mount_point: reported,
            protocol,
        } => {
            assert_eq!(store, field(&record, "store"), "cell A11: store");
            assert_eq!(
                workspace,
                field(&record, "workspace"),
                "cell A11: workspace"
            );
            assert_eq!(branch, field(&record, "branch"), "cell A11: branch");
            assert_eq!(pid.to_string(), field(&record, "pid"), "cell A11: pid");
            assert_eq!(
                reported,
                field(&record, "mount_point"),
                "cell A11: mount point"
            );
            assert_eq!(protocol, PROTOCOL, "cell A11: protocol");
        }
        other => panic!("cell A11: expected hello, got {other:?}"),
    }

    // A raw connection that sends more than the limit: the answer names the limit, and the next client is still served.
    let pipe = field(&record, "pipe");
    let mut connection = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe)
        .expect("open the control pipe directly");
    connection
        .write_all(&vec![b'x'; fs_cli::pipe::FRAME_MAX + 1024])
        .expect("write past the limit");
    connection.flush().expect("flush");
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = connection.read(&mut byte) {
        if byte[0] == b'\n' {
            break;
        }
        answer.push(byte[0]);
    }
    drop(connection);
    let refused: Response = serde_json::from_slice(&answer).expect("a frame comes back");
    assert_eq!(
        refused,
        Response::FrameTooLarge {
            max: fs_cli::pipe::FRAME_MAX
        },
        "cell A11: the refusal names the limit it hit"
    );
    assert!(
        matches!(ask(&mount_point, "hello"), Response::Hello { .. }),
        "cell A11: after a bad client the mount must still answer"
    );
}

/// **A14**: `status.dirty` follows the workspace and `status.handles` follows the mount — asserted as **increments**, because the test process itself may hold
/// handles on the volume.
#[test]
fn status_reports_dirty_and_open_handles() {
    common::strict_gate();
    let mount_point = mount_point("PIPE_B", "M:");
    let _child = start_mount("status", &mount_point);
    let root = root_of(&mount_point);

    let (dirty_before, handles_before) = match ask(&mount_point, "status") {
        Response::Status { dirty, handles, .. } => (dirty, handles),
        other => panic!("cell A14: expected status, got {other:?}"),
    };
    assert!(!dirty_before, "cell A14: a fresh mount is clean");

    std::fs::write(root.join("dirty.txt"), b"x").expect("write through the mount");
    let held = std::fs::File::open(root.join("dirty.txt")).expect("hold a handle");
    let (dirty_during, handles_during) = match ask(&mount_point, "status") {
        Response::Status { dirty, handles, .. } => (dirty, handles),
        other => panic!("cell A14: expected status, got {other:?}"),
    };
    assert!(
        dirty_during,
        "cell A14: a write through the mount makes it dirty"
    );
    assert!(
        handles_during > handles_before,
        "cell A14: an open file must show up (before {handles_before}, during {handles_during})"
    );
    drop(held);
}

/// **A12**: a remote `unmount` with a file open is refused and changes nothing; after closing, it succeeds **and the record is gone when the answer arrives**.
#[test]
fn unmount_refuses_while_a_handle_is_open() {
    common::strict_gate();
    let mount_point = mount_point("PIPE_C", "L:");
    let _child = start_mount("unmount", &mount_point);
    let root = root_of(&mount_point);
    std::fs::write(root.join("held.txt"), b"x").expect("write");
    let held = std::fs::File::open(root.join("held.txt")).expect("hold a handle");

    match ask(&mount_point, "unmount") {
        Response::HandlesOpen { handles } => assert!(handles >= 1, "cell A12: at least one handle"),
        other => panic!("cell A12: with a file open the mount must refuse, got {other:?}"),
    }
    assert!(
        mount_point.exists(),
        "cell A12: a refusal leaves the volume attached"
    );
    assert!(
        record_path(&mount_point).exists(),
        "cell A12: and the record in place"
    );

    drop(held);
    let started = Instant::now();
    loop {
        match ask(&mount_point, "unmount") {
            Response::Unmounted {} => break,
            Response::HandlesOpen { .. } if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => {
                panic!("cell A12: expected Unmounted once the handle is closed, got {other:?}")
            }
        }
    }
    assert!(
        !record_path(&mount_point).exists(),
        "cell A12: the record is removed before the answer is sent"
    );
}

/// **A15s**: the old stop **file** no longer stops a mount — the control channel is the only signal.
#[test]
fn the_old_stop_file_does_not_stop_a_mount() {
    common::strict_gate();
    let mount_point = mount_point("PIPE_D", "K:");
    let _child = start_mount("stops", &mount_point);
    let stop = fs_cli::mounts_dir().join(format!("{}.stop", fs_cli::record_name(&mount_point)));
    std::fs::write(&stop, b"stop\n").expect("write the file the old build listened for");
    std::thread::sleep(Duration::from_millis(300));

    assert!(
        mount_point.exists(),
        "cell A15s: the volume must still be attached"
    );
    assert!(
        matches!(ask(&mount_point, "hello"), Response::Hello { .. }),
        "cell A15s: and the mount must still answer"
    );
    let _ = std::fs::remove_file(&stop);
}

/// **W1's mechanism, tested on its own** — a record rewritten through the held handle is never seen as `Gone` or `Stale` by a reader polling it.
///
/// The switch cannot delete and recreate the record: W1 polls liveness every 10 ms for the whole switch, and a recreate would show up as `Gone`. What a reader
/// *can* see is an empty or half-written file, so the rule is "cannot parse ⇒ retry once", and that is asserted here rather than assumed.
#[test]
fn a_record_rewritten_in_place_never_reads_as_gone() {
    // **Unique per run, not per pid**: Windows recycles pids, and a leftover directory from an earlier run made this criterion start from the previous run's
    // files. The stamp is what makes "already there" impossible rather than unlikely.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let directory = std::env::temp_dir().join(format!(
        "fs-cli-record-rewrite-{}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp");
    let path = directory.join("U.mount");
    let first = "store=C:/repo\nworkspace=dev\nbranch=dev\npid=1\nmount=C:/mnt/u\n";
    let second = "store=C:/repo\nworkspace=feat\nbranch=feat\npid=1\nmount=C:/mnt/u\n";
    let mut record = fs_cli::hold_record_for_test(&path, first).expect("hold the record");

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let seen = std::sync::Arc::clone(&seen);
        let stop = std::sync::Arc::clone(&stop);
        let path = path.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                // The card's rule: cannot parse ⇒ wait 50 ms and read once more, then judge.
                let text = if text.contains("workspace=") {
                    text
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    std::fs::read_to_string(&path).unwrap_or_default()
                };
                let reading = if text.is_empty() {
                    "gone"
                } else if text.contains("workspace=") {
                    "live"
                } else {
                    "unparseable"
                };
                seen.lock().expect("seen").push(match reading {
                    "gone" => "gone",
                    "live" => "live",
                    _ => "unparseable",
                });
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })
    };
    // **20 rounds with a pause between them**, so the poller gets dozens of samples instead of one: a reading of "1 sample, no `gone`" would prove almost nothing
    // about the window this test is about.
    for _ in 0..20 {
        fs_cli::rewrite_record(&mut record, second).expect("rewrite");
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs_cli::rewrite_record(&mut record, first).expect("rewrite back");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    poller.join().expect("the poller finishes");

    let seen = seen.lock().expect("seen");
    let gone = seen.iter().filter(|reading| **reading == "gone").count();
    let unparseable = seen
        .iter()
        .filter(|reading| **reading == "unparseable")
        .count();
    println!(
        "W1-mechanism reading: {} samples, gone={gone}, unparseable-after-retry={unparseable}",
        seen.len()
    );
    assert!(!seen.is_empty(), "the poller must have sampled something");
    assert_eq!(
        gone, 0,
        "cell W1: rewriting in place must never look like the record vanished"
    );
    assert_eq!(
        unparseable, 0,
        "cell W1: the retry must cover the window where the file is truncated"
    );
    let _ = std::fs::remove_dir_all(&directory);
}
