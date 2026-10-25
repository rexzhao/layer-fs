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

/// §25.5: **both** directions of the store's lock are released by a run.
///
/// `Repo::open` succeeding only proves the exclusive lock went away; a maintenance handle that
/// lingered would still block the *next* maintenance open.
#[test]
fn a_run_releases_the_store_for_an_ordinary_open_and_for_another_run() {
    let (root, _, _) = repo_with_one_orphan("release");
    run(root.store(), GcOptions::default()).expect("first run");

    Repo::open(root.store(), false).expect("an ordinary open works after a run");
    let second_handle = fs_workspace::MaintenanceRepo::open(root.store())
        .expect("a second maintenance open works after a run");
    // Not a count assertion: the point is that the handle could be taken at all. Reading it keeps the
    // value from being an unused `#[must_use]` result, which would be a warning rather than a check.
    assert_eq!(second_handle.store_dir(), root.store());
    drop(second_handle);

    // And a second collection over the (now clean) store is a no-op rather than an error.
    let second = run(root.store(), GcOptions::default()).expect("second run");
    assert!(
        second.report.freed.is_empty(),
        "nothing is left to free: {:?}",
        second.report.freed
    );
}

/// §25.6: the report can be reconciled without trusting any single number.
#[test]
fn the_report_reconciles_with_a_hand_counted_store() {
    let root = TempRoot::new("accounting");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"kept");
    workspace.commit("one", false).expect("commit");
    drop(workspace);
    drop(repo);

    let packs_before: i64 = {
        let connection =
            rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
        connection
            .query_row("SELECT COUNT(*) FROM pack", [], |row| row.get(0))
            .expect("count")
    };

    let outcome = run(root.store(), GcOptions::default()).expect("run");
    assert_eq!(
        outcome.report.marked_objects, outcome.report.reachable_count,
        "the two names for the marked set must agree"
    );
    assert!(
        outcome.report.packs_deleted + outcome.report.packs_repacked
            <= usize::try_from(packs_before).expect("fits"),
        "a pack can be deleted or repacked, never both: {} + {} > {}",
        outcome.report.packs_deleted,
        outcome.report.packs_repacked,
        packs_before
    );
    assert_eq!(
        outcome.report.reclaimed_dead_bytes, 0,
        "this store has no dead frames, so nothing was reclaimed by repacking"
    );
}

/// §29: two labels on **one tree** must both be recorded.
///
/// The most ordinary way this happens is a workspace created on a commit: its `base_root` is that
/// commit's tree, so `commit:<id>` and `workspace:<name>` name the same object. Marking must not walk the
/// tree twice (§13.1), and the report must still say both facts were covered — otherwise G2 cannot tell
/// "a shared tree" from "a missed root", and the criterion stops being evidence.
#[test]
fn two_labels_on_one_tree_are_both_recorded_and_the_tree_is_walked_once() {
    let root = TempRoot::new("shared-label");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    // A workspace on `main` shares its base tree with the initial commit — the QA reproduction.
    let _workspace = repo.workspace_create("w1", "main").expect("create");
    drop(_workspace);
    drop(repo);

    let outcome = run(root.store(), GcOptions::default()).expect("run");

    let declared: std::collections::BTreeSet<&String> =
        outcome.report.roots_declared.iter().collect();
    let visited: std::collections::BTreeSet<&String> =
        outcome.report.roots_visited.iter().collect();
    assert_eq!(
        declared, visited,
        "every declared label must be recorded as visited: declared {declared:?} vs visited {visited:?}"
    );
    assert!(
        outcome.report.roots_declared.len() > outcome.report.root_oids.len()
            || outcome.report.roots_declared.len() >= 2,
        "the fixture must have more than one label"
    );
    assert!(
        outcome
            .report
            .visited_by_kind
            .values()
            .all(|count| *count > 0),
        "per-kind counts must include every kind that was declared: {:?}",
        outcome.report.visited_by_kind
    );

    // The tree was still walked once: the marked set is a set of objects, not of labels.
    assert_eq!(
        outcome.report.marked_objects,
        outcome.report.reachable.len(),
        "the marked set stays deduplicated"
    );
}

