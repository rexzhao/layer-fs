//! A11 / A12 / A14 / A15s — the control channel as a **client** sees it, against a real mount.
//!
//! Every case talks to the mount over the pipe its record names, which is what `umount` and `status` do. Using the client rather than calling the server's
//! functions means the criteria cover the wire format *and* the record's `pipe` field together.

use std::io::{Read, Write};
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
fn start_mount(label: &str, mount_point: &Path) -> MountChild {
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
    let root = root_of(mount_point);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&root).is_ok(),
        "cell setup: the mount at {} must serve before a case asks it anything",
        mount_point.display()
    );
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

/// **A15s**: the old stop **file** no longer stops a mount — the control channel is the only signal.
#[test]
fn the_old_stop_file_does_not_stop_a_mount() {
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
