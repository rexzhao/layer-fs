//! Mark: the reachable set (ADR-008 §6.3, P2b-6 §13, §18.1).
//!
//! These tests are about **what mark puts in the marked set**, not about sweep. Two properties are
//! load-bearing and both are negative-capable:
//!
//! - a root is marked **before** its children are walked, so a root that references nothing is still
//!   reachable (otherwise "every declared root is reachable" passes on a run that visited nothing);
//! - a decode failure **aborts**, it does not skip the subtree: skipping is deleting.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cas_core::{Object, ObjectId, Role};
use cas_store::{OpenMode, Store};
use fs_namespace::Path as FsPath;
use fs_snapshot::SnapshotReader;
use fs_workspace::gc::{
    mark_roots, GcHooks, Marker, RepoRoots, RootKind, RootRef, RootSet, RootSource,
};
use fs_workspace::{Repo, WorkspaceError};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gcmark-{label}-{}-{}",
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

/// `{(relative path, length)}` over the store directory, `-shm` excluded (ADR-008 §6.1).
fn fingerprint(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read dir").flatten() {
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                let relative = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("inside")
                    .to_string_lossy()
                    .replace('\\', "/");
                if !relative.ends_with("-shm") && !relative.ends_with(Marker::FILE_NAME) {
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    out.sort();
    out
}

fn repository(label: &str) -> (TempRoot, Arc<Repo>) {
    let root = TempRoot::new(label);
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    (root, repo)
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

/// The production root source for this repository.
fn roots_of(repo: &Repo) -> RootSet {
    let source = RepoRoots { repo };
    RootSource::roots(&source).expect("roots")
}

/// A store handle opened **outside** the repository, so a test's view of the objects does not go
/// through the code under test.
fn store_of(root: &TempRoot) -> Store {
    Store::open(root.store(), OpenMode::ReadWrite).expect("open store")
}

/// The commit's tree root, as the production source computes it.
fn head_root(repo: &Repo) -> ObjectId {
    let roots = roots_of(repo);
    roots
        .entries()
        .iter()
        .find(|entry| entry.kind == RootKind::Commit)
        .expect("a commit root")
        .root
}

#[test]
fn mark_reaches_the_file_content_of_a_commit() {
    common::strict_gate();
    let (root, repo) = repository("content");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"the bytes that must survive");
    workspace.commit("one", false).expect("commit");
    drop(workspace);

    let root_oid = head_root(&repo);
    let declared = roots_of(&repo);
    let store = store_of(&root);
    let reader = SnapshotReader::open_any(&store, root_oid).expect("reader");
    let mut marker = Marker::create_in(root.store()).expect("marker");
    let report = mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut GcHooks::default(),
    )
    .expect("mark");

    // Hand-computed: the object the file's bytes hash to must be in the marked set. Its id depends
    // only on the bytes and the role, not on the tree, the commit or the traversal — so this
    // expectation is independent of mark.
    //
    // Both roles are computed because the writer decides (`Bytes` for a whole-file object, `Chunk`
    // for a chunked one) and this test must not assume which: **exactly one** of the two must be
    // there. Zero means the content was not reached; two would mean the fixture is confused.
    let as_bytes = Object::new(Role::Bytes, b"the bytes that must survive")
        .expect("framed")
        .id()
        .expect("hashed");
    let as_chunk = Object::new(Role::Chunk, b"the bytes that must survive")
        .expect("framed")
        .id()
        .expect("hashed");
    let hits = [as_bytes, as_chunk]
        .into_iter()
        .filter(|id| report.reachable.contains(id))
        .count();
    assert_eq!(
        hits, 1,
        "the file's content object must be reachable exactly once (as Bytes or as Chunk); reachable = {} objects",
        report.reachable_count
    );
    assert!(
        report.reachable.contains(&root_oid),
        "the commit's tree root must itself be reachable"
    );
    assert_eq!(
        report.reachable_count,
        report.reachable.len(),
        "reachable_count must be the deduplicated size"
    );
}

