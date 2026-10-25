//! K5b/K5c: a shared object survives dropping **one** of the two roots that hold it, and is freed once
//! **both** are gone (P2b-6 §9).
//!
//! The two canaries are one fixture by design: K5b's "it survived" means nothing without K5c's "and then
//! it goes", and K5c's "it goes" means nothing if the object was never shared. So the fixture asserts
//! the sharing explicitly — the same object id, named by both roots — before either drop happens.
//!
//! The drop is the **real** `Repo::snapshot_drop`, not a hand-written `DELETE`: a row deleted by the test
//! proves the collector reacts to a missing row, not that dropping a snapshot is safe.

use std::path::PathBuf;
use std::sync::Arc;

use cas_core::{ObjectId, Role};
use cas_store::{OpenMode, Store};
use fs_namespace::Path as FsPath;
use fs_snapshot::SnapshotReader;
use fs_workspace::gc::{run, GcOptions};
use fs_workspace::{Repo, Workspace};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-k5-{label}-{}-{}",
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

fn put_file(workspace: &Workspace, path: &str, bytes: &[u8]) {
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

/// Every object id reachable from `root`, walked with `references` (the test's own walk, so the
/// expectation does not come from the collector).
fn reachable_from(store: &Store, root: ObjectId) -> Vec<ObjectId> {
    let reader = SnapshotReader::open_any(store, root).expect("open the root");
    let mut seen = std::collections::BTreeSet::new();
    let mut queue = vec![root];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        queue.extend(reader.references(oid).expect("references"));
    }
    seen.into_iter().collect()
}

/// K5b + K5c in one fixture: two snapshot roots share a content object.
///
/// - K5b: drop one root → a collection keeps the shared object, and the surviving root still reads.
/// - K5c: drop the other → the shared object is now unreachable, and the next collection frees it.
#[test]
fn a_shared_object_survives_dropping_one_root_and_is_freed_once_both_are_gone() {
    let root = TempRoot::new("shared");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");

    // Two trees over the **same bytes**: the content object is shared by construction (content
    // addressing), which is what makes "drop one root" a real question.
    put_file(&workspace, "/shared.txt", b"bytes both snapshots hold");
    let first = workspace.snapshot().expect("first snapshot");
    put_file(
        &workspace,
        "/extra.txt",
        b"only the second snapshot has this",
    );
    let second = workspace.snapshot().expect("second snapshot");
    let first_root: ObjectId = first.root.parse().expect("a root id");
    let second_root: ObjectId = second.root.parse().expect("a root id");
    assert_ne!(
        first_root, second_root,
        "the two roots must be different trees, or 'dropping one' is meaningless"
    );

    // Leave no third holder: `snapshot` moved the workspace's own `base_root` to the tree it published,
    // so the workspace row is a *third* root until it is moved back. Without this, K5c could never pass
    // and the test would silently be measuring the workspace rather than the snapshots.
    let head = repo.branch_head("main").expect("head");
    workspace
        .restore(&head, true)
        .expect("move the workspace back to the head");
    drop(workspace);
    // No `Repo` handle while the collector runs: it holds two connections, and the collector opens the
    // store exclusively — a live handle would make the collector refuse its own caller.
    drop(repo);

    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
    let from_first = reachable_from(&store, first_root);
    let from_second = reachable_from(&store, second_root);
    let shared: Vec<ObjectId> = from_first
        .iter()
        .copied()
        .filter(|oid| from_second.contains(oid))
        .collect();
    assert!(
        !shared.is_empty(),
        "the fixture must actually share objects between the two roots"
    );
    // The shared set includes the content object itself, not just the root/inode-table scaffolding:
    // prove it by hashing what the test wrote.
    let content = cas_core::Object::new(Role::Bytes, b"bytes both snapshots hold")
        .expect("framed")
        .id()
        .expect("id");
    let shared_content = shared.contains(&content)
        || shared.iter().any(|oid| {
            store
                .get(*oid)
                .map(|bytes| bytes.ends_with(b"bytes both snapshots hold"))
                .unwrap_or(false)
        });
    assert!(
        shared_content,
        "the shared set must include the file's content object, otherwise K5b proves nothing"
    );
    drop(store);

    // ------------------------------------------------ K5b: drop one root, the object survives
    {
        let handle = Repo::open(root.store(), false).expect("open for the drop");
        assert!(
            handle
                .snapshot_drop(&first_root.to_string())
                .expect("drop one"),
            "the first snapshot existed"
        );
    }
    let after_one = run(root.store(), GcOptions::default()).expect("collect after the first drop");
    for oid in &shared {
        assert!(
            !after_one.report.freed.contains(oid),
            "K5b: {oid} is still reachable from the surviving root and must not be freed"
        );
    }
    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
    assert!(
        store.get(content).is_ok() || shared.iter().any(|oid| store.get(*oid).is_ok()),
        "the surviving root still reads its bytes"
    );
    // The surviving root is still traversable end to end.
    assert!(
        !reachable_from(&store, second_root).is_empty(),
        "the second root still reads after the first was dropped"
    );
    drop(store);

    // ------------------------------------------------ K5c: drop the other, now it goes
    {
        let handle = Repo::open(root.store(), false).expect("open for the drop");
        assert!(
            handle
                .snapshot_drop(&second_root.to_string())
                .expect("drop the other"),
            "the second snapshot existed"
        );
    }
    let after_both = run(root.store(), GcOptions::default()).expect("collect after both drops");
    assert!(
        !after_both.report.freed.is_empty(),
        "with both roots gone, the shared tree is garbage"
    );
    assert!(
        shared
            .iter()
            .any(|oid| after_both.report.freed.contains(oid)),
        "the shared objects must be among what was freed: freed = {:?}, shared = {:?}",
        after_both.report.freed,
        shared
    );

    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
    for oid in &shared {
        assert!(
            store.get(*oid).is_err(),
            "K5c: {oid} had no other holder and must be gone"
        );
    }

    // The workspace's base root (the branch head's tree) is untouched by any of this.
    let handle = Repo::open(root.store(), false).expect("open to read the head");
    let info = handle.commit_info(&head).expect("info");
    drop(handle);
    let base_root: ObjectId = info.root.parse().expect("a root id");
    let reader = SnapshotReader::open_any(&store, base_root).expect("the base root still reads");
    assert!(
        !reader.references(base_root).expect("references").is_empty(),
        "verify --deep's coverage (snapshot rows + workspace base) must stay intact"
    );
}
