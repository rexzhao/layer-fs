//! Workspace lifecycle, publication and history, against a real store (ADR-006 §18).
//!
//! Each test names the acceptance item it covers. They drive `fs_namespace::Rules` and the
//! overlay's content API directly — `fs-api` is P2a-7 and is deliberately not involved.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cas_store::{OpenMode, Store};
use fs_namespace::{Path as FsPath, Rules};
use fs_workspace::{Workspace, WorkspaceError, WorkspaceStatus};

type Repo = Arc<fs_workspace::Repo>;

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
                "fs-workspace-{label}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
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

/// An initialised repository in a fresh directory.
fn repository(label: &str) -> (TempRoot, Repo) {
    let root = TempRoot::new(label);
    let repo = Arc::new(fs_workspace::Repo::open(root.join("store"), true).expect("open repo"));
    repo.init().expect("init");
    (root, repo)
}

fn p(text: &str) -> FsPath {
    FsPath::parse(text).expect("path")
}

/// Creates a file with content through the overlay, the way a caller would.
fn put_file(workspace: &Workspace, path: &str, bytes: &[u8]) {
    let store = workspace.store();
    // These helpers rewrite the same path to change its content, so an existing file is
    // reused rather than being an error.
    if store.content_state(&p(path)).is_err() {
        store.create_file(&p(path)).expect("create file");
    }
    store.write_at(&p(path), 0, bytes).expect("write");
    store
        .truncate(&p(path), u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate to the new length");
}

fn read_file(workspace: &Workspace, path: &str) -> Vec<u8> {
    let size = workspace
        .store()
        .content_state(&p(path))
        .expect("state")
        .size;
    workspace
        .store()
        .read_at(&p(path), 0, usize::try_from(size).unwrap())
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

/// **§7.4** — a repository and a workspace handle may be shared between threads.
///
/// ADR-006 §7.4 says the workspace's own threads are serialised by the handle's mutex and
/// that workspaces never contend with each other. Both need these bounds: without them the
/// API would work only on the thread that opened it.
#[test]
fn the_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<fs_workspace::Repo>();
    assert_send_sync::<fs_workspace::Workspace>();
    assert_send_sync::<fs_workspace::WorkspaceStore>();
}
/// **§18.1** — `init` publishes an empty tree and records the first commit.
#[test]
fn init_creates_the_default_branch_and_one_commit() {
    let (_root, repo) = repository("init");
    let head = repo.branch_head("main").expect("head");
    let info = repo.commit_info(&head).expect("commit");
    assert_eq!(info.parent, None, "the first commit has no parent");
    assert_eq!(info.message, "init");
    let log = repo.log("main").expect("log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].id, head);
    assert_eq!(repo.branches().expect("branches").len(), 1);
    // Serial 0 is the root directory, so a fresh workspace has exactly one inode.
    assert_eq!(repo.workspace_list().expect("list").len(), 0);
}

/// **§18.9 / lifecycle matrix** — create, open, list, close, discard.
#[test]
fn workspace_lifecycle_matrix() {
    let (_root, repo) = repository("lifecycle");

    let workspace = repo.workspace_create("w1", "main").expect("create");
    assert_eq!(workspace.name(), "w1");
    assert_eq!(workspace.branch(), "main");
    let id = workspace.id().to_owned();
    workspace.close().expect("close");

    let listed = repo.workspace_list().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "w1");
    assert_eq!(listed[0].state, "ready");
    assert!(repo.workspace_dir(&id).join("overlay.sqlite").is_file());

    // Reopening finds the same overlay, and the view is still the base.
    let reopened = repo.workspace_open("w1").expect("open");
    assert_eq!(reopened.id(), id);
    assert!(!reopened.is_dirty().expect("dirty"));
    let status: WorkspaceStatus = reopened.status().expect("status");
    assert_eq!(status.branch, "main");
    assert!(!status.dirty);
    assert_eq!(status.base_root, reopened.base_root().to_string());

    // A workspace is exclusive: a second handle cannot open the same overlay file.
    let in_use = repo.workspace_open("w1");
    assert!(
        matches!(in_use, Err(WorkspaceError::Overlay(_))),
        "a second open of an in-use workspace must fail at the overlay, got {in_use:?}"
    );

    reopened.close().expect("close again");
    repo.workspace_discard("w1").expect("discard");
    assert_eq!(repo.workspace_list().expect("list").len(), 0);
    assert!(
        !repo.workspace_dir(&id).exists(),
        "discard removes the directory too"
    );
}

/// **Lifecycle failure paths** — every "not there / already there" shape is typed.
#[test]
fn lifecycle_failures_are_typed() {
    let (_root, repo) = repository("lifecycle-fail");

    assert!(matches!(
        repo.workspace_open("missing"),
        Err(WorkspaceError::NotFound { .. })
    ));
    assert!(matches!(
        repo.workspace_discard("missing"),
        Err(WorkspaceError::NotFound { .. })
    ));
    assert!(matches!(
        repo.workspace_create("w1", "no-such-branch"),
        Err(WorkspaceError::NotFound { .. })
    ));

    let workspace = repo.workspace_create("w1", "main").expect("create");
    workspace.close().expect("close");
    assert!(matches!(
        repo.workspace_create("w1", "main"),
        Err(WorkspaceError::AlreadyExists { .. })
    ));
    assert!(matches!(
        repo.branch_create("main", None),
        Err(WorkspaceError::AlreadyExists { .. })
    ));
    assert!(matches!(
        repo.init(),
        Err(WorkspaceError::AlreadyExists { .. })
    ));
    assert!(matches!(
        repo.workspace_rebind("w1", "nope"),
        Err(WorkspaceError::NotFound { .. })
    ));
}

/// **§18.8** — a commit with no change is refused, and nothing moves.
#[test]
fn nothing_to_commit_when_the_view_equals_the_base() {
    let (_root, repo) = repository("nothing");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    let before = repo.branch_head("main").expect("head");
    assert!(matches!(
        workspace.commit("empty", false),
        Err(WorkspaceError::NothingToCommit)
    ));
    assert_eq!(repo.branch_head("main").expect("head"), before);
}

/// **Acceptance: two snapshots of an unchanged workspace give the same root id.**
///
/// This is history independence (ADR-005) as seen from the workspace layer: re-encoding a
/// tree that did not change must reproduce the identical root, or the branch's content
/// token would be meaningless.
#[test]
fn two_snapshots_without_change_produce_the_same_root() {
    let (_root, repo) = repository("idempotent");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    Rules::new(workspace.store())
        .mkdir(&p("/a"))
        .expect("mkdir");
    put_file(&workspace, "/a/f", b"hello");

    let first = workspace.snapshot().expect("snapshot");
    assert_eq!(first.commit, None, "a snapshot does not move the branch");
    let second = workspace.snapshot().expect("snapshot again");
    assert_eq!(
        first.root, second.root,
        "an unchanged view must materialise to the same root"
    );
    // Clearing the overlay must not have changed the view itself.
    assert_eq!(read_file(&workspace, "/a/f"), b"hello");
    assert_eq!(names(&workspace, "/a"), vec!["f".to_owned()]);
}

/// **§18.5** — a commit moves the branch, and the tree stays readable afterwards.
#[test]
fn commit_moves_the_branch_and_grows_history() {
    let (_root, repo) = repository("commit");
    let initial = repo.branch_head("main").expect("head");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"first");

    let published = workspace.commit("one", false).expect("commit");
    let commit = published.commit.clone().expect("a commit id");
    assert_eq!(repo.branch_head("main").expect("head"), commit);
    let info = repo.commit_info(&commit).expect("info");
    assert_eq!(info.parent.as_deref(), Some(initial.as_str()));
    assert_eq!(info.message, "one");
    assert_eq!(info.root, published.root);
    assert_eq!(repo.log("main").expect("log").len(), 2);

    // The workspace adopted the commit, so its view is clean and based on the new head.
    assert_eq!(workspace.base_commit(), commit);
    assert!(!workspace.is_dirty().expect("dirty"));
    assert_eq!(read_file(&workspace, "/f"), b"first");

    // A second round: the parent chain keeps growing.
    put_file(&workspace, "/f", b"second");
    let second = workspace.commit("two", false).expect("commit").commit;
    let info = repo.commit_info(&second.expect("id")).expect("info");
    assert_eq!(info.parent.as_deref(), Some(commit.as_str()));
    assert_eq!(read_file(&workspace, "/f"), b"second");
}

