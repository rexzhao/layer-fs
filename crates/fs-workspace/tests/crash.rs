//! **I4 / §9.4** — the crash points of a publication, exercised by killing a real process.
//!
//! > 崩溃注入点：overlay 操作事务前后；flush 前后；物化中途（任意 put 之后）；第 2 步前后；
//! > 第 3 步前后；第 4 步前后；discard 删行后删文件前。
//!
//! Each test spawns this same binary as a child, tells it to abort at one point, and then
//! reopens the store in the parent. `std::process::abort` is the crash: no unwinding, no
//! `Drop`, no SQLite close, no checkpoint — the same thing `Child::kill` produces.
//!
//! # What these prove, and what they do not
//!
//! They prove the *recovery protocol*: whatever survived, the repository reopens into one
//! of the states §9.3 lists, and the workspace's view is exactly the reference model's.
//! They do **not** prove fsync or power-loss semantics — a killed process still leaves the
//! operating system's page cache intact (ADR-006 §8, §17.3).

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use cas_store::{OpenMode, Store};
use fs_namespace::{Path as FsPath, Rules};
use fs_snapshot::SnapshotReader;
use fs_workspace::{Repo, Workspace};

const CHILD_ENV: &str = "WCAS_WORKSPACE_CRASH_CHILD";
const POINT_ENV: &str = "WCAS_WORKSPACE_CRASH_AT";
const FAILED_MARKER: &str = "child-survived";

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
                // Clock + counter + pid: a later run can reuse the pid, and reusing an
                // earlier run's leftover store would make the test depend on history.
                "fs-workspace-crash-{label}-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        assert!(
            std::fs::read_dir(&path)
                .expect("temp root")
                .next()
                .is_none(),
            "temp root {} is not empty: the name collided with an earlier run",
            path.display()
        );
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn p(text: &str) -> FsPath {
    FsPath::parse(text).expect("path")
}

