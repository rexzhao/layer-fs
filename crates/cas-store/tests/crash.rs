//! Crash injection: a child process is killed at a named publication step, then
//! the Store is reopened and its state is checked (ADR-002 §5, spec §4-8).
//!
//! The child is a real process and it is really killed: the parent waits until
//! the child reports the step it reached on stdout and then terminates it, so
//! there is no "simulated failure" flag that could drift from the code path it
//! claims to model.
//!
//! The child binary is this test's own executable: `crash_child` runs when
//! `CAS_STORE_CRASH_CHILD` is set, and exits before any test runs.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use cas_core::{Object, ObjectId, Role};
use cas_store::{CorruptCause, OpenMode, PutOutcome, Store};

/// The child's entry point. Runs before every test in this binary.
#[test]
fn crash_child() {
    let Ok(specification) = std::env::var("CAS_STORE_CRASH_CHILD") else {
        return;
    };
    let mut parts = specification.split('|');
    let root = PathBuf::from(parts.next().expect("root"));
    let step = parts.next().expect("step").to_owned();

    let store = cas_store::testing::open_for_child(&root).expect("child open");
    cas_store::testing::put_with_crash_point(&store, Role::Bytes, b"crash payload", &step);
}

/// A directory that removes itself when dropped.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cas-store-crash-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const VALUE: &[u8] = b"crash payload";

fn oid() -> ObjectId {
    Object::new(Role::Bytes, VALUE)
        .expect("framed")
        .id()
        .expect("hashed")
}

fn count_files(directory: &Path) -> usize {
    fn walk(directory: &Path, found: &mut usize) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                *found += 1;
            }
        }
    }
    let mut found = 0;
    walk(directory, &mut found);
    found
}

/// Starts the child, waits until it reaches `step`, then confirms it died.
///
/// The step is announced on the child's **stderr**: `abort` runs no
/// destructors, so a buffered stdout line would never be flushed. The child is
/// a separate process that really dies at the point it names; the parent reaps
/// it here and checks the exit status.
fn kill_child_at(root: &Path, step: &str) {
    // The step names map to the `CrashPoint` debug spelling the child prints.
    let announced = match step {
        "staged" => "STEP Staged",
        "synced" => "STEP Synced",
        "renamed" => "STEP Renamed",
        "before-commit" => "STEP BeforeCommit",
        other => panic!("unknown step {other}"),
    };

    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("crash_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(
            "CAS_STORE_CRASH_CHILD",
            format!("{}|{step}", root.display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child");

    let stderr = child.stderr.take().expect("child stderr");
    let reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut reached = false;
    for line in reader.lines() {
        let line = line.expect("child line");
        if line.trim() == announced {
            reached = true;
            break;
        }
        assert!(Instant::now() < deadline, "child did not reach {step}");
    }
    assert!(reached, "child never reported {announced}");
    // The child is about to abort on its own; reap it and confirm it died
    // abnormally rather than completing the put.
    let status = child.wait().expect("wait for child");
    assert!(
        !status.success(),
        "the child exited normally at {step}; the crash did not happen"
    );
}

fn assert_recovered(root: &Path, expect_published: bool, step: &str) {
    let store = Store::open(root, OpenMode::ReadWrite).expect("reopen after crash");
    assert!(store.opened().temporaries_removed <= 1);
    let id = oid();
    assert_eq!(
        store.exists(id).expect("exists"),
        expect_published,
        "publication state after crashing at {step}"
    );
    if expect_published {
        let frame = store.get(id).expect("get after crash");
        let object = Object::decode(&frame).expect("decode after crash");
        assert_eq!(object.value(), VALUE);
        assert_eq!(store.object_count().expect("count"), 1);
    }
    // Whatever happened, the reopen must leave tmp/ empty and the Store clean.
    assert_eq!(
        std::fs::read_dir(root.join("tmp")).expect("tmp").count(),
        0,
        "tmp/ is cleared on reopen"
    );
    assert!(store.verify().expect("verify").is_clean());
}

#[test]
fn crash_while_the_object_is_staged_leaves_it_unpublished() {
    let root = TempRoot::new("staged");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "staged");
    assert_recovered(root.path(), false, "staged");
}

#[test]
fn crash_after_the_staged_file_is_synced_leaves_it_unpublished() {
    let root = TempRoot::new("synced");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "synced");
    assert_recovered(root.path(), false, "synced");
}

#[test]
fn crash_after_the_rename_leaves_an_invisible_orphan() {
    let root = TempRoot::new("renamed");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "renamed");
    // The file is on disk but not published.
    assert_eq!(count_files(&root.path().join("objects")), 1);
    assert_recovered(root.path(), false, "renamed");

    // A later put adopts it and publishes it without rewriting.
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let outcome = store.put(Role::Bytes, VALUE).expect("adopting put");
    assert_eq!(outcome.outcome, PutOutcome::AdoptedOrphan);
    assert_eq!(store.object_count().expect("count"), 1);
    Object::decode(&store.get(oid()).expect("get")).expect("decode");
}

#[test]
fn crash_before_the_index_commit_leaves_it_unpublished() {
    let root = TempRoot::new("precommit");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "before-commit");
    assert_eq!(count_files(&root.path().join("objects")), 1);
    assert_recovered(root.path(), false, "before-commit");
}

#[test]
fn completing_the_put_after_a_crash_publishes_exactly_one_copy() {
    let root = TempRoot::new("recover");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "renamed");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    store.put(Role::Bytes, VALUE).expect("put");
    store.put(Role::Bytes, VALUE).expect("repeat put");
    assert_eq!(store.object_count().expect("count"), 1);
    assert_eq!(count_files(&root.path().join("objects")), 1);
    assert!(store.verify().expect("verify").is_clean());
}

#[test]
fn a_crash_between_two_puts_keeps_the_first() {
    let root = TempRoot::new("two-puts");
    let first = {
        let store = Store::open(root.path(), OpenMode::Create).expect("create");
        store.put(Role::Bytes, b"survivor").expect("first put").oid
    };
    kill_child_at(root.path(), "renamed");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    assert!(store.exists(first).expect("exists"));
    let broken = store.verify().expect("verify");
    assert!(broken.is_clean(), "an unpublished orphan is not a failure");
    assert!(matches!(
        store.get(oid()),
        Err(cas_store::StoreError::NotFound { .. })
    ));
}

#[test]
fn a_corrupt_orphan_is_reported_rather_than_adopted() {
    let root = TempRoot::new("corrupt-orphan");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let id = oid();
    let path = store.layout().object_path(id);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(&path, b"truncated").expect("write");
    match store.put(Role::Bytes, VALUE) {
        Err(cas_store::StoreError::Corrupt { cause, .. }) => {
            assert!(matches!(cause, CorruptCause::LengthMismatch { .. }));
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}
