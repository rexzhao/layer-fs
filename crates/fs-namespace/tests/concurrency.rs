//! Concurrency: threads and processes over one Store (ADR-003 §7).

use std::path::{Path as StdPath, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::{OpenMode, Store};
use fs_namespace::{Namespace, Path};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-ns-concurrency-{label}-{}-{}-{sequence}",
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

/// The child entry point: creates one directory, reporting the outcome.
#[test]
fn namespace_child() {
    let Ok(specification) = std::env::var("FS_NS_CHILD") else {
        return;
    };
    let mut parts = specification.split('|');
    let root = PathBuf::from(parts.next().expect("root"));
    let name = parts.next().expect("name").to_owned();

    let store = Store::open(&root, OpenMode::ReadWrite).expect("child store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadWrite).expect("child ns");
    match namespace.mkdir(&p(&format!("/{name}"))) {
        Ok(()) => println!("OK {name}"),
        Err(error) => println!("ERR {name} {error}"),
    }
}

/// Different paths from several threads all land; each gets one root record.
#[test]
fn concurrent_mkdir_of_different_paths_all_succeed() {
    let root = TempRoot::new("threads-different");
    {
        let store = Store::open(root.path(), OpenMode::Create).expect("store");
        Namespace::open(&store, "default", OpenMode::Create).expect("ns");
    }
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|index| {
                let root = root.path();
                scope.spawn(move || {
                    let store = Store::open(root, OpenMode::ReadWrite).expect("store");
                    let namespace =
                        Namespace::open(&store, "default", OpenMode::ReadWrite).expect("ns");
                    namespace.mkdir(&p(&format!("/d{index}")))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let mut created = 0;
    for outcome in outcomes {
        match outcome {
            Ok(()) => created += 1,
            // A concurrent writer holding the write lane is a typed refusal,
            // never a wait or a retry (ADR-003 §7).
            Err(fs_namespace::NamespaceError::Store(cas_store::StoreError::Busy)) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }
    assert!(created > 0, "at least one writer succeeds");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadWrite).expect("ns");
    let entries = namespace.read_dir(&p("/")).expect("read_dir");
    assert_eq!(entries.len() as u32, created);
    // Every successful mkdir appended exactly one root record.
    assert_eq!(namespace.root_count(), 1 + created as u64);
}

/// The same path from several threads: whatever wins, the tree holds one entry
/// and the losers report `AlreadyExists` (ADR-003 §7: last write wins, no CAS).
#[test]
fn concurrent_mkdir_of_the_same_path_leaves_one_entry() {
    let root = TempRoot::new("threads-same");
    {
        let store = Store::open(root.path(), OpenMode::Create).expect("store");
        Namespace::open(&store, "default", OpenMode::Create).expect("ns");
    }
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = root.path();
                scope.spawn(move || {
                    let store = Store::open(root, OpenMode::ReadWrite).expect("store");
                    let namespace =
                        Namespace::open(&store, "default", OpenMode::ReadWrite).expect("ns");
                    namespace.mkdir(&p("/same"))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let mut created = 0;
    for outcome in outcomes {
        match outcome {
            Ok(()) => created += 1,
            Err(fs_namespace::NamespaceError::Store(cas_store::StoreError::Busy)) => {}
            Err(fs_namespace::NamespaceError::AlreadyExists { .. }) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }
    assert_eq!(created, 1, "exactly one thread creates the directory");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadWrite).expect("ns");
    assert_eq!(namespace.read_dir(&p("/")).expect("read_dir").len(), 1);
}

/// A second process writing the same Store (ADR-003 §7, cross-process case).
#[test]
fn a_second_process_can_create_a_directory() {
    let root = TempRoot::new("process");
    {
        let store = Store::open(root.path(), OpenMode::Create).expect("store");
        Namespace::open(&store, "default", OpenMode::Create).expect("ns");
    }

    let output = Command::new(std::env::current_exe().expect("test binary"))
        .arg("namespace_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(
            "FS_NS_CHILD",
            format!("{}|fromchild", root.path().display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn child");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("OK fromchild"),
        "child did not create the directory: {text}"
    );

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("store");
    let namespace = Namespace::open(&store, "default", OpenMode::ReadWrite).expect("ns");
    assert!(namespace.stat(&p("/fromchild")).is_ok());
}
