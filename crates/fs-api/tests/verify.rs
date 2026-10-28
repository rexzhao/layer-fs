//! `verify`, `stats`, and the refusal to open a P1 store.
//!
//! These cover the two commands the CLI gained in P2a-7b and the one place where a store
//! from an older schema must be named rather than silently reinterpreted.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{ApiError, Repo};

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
                "fs-api-verify-{label}-{}-{sequence}",
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

fn put(workspace: &fs_api::Workspace, path: &str, bytes: &[u8]) {
    if workspace.stat(path).is_err() {
        workspace.create(path).expect("create");
    }
    workspace.write_at(path, 0, bytes).expect("write");
    workspace
        .truncate(path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

/// A clean repository verifies at both levels, and `stats` agrees with what exists.
#[test]
fn verify_and_stats_agree_with_a_clean_repository() {
    common::strict_gate();
    let root = TempRoot::new("clean");
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    put(&workspace, "/a", b"payload");
    workspace.mkdir("/dir").expect("mkdir");
    put(&workspace, "/dir/b", b"more");
    workspace.commit("first", false).expect("commit");
    workspace.close().expect("close");

    let shallow = repo.verify(false).expect("verify");
    assert!(shallow.is_clean(), "got {shallow:?}");
    assert!(shallow.objects >= 4, "objects: {}", shallow.objects);
    assert_eq!(shallow.roots_checked, 2, "the genesis and the commit");
    assert_eq!(shallow.inodes_checked, 0, "shallow verify walks no inodes");

    let deep = repo.verify(true).expect("deep verify");
    assert!(deep.is_clean(), "got {deep:?}");
    // Two roots are walked: the genesis tree (its root directory) and the commit's tree
    // (root directory + /a + /dir + /dir/b).
    assert_eq!(deep.inodes_checked, 5, "got {deep:?}");

    let stats = repo.stats().expect("stats");
    assert_eq!(stats.branches, 1);
    assert_eq!(stats.commits, 2, "genesis + first");
    assert_eq!(stats.workspaces, 1);
    assert_eq!(stats.snapshots, 0);
    assert_eq!(stats.branch_log_entries, 2, "init + commit");
    assert!(stats.objects >= 4);
}

/// A verified snapshot keeps verifying after a workspace is discarded, because the commit
/// — not the workspace — is what holds the tree.
#[test]
fn verify_still_passes_after_the_workspace_is_gone() {
    common::strict_gate();
    let root = TempRoot::new("after-discard");
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    put(&workspace, "/f", b"kept");
    workspace.snapshot().expect("snapshot");
    workspace.commit("committed", false).expect("commit");
    workspace.close().expect("close");
    repo.workspace_discard("w1").expect("discard");

    let report = repo.verify(true).expect("verify");
    assert!(report.is_clean(), "got {report:?}");
    assert_eq!(report.roots_checked, 2, "the snapshot row and the commit");
}

/// **A P1 store is refused by name**, not silently reinterpreted as an empty one.
#[test]
fn a_previous_schema_is_named_and_refused() {
    common::strict_gate();
    let root = TempRoot::new("legacy");
    let store = root.store();
    std::fs::create_dir_all(&store).expect("store dir");
    // The shape P1 left behind: a metadata database with `namespace_policy` and no
    // workspace tables.
    // A real cas-store first: the metadata database is shared, so a P1 store has
    // cas-store's tables *and* the namespace tables.
    drop(cas_store::Store::open(&store, cas_store::OpenMode::Create).expect("cas store"));
    let path = store.join("store.sqlite");
    let connection = rusqlite::Connection::open(&path).expect("open");
    connection
        .execute_batch(
            "CREATE TABLE namespace_policy (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL) STRICT;
             INSERT INTO namespace_policy (key, value) VALUES ('schema_version', '2');",
        )
        .expect("create P1 tables");
    drop(connection);

    match Repo::open(&store) {
        Err(ApiError::Workspace(fs_workspace::WorkspaceError::SchemaMismatch {
            found, ..
        })) => {
            assert!(found.contains("namespace_policy"), "got {found}");
        }
        other => panic!("expected a schema mismatch naming the P1 tables, got {other:?}"),
    }
    // And it is not silently created over.
    assert!(Repo::create(&store).is_err(), "create must not migrate");
}

/// **A damaged store is reported, not hidden.**
///
/// This is the part of the retired `fs-api/tests/consistency.rs` that still means
/// something: when something referenced is missing, `verify` must name it. The old suite
/// damaged namespace *rows*; there are no rows any more, so the equivalent damage is a
/// deleted object file.
#[test]
fn verify_names_an_object_that_is_gone() {
    common::strict_gate();
    let root = TempRoot::new("damaged");
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    put(&workspace, "/f", b"payload that will lose its object");
    let commit = workspace.commit("first", false).expect("commit");
    workspace.close().expect("close");

    // Remove the packs that hold the commit's objects, leaving the index rows behind.
    // ADR-008 §3: objects live in packs now, so "the object file is gone" means "the pack
    // that carries it is gone" — and the index still claims it.
    let info = repo.commit_info(&commit).expect("info");
    let packs = root.store().join("packs");
    let removed = std::fs::read_dir(&packs)
        .expect("packs")
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| std::fs::remove_file(entry.path()).expect("remove pack"))
        .count();
    assert!(
        removed >= 1,
        "expected at least one pack under {}",
        packs.display()
    );

    let report = repo.verify(false).expect("verify");
    assert!(!report.is_clean(), "a missing object must be reported");
    assert!(
        report
            .dangling
            .iter()
            .any(|entry| entry.object == info.root),
        "the report must name the missing root: {report:?}"
    );
}