/// §18.1 / §13.1: the root enters the marked set **before** expansion.
///
/// The probe is a root whose value references nothing: if the root were only marked while walking its
/// children, it would never be inserted, and a criterion asking "is every declared root reachable?"
/// would answer yes on a walk that marked nothing at all.
#[test]
fn a_root_that_references_nothing_is_still_marked() {
    common::strict_gate();
    let (root, repo) = repository("empty-root");
    let store = store_of(&root);
    let leaf = store
        .put(Role::Bytes, b"a root with no children")
        .expect("put")
        .oid;
    let declared = RootSet::from_entries(vec![RootRef::new(
        RootKind::Snapshot,
        leaf,
        "snapshot:childless",
    )]);

    let reader = SnapshotReader::open_any(&store, head_root(&repo)).expect("reader");
    let mut marker = Marker::create_in(root.store()).expect("marker");
    let report = mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut GcHooks::default(),
    )
    .expect("mark");

    assert_eq!(report.reachable, vec![leaf], "the root is the whole set");
    assert_eq!(report.reachable_count, 1);
    assert_eq!(report.roots_visited, vec!["snapshot:childless".to_owned()]);
}

/// Counter-example F's mark half: a skipped root contributes nothing to the marked set.
///
/// The paired control is in the same test: the same fixture without the hook marks a non-empty set,
/// so "mark always returns zero" cannot pass this test.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn a_skipped_root_marks_nothing_and_its_control_marks_something() {
    common::strict_gate();
    let (root, repo) = repository("skip");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"content that only a walked root reaches");
    workspace.commit("one", false).expect("commit");
    drop(workspace);

    let declared = roots_of(&repo);
    let root_oid = head_root(&repo);
    let store = store_of(&root);
    let reader = SnapshotReader::open_any(&store, root_oid).expect("reader");

    // Control: no hook, so the subtree is walked.
    let mut marker_control = Marker::create_in(root.store()).expect("marker");
    let control = mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker_control,
        &mut GcHooks::default(),
    )
    .expect("mark");
    assert!(
        control.reachable_count > 1,
        "the control must mark more than the root itself, got {}",
        control.reachable_count
    );
    marker_control.remove().expect("remove the control marker");

    // Injection: skip every root. The skip seam answers "should this root be skipped?".
    let mut marker = Marker::create_in(root.store()).expect("marker");
    let mut skip = |_: &RootRef| true;
    let mut hooks = GcHooks {
        skip_root: Some(&mut skip),
        ..GcHooks::default()
    };
    let report = mark_roots(&declared, &declared, &reader, &mut marker, &mut hooks).expect("mark");
    assert!(
        report.reachable.is_empty(),
        "no root was visited, so nothing may be reachable: {:?}",
        report.reachable
    );
    assert!(report.roots_visited.is_empty(), "G3's left-hand side");
    assert!(
        !report.roots_declared.is_empty(),
        "the roots were still declared; that asymmetry is exactly what G3 catches"
    );
}

/// §12: a decode failure aborts the whole mark, and nothing in the store moves.
///
/// The positive control is in the test: the sound root set marks fine first, so an implementation that
/// always failed would not pass.
#[test]
fn a_damaged_object_aborts_the_mark_and_changes_nothing() {
    common::strict_gate();
    let (root, repo) = repository("damaged");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"still here");
    workspace.commit("one", false).expect("commit");
    drop(workspace);

    let declared = roots_of(&repo);
    let store = store_of(&root);
    let reader = SnapshotReader::open_any(&store, head_root(&repo)).expect("reader");

    // Control: the real roots mark without error.
    let mut marker = Marker::create_in(root.store()).expect("marker");
    mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut GcHooks::default(),
    )
    .expect("the sound root set must mark");
    marker.remove().expect("remove the control marker");

    // A truncated inode table, published as an object and declared as the only root.
    let damaged = store
        .put(Role::InodeTable, &[1u8, 0, 1, 0, 0, 0])
        .expect("put")
        .oid;
    let bad = RootSet::from_entries(vec![RootRef::new(
        RootKind::Snapshot,
        damaged,
        "snapshot:damaged",
    )]);
    let mut marker = Marker::create_in(root.store()).expect("marker");
    let before = fingerprint(&root.store());
    let error = mark_roots(&bad, &bad, &reader, &mut marker, &mut GcHooks::default())
        .expect_err("a damaged object must abort the mark");
    assert!(
        matches!(error, WorkspaceError::Snapshot(_)),
        "expected a typed snapshot error, got {error:?}"
    );
    assert_eq!(
        marker.count().expect("count"),
        1,
        "only the root itself was marked before the failure"
    );
    assert_eq!(
        before,
        fingerprint(&root.store()),
        "an aborted mark must not change a byte of the store"
    );
}