/// **§18.5 / I1** — a published tree is readable with no workspace and no rows left.
#[test]
fn a_committed_tree_is_readable_after_the_workspace_is_gone() {
    let (_root, repo) = repository("zero-table");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/kept", b"payload");
    let commit = workspace.commit("keep", false).expect("commit").commit;
    workspace.close().expect("close");

    // Drop every trace of the workspace, as §18.5 requires.
    repo.workspace_discard("w1").expect("discard");

    let root = repo
        .commit_info(&commit.clone().expect("id"))
        .expect("info")
        .root;
    let store = Store::open(repo.store_dir(), OpenMode::ReadOnly).expect("store");
    // `open_any` reads the profile the root records, which is how a v2 build still opens a
    // tree it published under the current profile.
    let reader = fs_snapshot::SnapshotReader::open_any(&store, root.parse().expect("root id"))
        .expect("reader");
    let root_inode = reader.inode(0).expect("root inode").expect("present");
    let serial = reader
        .lookup(&root_inode, b"kept")
        .expect("lookup")
        .expect("entry");
    let inode = reader.inode(serial).expect("inode").expect("present");
    assert_eq!(inode.size, 7);
}

/// **F1 / §18.6** — two workspaces on one branch: the second is refused, keeps its work, and
/// `--force` records the displaced head.
#[test]
fn branch_moved_is_typed_and_force_records_the_displaced_head() {
    let (_root, repo) = repository("f1");
    let base = repo.branch_head("main").expect("head");
    let mut first = repo.workspace_create("w1", "main").expect("create");
    let mut second = repo.workspace_create("w2", "main").expect("create");

    put_file(&first, "/one", b"first");
    put_file(&second, "/two", b"second");

    let winner = first.commit("from w1", false).expect("commit").commit;
    let displaced = winner.clone().expect("id");

    let refused = second.commit("from w2", false);
    match refused {
        Err(WorkspaceError::BranchMoved {
            branch,
            expected,
            actual,
        }) => {
            assert_eq!(branch, "main");
            assert_eq!(
                expected, base,
                "the expectation is the workspace's base commit"
            );
            assert_eq!(actual, displaced, "the actual head is the other commit");
        }
        other => panic!("expected BranchMoved, got {other:?}"),
    }
    // Exactly §10.3's table: the branch and the workspace are untouched, the work is still
    // in the overlay.
    assert_eq!(repo.branch_head("main").expect("head"), displaced);
    assert_eq!(second.base_commit(), base);
    assert!(second.is_dirty().expect("dirty"));
    assert_eq!(read_file(&second, "/two"), b"second");

    let forced = second
        .commit("from w2, forced", true)
        .expect("force")
        .commit;
    let forced = forced.expect("id");
    assert_eq!(repo.branch_head("main").expect("head"), forced);
    let info = repo.commit_info(&forced).expect("info");
    assert_eq!(
        info.parent.as_deref(),
        Some(base.as_str()),
        "a forced commit's parent is the workspace's base, not the displaced head"
    );

    let (op, old) = repo
        .branch_log_entry_for("main", &forced)
        .expect("log entry");
    assert_eq!(op, "commit-force");
    assert_eq!(old.as_deref(), Some(displaced.as_str()));
}