/// §32: the *mark* is what de-duplicates — a shared subtree is fetched **once** even though two roots
/// reach it.
///
/// Paired with `fs-snapshot`'s `references_fetches_the_same_object_on_every_call` (the library remembers
/// nothing), this pins down which layer owns the de-duplication instead of asserting it in prose.
#[test]
fn a_subtree_reached_from_two_roots_is_fetched_once() {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    struct Counting<'a> {
        inner: &'a Store,
        counts: Rc<RefCell<BTreeMap<cas_core::ObjectId, usize>>>,
    }

    impl fs_snapshot::ObjectSource for Counting<'_> {
        fn get(&self, oid: cas_core::ObjectId) -> fs_snapshot::SnapshotResult<Vec<u8>> {
            *self.counts.borrow_mut().entry(oid).or_insert(0) += 1;
            fs_snapshot::ObjectSource::get(self.inner, oid)
        }
    }

    let root = TempRoot::new("mark-dedupe");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"one tree, two labels");
    workspace.commit("one", false).expect("commit");
    // A second workspace on the same branch: its `base_root` is the same tree, so two labels name it.
    drop(workspace);
    let _second = repo.workspace_create("w2", "main").expect("create");
    drop(_second);
    drop(repo);

    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
    let counts = Rc::new(RefCell::new(BTreeMap::new()));
    let counting = Counting {
        inner: &store,
        counts: Rc::clone(&counts),
    };
    let declared = {
        let handle = Repo::open(root.store(), false).expect("open repo for roots");
        let roots = fs_workspace::gc::RepoRoots { repo: &handle };
        <fs_workspace::gc::RepoRoots<'_> as fs_workspace::gc::RootSource>::roots(&roots)
            .expect("roots")
    };

    let mut marker = fs_workspace::gc::Marker::create_in(root.store()).expect("marker");
    let shared_tree = declared.entries()[0].root;
    let reader = fs_snapshot::SnapshotReader::open_any(&counting, shared_tree).expect("reader");
    let report = fs_workspace::gc::mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut fs_workspace::gc::GcHooks::default(),
    )
    .expect("mark");

    assert!(
        report.roots_declared.len() >= 2,
        "the fixture needs two labels on one tree: {:?}",
        report.roots_declared
    );
    assert_eq!(
        report.roots_declared.len(),
        report.roots_visited.len(),
        "every label is recorded (§29)"
    );
    let with_two_labels = counts.borrow().get(&shared_tree).copied().unwrap_or(0);
    marker
        .remove()
        .expect("close the first marker; Windows will not delete an open file");
    // The property that matters is not the absolute number (opening the reader fetches the root once, and
    // `references` fetches it again — both are by design). It is that the number does **not grow with the
    // labels**: a third workspace on the same tree must not add a single fetch.
    let repo = Arc::new(Repo::open(root.store(), false).expect("open repo"));
    let _third = repo.workspace_create("w3", "main").expect("create");
    drop(_third);
    drop(repo);
    let declared = {
        let handle = Repo::open(root.store(), false).expect("open repo for roots");
        let roots = fs_workspace::gc::RepoRoots { repo: &handle };
        <fs_workspace::gc::RepoRoots<'_> as fs_workspace::gc::RootSource>::roots(&roots)
            .expect("roots")
    };
    assert!(
        declared.entries().len() > report.roots_declared.len(),
        "the fixture must have gained a label"
    );
    let counts_again = Rc::new(RefCell::new(BTreeMap::new()));
    let counting_again = Counting {
        inner: &store,
        counts: Rc::clone(&counts_again),
    };
    let mut marker = fs_workspace::gc::Marker::create_in(root.store()).expect("marker");
    let reader =
        fs_snapshot::SnapshotReader::open_any(&counting_again, shared_tree).expect("reader");
    fs_workspace::gc::mark_roots(
        &declared,
        &declared,
        &reader,
        &mut marker,
        &mut fs_workspace::gc::GcHooks::default(),
    )
    .expect("mark");
    let with_three_labels = counts_again
        .borrow()
        .get(&shared_tree)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        with_two_labels, with_three_labels,
        "an extra label on the same tree must not add a fetch: de-duplication belongs to the mark"
    );
}
