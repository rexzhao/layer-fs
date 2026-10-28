//! The `Repo`/`Workspace`/`ReadView` surface: one round trip, end to end.
//!
//! These are the tests that pin the **shape** of the new API (ADR-006 §13) while the old
//! `Fs` surface still exists. They deliberately go through nothing but `fs_api`: a caller
//! that only has `Repo` must be able to create a store, edit a workspace, publish a
//! commit, and read that commit back later.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{ApiError, Kind, Repo};

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
                "fs-api-workspace-{label}-{}-{sequence}",
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

/// Writes a whole file the way a caller would: create when absent, then write and size it.
fn put(workspace: &fs_api::Workspace, path: &str, bytes: &[u8]) {
    if workspace.stat(path).is_err() {
        workspace.create(path).expect("create");
    }
    workspace.write_at(path, 0, bytes).expect("write");
    workspace
        .truncate(path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

/// **ADR-006 §13** — the whole surface in one pass: create, edit, commit, read history.
#[test]
fn a_workspace_round_trip_publishes_a_readable_commit() {
    common::strict_gate();
    let root = TempRoot::new("round-trip");
    let repo = Repo::create(root.store()).expect("create");

    // A fresh store has one branch with one commit.
    let branches = repo.branches().expect("branches");
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "main");
    let genesis = repo.branch_head("main").expect("head");

    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    assert_eq!(workspace.name(), "w1");
    assert_eq!(workspace.branch(), "main");
    assert_eq!(workspace.base_commit(), genesis);
    assert!(!workspace.status().expect("status").dirty);

    workspace.mkdir("/docs").expect("mkdir");
    put(&workspace, "/docs/readme", b"hello, workspace");
    put(&workspace, "/docs/second", b"x");
    assert!(workspace.status().expect("status").dirty);

    // Path rules still apply through the new surface.
    assert_eq!(
        workspace
            .read_dir("/docs")
            .expect("read_dir")
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
        vec!["readme".to_owned(), "second".to_owned()]
    );
    let stat = workspace.stat("/docs/readme").expect("stat");
    assert_eq!(stat.kind, Kind::File);
    assert_eq!(stat.size, 16);
    let mut buffer = [0u8; 5];
    assert_eq!(
        workspace
            .read_at("/docs/readme", 0, &mut buffer)
            .expect("read"),
        5
    );
    assert_eq!(&buffer, b"hello");

    // Rename inside the workspace, then remove one entry.
    workspace
        .rename("/docs/second", "/docs/third")
        .expect("rename");
    assert_eq!(workspace.read_dir("/docs").expect("dir").len(), 2);
    workspace.remove("/docs/third").expect("remove");
    assert_eq!(workspace.read_dir("/docs").expect("dir").len(), 1);

    let commit = workspace.commit("first", false).expect("commit");
    assert_eq!(repo.branch_head("main").expect("head"), commit);
    assert_eq!(workspace.base_commit(), commit);
    assert!(!workspace.status().expect("status").dirty);

    // The history records both commits, newest first.
    let log = repo.log("main").expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].id, commit);
    assert_eq!(log[0].parent.as_deref(), Some(genesis.as_str()));

    // A second commit on top, to give the history something to compare.
    put(&workspace, "/docs/readme", b"hello, again!!!!");
    let second = workspace.commit("second", false).expect("commit");
    assert_eq!(repo.log("main").expect("log").len(), 3);
    workspace.close().expect("close");

    // ---- history is readable with no table of ours involved (I1) ----
    let first_view = repo.read_view(&commit).expect("read_view");
    assert_eq!(
        first_view.read_all("/docs/readme").expect("read_all"),
        b"hello, workspace"
    );
    let entries = first_view.read_dir("/docs").expect("read_dir");
    assert_eq!(entries.len(), 1, "the removed entry must not come back");
    assert_eq!(entries[0].name, "readme");
    assert_eq!(
        first_view.stat("/docs").expect("stat").kind,
        Kind::Directory
    );
    assert_eq!(
        first_view.root().to_string(),
        repo.commit_info(&commit).expect("info").root
    );

    let second_view = repo.read_view(&second).expect("read_view");
    assert_eq!(
        second_view.read_all("/docs/readme").expect("read_all"),
        b"hello, again!!!!"
    );
    // A path that only exists in the old commit is not found in the newer one.
    assert!(second_view.stat("/docs/third").is_err());

    // And it stays readable after the workspace's own files are gone.
    repo.workspace_discard("w1").expect("discard");
    let after = repo.read_view(&commit).expect("read_view after discard");
    assert_eq!(
        after.read_all("/docs/readme").expect("read_all"),
        b"hello, workspace"
    );
}