/// **§10.3 出路 1** — after a conflict, rebind onto a branch created from the base commit.
#[test]
fn rebind_lets_the_loser_publish_on_another_branch() {
    let (_root, repo) = repository("rebind");
    let base = repo.branch_head("main").expect("head");
    let mut first = repo.workspace_create("w1", "main").expect("create");
    let mut second = repo.workspace_create("w2", "main").expect("create");
    put_file(&first, "/one", b"first");
    put_file(&second, "/two", b"second");
    first.commit("from w1", false).expect("commit");
    assert!(matches!(
        second.commit("from w2", false),
        Err(WorkspaceError::BranchMoved { .. })
    ));

    repo.branch_create("side", Some(&base)).expect("branch");
    second.rebind("side").expect("rebind");
    assert_eq!(second.branch(), "side");
    let published = second.commit("from w2", false).expect("commit").commit;
    assert_eq!(
        repo.branch_head("side").expect("head"),
        published.expect("id")
    );
    // The original branch is untouched by the rebind.
    assert_ne!(repo.branch_head("main").expect("head"), base);
}

/// **§18.7** — reset moves the head without growing history; revert grows it.
#[test]
fn reset_moves_and_revert_grows() {
    let (_root, repo) = repository("history");
    let initial = repo.branch_head("main").expect("head");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"one");
    let first = workspace.commit("one", false).expect("commit").commit;
    let first = first.expect("id");
    put_file(&workspace, "/f", b"two");
    let second = workspace.commit("two", false).expect("commit").commit;
    let second = second.expect("id");

    // reset: the branch moves back, history does not grow.
    repo.reset("main", &first, Some(&second)).expect("reset");
    assert_eq!(repo.branch_head("main").expect("head"), first);
    assert_eq!(repo.log("main").expect("log").len(), 2);

    // reset with a stale expectation is a typed conflict.
    assert!(matches!(
        repo.reset("main", &initial, Some(&second)),
        Err(WorkspaceError::BranchMoved { .. })
    ));

    // revert: a new commit whose root is the head's parent's root.
    let reverted = repo.revert("main").expect("revert");
    assert_eq!(repo.branch_head("main").expect("head"), reverted.id);
    // Revert undoes the *head*, which after the reset is `first`, so the new commit's tree
    // is `first`'s parent's tree — the initial empty one.
    assert_eq!(reverted.parent.as_deref(), Some(first.as_str()));
    let undone = repo.commit_info(&first).expect("info");
    let before = repo
        .commit_info(undone.parent.as_deref().expect("first has a parent"))
        .expect("info");
    assert_eq!(reverted.root, before.root);
    assert_ne!(
        reverted.root, undone.root,
        "revert must change the tree back"
    );
    assert_eq!(repo.log("main").expect("log").len(), 3);
}

