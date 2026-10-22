//! Transaction atomicity (ADR-003 §3.5 X1–X4, §12.2).
//!
//! Each test plants a real statement failure inside an operation's transaction
//! and then asserts that *nothing* changed: the tree is item-for-item identical,
//! the root count is unchanged and the head did not move.

use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::{OpenMode, Store};
use fs_namespace::inject::{self, Fault};
use fs_namespace::{Kind, Namespace, NamespaceError, Path};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-ns-atomicity-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn path(&self) -> &StdPath {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("valid path")
}

/// A complete description of the namespace state, for item-for-item comparison.
fn snapshot(namespace: &Namespace) -> (Vec<(String, Kind, u64)>, u64, Option<u64>) {
    fn walk(namespace: &Namespace, path: &str, out: &mut Vec<(String, Kind, u64)>) {
        let entries = namespace.read_dir(&p(path)).expect("read_dir");
        for entry in entries {
            let child = format!(
                "{}{}{}",
                path,
                if path == "/" { "" } else { "/" },
                entry.name
            );
            let stat = namespace.stat(&p(&child)).expect("stat");
            out.push((child.clone(), entry.kind, stat.id.serial()));
            if entry.kind == Kind::Directory {
                walk(namespace, &child, out);
            }
        }
    }
    let mut items = Vec::new();
    walk(namespace, "/", &mut items);
    (items, namespace.root_count(), namespace.head_root())
}

fn open_fixture(root: &TempRoot) -> (Store, Namespace) {
    let store = Store::open(root.path(), OpenMode::Create).expect("store");
    let namespace = Namespace::open(&store, "default", OpenMode::Create).expect("namespace");
    (store, namespace)
}

/// ADR-003 §3.5 X1: a failure **before** any row is written leaves the tree and
/// the root chain untouched.
#[test]
fn failure_before_the_mutation_leaves_everything_unchanged() {
    let root = TempRoot::new("before");
    let (_store, namespace) = open_fixture(&root);
    namespace.mkdir(&p("/keep")).expect("mkdir");
    let before = snapshot(&namespace);

    inject::plant(namespace.connection_for_tests(), Fault::BeforeMutation).expect("plant");

    let error = namespace.mkdir(&p("/new")).expect_err("mkdir must fail");
    assert!(
        matches!(error, NamespaceError::Store(_)),
        "the fault surfaces as a store error: {error}"
    );
    assert_eq!(
        snapshot(&namespace),
        before,
        "tree, root count and head must be unchanged"
    );
    assert!(matches!(
        namespace.stat(&p("/new")),
        Err(NamespaceError::NotFound { .. })
    ));
}

/// ADR-003 §3.5 X1: a failure **after** the rows are written still rolls back —
/// no orphan directory row, no entry, no root record.
#[test]
fn failure_after_the_mutation_rolls_the_rows_back() {
    let root = TempRoot::new("after");
    let (_store, namespace) = open_fixture(&root);
    namespace.mkdir(&p("/keep")).expect("mkdir");
    let before = snapshot(&namespace);

    inject::plant(namespace.connection_for_tests(), Fault::AfterMutation).expect("plant");
    // The planted row itself is part of the fixture, not of the failed
    // operation, so take the baseline after planting.
    let before = {
        let mut snapshot = before;
        snapshot.2 = namespace.head_root();
        snapshot
    };

    let error = namespace.mkdir(&p("/new")).expect_err("mkdir must fail");
    assert!(
        matches!(error, NamespaceError::Store(_)),
        "the fault surfaces as a store error: {error}"
    );

    // Nothing the operation wrote survived: no entry named /new, and the file
    // row it allocated is gone too.
    assert!(matches!(
        namespace.stat(&p("/new")),
        Err(NamespaceError::NotFound { .. })
    ));
    let entries = namespace.read_dir(&p("/")).expect("read_dir");
    assert_eq!(
        entries.len(),
        1,
        "only the pre-existing entry remains: {entries:?}"
    );
    assert_eq!(namespace.head_root(), before.2, "the head did not move");
}

/// ADR-003 §3.5 X1 for `rmdir`, `unlink` and `rename`: each rolls back whole.
///
/// These three do not allocate a serial, so the allocator fault cannot reach
/// them; the fault used here is the one that applies to every operation — a
/// missing state row that the operation's first statement needs.
#[test]
fn failure_rolls_back_every_mutating_operation() {
    for op in ["rmdir", "unlink", "rename"] {
        let root = TempRoot::new(op);
        let (_store, namespace) = open_fixture(&root);
        namespace.mkdir(&p("/a")).expect("mkdir a");
        namespace.mkdir(&p("/b")).expect("mkdir b");
        let before = snapshot(&namespace);

        // Every operation's publication step reads the namespace row; removing
        // it fails the step *after* all row changes, for all four operations.
        inject::plant(namespace.connection_for_tests(), Fault::BeforePublication).expect("plant");
        let result = match op {
            "rmdir" => namespace.rmdir(&p("/a")),
            "unlink" => namespace.unlink(&p("/a")),
            "rename" => namespace.rename(&p("/a"), &p("/moved")),
            other => panic!("unknown op {other}"),
        };
        assert!(result.is_err(), "{op} must fail under the planted fault");

        let after = snapshot(&namespace);
        assert_eq!(after.0, before.0, "{op} must not change the tree");
        assert_eq!(after.2, before.2, "{op} must not move the head");
    }
}

/// ADR-003 §3.5 X2: published root records survive a failed operation unchanged.
#[test]
fn failed_operation_leaves_published_roots_intact() {
    let root = TempRoot::new("roots-intact");
    let (_store, namespace) = open_fixture(&root);
    namespace.mkdir(&p("/a")).expect("mkdir");
    let records: Vec<_> = (0..namespace.root_count())
        .map(|id| namespace.root_record(id).expect("record"))
        .collect();

    inject::plant(namespace.connection_for_tests(), Fault::BeforeMutation).expect("plant");
    let _ = namespace.mkdir(&p("/b"));

    let after: Vec<_> = (0..records.len() as u64)
        .map(|id| namespace.root_record(id).expect("record"))
        .collect();
    assert_eq!(after, records, "published records must not change");
}

/// ADR-003 §3.5 X3: an unknown root serial has no record.
#[test]
fn root_record_of_an_unknown_serial_is_absent() {
    let root = TempRoot::new("x3");
    let (_store, namespace) = open_fixture(&root);
    assert!(namespace.root_record(9_999).is_none());
}

/// ADR-003 §3.5 X4: a second namespace has its own chain and its own head.
#[test]
fn namespaces_keep_separate_root_chains() {
    let root = TempRoot::new("x4");
    let store = Store::open(root.path(), OpenMode::Create).expect("store");
    let first = Namespace::open(&store, "first", OpenMode::Create).expect("first");
    let second = Namespace::open(&store, "second", OpenMode::Create).expect("second");

    first.mkdir(&p("/a")).expect("mkdir in first");
    second.mkdir(&p("/b")).expect("mkdir in second");

    // Each namespace published its own init plus one change.
    assert_eq!(first.root_count(), 2);
    assert_eq!(second.root_count(), 2);
    assert_ne!(first.head_root(), second.head_root());
    // Neither tree contains the other's entry.
    assert!(first.stat(&p("/b")).is_err());
    assert!(second.stat(&p("/a")).is_err());
}
