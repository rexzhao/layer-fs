//! The collector end to end (P2b-6 §18, §15): one run over a real store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cas_core::Role;
use cas_store::{OpenMode, Store};
use fs_namespace::Path as FsPath;
use fs_workspace::gc::{run, GcOptions};
use fs_workspace::Repo;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gcrun-{label}-{}-{}",
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

fn put_file(workspace: &fs_workspace::Workspace, path: &str, bytes: &[u8]) {
    let store = workspace.store();
    let path = FsPath::parse(path).expect("path");
    if store.content_state(&path).is_err() {
        store.create_file(&path).expect("create");
    }
    store.write_at(&path, 0, bytes).expect("write");
    store
        .truncate(&path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

/// A repository with one commit and one **orphan** object nobody references.
///
/// The orphan is what a failed or abandoned publication leaves behind: it has an index row, so the
/// sweep can see it, and no root reaches it, so it must go.
fn repo_with_one_orphan(label: &str) -> (TempRoot, cas_core::ObjectId, cas_core::ObjectId) {
    let root = TempRoot::new(label);
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(
        &workspace,
        "/f",
        b"the bytes that must survive a collection",
    );
    workspace.commit("one", false).expect("commit");
    drop(workspace);
    drop(repo);

    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
    let orphan = store
        .put(Role::Bytes, b"nobody references these bytes")
        .expect("put")
        .oid;
    let loser = store
        .get(orphan)
        .expect("the orphan is readable before the run");
    assert!(!loser.is_empty());
    drop(store);
    (root, orphan, orphan)
}

#[test]
fn a_run_frees_the_orphan_and_keeps_the_committed_tree_readable() {
    let (root, orphan, _) = repo_with_one_orphan("real");

    let outcome = run(root.store(), GcOptions::default()).expect("run the collector");
    assert_eq!(
        outcome.report.freed,
        vec![orphan],
        "exactly the orphan was freed: {:?}",
        outcome.report.freed
    );
    assert_eq!(
        outcome.report.marked_objects,
        outcome.report.reachable_count
    );
    assert!(
        outcome.report.marked_objects > 0,
        "G5's non-vacuity is about marked objects, not bytes"
    );
    assert!(
        outcome.report.freed_object_bytes > 0,
        "the byte count follows the deleted object"
    );

    // The dead object is gone, the live tree still reads.
    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
    assert!(store.get(orphan).is_err(), "the orphan must be unreadable");

    let repo = Repo::open(root.store(), false).expect("repo opens after a run");
    let head = repo.branch_head("main").expect("head");
    let info = repo.commit_info(&head).expect("info");
    let tree = info.root.parse::<cas_core::ObjectId>().expect("a root id");
    let reader = fs_snapshot::SnapshotReader::open_any(&store, tree).expect("root reads");
    assert!(
        !reader.references(tree).expect("references").is_empty(),
        "the committed tree must still be traversable"
    );
}

#[test]
fn a_dry_run_decides_but_changes_nothing() {
    let (root, orphan, _) = repo_with_one_orphan("dry");

    let outcome = run(
        root.store(),
        GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .expect("dry run");
    assert!(
        outcome.report.freed.is_empty(),
        "a dry run frees nothing: {:?}",
        outcome.report.freed
    );
    assert_eq!(
        outcome.plan.dead_objects,
        vec![orphan],
        "but it still decides"
    );
    assert!(outcome.applied.is_none());

    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("reopen read-only");
    assert!(
        store.get(orphan).is_ok(),
        "the orphan must still be there after a dry run"
    );
}

/// The threshold is a policy value; a run with an impossible one must be refused before anything moves.
#[test]
fn an_impossible_threshold_is_refused_before_anything_moves() {
    let (root, orphan, _) = repo_with_one_orphan("threshold");
    let error = run(
        root.store(),
        GcOptions {
            dry_run: false,
            repack_threshold: 1.5,
        },
    )
    .expect_err("1.5 is not a ratio");
    assert!(
        matches!(error, fs_workspace::WorkspaceError::Inconsistent { .. }),
        "expected a typed refusal, got {error:?}"
    );
    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("reopen");
    assert!(store.get(orphan).is_ok(), "a refusal changes nothing");
}

/// While a workspace is open, the collector refuses instead of collecting a half-visible store.
#[test]
fn a_run_refuses_while_a_workspace_is_open() {
    let root = TempRoot::new("busy");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"open workspace");

    let error = run(root.store(), GcOptions::default())
        .expect_err("an open overlay must make the collector refuse");
    assert!(
        matches!(error, fs_workspace::WorkspaceError::StoreInUse { .. }),
        "expected StoreInUse, got {error:?}"
    );
    assert!(
        Path::new(&root.store()).join("store.sqlite").is_file(),
        "the store is still there"
    );
}