/// **§10.2 `restore`** — the view changes, the base commit does not.
#[test]
fn restore_changes_the_view_but_not_the_base_commit() {
    let (_root, repo) = repository("restore");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"one");
    let first = workspace.commit("one", false).expect("commit").commit;
    let first = first.expect("id");
    put_file(&workspace, "/f", b"two");
    let second = workspace.commit("two", false).expect("commit").commit;
    let second = second.expect("id");
    assert_eq!(read_file(&workspace, "/f"), b"two");

    // Dirty workspaces refuse unless told to discard.
    put_file(&workspace, "/f", b"dirty");
    assert!(matches!(
        workspace.restore(&first, false),
        Err(WorkspaceError::WorkspaceDirty)
    ));
    workspace.restore(&first, true).expect("restore");
    assert_eq!(workspace.base_commit(), second, "restore keeps base_commit");
    assert_eq!(read_file(&workspace, "/f"), b"one");
    assert!(!workspace.is_dirty().expect("dirty"));

    // A commit now records "put the content back", growing history past `second`.
    let restored = workspace
        .commit("back to one", false)
        .expect("commit")
        .commit;
    let info = repo.commit_info(&restored.expect("id")).expect("info");
    assert_eq!(info.parent.as_deref(), Some(second.as_str()));
    assert_eq!(read_file(&workspace, "/f"), b"one");
}

