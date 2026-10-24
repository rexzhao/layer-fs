//! Two workspaces publishing the same objects at the same time (ADR-008 §5).
//!
//! The interesting claim is not "both succeed" — the branch head check may legitimately refuse
//! the second committer — it is what the *store* looks like afterwards: one `object_location`
//! row per object identity (`INSERT OR IGNORE`: first committer wins), a pack that may contain
//! the loser's dead frame, and a `verify` that stays clean. A dead frame is legal (ADR-008 §5,
//! §6.3) and is exactly what GC's repack is for; a store that reports it as corruption would
//! push an operator to "repair" healthy data (KI-10).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use cas_core::ObjectId;
use cas_store::{OpenMode, Store};
use fs_snapshot::SnapshotReader;
use fs_workspace::{Repo, WorkspaceError};

const PAYLOAD: &[u8] = b"the same page in both workspaces";

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        // The clock is part of the name, not just the pid: a later run of this binary can
        // reuse the pid, and reusing an earlier run's leftover store would make the test
        // depend on history instead of on this run.
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-pack-concurrency-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn prepare(dir: &Path) {
    let repo = Arc::new(Repo::open(dir, true).expect("open"));
    repo.init().expect("init");
    repo.branch_create("b2", Some("main")).expect("branch");
    for (name, branch) in [("w1", "main"), ("w2", "b2")] {
        let workspace = repo.workspace_create(name, branch).expect("workspace");
        workspace.create_file("/f").expect("create");
        workspace.write_at("/f", 0, PAYLOAD).expect("write");
        workspace.close().expect("close");
    }
}

/// The tree of `commit` names `/f` with the shared payload.
fn assert_root_reads(dir: &Path, commit: &str) {
    let store = Store::open(dir, OpenMode::ReadOnly).expect("store");
    let root: ObjectId = commit.parse().expect("root id");
    let reader = SnapshotReader::open_any(&store, root).expect("reader");
    let root_inode = reader.inode(0).expect("root inode").expect("present");
    let serial = reader
        .lookup(&root_inode, b"f")
        .expect("lookup")
        .expect("the published tree must hold /f");
    let inode = reader.inode(serial).expect("inode").expect("present");
    assert_eq!(inode.size, PAYLOAD.len() as u64);
}

fn commit(workspace: &mut fs_workspace::Workspace) -> Result<String, WorkspaceError> {
    workspace.commit("published", false).map(|outcome| {
        outcome
            .commit
            .expect("a successful publish names its commit")
    })
}

#[test]
fn two_workspaces_publishing_the_same_page_leave_one_row_and_a_clean_store() {
    for round in 0..5 {
        let root = TempRoot::new(&format!("round-{round}"));
        let dir = root.store();
        prepare(&dir);

        // One handle per thread: a `Repo` owns a SQLite connection, which is neither `Send`
        // nor `Sync`. The main database's connections take a bounded busy wait (ADR-006
        // §11.1), so a contended commit either wins or reports a typed failure.
        let outcomes: Vec<Result<String, WorkspaceError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = [("w1", "main"), ("w2", "b2")]
                .into_iter()
                .map(|(name, _branch)| {
                    let dir = dir.clone();
                    scope.spawn(move || {
                        let repo = Arc::new(Repo::open(&dir, false).expect("open"));
                        let mut workspace = repo.workspace_open(name).expect("workspace");
                        commit(&mut workspace)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        let mut published = Vec::new();
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(commit) => published.push(commit),
                // A typed refusal is fine: a concurrent writer holding the lane reports Busy
                // rather than waiting (ADR-002 §8), and a moved branch reports BranchMoved.
                Err(WorkspaceError::Busy { .. }) | Err(WorkspaceError::BranchMoved { .. }) => {}
                Err(WorkspaceError::NothingToCommit) => {
                    panic!("round {round}: workspace {index} had nothing to publish")
                }
                Err(other) => panic!("round {round}: unexpected failure: {other:?}"),
            }
        }
        assert!(
            !published.is_empty(),
            "round {round}: at least one commit lands"
        );

        let store = Store::open(&dir, OpenMode::ReadOnly).expect("store");
        let report = store.verify().expect("verify");
        assert!(
            report.is_clean(),
            "round {round}: the store must stay clean after a concurrent publish: {report:?}"
        );
        assert!(report.packs_checked >= 1, "round {round}: packs exist");

        // Object identities are unique: the shared page is one row, not two.
        let connection = rusqlite::Connection::open(dir.join("store.sqlite")).expect("db");
        let duplicates: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM (SELECT oid FROM object_location GROUP BY oid HAVING COUNT(*) > 1)",
                [],
                |row| row.get(0),
            )
            .expect("duplicates");
        assert_eq!(
            duplicates, 0,
            "round {round}: object_location is unique per oid"
        );
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM object_location", [], |row| row.get(0))
            .expect("rows");
        assert_eq!(
            rows as usize, report.checked,
            "round {round}: every row is a checked object"
        );

        // Every branch head that exists is readable.
        for branch in ["main", "b2"] {
            let repo = Arc::new(Repo::open(&dir, false).expect("open"));
            if let Ok(head) = repo.branch_head(branch) {
                if let Ok(info) = repo.commit_info(&head) {
                    assert_root_reads(&dir, &info.root);
                }
            }
        }
    }
}

/// Concurrent **opens** must succeed inside the bounded wait (ADR-006 §11.1).
///
/// This is the regression for the `ids.rs` defect: reserving serials read the allocator row
/// and then wrote it inside a `DEFERRED` transaction, so two workspaces opening at the same
/// time upgraded a read lock to a write lock and got an immediate `SQLITE_BUSY` — after ~5 ms,
/// while the error claimed a 5000 ms wait. With `BEGIN IMMEDIATE` the busy timeout applies,
/// and the publish that follows is still correct.
#[test]
fn concurrent_opens_wait_then_publish_correctly() {
    for round in 0..10 {
        let root = TempRoot::new(&format!("open-{round}"));
        let dir = root.store();
        prepare(&dir);

        let outcomes: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = ["w1", "w2"]
                .into_iter()
                .map(|name| {
                    let dir = dir.clone();
                    scope.spawn(move || {
                        let repo = Arc::new(Repo::open(&dir, false).expect("open"));
                        // The two opens overlap here: each reserves a serial segment.
                        match repo.workspace_open(name) {
                            Ok(mut workspace) => workspace.commit("published", false).map(Some),
                            Err(error) => Err(error),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        let mut commits = 0;
        for outcome in outcomes {
            match outcome {
                Ok(Some(_)) => commits += 1,
                Ok(None) => {}
                Err(WorkspaceError::Busy { waited_ms }) => panic!(
                    "round {round}: a concurrent open must wait, not fail                      (reported wait {waited_ms} ms)"
                ),
                Err(WorkspaceError::BranchMoved { .. }) => {}
                Err(other) => panic!("round {round}: unexpected failure: {other:?}"),
            }
        }
        assert!(commits >= 1, "round {round}: at least one publish lands");

        let store = Store::open(&dir, OpenMode::ReadOnly).expect("store");
        let report = store.verify().expect("verify");
        assert!(
            report.is_clean(),
            "round {round}: the store stays clean: {report:?}"
        );
    }
}
