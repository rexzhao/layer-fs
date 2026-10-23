//! **P2-0a** — `verify` reports namespace internal inconsistencies.
//!
//! Each test damages one table directly and checks that `verify` names the exact
//! problem, because a report that only says "inconsistent" cannot be acted on.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{Fs, Inconsistency, OpenMode, BLOCK_BYTES};

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
                "fs-consistency-{label}-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn sqlite(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.path().join("store.sqlite")).expect("open metadata")
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

/// A healthy namespace reports nothing.
#[test]
fn a_healthy_namespace_has_no_inconsistencies() {
    let (_root, fs) = fixture("healthy");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, &vec![1u8; BLOCK_BYTES as usize])
        .expect("write");
    fs.mkdir("/d").expect("mkdir");
    fs.create("/d/b").expect("create");

    let report = fs.verify().expect("verify");
    assert!(report.is_clean(), "{:?}", report.inconsistencies);
    assert!(report.inconsistencies.is_empty());
}

/// **A1 regression**: the operations that used to leave rows behind now leave none.
#[test]
fn the_cascading_operations_leave_no_inconsistency() {
    let (root, fs) = fixture("cascade");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, &vec![1u8; BLOCK_BYTES as usize])
        .expect("write");
    fs.create("/b").expect("create");
    fs.write_at("/b", 0, &vec![2u8; BLOCK_BYTES as usize])
        .expect("write");
    fs.create("/c").expect("create");
    fs.write_at("/c", 0, &vec![3u8; BLOCK_BYTES as usize])
        .expect("write");

    fs.rename("/a", "/b").expect("rename over");
    fs.remove("/c").expect("remove");

    let report = fs.verify().expect("verify");
    assert!(report.is_clean(), "{:?}", report.inconsistencies);

    // The damaged-state detector below is what would have caught A1, so prove it
    // still works by recreating the old damage by hand.
    let connection = root.sqlite();
    connection
        .execute(
            "INSERT INTO file (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
             VALUES ('default', 9999, 2, 0, NULL, 0, 0)",
            [],
        )
        .expect("insert orphan inode");
    let report = fs.verify().expect("verify");
    assert!(matches!(
        report.inconsistencies.as_slice(),
        [Inconsistency::UnreachableInode { file_id: 9999 }]
    ));
}

/// An inode no directory entry reaches is reported.
#[test]
fn an_unreachable_inode_is_reported() {
    let (root, fs) = fixture("orphan-inode");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"payload").expect("write");

    // Remove the name but keep the inode, as the pre-A1 `unlink` did.
    let connection = root.sqlite();
    connection
        .execute(
            "DELETE FROM directory_entry WHERE cast(name as text) = 'f'",
            [],
        )
        .expect("drop the entry");

    let report = fs.verify().expect("verify");
    assert!(!report.is_clean());
    let found = report
        .inconsistencies
        .iter()
        .find(|item| matches!(item, Inconsistency::UnreachableInode { .. }))
        .expect("the orphaned inode must be reported");
    assert!(found.to_string().contains("not reachable"), "{found}");
}

/// A directory entry whose child has no inode is reported.
#[test]
fn an_entry_without_an_inode_is_reported() {
    let (root, fs) = fixture("entry-no-inode");
    fs.create("/f").expect("create");

    let connection = root.sqlite();
    connection
        .execute("DELETE FROM file WHERE file_id != 0", [])
        .expect("drop the inode, keep the name");

    let report = fs.verify().expect("verify");
    assert!(!report.is_clean());
    let found = report
        .inconsistencies
        .iter()
        .find(|item| matches!(item, Inconsistency::EntryWithoutInode { .. }))
        .expect("the dangling name must be reported");
    let text = found.to_string();
    assert!(text.contains('f'), "{text}");
    assert!(text.contains("no inode"), "{text}");
}

/// An extent row whose inode is gone is reported — the A1(a) signature.
#[test]
fn an_extent_without_an_inode_is_reported() {
    let (root, fs) = fixture("extent-no-inode");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &vec![1u8; BLOCK_BYTES as usize])
        .expect("write");

    // Drop the inode but keep the extent, exactly what the pre-A1 `unlink` left.
    let connection = root.sqlite();
    connection
        .execute("DELETE FROM file WHERE file_id != 0", [])
        .expect("drop the inode");

    let report = fs.verify().expect("verify");
    assert!(!report.is_clean());
    let found = report
        .inconsistencies
        .iter()
        .find(|item| matches!(item, Inconsistency::ExtentWithoutInode { .. }))
        .expect("the unreachable block must be reported");
    let text = found.to_string();
    assert!(text.contains("does not exist"), "{text}");
}

/// The root inode is never reported as unreachable, since no entry points at it.
#[test]
fn the_root_inode_is_not_reported_as_unreachable() {
    let (_root, fs) = fixture("root");
    fs.create("/f").expect("create");
    let report = fs.verify().expect("verify");
    assert!(!report
        .inconsistencies
        .iter()
        .any(|item| matches!(item, Inconsistency::UnreachableInode { file_id: 0 })));
}

/// Both classes of problem can be present at once, and both are listed.
#[test]
fn several_inconsistencies_are_all_reported() {
    let (root, fs) = fixture("several");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, &vec![1u8; BLOCK_BYTES as usize])
        .expect("write");
    fs.create("/b").expect("create");

    let connection = root.sqlite();
    // Orphan one inode and break one name.
    connection
        .execute(
            "DELETE FROM directory_entry WHERE cast(name as text) = 'a'",
            [],
        )
        .expect("orphan a");
    connection
        .execute("DELETE FROM file WHERE file_id = (SELECT child_id FROM (SELECT child_id FROM directory_entry WHERE cast(name as text) = 'b'))", [])
        .expect("break b");

    let report = fs.verify().expect("verify");
    assert!(
        report
            .inconsistencies
            .iter()
            .any(|i| matches!(i, Inconsistency::UnreachableInode { .. })),
        "{:?}",
        report.inconsistencies
    );
    assert!(
        report
            .inconsistencies
            .iter()
            .any(|i| matches!(i, Inconsistency::EntryWithoutInode { .. })),
        "{:?}",
        report.inconsistencies
    );
}