/// **§11.1 / F2** — the main database waits a bounded time, then reports `Busy`.
///
/// A second connection holds the write lock for longer than the timeout; the publication
/// path must give up after roughly the configured wait rather than hang or fail instantly.
#[test]
fn a_held_main_database_lock_ends_in_a_typed_busy() {
    let (_root, repo) = repository("busy");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"x");

    let blocker = rusqlite::Connection::open(repo.metadata_path()).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    // A pure main-database write, so this isolates the metadata connection's wait.
    let started = Instant::now();
    let outcome = repo.branch_create("side", None);
    let waited = started.elapsed();
    let _ = blocker.execute_batch("ROLLBACK");

    match outcome {
        Err(WorkspaceError::Busy { waited_ms }) => {
            // `waited_ms` is **this crate's own wait**, measured — not the policy constant (the old code filled it with
            // `MAIN_BUSY_TIMEOUT_MS`, so a 5 ms failure claimed a 5000 ms wait). It is *not* the caller's total: the
            // driver's clock starts at the main-database lock, the test's starts earlier, so the two are not
            // comparable quantities (see `busy.rs` on clock origins).
            // **Only the policy bound is asserted.** The two empirical assertions that used to sit here
            // (a cross-clock comparison, and an empirical upper bound) compared quantities with different clock origins — the
            // driver starts its clock where it begins waiting on the database, the test starts its own before
            // `commit` — so they measured the machine, not the code (that is the flake QA reproduced). What remains is
            // the one claim this layer makes: the wait was configured to be at least the policy, and the caller is told
            // that much.
            assert!(
                waited_ms >= fs_workspace::MAIN_BUSY_TIMEOUT_MS,
                "cell reported wait: {waited_ms} ms must be at least the configured {} ms",
                fs_workspace::MAIN_BUSY_TIMEOUT_MS
            );
        }
        other => panic!("expected a typed Busy, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_secs(4),
        "the wait must be the configured ~5 s, measured {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "the wait must be bounded, measured {waited:?}"
    );
    // The lock is gone, and the failed attempt left nothing behind: the same call works.
    repo.branch_create("side", None)
        .expect("the same operation succeeds once the lock is released");
    // The workspace still holds its work and its base is unchanged.
    assert!(workspace.is_dirty().expect("dirty"));
}

/// **§9.3 `creating` recovery** — a workspace row left in `creating` is rebuilt on open.
#[test]
fn a_workspace_stuck_in_creating_is_rebuilt_when_opened() {
    let (_root, repo) = repository("creating");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    let id = workspace.id().to_owned();
    workspace.close().expect("close");

    // Simulate a crash between the row and the ready flag, with a poisoned overlay file.
    let connection = rusqlite::Connection::open(repo.metadata_path()).expect("connection");
    connection
        .execute(
            "UPDATE workspace SET state = 'creating' WHERE name = 'w1'",
            [],
        )
        .expect("mark creating");
    drop(connection);
    std::fs::write(
        repo.workspace_dir(&id).join("overlay.sqlite"),
        b"not a database",
    )
    .expect("poison");

    let reopened = repo.workspace_open("w1").expect("open rebuilds");
    assert_eq!(reopened.id(), id);
    assert!(!reopened.is_dirty().expect("dirty"));
    assert_eq!(names(&reopened, "/"), Vec::<String>::new());
    assert_eq!(
        repo.workspace_list().expect("list")[0].state,
        "ready",
        "recovery flips the row back to ready"
    );
    // And it is usable again.
    put_file(&reopened, "/f", b"after recovery");
    assert!(reopened.is_dirty().expect("dirty"));
}

/// **§9.3 orphan sweep** — a workspace directory with no row is removed on open.
#[test]
fn an_orphan_workspace_directory_is_swept_on_open() {
    let (root, repo) = repository("sweep");
    let orphan = repo.workspace_dir("deadbeef");
    std::fs::create_dir_all(&orphan).expect("orphan dir");
    std::fs::write(orphan.join("overlay.sqlite"), b"junk").expect("file");
    drop(repo);

    let reopened = fs_workspace::Repo::open(root.join("store"), false).expect("reopen");
    assert!(!orphan.exists(), "a directory without a row must be swept");
    assert_eq!(reopened.workspace_list().expect("list").len(), 0);
}

