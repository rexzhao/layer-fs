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
use std::process::{Child, Command, Stdio};
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

    if step == "hold-write-lock" {
        hold_write_lock(&root);
    }

    let store = cas_store::testing::open_for_child(&root).expect("child open");
    cas_store::testing::put_with_crash_point(&store, Role::Bytes, b"crash payload", &step);
}

/// Opens the Store's database, takes the write lock and keeps it until killed.
///
/// Used to construct a *deterministic* cross-process `Busy`: the parent only
/// proceeds once this process announces that the transaction is open.
fn hold_write_lock(root: &Path) {
    let connection = rusqlite::Connection::open(root.join("store.sqlite")).expect("child db");
    // `BEGIN IMMEDIATE` takes the write lock now rather than at first write.
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("child write lock");
    eprintln!("STEP WriteLockHeld");
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
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
        "synced" => "STEP StagedSynced",
        "renamed" => "STEP Renamed",
        "dirsynced" => "STEP DirectorySynced",
        "committed" => "STEP Committed",
        other => panic!("unknown step {other}"),
    };

    let mut child = spawn_child(root, step);
    wait_for_signal(&mut child, announced, step);
    // The child is about to abort on its own; reap it and confirm it died
    // abnormally rather than completing the put.
    let status = child.wait().expect("wait for child");
    assert!(
        !status.success(),
        "the child exited normally at {step}; the crash did not happen"
    );
}

/// Starts a child running the `crash_child` test with a `root|step` spec.
fn spawn_child(root: &Path, step: &str) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
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
        .expect("spawn child")
}

/// Waits until the child prints `signal`, or panics.
fn wait_for_signal(child: &mut Child, signal: &str, step: &str) {
    let stderr = child.stderr.take().expect("child stderr");
    let reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(60);
    for line in reader.lines() {
        let line = line.expect("child line");
        if line.trim() == signal {
            return;
        }
        assert!(Instant::now() < deadline, "child did not reach {step}");
    }
    panic!("child never reported {signal} for {step}");
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
fn crash_after_the_rename_leaves_an_unreferenced_pack() {
    let root = TempRoot::new("renamed");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "renamed");
    // ADR-008 §5.1 row 2: the pack is in packs/ but no index row points at it, so nothing
    // is visible and an ordinary open leaves the file alone (it is GC's business).
    assert_eq!(count_files(&root.path().join("packs")), 1);
    assert_recovered(root.path(), false, "renamed");

    // Publishing the object again writes a *second* pack rather than adopting the first:
    // adoption was the loose-object shape, and a pack is an immutable container.
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let outcome = store.put(Role::Bytes, VALUE).expect("second put");
    assert_eq!(outcome.outcome, PutOutcome::Inserted);
    assert_eq!(store.object_count().expect("count"), 1);
    Object::decode(&store.get(oid()).expect("get")).expect("decode");
    assert_eq!(
        count_files(&root.path().join("packs")),
        2,
        "the unreferenced pack stays until GC removes it"
    );
}

#[test]
fn crash_before_the_index_commit_leaves_it_unpublished() {
    // `dirsynced` is the last step before the index row is committed: the file
    // is fully durable in objects/ and nothing points at it yet. This is the
    // "before index commit" fault point of spec §4-8, as distinct from `renamed`
    // which fires while the directory entry may still be unflushed.
    let root = TempRoot::new("precommit");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "dirsynced");
    assert_eq!(count_files(&root.path().join("packs")), 1);
    assert_recovered(root.path(), false, "dirsynced");

    // The last step before the transaction: the pack is durable and unreferenced, and the
    // next put publishes the object into a new pack (the old one is GC's to reclaim).
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let outcome = store.put(Role::Bytes, VALUE).expect("second put");
    assert_eq!(outcome.outcome, PutOutcome::Inserted);
    assert_eq!(store.object_count().expect("count"), 1);
}

