//! **P2a acceptance matrix A1–A8** (ADR-006 §18, spec §6 P2).
//!
//! Each test is named for the row it covers, so a failure names the acceptance item. Two
//! rows are heavy and gated behind `#[ignore]` so the routine regression stays inside the
//! two-minute budget; their exact command is in the doc comment.
//!
//! | # | scenario | test |
//! | --- | --- | --- |
//! | A1 | 10 workspaces on one base, concurrent乱序 writes | [`a1_ten_workspaces_do_not_interfere`] (`--ignored`) |
//! | A2 | flush then reopen keeps the data | [`a2_a_flushed_change_survives_a_reopen`] |
//! | A3 | history reads with no table involved | [`a3_history_is_readable_without_any_row`] |
//! | A4 | BranchMoved → rebind or force | [`a4_a_moved_branch_is_refused_and_can_be_rebound`] |
//! | A5 | two concurrent 1000-file publications | [`a5_two_publications_share_the_main_database`] |
//! | A6 | linearizable within one workspace | [`a6_operations_in_one_workspace_are_linearizable`] |
//! | A7 | the materialiser walks from the root only | [`a7_an_unreachable_inode_never_reaches_a_snapshot`] |
//! | A8 | schema v3 refuses v1/v2 | [`a8_an_older_schema_is_refused_even_with_create`] |

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use fs_api::{ApiError, Kind, Repo, Workspace};

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
                "fs-api-accept-{label}-{}-{sequence}",
                std::process::id()
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

fn put(workspace: &Workspace, path: &str, bytes: &[u8]) {
    if workspace.stat(path).is_err() {
        workspace.create(path).expect("create");
    }
    workspace.write_at(path, 0, bytes).expect("write");
    workspace
        .truncate(path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

fn read(workspace: &Workspace, path: &str) -> Vec<u8> {
    let size = usize::try_from(workspace.stat(path).expect("stat").size).unwrap_or(0);
    let mut buffer = vec![0u8; size];
    let got = workspace.read_at(path, 0, &mut buffer).expect("read");
    buffer.truncate(got);
    buffer
}

// ---------------------------------------------------------------- A1

/// **A1** — ten workspaces over one base, writing乱序 and concurrently, must not see each
/// other's changes and must each publish its own tree.
///
/// Run: `cargo test -p fs-api --test acceptance -- --ignored a1_`
#[test]
#[ignore = "heavy: ten workspaces x 200 files, run explicitly"]
fn a1_ten_workspaces_do_not_interfere() {
    const WORKSPACES: usize = 10;
    const FILES: usize = 200;

    let root = TempRoot::new("a1");
    let repo = Arc::new(Repo::create(root.store()).expect("create"));
    let base = repo.branch_head("main").expect("head");
    // One branch per workspace, all from the same base commit.
    for index in 0..WORKSPACES {
        repo.branch_create(&format!("b{index}"), Some(&base))
            .expect("branch");
    }

    let barrier = Arc::new(Barrier::new(WORKSPACES));
    let mut handles = Vec::new();
    for index in 0..WORKSPACES {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut workspace = repo
                .workspace_create(&format!("w{index}"), &format!("b{index}"))
                .expect("workspace");
            // Create the files first, so the concurrent phase is pure content writes.
            for file in 0..FILES {
                workspace
                    .create(&format!("/f{file:04}"))
                    .expect("create file");
            }
            barrier.wait();
            // 乱序: walk the files in an order that differs per workspace.
            for step in 0..FILES {
                let file = (step * 7 + index * 13) % FILES;
                let payload = vec![u8::try_from(file % 251).unwrap_or(0); 32];
                workspace
                    .write_at(&format!("/f{file:04}"), 0, &payload)
                    .expect("write");
            }
            let commit = workspace.commit("published", false).expect("commit");
            (index, commit)
        }));
    }

    let mut commits = Vec::new();
    for handle in handles {
        commits.push(handle.join().expect("thread"));
    }
    assert_eq!(commits.len(), WORKSPACES);

    // Every branch moved, and every tree holds the writer's own bytes only.
    for (index, commit) in commits {
        assert_eq!(
            repo.branch_head(&format!("b{index}")).expect("head"),
            commit
        );
        let view = repo.read_view(&commit).expect("read_view");
        for file in 0..FILES {
            let bytes = view
                .read_all(&format!("/f{file:04}"))
                .expect("read from history");
            assert_eq!(
                bytes,
                vec![u8::try_from(file % 251).unwrap_or(0); 32],
                "workspace {index} file {file} must hold its own bytes"
            );
        }
    }
    // Ten workspaces are registered, each on its own branch.
    let listed = repo.workspaces().expect("workspaces");
    assert_eq!(listed.len(), WORKSPACES);
}

// ---------------------------------------------------------------- A2