/// **§6** — opening a store that has no workspace tables is a typed error, not a panic.
#[test]
fn a_store_without_workspace_tables_reports_not_initialised() {
    let root = TempRoot::new("not-initialised");
    let store = Store::open(root.join("store"), OpenMode::Create).expect("store");
    drop(store);
    assert!(matches!(
        fs_workspace::Repo::open(root.join("store"), false),
        Err(WorkspaceError::NotInitialised)
    ));
}

/// **§6** — each workspace reserves its own segment of the shared scope.
#[test]
fn every_workspace_reserves_a_serial_segment() {
    let (_root, repo) = repository("serials");
    let scope = repo.branches().expect("branches")[0].scope_id.clone();
    let read_next = |_label: &str| -> i64 {
        let connection = rusqlite::Connection::open(repo.metadata_path()).expect("connection");
        connection
            .query_row(
                "SELECT next_serial FROM scope_allocator WHERE scope_id = ?1",
                [&scope],
                |row| row.get(0),
            )
            .expect("next_serial")
    };
    let start = read_next("start");
    assert_eq!(start, 1, "init leaves serial 1 as the next unallocated one");
    let first = repo.workspace_create("w1", "main").expect("create");
    let after_first = read_next("first");
    assert_eq!(
        after_first - start,
        i64::try_from(fs_workspace::SERIAL_SEGMENT).expect("fits"),
        "a workspace reserves one segment"
    );
    first.close().expect("close");
    let second = repo.workspace_create("w2", "main").expect("create");
    assert_eq!(read_next("second") - after_first, 1024);
    // Two workspaces on one branch share the scope, so their serials cannot collide.
    assert_eq!(second.branch(), "main");
}

/// **K3 within one workspace** — writes through the tree backend are transactional.
#[test]
fn a_rejected_rule_leaves_the_view_unchanged() {
    let (_root, repo) = repository("atomic");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    Rules::new(workspace.store())
        .mkdir(&p("/a"))
        .expect("mkdir");
    let before = names(&workspace, "/");
    let refused = Rules::new(workspace.store()).mkdir(&p("/a"));
    assert!(refused.is_err(), "mkdir of an existing name must fail");
    assert_eq!(names(&workspace, "/"), before);
}

