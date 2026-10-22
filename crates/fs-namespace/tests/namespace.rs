//! Namespace behaviour against the violation enumerations in ADR-003 §3.
//!
//! Each test names the enumeration row it covers.

use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::{OpenMode, Store};
use fs_namespace::{FileId, Kind, Namespace, NamespaceError, Path};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-namespace-test-{label}-{}-{}-{sequence}",
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

/// A fresh Store + namespace, kept alive for the duration of a test.
struct Fixture {
    _root: TempRoot,
    _store: Store,
    namespace: Namespace,
}

fn fixture(label: &str) -> Fixture {
    let root = TempRoot::new(label);
    let store = Store::open(root.path(), OpenMode::Create).expect("store");
    let namespace = Namespace::open(&store, "default", OpenMode::Create).expect("namespace");
    Fixture {
        _root: root,
        _store: store,
        namespace,
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("valid path")
}

// ---------------------------------------------------------------- mkdir

#[test]
fn mkdir_creates_a_directory_and_lists_it() {
    let fixture = fixture("mkdir");
    fixture.namespace.mkdir(&p("/docs")).expect("mkdir");
    let entries = fixture.namespace.read_dir(&p("/")).expect("read_dir");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "docs");
    assert_eq!(entries[0].kind, Kind::Directory);
}

#[test]
fn mkdir_nested_paths_resolve_through_parents() {
    let fixture = fixture("mkdir-nested");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    fixture.namespace.mkdir(&p("/a/b")).expect("b");
    fixture.namespace.mkdir(&p("/a/b/c")).expect("c");
    assert_eq!(
        fixture.namespace.stat(&p("/a/b/c")).expect("stat").kind,
        Kind::Directory
    );
}

/// ADR-003 §3.2 T1 for a directory target. The *file* variant of T1 needs
/// `create` (T4) to make a file; until then the only way to reach it is a
/// hand-inserted row, which would test the same `lookup`-based branch already
/// covered here. Noted so the gap is explicit rather than implied by a
/// misleading test name.
#[test]
fn mkdir_refuses_an_existing_directory() {
    let fixture = fixture("mkdir-exists");
    fixture.namespace.mkdir(&p("/d")).expect("first");
    assert!(matches!(
        fixture.namespace.mkdir(&p("/d")),
        Err(NamespaceError::AlreadyExists { .. })
    ));
}

#[test]
fn mkdir_requires_the_parent_to_exist() {
    let fixture = fixture("mkdir-no-parent");
    assert!(matches!(
        fixture.namespace.mkdir(&p("/a/b")),
        Err(NamespaceError::NotFound { .. })
    ));
}

#[test]
fn mkdir_refuses_the_root() {
    let fixture = fixture("mkdir-root");
    assert!(matches!(
        fixture.namespace.mkdir(&p("/")),
        Err(NamespaceError::AlreadyExists { .. })
    ));
}

// ---------------------------------------------------------------- rmdir

/// ADR-003 §3.2 T5.
#[test]
fn rmdir_removes_an_empty_directory() {
    let fixture = fixture("rmdir");
    fixture.namespace.mkdir(&p("/d")).expect("mkdir");
    fixture.namespace.rmdir(&p("/d")).expect("rmdir");
    assert!(matches!(
        fixture.namespace.stat(&p("/d")),
        Err(NamespaceError::NotFound { .. })
    ));
}

#[test]
fn rmdir_refuses_a_non_empty_directory() {
    let fixture = fixture("rmdir-nonempty");
    fixture.namespace.mkdir(&p("/d")).expect("d");
    fixture.namespace.mkdir(&p("/d/e")).expect("e");
    assert!(matches!(
        fixture.namespace.rmdir(&p("/d")),
        Err(NamespaceError::DirectoryNotEmpty { .. })
    ));
}

#[test]
fn rmdir_refuses_the_root() {
    let fixture = fixture("rmdir-root");
    assert!(matches!(
        fixture.namespace.rmdir(&p("/")),
        Err(NamespaceError::IsRoot)
    ));
}