#[test]
fn crash_after_the_index_commit_leaves_it_published() {
    // The "after index commit" fault point: the commit happened, so a crash
    // right after it must leave the object readable, with no staging file and
    // no second copy.
    let root = TempRoot::new("postcommit");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    kill_child_at(root.path(), "committed");
    assert_eq!(count_files(&root.path().join("packs")), 1);
    assert_recovered(root.path(), true, "committed");

    // Re-putting it is a no-op, and no second pack appears.
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let again = store.put(Role::Bytes, VALUE).expect("repeat put");
    assert_eq!(again.outcome, PutOutcome::AlreadyPresent);
    assert_eq!(count_files(&root.path().join("packs")), 1);
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
    // One *published* pack plus the unreferenced one the crash left behind (GC's business).
    assert_eq!(count_files(&root.path().join("packs")), 2);
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
fn the_crash_path_observes_the_production_sequence() {
    // F-T2-1: the crash path must not carry its own copy of the publication
    // sequence. This test drives `Store::put` itself with a recording observer
    // and asserts the exact order; the crash child uses the same entry point, so
    // a sequence change shows up here and in the crash tests together.
    use cas_store::{NoPackObservation, PackObserver, PackStep};

    let root = TempRoot::new("sequence");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");

    let mut steps = Vec::new();
    store
        .put_observed(Role::Bytes, VALUE, &mut |step: PackStep| steps.push(step))
        .expect("observed put");
    assert_eq!(
        steps,
        vec![
            PackStep::Staged,
            PackStep::StagedSynced,
            PackStep::Renamed,
            PackStep::DirectorySynced,
            PackStep::Committed,
        ],
        "the production sequence, in order"
    );

    // `put` is the same call with the no-op observer: identical outcome, no
    // extra steps, and the object is published exactly once.
    let plain = store.put(Role::Bytes, VALUE).expect("plain put");
    assert_eq!(plain.outcome, PutOutcome::AlreadyPresent);
    // The no-op observer accepts every step and does nothing, which is what
    // makes `put` and `put_observed` the same sequence.
    let mut none = NoPackObservation;
    for step in steps.iter().copied() {
        PackObserver::step(&mut none, step);
    }

    // Every crash point maps onto a step of that sequence.
    for (name, expected) in [
        ("staged", PackStep::Staged),
        ("synced", PackStep::StagedSynced),
        ("renamed", PackStep::Renamed),
        ("dirsynced", PackStep::DirectorySynced),
        ("committed", PackStep::Committed),
    ] {
        let point = cas_store::testing::CrashPoint::parse(name).expect("known point");
        assert_eq!(point.step(), expected, "crash point {name}");
        assert!(
            steps.contains(&expected),
            "crash point {name} names a step the production put never reaches"
        );
    }
}

#[test]
fn a_cross_process_writer_reports_busy_without_retrying() {
    // ADR-002 §8: one writer at a time, `busy_timeout = 0`, no busy handler and
    // no retry. A second process holding the write lock must make this process's
    // put fail with a typed Busy rather than silently waiting or retrying.
    let root = TempRoot::new("busy");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));

    let mut child = spawn_child(root.path(), "hold-write-lock");
    wait_for_signal(&mut child, "STEP WriteLockHeld", "hold-write-lock");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("parent open");
    let started = Instant::now();
    let result = store.put(Role::Bytes, b"blocked");
    let elapsed = started.elapsed();

    // Read the answer before releasing the child so the assertion is about the
    // contended call, not about a race with its teardown.
    child.kill().expect("kill lock holder");
    let _ = child.wait();

    match result {
        Err(cas_store::StoreError::Busy) => {}
        Err(other) => panic!("expected Busy, got {other}"),
        Ok(_) => panic!("a put succeeded while another process held the write lock"),
    }
    // `busy_timeout = 0` means it fails promptly; a bounded sanity check that it
    // did not wait for a handler that should not exist.
    assert!(
        elapsed < Duration::from_secs(5),
        "the contended put took {elapsed:?}; it should fail at once"
    );

    // With the lock released the same put succeeds.
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    store
        .put(Role::Bytes, b"blocked")
        .expect("put after release");
}

#[test]
fn a_corrupt_loose_leftover_is_ignored_rather_than_adopted() {
    // Loose writes are retired (ADR-008 §3): a truncated file at the old loose path is not
    // read, not adopted and not reported — the object is published into a pack instead, and
    // the leftover is GC's business.
    let root = TempRoot::new("corrupt-orphan");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let id = oid();
    let path = store.layout().object_path(id);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(&path, b"truncated").expect("write");
    let published = store.put(Role::Bytes, VALUE).expect("put into a pack");
    assert_eq!(published.outcome, PutOutcome::Inserted);
    assert_eq!(
        Object::decode(&store.get(id).expect("get"))
            .expect("decode")
            .value(),
        VALUE
    );
    assert!(store.verify().expect("verify").is_clean());
}