/// **§11.1 / KI-7** — contention on the CAS index is the same typed `Busy`, after the same wait.
///
/// Before the KI-7 ruling the CAS connection kept `busy_timeout = 0`, so a workspace whose
/// publication collided with another writer failed **immediately**, and with a different
/// error shape (`Store(Busy)`) than the metadata path. Now both connections read one number
/// from `workspace_policy`, and the caller cannot tell which of the two was blocked.
///
/// Two independent problems make this an **observation** rather than a criterion (see **B-12** in the backlog for the
/// follow-up that would make it discriminative):
///
/// 1. **Different clock origins.** `busy::start` runs where this crate begins waiting on the main database; the test's
///    own `Instant` starts before `commit` and therefore includes staging and connection work. `waited_ms` can
///    legitimately be *larger or smaller* than the test's `measured`, so `waited_ms ≈ measured` compares unlike
///    quantities, and a one-sided comparison across them is only an approximation that happens to hold here.
/// 2. **The policy is not configurable.** `workspace_policy.main_busy_timeout_ms` is pinned by the schema to the
///    compiled default, so "reported the constant" and "reported the measurement" are the same number.
///
/// Also: **This test is an observation, not a discriminator.** With `workspace_policy.main_busy_timeout_ms`
/// pinned by the schema to the compiled default, "reported the policy constant" and "reported the measured wait" are
/// the same number here, so the assertion cannot tell them apart. What it does establish: the CAS connection waits at
/// all, the refusal is the typed `Busy`, and the reported number does not *under*-report the wall clock. Making it
/// discriminative needs a configurable policy — **B-12**, a separate card, because that is a behaviour change. (Not
/// "B-1": the backlog's B-1 is Seal/Compact, and reusing that number would point a reader at the wrong item.)
#[test]
fn a_blocked_cas_index_also_waits_boundedly() {
    let (_root, repo) = repository("cas-busy");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"x");

    let blocker = rusqlite::Connection::open(repo.metadata_path()).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");

    let started = Instant::now();
    // Materialisation publishes objects, so this reaches cas-store's connection first.
    let outcome = workspace.commit("blocked", false);
    let waited = started.elapsed();
    let _ = blocker.execute_batch("ROLLBACK");

    match outcome {
        Err(WorkspaceError::Busy { waited_ms }) => {
            // `waited_ms` is **this crate's own wait**, measured — not the policy constant (the old code filled it with
            // `MAIN_BUSY_TIMEOUT_MS`, so a 5 ms failure claimed a 5000 ms wait). It is *not* the caller's total: the
            // driver's clock starts at the main-database lock, the test's starts earlier, so the two are not
            // comparable quantities (see `busy.rs` on clock origins).
            // **Only the policy bound is asserted.** The two empirical assertions that used to sit here
            // (a cross-clock comparison, and an empirical upper bound) compared quantities with different clock origins — the
            // driver starts its clock where it begins waiting on the database, the test starts its own before
            // `commit` — so they measured the machine, not the code (that is the flake QA reproduced). What remains is
            // the one claim this layer makes: the wait was configured to be at least the policy, and the caller is told
            // that much.
            assert!(
                waited_ms >= fs_workspace::MAIN_BUSY_TIMEOUT_MS,
                "cell reported wait: {waited_ms} ms must be at least the configured {} ms",
                fs_workspace::MAIN_BUSY_TIMEOUT_MS
            );
        }
        other => panic!("expected a typed Busy from the CAS connection, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_secs(4),
        "the CAS connection must wait too, measured {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "and the wait must still be bounded, measured {waited:?}"
    );
    // The failed attempt left the work in the overlay, and the same call now succeeds.
    assert!(workspace.is_dirty().expect("dirty"));
    workspace
        .commit("after the lock", false)
        .expect("the publication succeeds once the lock is released");
}

/// Fills a workspace with `count` one-byte files, the payload §11.1's acceptance uses.
fn fill(workspace: &Workspace, prefix: char, count: usize) {
    let store = workspace.store();
    for index in 0..count {
        let path = p(&format!("/{prefix}{index:04}"));
        store.create_file(&path).expect("create file");
        store.write_at(&path, 0, b"x").expect("write");
    }
}

/// **§11.1 acceptance (F2)** — two workspaces publish 1000 files each, at the same time.
///
/// Both must succeed: the point of the bounded wait is that correct concurrent use does not
/// fail at random. The durations are printed as the margin evidence the architect asked
/// for — if a contended publication of this size finished anywhere near the 5 s wait, the
/// wait would be a routinely-hit path rather than a safety net.
///
/// The architect's other question — whether the per-object fsync happens while the index
/// write lane is held — is answered by `cas-store`'s publication sequence: `stage()` writes,
/// syncs and renames the object before `record()` opens the single index INSERT. No lock is
/// held across an fsync, so a batch publisher (ADR-006 §12, P2a-5) can keep the same
/// discipline: fsync the chunk first, then one short index transaction.
#[test]
fn two_workspaces_publish_a_thousand_files_concurrently() {
    const FILES: usize = 1000;

    let (_root, repo) = repository("concurrent");
    let base = repo.branch_head("main").expect("head");
    repo.branch_create("side", Some(&base)).expect("branch");
    let left = repo.workspace_create("left", "main").expect("create");
    let right = repo.workspace_create("right", "side").expect("create");
    fill(&left, 'l', FILES);
    fill(&right, 'r', FILES);

    let left_thread = std::thread::spawn(move || {
        let mut left = left;
        let started = Instant::now();
        let outcome = left.commit("left", false);
        (started.elapsed(), outcome)
    });
    let right_thread = std::thread::spawn(move || {
        let mut right = right;
        let started = Instant::now();
        let outcome = right.commit("right", false);
        (started.elapsed(), outcome)
    });

    let (left_time, left_outcome) = left_thread.join().expect("left thread");
    let (right_time, right_outcome) = right_thread.join().expect("right thread");
    let left = left_outcome.expect("the left publication must survive contention");
    let right = right_outcome.expect("the right publication must survive contention");

    assert_ne!(left.root, right.root, "different content, different roots");
    assert_ne!(left.commit, right.commit);
    for (label, elapsed) in [("left", left_time), ("right", right_time)] {
        eprintln!("§11.1 concurrent snapshot of {FILES} files: {label} {elapsed:?}");
    }
    let worst = left_time.max(right_time);
    assert!(
        worst < Duration::from_secs(120),
        "a concurrent publication must not be starved, took {worst:?}"
    );
}

