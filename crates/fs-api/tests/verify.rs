//! Namespace-to-store closure verification (ADR-004 §7).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, FsError};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-verify-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(label: &str) -> (TempRoot, Fs) {
    let root = TempRoot::new(label);
    let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
    (root, fs)
}

#[test]
fn a_healthy_namespace_verifies_clean() {
    let (_root, fs) = fixture("clean");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, b"hello").expect("write");
    fs.create("/b").expect("create");
    fs.write_at("/b", 0, b"world").expect("write");

    let report = fs.verify().expect("verify");
    assert!(report.is_clean());
    assert_eq!(report.references, 2, "two distinct contents, two objects");
}

/// An empty file references nothing, so it must not be reported as dangling.
#[test]
fn an_empty_file_contributes_no_reference() {
    let (_root, fs) = fixture("empty");
    fs.create("/a").expect("create");
    let report = fs.verify().expect("verify");
    assert!(report.is_clean());
    assert_eq!(report.references, 0);
}

/// Identical content is one object, counted once.
#[test]
fn identical_contents_are_counted_once() {
    let (_root, fs) = fixture("dedup");
    for name in ["/a", "/b", "/c"] {
        fs.create(name).expect("create");
        fs.write_at(name, 0, b"same bytes").expect("write");
    }
    let report = fs.verify().expect("verify");
    assert!(report.is_clean());
    assert_eq!(report.references, 1);
}

/// A reference that cannot be satisfied is reported, not hidden.
#[test]
fn a_dangling_reference_is_reported() {
    let (root, fs) = fixture("dangling");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, b"payload").expect("write");
    let object = {
        let report = fs.verify().expect("verify");
        assert_eq!(report.references, 1);
        // Recover the referenced id from the stat, which is the same object.
        fs.stat("/a")
            .expect("stat")
            .content_root
            .expect("content root")
    };
    drop(fs);

    // Remove the object file behind the namespace's back, the way a damaged
    // store would lose it.
    let layout_object = root
        .path
        .join("objects")
        .join(&object.to_string()[..2])
        .join(&object.to_string()[2..4])
        .join(object.to_string());
    assert!(layout_object.exists(), "the object should be on disk first");
    std::fs::remove_file(&layout_object).expect("remove object");

    let fs = Fs::open(&root.path, "default", OpenMode::ReadWrite).expect("reopen");
    let report = fs.verify().expect("verify");
    assert!(!report.is_clean(), "the missing object must be reported");
    assert_eq!(report.dangling.len(), 1);
    assert_eq!(report.dangling[0].object, object.to_string());

    // Reading the file surfaces the same condition as a typed error.
    let mut buffer = [0u8; 7];
    assert!(matches!(
        fs.read_at("/a", 0, &mut buffer),
        Err(FsError::DanglingReference { .. })
    ));
}

/// A large file's extents are all covered by the closure check.
#[test]
fn extents_are_verified_too() {
    let (root, fs) = fixture("extents");
    // Distinct block contents, so all three blocks reference distinct objects;
    // identical blocks would dedupe and prove less.
    let block = fs_api::BLOCK_BYTES as usize;
    let mut big = Vec::with_capacity(block * 3);
    big.extend(vec![1u8; block]);
    big.extend(vec![2u8; block]);
    big.extend(vec![3u8; block]);
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &big).expect("write");

    let report = fs.verify().expect("verify");
    assert!(report.is_clean());
    assert_eq!(report.references, 3, "three distinct blocks, three objects");

    let mut probe = vec![0u8; block];
    fs.read_at("/big", 0, &mut probe).expect("read");
    assert!(probe.iter().all(|byte| *byte == 1));

    // Find the on-disk object files and remove them all, the way a damaged
    // store would lose them.
    let mut removed = 0usize;
    let objects = root.path.join("objects");
    for level1 in std::fs::read_dir(&objects).expect("objects dir") {
        for level2 in std::fs::read_dir(level1.expect("dir").path()).expect("level2") {
            for file in std::fs::read_dir(level2.expect("dir").path()).expect("level3") {
                let file = file.expect("file");
                std::fs::remove_file(file.path()).expect("remove block");
                removed += 1;
            }
        }
    }
    assert_eq!(removed, 3, "three block objects on disk");
    drop(fs);

    let fs = Fs::open(&root.path, "default", OpenMode::ReadWrite).expect("reopen");
    let report = fs.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.dangling.len(), 3, "every removed block is reported");
}
