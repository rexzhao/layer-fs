//! **I4** — the crash contract (ADR-006 §8, §9.4).
//!
//! > 崩溃后打开：状态 = 操作序列的一个前缀 ⊇ 最后一次成功 flush 前的全部操作（W4）
//!
//! The test kills a real child process, so the guarantees under test are the ones the
//! filesystem and SQLite actually provide, not a simulated flag: `Child::kill` on
//! Windows terminates the process without running destructors, exactly like a crash
//! (no `Drop`, no checkpoint, no clean close).
//!
//! Two seams are exercised, matching the two halves of W4:
//!
//! - **after a successful `flush`** — everything that flush covered must be there;
//! - **before any `flush`** — whatever survived must be a *prefix*: the flushed set is
//!   still guaranteed, and the unflushed work may be present or absent but must never be
//!   partially applied in a way that breaks reopening.
//!
//! The child is a `#[test]` in this same binary, spawned with `--exact`; it does the
//! work, drops a marker file, and then blocks until the parent kills it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use cas_core::{ObjectId, Role};
use cas_store::{OpenMode, Store};
use fs_namespace::{Path as FsPath, Rules};
use fs_overlay::{OverlayOptions, OverlayStore};
use fs_snapshot::{
    encode_inode_table, encode_root, Content, InodeRecord, Kind, PageSink, Profile, RootValue,
    SnapshotResult,
};

const CHILD_ENV: &str = "WCAS_OVERLAY_CRASH_CHILD";
const READY_MARKER: &str = "child-ready";

/// A temp directory that cleans itself up.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "fs-overlay-crash-{label}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct StoreSink<'a> {
    store: &'a Store,
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        Ok(self.store.put(role, value).expect("put").oid)
    }
}

/// Creates the base snapshot (an empty root directory) and returns its root id.
fn create_base(dir: &Path) -> ObjectId {
    let store = Store::open(dir.join("base"), OpenMode::Create).expect("open store");
    let profile = Profile::V1;
    let mut sink = StoreSink { store: &store };
    let records = vec![InodeRecord {
        serial: 0,
        kind: Kind::Directory,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Empty,
    }];
    let table = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("table");
    encode_root(
        &profile,
        &RootValue {
            scope_id: [1u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("root")
}

fn open_overlay(dir: &Path, base_root: ObjectId) -> OverlayStore {
    let store = Store::open(dir.join("base"), OpenMode::ReadWrite).expect("reopen store");
    OverlayStore::open(
        dir.join("overlay.sqlite"),
        store,
        &OverlayOptions {
            base_root,
            profile: Profile::V1,
            scope_id: [1u8; 32],
            serial_first: 1,
            serial_end: 1 << 20,
        },
    )
    .expect("open overlay")
}

fn p(text: &str) -> FsPath {
    FsPath::parse(text).expect("path")
}

/// The child: writes a flushed file, then an unflushed one, then blocks.
///
/// `phase=flush` writes `/flushed`, flushes, writes `/unflushed`, marks ready, blocks.
/// `phase=noflush` writes `/unflushed` only, marks ready, blocks.
///
/// Skipped unless the driver spawned it with the marker set.
#[test]
#[ignore = "spawned as a child process by the I4 driver below"]
fn crash_child() {
    let Some(spec) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let spec = spec.to_string_lossy().into_owned();
    let mut parts = spec.split('|');
    let dir = PathBuf::from(parts.next().expect("dir"));
    let base_root = parts
        .next()
        .expect("base root")
        .parse::<ObjectId>()
        .expect("parse base root");
    let phase = parts.next().expect("phase");

    let overlay = open_overlay(&dir, base_root);
    let rules = Rules::new(overlay);
    if phase == "flush" {
        rules.backend().create_file(&p("/flushed")).expect("create");
        rules
            .backend()
            .write_at(&p("/flushed"), 0, b"durable")
            .expect("write");
        rules.backend().flush().expect("flush");
    }
    rules
        .backend()
        .create_file(&p("/unflushed"))
        .expect("create");
    rules
        .backend()
        .write_at(&p("/unflushed"), 0, b"volatile")
        .expect("write");

    std::fs::write(dir.join(READY_MARKER), b"ready").expect("marker");
    // Block until the parent kills us: a clean exit would checkpoint the WAL and stop
    // being a crash.
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spawns the child, waits for its marker, kills it, and returns the work directory.
fn run_until_killed(phase: &str) -> TempRoot {
    let root = TempRoot::new(phase);
    let base_root = create_base(&root.path);
    let exe = std::env::current_exe().expect("current exe");
    let spec = format!("{}|{}|{}", root.path.display(), base_root, phase);

    let mut child = std::process::Command::new(exe)
        .args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env(CHILD_ENV, spec)
        .spawn()
        .expect("spawn child");

    let marker = root.path.join(READY_MARKER);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never signalled readiness");
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("child exited early with {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The kill is the crash: no destructors, no checkpoint, no clean close.
    child.kill().expect("kill child");
    let _ = child.wait();
    root
}

/// **I4 after a flush** — everything the flush covered is there after the crash.
#[test]
fn i4_a_kill_after_flush_keeps_everything_flushed() {
    let root = run_until_killed("flush");
    // The child recorded the base root it opened with; reading it back from the overlay
    // is also a check that the meta table survived the crash.
    let base_root = read_base_root(&root.path);
    let overlay = open_overlay(&root.path, base_root);
    let rules = Rules::new(overlay);

    let entries = rules.read_dir(&p("/")).expect("relist");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(
        names.contains(&"flushed"),
        "a flushed change must survive a crash, got {names:?}"
    );
    assert_eq!(
        rules.backend().read_at(&p("/flushed"), 0, 7).expect("read"),
        b"durable"
    );
    // The unflushed change may or may not be there — W4 only promises a prefix — but it
    // must never be partially applied: if the name exists, its bytes are complete.
    if names.contains(&"unflushed") {
        assert_eq!(
            rules
                .backend()
                .read_at(&p("/unflushed"), 0, 8)
                .expect("read"),
            b"volatile"
        );
    }
}

/// **I4 without a flush** — the recovered state is a prefix, and the file reopens.
#[test]
fn i4_a_kill_without_flush_reopens_to_a_prefix() {
    let root = run_until_killed("noflush");
    let base_root = read_base_root(&root.path);
    let overlay = open_overlay(&root.path, base_root);
    let rules = Rules::new(overlay);

    // The only hard requirement without a flush: the overlay reopens and its view is
    // self-consistent. Either the unflushed file survived whole, or it did not appear.
    let entries = rules.read_dir(&p("/")).expect("relist");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    if names.contains(&"unflushed") {
        assert_eq!(
            rules
                .backend()
                .read_at(&p("/unflushed"), 0, 8)
                .expect("read"),
            b"volatile",
            "a surviving change must be whole, never half applied"
        );
    } else {
        assert!(
            !names.contains(&"flushed"),
            "nothing was flushed, so nothing may be guaranteed"
        );
    }
}

/// Reads `base_root` out of the overlay's meta table without opening the overlay.
fn read_base_root(dir: &Path) -> ObjectId {
    let connection = rusqlite::Connection::open_with_flags(
        dir.join("overlay.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open overlay for meta");
    let text: String = connection
        .query_row(
            "SELECT value FROM ov_meta WHERE key = 'base_root'",
            [],
            |row| row.get(0),
        )
        .expect("base_root");
    text.parse::<ObjectId>().expect("parse base root")
}