fn put_file(workspace: &Workspace, path: &str, bytes: &[u8]) {
    let store = workspace.store();
    if store.content_state(&p(path)).is_err() {
        store.create_file(&p(path)).expect("create file");
    }
    store.write_at(&p(path), 0, bytes).expect("write");
    store
        .truncate(&p(path), u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

fn read_file(workspace: &Workspace, path: &str) -> Vec<u8> {
    let size = workspace
        .store()
        .content_state(&p(path))
        .expect("state")
        .size;
    workspace
        .store()
        .read_at(&p(path), 0, usize::try_from(size).expect("fits"))
        .expect("read")
}

fn names(workspace: &Workspace, dir: &str) -> Vec<String> {
    Rules::new(workspace.store())
        .read_dir(&p(dir))
        .expect("read_dir")
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

/// Creates the repository the child will crash in: one workspace with one unflushed file.
fn prepared(label: &str) -> (TempRoot, PathBuf, String) {
    let root = TempRoot::new(label);
    let dir = root.join("store");
    let repo = Arc::new(Repo::open(&dir, true).expect("open"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"payload");
    workspace.flush().expect("flush");
    workspace.close().expect("close");
    let head = repo.branch_head("main").expect("head");
    drop(repo);
    (root, dir, head)
}

/// The child: opens the prepared repository and aborts at `mode`'s crash point.
///
/// Skipped unless the driver spawned it with the marker set.
#[test]
#[ignore = "spawned as a child process by the crash drivers below"]
fn crash_child() {
    common::strict_gate();
    let Some(spec) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let spec = spec.to_string_lossy().into_owned();
    let mut parts = spec.split('|');
    let dir = PathBuf::from(parts.next().expect("dir"));
    let mode = parts.next().expect("mode");

    let repo = Arc::new(Repo::open(&dir, true).expect("open"));
    match mode {
        "commit" => {
            let workspace = repo.workspace_open("w1").expect("open workspace");
            // Every crash point of a commit is inside this call.
            let _ = workspace.commit("from the child", false);
        }
        "discard" => {
            repo.workspace_discard("w1").expect("discard");
        }
        "snapshot" => {
            let workspace = repo.workspace_open("w1").expect("open workspace");
            let _ = workspace.snapshot();
        }
        other => panic!("unknown child mode {other}"),
    }
    // Reaching this line means the crash point did not fire, which the parent treats as a
    // failure rather than a pass.
    std::fs::write(
        dir.join(FAILED_MARKER),
        b"the process was supposed to abort",
    )
    .ok();
}

/// Runs the child with `mode` and `point`, expecting it to abort.
fn run_child(dir: &Path, mode: &str, point: &str) {
    let exe = std::env::current_exe().expect("current exe");
    let spec = format!("{}|{}", dir.display(), mode);
    let status = std::process::Command::new(exe)
        .args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env(CHILD_ENV, spec)
        .env(POINT_ENV, point)
        .status()
        .expect("spawn child");
    assert!(
        !status.success(),
        "the child was supposed to abort at {point} but exited with {status}"
    );
    assert!(
        !dir.join(FAILED_MARKER).exists(),
        "the crash point {point} never fired: the child ran to completion"
    );
}

/// Reopens the repository and returns it plus the workspace, after recovery ran.
fn recovered(dir: &Path) -> (Arc<Repo>, Workspace) {
    let repo = Arc::new(Repo::open(dir, false).expect("reopen"));
    let workspace = repo.workspace_open("w1").expect("open recovered workspace");
    (repo, workspace)
}

/// The published tree is readable without any of our tables (I1), which also proves the
/// objects existed before the row that names them (I5).
fn assert_tree_holds_payload(dir: &Path, root: &str) {
    let store = Store::open(dir, OpenMode::ReadOnly).expect("store");
    let reader = SnapshotReader::open_any(&store, root.parse().expect("root id")).expect("reader");
    let root_inode = reader.inode(0).expect("root inode").expect("present");
    let serial = reader
        .lookup(&root_inode, b"f")
        .expect("lookup")
        .expect("the file must be in the published tree");
    let inode = reader.inode(serial).expect("inode").expect("present");
    assert_eq!(inode.size, 7, "the published file must have its bytes");
}

/// **§9.4 物化中途** — a crash after some objects are published but before any row names
/// them leaves the repository exactly as it was, plus unreferenced objects.
#[test]
fn a_crash_during_materialisation_publishes_nothing() {
    common::strict_gate();
    let (_root, dir, head) = prepared("during-materialise");
    run_child(&dir, "commit", "during-materialise");
    let (repo, workspace) = recovered(&dir);

    assert_eq!(repo.branch_head("main").expect("head"), head);
    assert_eq!(repo.log("main").expect("log").len(), 1);
    assert_eq!(workspace.base_commit(), head);
    assert!(
        workspace.is_dirty().expect("dirty"),
        "the work is still in the overlay"
    );
    assert_eq!(read_file(&workspace, "/f"), b"payload");
}

/// **§9.4 第 2 步之前** — nothing was recorded, nothing moved.
#[test]
fn a_crash_before_the_intent_leaves_the_branch_and_the_work() {
    common::strict_gate();
    let (_root, dir, head) = prepared("after-materialise");
    run_child(&dir, "commit", "after-materialise");
    let (repo, workspace) = recovered(&dir);

    assert_eq!(repo.branch_head("main").expect("head"), head);
    assert_eq!(workspace.base_commit(), head);
    assert!(workspace.is_dirty().expect("dirty"));
    assert_eq!(read_file(&workspace, "/f"), b"payload");
}

/// **§9.4 第 2 步之后、第 3 步之前** — the stale intent is cleared and the work survives.
///
/// This is §9.3's third row: `pending_root = R` but the main database never adopted R.
#[test]
fn a_crash_after_the_intent_clears_it_and_keeps_the_work() {
    common::strict_gate();
    let (_root, dir, head) = prepared("after-intent");
    run_child(&dir, "commit", "after-intent");
    {
        // While the intent is still on disk, it names a root the repository does not
        // reference; the recovery is what must not believe it.
        let (repo, workspace) = recovered(&dir);
        assert_eq!(repo.branch_head("main").expect("head"), head);
        assert_eq!(workspace.base_commit(), head);
        assert!(workspace.is_dirty().expect("dirty"));
        assert_eq!(read_file(&workspace, "/f"), b"payload");
    }
    // A second open sees no intent left behind.
    let (repo, workspace) = recovered(&dir);
    assert_eq!(repo.branch_head("main").expect("head"), head);
    assert_eq!(read_file(&workspace, "/f"), b"payload");
}

/// **§9.4 第 3 步之后、第 4 步之前** — the head moved, so recovery must finish step 4
/// (§9.3's second row) and the published tree must be complete.
#[test]
fn a_crash_after_the_publication_completes_it_on_reopen() {
    common::strict_gate();
    let (_root, dir, head) = prepared("after-publish");
    run_child(&dir, "commit", "after-publish");
    let (repo, workspace) = recovered(&dir);

    let published = repo.branch_head("main").expect("head");
    assert_ne!(
        published, head,
        "the publication committed before the crash"
    );
    assert_eq!(repo.log("main").expect("log").len(), 2);
    // The workspace row was updated in the same transaction, so it points at the commit.
    assert_eq!(workspace.base_commit(), published);
    // Step 4 ran during recovery: the overlay is empty and the view comes from the base.
    assert!(
        !workspace.is_dirty().expect("dirty"),
        "recovery must finish adopting the published root"
    );
    assert_eq!(read_file(&workspace, "/f"), b"payload");
    assert_eq!(names(&workspace, "/"), vec!["f".to_owned()]);
    let info = repo.commit_info(&published).expect("info");
    assert_tree_holds_payload(&dir, &info.root);
}

/// **§9.4 snapshot 的第 3 步之后** — a snapshot records a GC root without moving a branch.
#[test]
fn a_crash_after_a_snapshot_publishes_keeps_the_root_readable() {
    common::strict_gate();
    let (_root, dir, head) = prepared("snapshot-publish");
    run_child(&dir, "snapshot", "after-publish");
    let (repo, workspace) = recovered(&dir);

    assert_eq!(
        repo.branch_head("main").expect("head"),
        head,
        "a snapshot never moves a branch"
    );
    assert_eq!(workspace.base_commit(), head);
    assert!(!workspace.is_dirty().expect("dirty"));
    assert_eq!(read_file(&workspace, "/f"), b"payload");
    // The snapshot row is the GC root that keeps the tree alive.
    let connection = rusqlite::Connection::open(repo.metadata_path()).expect("connection");
    let roots: i64 = connection
        .query_row("SELECT COUNT(*) FROM snapshot", [], |row| row.get(0))
        .expect("count");
    assert_eq!(roots, 1, "the snapshot row survived the crash");
    let root_oid: String = connection
        .query_row("SELECT root_oid FROM snapshot", [], |row| row.get(0))
        .expect("root");
    assert_tree_holds_payload(&dir, &root_oid);
}

/// **§9.4 discard 删行后删文件前** — the next open sweeps the orphan directory.
#[test]
fn a_crash_during_discard_is_swept_on_reopen() {
    common::strict_gate();
    let (_root, dir, _head) = prepared("discard");
    let workspace_dir = {
        let repo = Repo::open(&dir, false).expect("open");
        let id = repo.workspace_list().expect("list")[0].id.clone();
        repo.workspace_dir(&id)
    };
    run_child(&dir, "discard", "after-discard-row");

    assert!(
        workspace_dir.exists(),
        "the crash happened between the row and the files"
    );
    let repo = Arc::new(Repo::open(&dir, false).expect("reopen"));
    assert_eq!(
        repo.workspace_list().expect("list").len(),
        0,
        "the row is gone, so the workspace is gone"
    );
    assert!(
        !workspace_dir.exists(),
        "the sweep removed the orphan directory"
    );
}

/// **P2b-2 / ADR-008 §5.1** — every commit crash point, then: the store verifies and **all
/// roots read back**.
///
/// The per-point tests above assert the recovery protocol for one aspect each. This one is the
/// grouped claim the pack/publish card asks for: whatever the injection point, reopening must
/// leave (a) a store whose objects *and packs* verify, and (b) every root the repository still
/// names — each branch head's commit root, the workspace's base root, and every `snapshot` row
/// — readable through `SnapshotReader`, i.e. without any of our tables.
#[test]
fn every_commit_crash_point_recovers_to_a_clean_store_with_all_roots_readable() {
    common::strict_gate();
    for point in [
        "during-materialise",
        "after-materialise",
        "after-intent",
        "after-publish",
    ] {
        let (_root, dir, _head) = prepared(&format!("roots-{point}"));
        run_child(&dir, "commit", point);
        let (repo, workspace) = recovered(&dir);

        // (a) objects and packs verify; a concurrent loser's dead frame is legal, a broken
        // pack is not.
        let store = Store::open(&dir, OpenMode::ReadOnly).expect("store");
        let report = store.verify().expect("verify");
        assert!(report.is_clean(), "{point}: store is not clean: {report:?}");
        drop(store);

        // (b) every root is readable. Commit ids first, then snapshot roots (already oids).
        let mut roots: Vec<cas_core::ObjectId> = Vec::new();
        for branch in repo.branches().expect("branches") {
            let info = repo.commit_info(&branch.head).expect("commit info");
            roots.push(info.root.parse().expect("root id"));
        }
        let base = repo
            .commit_info(&workspace.base_commit())
            .expect("base commit info");
        roots.push(base.root.parse().expect("root id"));
        let connection = rusqlite::Connection::open(repo.metadata_path()).expect("db");
        let mut statement = connection
            .prepare("SELECT root_oid FROM snapshot")
            .expect("prepare");
        for row in statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("rows")
        {
            roots.push(row.expect("root id").parse().expect("oid"));
        }
        assert!(!roots.is_empty(), "{point}: no roots to check");
        for root in roots {
            let store = Store::open(&dir, OpenMode::ReadOnly).expect("store");
            let reader = SnapshotReader::open_any(&store, root)
                .unwrap_or_else(|error| panic!("{point}: root {root} is unreadable: {error:?}"));
            let root_inode = reader.inode(0).expect("root inode").expect("present");
            // A root may legitimately be an empty tree (the repository's initial commit), so
            // the claim is "readable", plus "if it names /f, its bytes are intact".
            if let Some(serial) = reader.lookup(&root_inode, b"f").expect("lookup") {
                let inode = reader.inode(serial).expect("inode").expect("present");
                assert_eq!(inode.size, 7, "{point}: root {root} lost /f's bytes");
            }
        }

        // The recovered workspace still reads its file through the overlay.
        assert_eq!(read_file(&workspace, "/f"), b"payload", "{point}");
    }
}