/// **§6 / regression** — a workspace may create more files than one serial segment holds.
///
/// The overlay's own content API cannot reserve serials (the allocator is in the main
/// database), so a workspace that bypassed `Workspace::create_file` used to run dry after
/// 1024 new files with `ov_meta.serial_next: reserved range is exhausted`. Found by the
/// P2a-8 flush benchmark at N=10 000.
#[test]
fn a_workspace_can_create_more_files_than_one_serial_segment() {
    let (_root, repo) = repository("serial-growth");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    // Two and a half segments: past the first reservation, and past one top-up.
    let count = 2500;
    for index in 0..count {
        let path = format!("/f{index:05}");
        workspace.create_file(&path).expect("create file");
        workspace.write_at(&path, 0, b"x").expect("write");
    }
    // The top-up happened: the scope's next serial moved past the first segment.
    let scope = repo.branches().expect("branches")[0].scope_id.clone();
    let connection = rusqlite::Connection::open(repo.metadata_path()).expect("connection");
    let next: i64 = connection
        .query_row(
            "SELECT next_serial FROM scope_allocator WHERE scope_id = ?1",
            [&scope],
            |row| row.get(0),
        )
        .expect("next_serial");
    assert!(
        next > 1 + i64::try_from(fs_workspace::SERIAL_SEGMENT).expect("fits"),
        "more than one segment must have been reserved, next = {next}"
    );
    // And the files are all there.
    assert_eq!(names(&workspace, "/").len(), count);
}
/// **§9.1 / regression** — the publication transaction is `BEGIN IMMEDIATE`.
///
/// A deferred transaction reads `branch.head_commit` and then writes. Under WAL, a
/// contested *upgrade* of a read snapshot answers `SQLITE_BUSY_SNAPSHOT` **without running
/// the busy handler**, so the 5 s bounded wait would never be used: with ten workspaces
/// publishing at once, two failed after ~200 ms instead of waiting. Found by acceptance
/// row A1.
///
/// This test does not need ten threads: it holds the write lane from another connection
/// and asserts the publication *waits* (rather than failing immediately), which is only
/// true if the transaction asks for the write lock up front.
#[test]
fn a_publication_waits_for_the_write_lane_before_reading_the_head() {
    let (_root, repo) = repository("immediate");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"x");

    let blocker = rusqlite::Connection::open(repo.metadata_path()).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lane");

    // Release the blocker after 300 ms, so a correct publication waits and then succeeds;
    // a deferred one would fail almost immediately with SQLITE_BUSY_SNAPSHOT.
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = blocker.execute_batch("ROLLBACK");
        blocker
    });

    let started = Instant::now();
    let outcome = workspace.commit("waited", false);
    let waited = started.elapsed();
    drop(release.join().expect("release thread"));

    assert!(
        outcome.is_ok(),
        "the publication must wait for the lane and then succeed, got {outcome:?}"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "it must actually have waited, measured {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "and it must not have waited out the timeout, measured {waited:?}"
    );
}