/// §6.3: the marker lives under the store root (not under `tmp/`, which the sweep deletes wholesale),
/// and removing it takes its companions with it.
#[test]
fn the_marker_lives_beside_the_store_and_is_removed_cleanly() {
    common::strict_gate();
    let (root, repo) = repository("marker-place");
    let declared = roots_of(&repo);
    let store = store_of(&root);
    let reader = SnapshotReader::open_any(&store, head_root(&repo)).expect("reader");
    let mut marker = Marker::create_in(root.store()).expect("marker");

    let path = marker.path().to_path_buf();
    assert_eq!(path.parent().expect("parent"), root.store());
    assert!(
        !path.to_string_lossy().contains("tmp"),
        "the marker must not live under tmp/: {}",
        path.display()
    );
    mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut GcHooks::default(),
    )
    .expect("mark");

    marker.remove().expect("remove");
    assert!(!path.exists(), "the marker file must be gone");
    for suffix in ["-wal", "-shm"] {
        assert!(
            !PathBuf::from(format!("{}{suffix}", path.display())).exists(),
            "companion {suffix} must be gone too"
        );
    }
}

/// A marker left behind by a crashed run must not be read as this run's marks.
#[test]
fn a_stale_marker_is_replaced_not_inherited() {
    common::strict_gate();
    let (root, repo) = repository("stale");
    let declared = roots_of(&repo);
    let store = store_of(&root);
    let reader = SnapshotReader::open_any(&store, head_root(&repo)).expect("reader");

    let mut first = Marker::create_in(root.store()).expect("marker");
    mark_roots(
        &declared,
        &declared,
        &reader,
        &mut first,
        &mut GcHooks::default(),
    )
    .expect("mark");
    let planted = first.count().expect("count");
    assert!(planted > 0, "the first run must mark something");
    drop(first);

    let second = Marker::create_in(root.store()).expect("marker");
    assert_eq!(
        second.count().expect("count"),
        0,
        "a new run must start from an empty marked set"
    );
}

// ------------------------------------------- reading through the maintenance connection (§23)

/// P2b-6 §23 acceptance: both `ObjectSource` implementations answer the same for the same root, and
/// each is stable across two calls.
///
/// The maintenance side is the one that matters: it reads pack frames through the connection that
/// holds the store's lock, which is the only way marking can work during an offline sweep.
#[test]
fn the_maintenance_source_answers_what_the_store_answers() {
    common::strict_gate();
    let (root, repo) = repository("source");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"bytes that both sources must read");
    workspace.commit("one", false).expect("commit");
    drop(workspace);
    let root_oid = head_root(&repo);
    drop(repo);

    let maintenance = fs_workspace::MaintenanceRepo::open(root.store()).expect("maintenance");
    let source = fs_workspace::gc::MaintenanceSource::new(maintenance.connection(), root.store());
    let via_maintenance =
        SnapshotReader::open_any(&source, root_oid).expect("open through the maintenance source");
    let first = via_maintenance.references(root_oid).expect("references");
    let second = via_maintenance
        .references(root_oid)
        .expect("references again");
    assert_eq!(first, second, "the same root must read the same twice");
    assert!(
        !first.is_empty(),
        "a commit root references its inode table"
    );
    drop(maintenance);

    // The same question through a `&Store` — a different implementation, the same answer.
    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
    let via_store = SnapshotReader::open_any(&store, root_oid).expect("open through the store");
    assert_eq!(
        via_store.references(root_oid).expect("references"),
        first,
        "the two ObjectSource implementations must agree"
    );
}