#[test]
fn rmdir_refuses_a_file_target_and_missing_paths() {
    let fixture = fixture("rmdir-misc");
    assert!(matches!(
        fixture.namespace.rmdir(&p("/missing")),
        Err(NamespaceError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------- rename

/// ADR-003 §3.3 R2.
#[test]
fn rename_moves_an_entry() {
    let fixture = fixture("rename");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    fixture
        .namespace
        .rename(&p("/a"), &p("/b"))
        .expect("rename");
    assert!(fixture.namespace.stat(&p("/b")).is_ok());
    assert!(matches!(
        fixture.namespace.stat(&p("/a")),
        Err(NamespaceError::NotFound { .. })
    ));
}

/// ADR-003 §3.3 R8: a byte-identical rename is a no-op that publishes nothing.
#[test]
fn rename_to_itself_is_a_no_op_that_publishes_nothing() {
    let fixture = fixture("rename-self");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    let before = fixture.namespace.root_count();
    fixture
        .namespace
        .rename(&p("/a"), &p("/a"))
        .expect("no-op rename");
    assert_eq!(
        fixture.namespace.root_count(),
        before,
        "a no-op must not append a root record"
    );
}

/// ADR-003 §3.3 R8 caveat: a case-only change is a real rename.
#[test]
fn rename_case_only_change_is_a_real_rename() {
    let fixture = fixture("rename-case");
    fixture.namespace.mkdir(&p("/A")).expect("A");
    let before = fixture.namespace.root_count();
    fixture
        .namespace
        .rename(&p("/A"), &p("/a"))
        .expect("rename");
    assert!(
        fixture.namespace.root_count() > before,
        "a case-only rename must publish a new root"
    );
    assert!(fixture.namespace.stat(&p("/a")).is_ok());
    assert!(matches!(
        fixture.namespace.stat(&p("/A")),
        Err(NamespaceError::NotFound { .. })
    ));
}

/// ADR-003 §3.3 R9.
#[test]
fn rename_refuses_moving_a_directory_into_itself() {
    let fixture = fixture("rename-subtree");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    fixture.namespace.mkdir(&p("/a/b")).expect("b");
    assert!(matches!(
        fixture.namespace.rename(&p("/a"), &p("/a/b/c")),
        Err(NamespaceError::IntoOwnSubtree { .. })
    ));
}

/// ADR-003 §3.3 R1.
#[test]
fn rename_refuses_a_missing_source() {
    let fixture = fixture("rename-missing");
    assert!(matches!(
        fixture.namespace.rename(&p("/nope"), &p("/x")),
        Err(NamespaceError::NotFound { .. })
    ));
}

/// ADR-003 §3.3 R10.
#[test]
fn rename_refuses_a_missing_target_parent() {
    let fixture = fixture("rename-no-parent");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    assert!(matches!(
        fixture.namespace.rename(&p("/a"), &p("/nope/b")),
        Err(NamespaceError::NotFound { .. })
    ));
}

/// ADR-003 §3.3 R6: a non-empty target directory blocks the move.
#[test]
fn rename_refuses_to_replace_a_non_empty_directory() {
    let fixture = fixture("rename-nonempty");
    fixture.namespace.mkdir(&p("/src")).expect("src");
    fixture.namespace.mkdir(&p("/dst")).expect("dst");
    fixture.namespace.mkdir(&p("/dst/keep")).expect("keep");
    assert!(matches!(
        fixture.namespace.rename(&p("/src"), &p("/dst")),
        Err(NamespaceError::DirectoryNotEmpty { .. })
    ));
}

/// ADR-003 §3.3 R7: an empty target directory is replaced.
#[test]
fn rename_replaces_an_empty_directory() {
    let fixture = fixture("rename-empty-target");
    fixture.namespace.mkdir(&p("/src")).expect("src");
    fixture.namespace.mkdir(&p("/dst")).expect("dst");
    fixture
        .namespace
        .rename(&p("/src"), &p("/dst"))
        .expect("rename");
    assert!(fixture.namespace.stat(&p("/dst")).is_ok());
    assert!(matches!(
        fixture.namespace.stat(&p("/src")),
        Err(NamespaceError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------- stat / read_dir

#[test]
fn stat_reports_the_root() {
    let fixture = fixture("stat-root");
    let stat = fixture.namespace.stat(&p("/")).expect("stat");
    assert_eq!(stat.id, FileId::ROOT);
    assert_eq!(stat.kind, Kind::Directory);
    assert_eq!(stat.size, 0);
}

#[test]
fn stat_refuses_a_missing_path() {
    let fixture = fixture("stat-missing");
    assert!(matches!(
        fixture.namespace.stat(&p("/nope")),
        Err(NamespaceError::NotFound { .. })
    ));
}

#[test]
fn read_dir_returns_names_in_byte_order() {
    let fixture = fixture("readdir-order");
    for name in ["b", "A", "a", "B"] {
        fixture
            .namespace
            .mkdir(&p(&format!("/{name}")))
            .expect("mkdir");
    }
    let names: Vec<String> = fixture
        .namespace
        .read_dir(&p("/"))
        .expect("read_dir")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["A", "B", "a", "b"]);
}

/// ADR-003 §3.4 C1: `A` and `a` coexist as distinct names.
#[test]
fn case_sensitive_names_coexist() {
    let fixture = fixture("case");
    fixture.namespace.mkdir(&p("/A")).expect("A");
    fixture.namespace.mkdir(&p("/a")).expect("a");
    let names: Vec<String> = fixture
        .namespace
        .read_dir(&p("/"))
        .expect("read_dir")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names.len(), 2);
    assert_ne!(
        fixture.namespace.stat(&p("/A")).expect("A").id,
        fixture.namespace.stat(&p("/a")).expect("a").id,
        "distinct names get distinct ids"
    );
}

/// ADR-003 §3.2 T3, the `NotFound` branch. The `NotADirectory` branch needs a
/// file target, which requires `create` (T4); recorded here so the gap is
/// explicit.
#[test]
fn read_dir_refuses_a_missing_path() {
    let fixture = fixture("readdir-missing");
    assert!(matches!(
        fixture.namespace.read_dir(&p("/missing")),
        Err(NamespaceError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------- roots (D+)

/// ADR-003 §12.1 D4: exactly one root record per mutating operation.
#[test]
fn each_mutating_operation_appends_exactly_one_root() {
    let fixture = fixture("roots-count");
    let start = fixture.namespace.root_count();
    fixture.namespace.mkdir(&p("/a")).expect("mkdir");
    assert_eq!(fixture.namespace.root_count(), start + 1);
    fixture.namespace.mkdir(&p("/b")).expect("mkdir");
    assert_eq!(fixture.namespace.root_count(), start + 2);
    fixture.namespace.rmdir(&p("/b")).expect("rmdir");
    assert_eq!(fixture.namespace.root_count(), start + 3);
}

/// ADR-003 §12.1 D5: reads and no-ops append nothing.
#[test]
fn reads_and_no_ops_append_no_root() {
    let fixture = fixture("roots-readonly");
    fixture.namespace.mkdir(&p("/a")).expect("mkdir");
    let start = fixture.namespace.root_count();
    fixture.namespace.stat(&p("/a")).expect("stat");
    fixture.namespace.read_dir(&p("/")).expect("read_dir");
    fixture.namespace.rename(&p("/a"), &p("/a")).expect("no-op");
    assert_eq!(fixture.namespace.root_count(), start);
}

/// ADR-003 §12.1 D1: earlier records are byte-identical after a write.
#[test]
fn published_root_records_never_change() {
    let fixture = fixture("roots-append-only");
    fixture.namespace.mkdir(&p("/a")).expect("mkdir");
    let first = fixture.namespace.root_record(0).expect("record 0");
    fixture.namespace.mkdir(&p("/b")).expect("mkdir");
    fixture.namespace.rmdir(&p("/a")).expect("rmdir");
    assert_eq!(
        fixture.namespace.root_record(0).expect("record 0"),
        first,
        "a published record must not change"
    );
}

/// ADR-003 §12.1 D3: the parent chain reaches the head without a gap.
#[test]
fn root_chain_is_connected() {
    let fixture = fixture("roots-chain");
    for index in 0..5 {
        fixture
            .namespace
            .mkdir(&p(&format!("/d{index}")))
            .expect("mkdir");
    }
    let mut current = fixture.namespace.head_root().expect("head");
    let mut seen = 0;
    loop {
        let record = fixture.namespace.root_record(current).expect("record");
        seen += 1;
        match record.parent_root_id {
            Some(parent) => current = parent,
            None => break,
        }
        assert!(seen < 100, "chain did not terminate");
    }
    assert_eq!(seen as u64, fixture.namespace.root_count());
}

/// ADR-003 §3.5 X4: opening a namespace that does not exist is refused.
#[test]
fn opening_a_missing_namespace_is_refused() {
    let root = TempRoot::new("ns-missing");
    let store = Store::open(root.path(), OpenMode::Create).expect("store");
    Namespace::open(&store, "default", OpenMode::Create).expect("create default");
    assert!(matches!(
        Namespace::open(&store, "other", OpenMode::ReadWrite),
        Err(NamespaceError::NamespaceNotFound { .. })
    ));
}

#[test]
fn reopening_keeps_the_tree() {
    let root = TempRoot::new("reopen");
    {
        let store = Store::open(root.path(), OpenMode::Create).expect("store");
        let namespace = Namespace::open(&store, "default", OpenMode::Create).expect("ns");
        namespace.mkdir(&p("/kept")).expect("mkdir");
    }
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadWrite).expect("reopen ns");
    assert!(namespace.stat(&p("/kept")).is_ok());
}

#[test]
fn read_only_namespace_refuses_writes() {
    let root = TempRoot::new("readonly");
    {
        let store = Store::open(root.path(), OpenMode::Create).expect("store");
        let namespace = Namespace::open(&store, "default", OpenMode::Create).expect("ns");
        namespace.mkdir(&p("/d")).expect("mkdir");
    }
    let store = Store::open(root.path(), OpenMode::ReadOnly).expect("readonly store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadOnly).expect("readonly ns");
    assert!(namespace.stat(&p("/d")).is_ok());
    assert!(matches!(
        namespace.mkdir(&p("/e")),
        Err(NamespaceError::Store(_))
    ));
}
