//! **P2-0a / A1** — referential leftovers in the namespace tables.
//!
//! Architect reported two deterministic leaks: `unlink` removes a file's row but
//! leaves its `file_extent` rows, and `rename`'s file-over-file branch replaces the
//! target's *name* without removing the target's own rows. Both leave the namespace
//! internally inconsistent — bytes nobody can reach, and inodes nobody points at.
//!
//! This file first reproduces them (each test started red), then pins the fixed
//! behaviour. The three reproductions are also copied into the TreeStore contract
//! tests (`crates/fs-api/tests/tree_store_contract.rs`) so any future backend must
//! satisfy the same invariants.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{Fs, OpenMode, BLOCK_BYTES};

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
                "fs-a1-{label}-{}-{}-{sequence}",
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

/// How many `file_extent` rows the store holds.
fn extent_rows(root: &TempRoot) -> i64 {
    let connection =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
    connection
        .query_row("SELECT COUNT(*) FROM file_extent", [], |row| row.get(0))
        .expect("count extents")
}

/// How many `file` rows the store holds.
fn file_rows(root: &TempRoot) -> i64 {
    let connection =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
    connection
        .query_row("SELECT COUNT(*) FROM file", [], |row| row.get(0))
        .expect("count files")
}

/// A block-sized body, so the file lands in the extent shape.
fn blocky(byte: u8) -> Vec<u8> {
    vec![byte; BLOCK_BYTES as usize]
}

// ------------------------------------------------------------------ A1 part 1

/// **A1(a)**: `unlink` of a large file removes its extent rows too.
///
/// Before the fix this test failed with 1 leftover row: the `file` row went away
/// but its block stayed in `file_extent`, pointing at an object nobody referenced.
#[test]
fn a1_unlink_removes_the_extent_rows() {
    let (root, fs) = fixture("unlink");
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &blocky(7)).expect("write");
    assert_eq!(extent_rows(&root), 1, "the file is a one-block extent file");

    fs.remove("/big").expect("remove");

    assert_eq!(
        extent_rows(&root),
        0,
        "unlink must cascade to file_extent, or the block is unreachable garbage"
    );
}

/// **A1(a)**: several extent rows all go.
#[test]
fn a1_unlink_removes_every_extent_row() {
    let (root, fs) = fixture("unlink-many");
    let mut body = Vec::new();
    for index in 0..3u8 {
        body.extend(std::iter::repeat_n(index + 1, BLOCK_BYTES as usize));
    }
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &body).expect("write");
    assert_eq!(extent_rows(&root), 3);

    fs.remove("/big").expect("remove");
    assert_eq!(extent_rows(&root), 0);
}

// ------------------------------------------------------------------ A1 part 2

/// **A1(b)**: renaming a file over another removes the overwritten file's rows.
///
/// `rename("/a", "/b")` where both are files: R3 says the destination is replaced.
/// Before the fix only the directory entry changed, so `b`'s inode row and its
/// extents survived, orphaned.
#[test]
fn a1_rename_over_a_file_removes_the_overwritten_rows() {
    let (root, fs) = fixture("rename-over");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, &blocky(1)).expect("write a");
    fs.create("/b").expect("create");
    fs.write_at("/b", 0, &blocky(2)).expect("write b");

    // The root directory plus two files; two extent rows.
    assert_eq!(file_rows(&root), 3, "root dir + /a + /b");
    assert_eq!(extent_rows(&root), 2);

    fs.rename("/a", "/b").expect("rename over");

    assert_eq!(
        file_rows(&root),
        2,
        "root dir plus the surviving file; the overwritten inode row must go"
    );
    assert_eq!(
        extent_rows(&root),
        1,
        "the overwritten file's extents must go"
    );
    // And the survivor is `a`'s content, reachable at `/b`.
    let mut buffer = vec![0xAAu8; BLOCK_BYTES as usize];
    let read = fs.read_at("/b", 0, &mut buffer).expect("read");
    assert_eq!(read, BLOCK_BYTES as usize);
    assert!(buffer.iter().all(|byte| *byte == 1), "a's bytes survive");
}

/// **A1(b)** for the directory case: replacing an empty directory removes its rows.
#[test]
fn a1_rename_over_an_empty_directory_removes_its_rows() {
    let (root, fs) = fixture("rename-dir");
    fs.mkdir("/src").expect("mkdir src");
    fs.mkdir("/dst").expect("mkdir dst");
    let before = file_rows(&root);
    assert_eq!(before, 3, "root directory plus the two created ones");

    fs.rename("/src", "/dst").expect("rename over empty dir");

    assert_eq!(
        file_rows(&root),
        before - 1,
        "the replaced directory's row must go"
    );
}

// ------------------------------------------------------------------ verify closure

/// After both operations, `verify` reports a clean namespace.
///
/// This is the end-to-end statement the two fixes add up to: no unreachable bytes
/// and no unreachable inodes.
#[test]
fn a1_operations_leave_no_unreachable_rows() {
    let (root, fs) = fixture("clean");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, &blocky(1)).expect("write");
    fs.create("/b").expect("create");
    fs.write_at("/b", 0, &blocky(2)).expect("write");
    fs.create("/c").expect("create");
    fs.write_at("/c", 0, &blocky(3)).expect("write");

    fs.rename("/a", "/b").expect("rename over");
    fs.remove("/c").expect("remove");

    let report = fs.verify().expect("verify");
    assert!(
        report.is_clean(),
        "namespace must be internally consistent: {:?}",
        report
    );
    assert_eq!(file_rows(&root), 2, "root plus the surviving file");
    assert_eq!(extent_rows(&root), 1, "one surviving block");
}