/// A missing path is reported as such, and `is_not_found` says so without matching enums.
#[test]
fn a_missing_path_is_a_not_found_error() {
    common::strict_gate();
    let root = TempRoot::new("not-found");
    let repo = Repo::create(root.store()).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");

    let error = workspace.stat("/nope").expect_err("must fail");
    assert!(error.is_not_found(), "got {error:?}");
    assert!(matches!(error, ApiError::Namespace(_)));

    // A path that is not absolute is a different failure, and not a "not found".
    let error = workspace.stat("relative").expect_err("must fail");
    assert!(!error.is_not_found(), "got {error:?}");
    assert!(matches!(error, ApiError::InvalidPath(_)));
}

/// `snapshot` publishes without moving the branch, and never loses the work.
#[test]
fn a_snapshot_does_not_move_the_branch() {
    common::strict_gate();
    let root = TempRoot::new("snapshot");
    let repo = Repo::create(root.store()).expect("create");
    let genesis = repo.branch_head("main").expect("head");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    put(&workspace, "/f", b"payload");

    let snapshot_root = workspace.snapshot().expect("snapshot");
    assert_eq!(repo.branch_head("main").expect("head"), genesis);
    // The snapshot became the view's base, so the workspace is clean but the branch is not.
    assert!(!workspace.status().expect("status").dirty);
    assert_eq!(workspace.status().expect("status").base_root, snapshot_root);
    assert_eq!(
        {
            let mut buffer = [0u8; 7];
            workspace.read_at("/f", 0, &mut buffer).expect("read");
            buffer
        },
        *b"payload"
    );

    // Committing now still works: the view differs from the genesis commit's tree.
    let commit = workspace.commit("from snapshot", false).expect("commit");
    let view = repo.read_view(&commit).expect("read_view");
    assert_eq!(view.read_all("/f").expect("read"), b"payload");
}

/// **F1 through the new surface** — a moved branch is a typed error and the work survives.
#[test]
fn a_moved_branch_is_typed_and_keeps_the_work() {
    common::strict_gate();
    let root = TempRoot::new("branch-moved");
    let repo = Repo::create(root.store()).expect("create");
    let base = repo.branch_head("main").expect("head");
    let mut left = repo.workspace_create("left", "main").expect("workspace");
    let mut right = repo.workspace_create("right", "main").expect("workspace");
    put(&left, "/left", b"1");
    put(&right, "/right", b"2");

    let published = left.commit("left", false).expect("commit");
    let error = right.commit("right", false).expect_err("must conflict");
    match error {
        ApiError::Workspace(fs_workspace::WorkspaceError::BranchMoved {
            expected, actual, ..
        }) => {
            assert_eq!(expected, base);
            assert_eq!(actual, published);
        }
        other => panic!("expected BranchMoved, got {other:?}"),
    }
    // Nothing moved, and the loser's work is still there.
    assert_eq!(repo.branch_head("main").expect("head"), published);
    assert!(right.status().expect("status").dirty);
    assert_eq!(
        {
            let mut buffer = [0u8; 1];
            right.read_at("/right", 0, &mut buffer).expect("read");
            buffer
        },
        *b"2"
    );

    // The escape hatch the ADR documents: force, which records the displaced head.
    let forced = right.commit("right, forced", true).expect("force");
    assert_eq!(repo.branch_head("main").expect("head"), forced);
    assert_eq!(
        repo.commit_info(&forced).expect("info").parent.as_deref(),
        Some(base.as_str()),
        "a forced commit's parent is the workspace's base"
    );
}

/// `NotFound` from the workspace layer is surfaced, not swallowed.
#[test]
fn an_unknown_workspace_or_branch_is_reported() {
    common::strict_gate();
    let root = TempRoot::new("unknown");
    let repo = Repo::create(root.store()).expect("create");
    assert!(matches!(
        repo.workspace_open("missing"),
        Err(ApiError::Workspace(
            fs_workspace::WorkspaceError::NotFound { .. }
        ))
    ));
    assert!(matches!(
        repo.workspace_create("w1", "no-such-branch"),
        Err(ApiError::Workspace(
            fs_workspace::WorkspaceError::NotFound { .. }
        ))
    ));
    // Reading a commit that never existed is a workspace-layer error too.
    assert!(matches!(
        repo.read_view("00"),
        Err(ApiError::Workspace(
            fs_workspace::WorkspaceError::NotFound { .. }
        ))
    ));
}
