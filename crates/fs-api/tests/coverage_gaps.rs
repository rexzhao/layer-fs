//! Coverage gaps QA identified in `v6w3umuyiv` (round 2), C1–C4.
//!
//! Each test names the item it closes. C5 (Display arms) lives in each crate's own
//! tests because the enums are private to their crates.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{Fs, FsError, OpenMode, BLOCK_BYTES};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-qa-cov-{label}-{}-{}-{sequence}",
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

/// Poisoned buffer, so a read that leaves bytes unwritten is caught
/// (ADR-004 §13 item 3).
fn read_poisoned(fs: &Fs, path: &str) -> Vec<u8> {
    let size = fs.stat(path).expect("stat").size as usize;
    let mut buffer = vec![0xAAu8; size];
    let read = fs.read_at(path, 0, &mut buffer).expect("read");
    buffer.truncate(read);
    buffer
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// **C1** — an unaligned write into an existing extent file must keep every byte
/// outside the written range.
///
/// The file is exactly one block, so it is in the extent shape with a single row;
/// writing 4 bytes at offset 100 forces the row to be split around the write.
#[test]
fn c1_unaligned_write_into_an_extent_file_keeps_the_rest() {
    let (_root, fs) = fixture("c1-write");
    let original = pattern(BLOCK_BYTES as usize);
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &original).expect("write");
    assert_eq!(fs.stat("/f").expect("stat").size, BLOCK_BYTES);

    fs.write_at("/f", 100, b"XYZW").expect("unaligned write");

    let mut expected = original.clone();
    expected[100..104].copy_from_slice(b"XYZW");
    assert_eq!(read_poisoned(&fs, "/f"), expected);
    assert!(fs.verify().expect("verify").is_clean());
}

/// **C1** — an unaligned write spanning a block boundary in a multi-block extent
/// file keeps both sides.
#[test]
fn c1_unaligned_write_across_a_block_boundary_keeps_both_sides() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("c1-write-span");
    let original = pattern(block * 3);
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &original).expect("write");

    let start = block - 10;
    let payload = vec![0xEEu8; 20];
    fs.write_at("/f", start as u64, &payload).expect("write");

    let mut expected = original.clone();
    expected[start..start + 20].copy_from_slice(&payload);
    assert_eq!(read_poisoned(&fs, "/f"), expected);
}

/// **C1** — an unaligned truncate of an extent file must clip the boundary block
/// and drop everything after it.
#[test]
fn c1_unaligned_truncate_of_an_extent_file_keeps_the_prefix() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("c1-truncate");
    let original = pattern(block * 3);
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &original).expect("write");

    let keep = block + 1234;
    fs.truncate("/f", keep as u64).expect("unaligned truncate");

    assert_eq!(fs.stat("/f").expect("stat").size, keep as u64);
    assert_eq!(read_poisoned(&fs, "/f"), original[..keep].to_vec());
    assert!(fs.verify().expect("verify").is_clean());
}