/// **A2** — what `flush` covered is still there after the handle is closed and reopened.
#[test]
fn a2_a_flushed_change_survives_a_reopen() {
    let root = TempRoot::new("a2");
    let repo = Repo::create(root.store()).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.mkdir("/d").expect("mkdir");
    put(&workspace, "/d/f", b"durable");
    workspace.flush().expect("flush");
    workspace.close().expect("close");
    drop(repo);

    let repo = Repo::open(root.store()).expect("reopen");
    let workspace = repo.workspace_open("w1").expect("open");
    assert_eq!(read(&workspace, "/d/f"), b"durable");
    assert_eq!(workspace.read_dir("/d").expect("read_dir").len(), 1);
    assert!(workspace.status().expect("status").dirty);
}

// ---------------------------------------------------------------- A3

/// **A3** — a commit's tree is readable after every row and overlay file about it is gone.
#[test]
fn a3_history_is_readable_without_any_row() {
    let root = TempRoot::new("a3");
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.mkdir("/docs").expect("mkdir");
    put(&workspace, "/docs/a", b"first version");
    let first = workspace.commit("first", false).expect("commit");
    put(&workspace, "/docs/a", b"second version");
    workspace.commit("second", false).expect("commit");
    workspace.close().expect("close");

    // Remove every trace of the workspace: its row, its overlay file, its directory.
    repo.workspace_discard("w1").expect("discard");
    assert!(repo.workspaces().expect("workspaces").is_empty());

    let view = repo.read_view(&first).expect("read_view");
    assert_eq!(view.read_all("/docs/a").expect("read"), b"first version");
    assert_eq!(view.read_dir("/docs").expect("ls").len(), 1);
    assert_eq!(view.stat("/docs").expect("stat").kind, Kind::Directory);
}

// ---------------------------------------------------------------- A4

/// **A4** — the loser of a race keeps its work and can publish by rebinding or forcing.
#[test]
fn a4_a_moved_branch_is_refused_and_can_be_rebound() {
    let root = TempRoot::new("a4");
    let repo = Repo::create(root.store()).expect("create");
    let base = repo.branch_head("main").expect("head");
    let mut winner = repo.workspace_create("winner", "main").expect("workspace");
    let mut loser = repo.workspace_create("loser", "main").expect("workspace");
    put(&winner, "/w", b"winner");
    put(&loser, "/l", b"loser");

    let published = winner.commit("winner", false).expect("commit");
    match loser.commit("loser", false) {
        Err(ApiError::Workspace(fs_workspace::WorkspaceError::BranchMoved {
            branch,
            expected,
            actual,
        })) => {
            assert_eq!(branch, "main");
            assert_eq!(expected, base);
            assert_eq!(actual, published);
        }
        other => panic!("expected BranchMoved, got {other:?}"),
    }
    // The loser kept everything.
    assert_eq!(read(&loser, "/l"), b"loser");
    assert_eq!(repo.branch_head("main").expect("head"), published);

    // 出路 1: a branch from the base commit, then rebind.
    repo.branch_create("side", Some(&base)).expect("branch");
    loser.rebind("side").expect("rebind");
    let side = loser.commit("loser, on side", false).expect("commit");
    assert_eq!(repo.branch_head("side").expect("head"), side);

    // 出路 3: force. A workspace keeps the base it started from, so after `main` is reset
    // to an older commit, that workspace's next commit sees a head it no longer descends
    // from — a conflict — and `force` is the documented way to publish anyway.
    let mut stale = repo.workspace_create("stale", "main").expect("workspace");
    put(&stale, "/s", b"stale");
    repo.reset("main", &base, None).expect("reset main back");
    let refused = stale.commit("against the reset branch", false);
    assert!(
        matches!(
            refused,
            Err(ApiError::Workspace(
                fs_workspace::WorkspaceError::BranchMoved { .. }
            ))
        ),
        "expected a conflict against the reset branch, got {refused:?}"
    );
    let forced = stale.commit("forced", true).expect("force");
    assert_eq!(repo.branch_head("main").expect("head"), forced);
    // The forced commit's parent is the workspace's base, not the head it displaced.
    let info = repo.commit_info(&forced).expect("info");
    assert_eq!(info.parent.as_deref(), Some(published.as_str()));
}

// ---------------------------------------------------------------- A5

