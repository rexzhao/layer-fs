//! **A17** — a file handle keeps its identity **across a commit**.
//!
//! The mount adopts a new base when it publishes: the tree the view is composed over changes under an open handle. If that reset the handle's identity, every
//! open file would silently become a different object — the kind of defect a user reports as "my editor overwrote someone else's file".
//!
//! Three claims, each about a different half of "identity":
//!
//! 1. reading through the **same** handle after the commit still returns the bytes it wrote before it;
//! 2. the handle's `FileIndex` (what the OS calls the identity) is **unchanged**;
//! 3. writing through it again works, makes the workspace dirty, and a **second** commit publishes.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fs_cli::pipe::{Request, Response, PROTOCOL};

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
    let path = std::env::temp_dir().join(format!("fs-cli-a17-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

fn record_of(mount_point: &Path) -> String {
    let path = fs_cli::mounts_dir().join(format!("{}.mount", fs_cli::record_name(mount_point)));
    std::fs::read_to_string(path).unwrap_or_default()
}

fn pipe_of(mount_point: &Path) -> String {
    record_of(mount_point)
        .lines()
        .find_map(|line| line.strip_prefix("pipe=").map(str::to_string))
        .unwrap_or_default()
}

fn call(mount_point: &Path, op: &str, args: serde_json::Value) -> Response {
    let request = Request {
        v: PROTOCOL,
        op: op.to_owned(),
        args,
    };
    fs_cli::pipe_win::call(&pipe_of(mount_point), &request, 2_000).expect("the mount answers")
}

#[cfg(windows)]
fn file_index(handle: &std::fs::File) -> u64 {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    // `Metadata::file_index` is still unstable, so the index is read the way the OS hands it out: `BY_HANDLE_FILE_INFORMATION`. The high half is included because
    // a 64-bit index with only its low half compared would let two volumes' worth of files look identical.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the duration of the call, and `info` is a valid out-structure.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as _, &mut info) };
    assert_ne!(ok, 0, "cell A17: GetFileInformationByHandle must answer");
    (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
}

#[test]
fn a_handle_keeps_its_identity_across_a_commit() {
    let mount_point = std::env::var_os("LAYER_FS_CLI_MNT_A17")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("I:"));
    let store = temp_dir("store");
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
    while started.elapsed() < Duration::from_secs(10) && std::fs::metadata(&root).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::metadata(&root).is_ok(),
        "cell setup: the mount must serve"
    );

    // 1. An open handle with content in it.
    let path = root.join("held.txt");
    {
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(b"a").expect("write a");
        file.flush().expect("flush");
    }
    let mut handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open the handle that will outlive the commit");
    let index_before = file_index(&handle);

    // 2. A commit happens **while the handle is open**.
    match call(
        &mount_point,
        "commit",
        serde_json::json!({"message": "with an open handle"}),
    ) {
        Response::Failed { kind, detail, .. } if kind == "committed" => {
            assert!(!detail.is_empty(), "cell A17: the commit is named");
        }
        other => panic!("cell A17: the commit must publish, got {other:?}"),
    }

    // 3. The same handle still reads the same bytes, and the OS still calls it the same object.
    handle.seek(SeekFrom::Start(0)).expect("seek");
    let mut bytes = Vec::new();
    handle.read_to_end(&mut bytes).expect("read after commit");
    assert_eq!(bytes, b"a", "cell A17: the handle still reads its bytes");
    assert_eq!(
        file_index(&handle),
        index_before,
        "cell A17: the handle's FileIndex must not change across a commit"
    );

    // 4. Writing through the same handle still works, and a **second** commit publishes it.
    handle.seek(SeekFrom::Start(0)).expect("seek");
    handle.write_all(b"b").expect("write after commit");
    handle.flush().expect("flush");
    match call(&mount_point, "status", serde_json::json!({})) {
        Response::Status { dirty, .. } => {
            assert!(dirty, "cell A17: the second write makes it dirty")
        }
        other => panic!("cell A17: expected status, got {other:?}"),
    }
    match call(
        &mount_point,
        "commit",
        serde_json::json!({"message": "second"}),
    ) {
        Response::Failed { kind, .. } if kind == "committed" => {}
        other => panic!("cell A17: the second commit must publish, got {other:?}"),
    }

    drop(handle);
    let _ = std::fs::remove_dir_all(&store);
}