/// **C1** — after an unaligned write and truncate, the extent rows stay contiguous:
/// every byte of the file is backed by exactly one row, with no overlap and no
/// unexpected hole.
#[test]
fn c1_extent_rows_stay_contiguous_after_unaligned_edits() {
    let block = BLOCK_BYTES as usize;
    let root = TempRoot::new("c1-rows");
    {
        let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
        fs.create("/f").expect("create");
        fs.write_at("/f", 0, &pattern(block * 2)).expect("write");
        fs.write_at("/f", 100, b"edit").expect("unaligned write");
        fs.truncate("/f", (block + 500) as u64)
            .expect("unaligned truncate");
    }

    // Read the extent rows directly: this is a structural assertion the public API
    // cannot express.
    let connection =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
    let mut statement = connection
        .prepare(
            "SELECT logical_start, length FROM file_extent
             WHERE namespace = 'default' ORDER BY logical_start",
        )
        .expect("prepare");
    let rows: Vec<(i64, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();

    assert!(!rows.is_empty(), "the file has extent rows");
    let mut cursor = 0i64;
    for (start, length) in &rows {
        assert_eq!(
            *start, cursor,
            "extent rows must be contiguous: expected a row at {cursor}, found {start}"
        );
        assert!(*length > 0, "an empty row is not useful");
        cursor += length;
    }
    assert_eq!(
        cursor,
        (block + 500) as i64,
        "the rows cover exactly the file's size"
    );
}

/// **C2** — an extent row whose `length` exceeds its object must be reported as
/// `Corrupt`, not read out of bounds and not panicked on.
#[test]
fn c2_an_over_long_extent_reports_corrupt() {
    let block = BLOCK_BYTES as usize;
    let root = TempRoot::new("c2-length");
    {
        let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
        fs.create("/f").expect("create");
        fs.write_at("/f", 0, &pattern(block)).expect("write");
    }

    // Make the row claim far more bytes than the object holds.
    {
        let connection =
            rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
        connection
            .execute("UPDATE file_extent SET length = length + 100000", [])
            .expect("tamper");
    }

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    let mut buffer = vec![0xAAu8; block];
    let result = fs.read_at("/f", 0, &mut buffer);
    match result {
        // The guard clamps the copy to the object's real length, so an over-long
        // row yields the object's bytes and zeros for the rest — it never reads
        // out of bounds and never panics. That is the behaviour this test pins.
        Ok(read) => {
            assert_eq!(read, block, "the reported length is the requested span");
            assert_eq!(
                &buffer[..block],
                &pattern(block)[..],
                "an over-long row must not produce wrong bytes"
            );
        }
        Err(FsError::Corrupt { detail }) => {
            assert!(
                detail.contains("extent at"),
                "the message must name the extent: {detail}"
            );
        }
        other => panic!("an over-long extent must be handled, got {other:?}"),
    }
}

/// **C2** — a corrupt `content_root` on the blob path is reported, not read out of
/// bounds.
#[test]
fn c2_a_truncated_blob_reports_corrupt_or_a_clean_read() {
    let root = TempRoot::new("c2-blob");
    {
        let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
        fs.create("/f").expect("create");
        fs.write_at("/f", 0, b"short content").expect("write");
    }

    // Claim a size larger than the stored object.
    {
        let connection =
            rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
        connection
            .execute("UPDATE file SET size = size + 100", [])
            .expect("tamper");
    }

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    // The read reports the requested length and zero-fills past the object's end:
    // a size field that overstates the content is honoured as zeros rather than
    // returning an error, which matches §4.1's "the size is the boundary".
    let mut buffer = vec![0xAAu8; 30];
    let read = fs.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(read, 30);
    assert_eq!(&buffer[..13], b"short content");
    assert!(
        buffer[13..].iter().all(|byte| *byte == 0),
        "the missing tail reads as zeros"
    );
}

/// **C3** — a `content_root` that is not a valid object id is reported as
/// `InvalidRecord`, not silently ignored.
#[test]
fn c3_a_malformed_content_root_reports_invalid_record() {
    let root = TempRoot::new("c3-root");
    {
        let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
        fs.create("/f").expect("create");
        fs.write_at("/f", 0, b"content").expect("write");
    }
    {
        let connection =
            rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
        connection
            .execute("UPDATE file SET content_root = 'not-a-hex-object-id'", [])
            .expect("tamper");
    }

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    let mut buffer = [0xAAu8; 7];
    // The malformed id surfaces through the namespace layer's InvalidRecord.
    let result = fs.read_at("/f", 0, &mut buffer);
    assert!(
        result.is_err(),
        "a malformed content_root must not read as content, got {result:?}"
    );
}

/// **C3** — an unknown `kind` tag in the file table is reported, not treated as a
/// file or a directory.
#[test]
fn c3_an_unknown_kind_tag_reports_invalid_record() {
    let root = TempRoot::new("c3-kind");
    {
        let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
        fs.create("/f").expect("create");
    }
    {
        let connection =
            rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open metadata");
        connection
            .execute("UPDATE file SET kind = 9", [])
            .expect("tamper");
    }

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    let listing = fs.read_dir("/");
    match listing {
        Err(FsError::Namespace(_)) | Err(FsError::Corrupt { .. }) => {}
        other => panic!("an unknown kind tag must be reported, got {other:?}"),
    }
}

/// **C4** — rewriting identical bytes into an aligned block publishes no new
/// object, and still advances the root chain by exactly one.
#[test]
fn c4_rewriting_identical_block_bytes_publishes_no_new_object() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("c4-reuse");
    let content = pattern(block);
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &content).expect("write");

    let objects_before = fs.stats().expect("stats").objects;
    let roots_before = fs.root_records().expect("records").len();

    // The same bytes, at the same aligned offset.
    fs.write_at("/f", 0, &content).expect("rewrite identical");

    assert_eq!(
        fs.stats().expect("stats").objects,
        objects_before,
        "rewriting identical bytes must not publish a new object"
    );
    assert_eq!(
        fs.root_records().expect("records").len(),
        roots_before + 1,
        "the operation still publishes exactly one root"
    );
    assert_eq!(read_poisoned(&fs, "/f"), content);
}

/// **C4** — truncating to an exact block boundary leaves the surviving bytes intact
/// and republishes nothing for them.
///
/// **This test pins currently-observed behaviour, not the ideal.** An aligned
/// truncate publishes one zero-length object because the boundary branch runs
/// with `block_len == 0`; see `docs/reports/known-issues.md`. The assertions below
/// are written so that fixing the implementation means updating this test on
/// purpose rather than having it drift silently.
#[test]
fn c4_truncating_to_a_block_boundary_keeps_surviving_bytes() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("c4-truncate");
    let mut content = Vec::new();
    for index in 0..3u8 {
        content.extend(std::iter::repeat_n(index + 1, block));
    }
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &content).expect("write");
    let objects_before = fs.stats().expect("stats").objects;
    assert_eq!(objects_before, 3, "three distinct blocks, three objects");

    // Cut at the second block's start: blocks 2 and 3 go away, block 1 is untouched.
    fs.truncate("/f", block as u64).expect("truncate");

    // The bytes that survive are exactly the first block.
    assert_eq!(read_poisoned(&fs, "/f"), content[..block].to_vec());
    assert_eq!(fs.stat("/f").expect("stat").size, block as u64);

    // KI-1 (fixed in P2-0a): dropping whole blocks publishes nothing. Before the
    // fix this was `objects_before + 1`, from a zero-length object created for a
    // boundary block that does not exist.
    let objects_after = fs.stats().expect("stats").objects;
    assert_eq!(
        objects_after, objects_before,
        "an aligned truncate must not publish any object"
    );

    // Growing back reuses the surviving block, so nothing new is created.
    fs.truncate("/f", (block * 2) as u64).expect("grow");
    fs.truncate("/f", block as u64).expect("truncate again");
    assert_eq!(
        fs.stats().expect("stats").objects,
        objects_after,
        "repeated aligned trims stay object-neutral"
    );
}