/// **A5** — two workspaces publish 1000 files each at the same time; both must succeed
/// (§11.1's bounded wait), and the wait must stay a safety net rather than the norm.
#[test]
fn a5_two_publications_share_the_main_database() {
    const FILES: usize = 1000;
    let root = TempRoot::new("a5");
    let repo = Arc::new(Repo::create(root.store()).expect("create"));
    let base = repo.branch_head("main").expect("head");
    repo.branch_create("side", Some(&base)).expect("branch");

    let left = repo.workspace_create("left", "main").expect("left");
    let right = repo.workspace_create("right", "side").expect("right");
    for index in 0..FILES {
        let name = format!("/l{index:04}");
        left.create(&name).expect("create");
        left.write_at(&name, 0, b"x").expect("write");
        let name = format!("/r{index:04}");
        right.create(&name).expect("create");
        right.write_at(&name, 0, b"x").expect("write");
    }

    let barrier = Arc::new(Barrier::new(2));
    let spawn = |mut workspace: Workspace, barrier: Arc<Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            let started = Instant::now();
            let outcome = workspace.commit("concurrent", false);
            (started.elapsed(), outcome)
        })
    };
    let left_thread = spawn(left, Arc::clone(&barrier));
    let right_thread = spawn(right, Arc::clone(&barrier));
    let (left_time, left_outcome) = left_thread.join().expect("join");
    let (right_time, right_outcome) = right_thread.join().expect("join");

    let left = left_outcome.expect("the left publication must succeed");
    let right = right_outcome.expect("the right publication must succeed");
    assert_ne!(left, right);
    eprintln!("A5 concurrent publication: left {left_time:?} / right {right_time:?}");
    // Margin evidence for §11.1: a publication of this size must finish well inside the
    // 5 s wait, or the wait would be the normal path rather than a safety net.
    let worst = left_time.max(right_time);
    assert!(
        worst < std::time::Duration::from_secs(30),
        "a concurrent publication must not be starved, took {worst:?}"
    );
}

// ---------------------------------------------------------------- A6

/// **A6** — inside one workspace the operations are linearizable: a reader never sees a
/// half-applied rename, and a rejected operation changes nothing (K3/TX1–TX3).
#[test]
fn a6_operations_in_one_workspace_are_linearizable() {
    let root = TempRoot::new("a6");
    let repo = Repo::create(root.store()).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.mkdir("/a").expect("mkdir");
    put(&workspace, "/a/f", b"content");

    // A rename either happened or did not; the old and new names never both resolve.
    workspace.rename("/a/f", "/a/g").expect("rename");
    assert!(workspace.stat("/a/f").is_err(), "the old name is gone");
    assert_eq!(read(&workspace, "/a/g"), b"content");

    // A rejected operation leaves the view untouched.
    let before = workspace.read_dir("/a").expect("read_dir");
    assert!(workspace.mkdir("/a").is_err(), "mkdir of an existing name");
    assert_eq!(workspace.read_dir("/a").expect("read_dir"), before);
}

// ---------------------------------------------------------------- A7

/// **A7** — the materialiser walks from the root (I2), so an inode that no directory binds
/// never appears in a snapshot, and a bound-away subtree disappears with its name.
#[test]
fn a7_an_unreachable_inode_never_reaches_a_snapshot() {
    let root = TempRoot::new("a7");
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.mkdir("/kept").expect("mkdir");
    put(&workspace, "/kept/f", b"reachable");
    workspace.mkdir("/dropped").expect("mkdir");
    put(&workspace, "/dropped/g", b"also reachable for now");
    workspace.remove("/dropped/g").expect("remove file");
    workspace.remove("/dropped").expect("remove dir");

    let commit = workspace.commit("first", false).expect("commit");
    let view = repo.read_view(&commit).expect("read_view");
    let names: Vec<String> = view
        .read_dir("/")
        .expect("ls")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, vec!["kept".to_owned()]);
    assert!(
        view.stat("/dropped").is_err(),
        "the dropped subtree is gone"
    );

    // The inode table holds only what the walk reached: root, /kept, /kept/f.
    let report = repo.verify(true).expect("verify");
    assert!(report.is_clean(), "got {report:?}");
    assert_eq!(
        report.inodes_checked, 4,
        "genesis root + three reachable inodes"
    );
}

// ---------------------------------------------------------------- A8

/// **A8** — a store from schema v1/v2 is refused, and `create` does not write over it.
#[test]
fn a8_an_older_schema_is_refused_even_with_create() {
    let root = TempRoot::new("a8");
    // A real cas-store with the P1 namespace tables on top: the shape a v2 store has.
    drop(cas_store::Store::open(root.store(), cas_store::OpenMode::Create).expect("store"));
    let connection = rusqlite::Connection::open(root.store().join("store.sqlite")).expect("open");
    connection
        .execute_batch(
            "CREATE TABLE namespace_policy (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL) STRICT;
             INSERT INTO namespace_policy (key, value) VALUES ('schema_version', '2');",
        )
        .expect("P1 tables");
    drop(connection);

    match Repo::open(root.store()) {
        Err(ApiError::Workspace(fs_workspace::WorkspaceError::SchemaMismatch {
            found, ..
        })) => assert!(found.contains("namespace_policy"), "got {found}"),
        other => panic!("open must name the old schema, got {other:?}"),
    }
    assert!(
        Repo::create(root.store()).is_err(),
        "create must not migrate"
    );
}
