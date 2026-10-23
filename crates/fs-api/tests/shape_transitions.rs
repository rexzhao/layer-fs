//! Content-shape transitions (ADR-004 §5, F-T4-1).
//!
//! A file has two shapes: a single `content_root` object (under one block) and
//! extent rows (at or over one block). Each shape has its own read and write
//! path, and those paths are covered elsewhere — these tests cover the
//! **transitions** between them, which is where data was silently lost.
//!
//! Every test asserts the prefix byte-for-byte, not just the newly written
//! range: the failure mode was that the untouched prefix vanished.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, BLOCK_BYTES};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-shape-{label}-{}-{}-{sequence}",
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

/// A pattern that changes every byte, so a shifted or zeroed region shows up.
fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn read_all(fs: &Fs, path: &str) -> Vec<u8> {
    let size = fs.stat(path).expect("stat").size as usize;
    let mut buffer = vec![0u8; size];
    let read = fs.read_at(path, 0, &mut buffer).expect("read");
    buffer.truncate(read);
    buffer
}

/// F-T4-1: extending a blob across the block boundary must keep its prefix.
///
/// The blob is 60 KiB (under one block, so `content_root` is set and there are
/// no extent rows); writing 10 KiB at offset 60 KiB makes the file 70 KiB, which
/// switches it to the extent shape.
#[test]
fn extending_a_blob_file_across_the_boundary_keeps_the_prefix() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("blob-to-extent");
    let prefix = pattern(60 * 1024, 7);
    let suffix = pattern(10 * 1024, 91);

    fs.mkdir("/d").expect("mkdir");
    fs.create("/d/f").expect("create");
    fs.write_at("/d/f", 0, &prefix).expect("write prefix");
    // Precondition: the file really is in the blob shape.
    assert_eq!(fs.stat("/d/f").expect("stat").size, prefix.len() as u64);
    assert!(prefix.len() < block);

    fs.write_at("/d/f", prefix.len() as u64, &suffix)
        .expect("extend across the boundary");

    let mut expected = prefix.clone();
    expected.extend_from_slice(&suffix);
    assert_eq!(fs.stat("/d/f").expect("stat").size, expected.len() as u64);
    assert_eq!(
        read_all(&fs, "/d/f"),
        expected,
        "the original 60 KiB prefix must survive the shape change"
    );
}

/// The same transition on a byte boundary: a blob exactly one byte under the
/// block size, extended by one byte, must keep every earlier byte.
#[test]
fn a_blob_extended_by_one_byte_across_the_boundary_keeps_every_byte() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("blob-by-one");
    let prefix = pattern(block - 1, 3);

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &prefix).expect("write prefix");
    fs.write_at("/f", prefix.len() as u64, b"Z")
        .expect("extend");

    let mut expected = prefix.clone();
    expected.push(b'Z');
    assert_eq!(read_all(&fs, "/f"), expected);
}

/// The sibling defect: growing a blob past the block size with `truncate` must
/// keep its content, not zero it.
#[test]
fn truncating_a_blob_file_upwards_keeps_the_prefix() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("blob-grow");
    let prefix = pattern(60 * 1024, 11);

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &prefix).expect("write");

    let new_size = (block * 2) as u64;
    fs.truncate("/f", new_size).expect("grow past the boundary");

    let mut expected = prefix.clone();
    expected.resize(new_size as usize, 0);
    assert_eq!(
        read_all(&fs, "/f"),
        expected,
        "growing a blob past a block must preserve its bytes and zero-fill the rest"
    );
}

/// Shrinking back under one block returns the file to the blob shape with the
/// surviving prefix intact.
#[test]
fn shrinking_an_extent_file_back_to_a_blob_keeps_the_prefix() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("extent-to-blob");
    let content = pattern(block * 2 + 100, 5);

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &content).expect("write large");

    let keep = 10 * 1024u64;
    fs.truncate("/f", keep).expect("shrink below one block");
    assert_eq!(read_all(&fs, "/f"), content[..keep as usize].to_vec());

    // And the file must still be writable after returning to the blob shape.
    fs.write_at("/f", 4, b"XY").expect("write into blob");
    let mut expected = content[..keep as usize].to_vec();
    expected[4..6].copy_from_slice(b"XY");
    assert_eq!(read_all(&fs, "/f"), expected);
}

/// A full round trip: blob, then extent, then edited in both shapes, then back.
#[test]
fn blob_extent_blob_round_trip_keeps_every_edit() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("round-trip");
    let mut expected: Vec<u8> = Vec::new();

    fs.create("/f").expect("create");

    // Blob shape.
    let first = pattern(1000, 1);
    fs.write_at("/f", 0, &first).expect("write blob");
    expected.extend_from_slice(&first);

    // Grow into the extent shape by appending past one block.
    let filler = vec![0u8; block + 500];
    fs.write_at("/f", expected.len() as u64, &filler)
        .expect("grow to extents");
    expected.extend_from_slice(&filler);

    // Edit inside the original blob range, which now lives in a block.
    fs.write_at("/f", 10, b"EDITED").expect("edit early");
    expected[10..16].copy_from_slice(b"EDITED");

    // Edit near the end.
    let last = expected.len() as u64;
    fs.write_at("/f", last - 8, b"TAILTAIL").expect("edit tail");
    let tail = expected.len() - 8;
    expected[tail..].copy_from_slice(b"TAILTAIL");

    assert_eq!(read_all(&fs, "/f"), expected, "both shapes must agree");

    // Back to a blob and edit again.
    let keep = 600u64;
    fs.truncate("/f", keep).expect("shrink to blob");
    expected.truncate(keep as usize);
    assert_eq!(read_all(&fs, "/f"), expected);

    fs.write_at("/f", 0, b"HEAD")
        .expect("edit after returning to blob");
    expected[..4].copy_from_slice(b"HEAD");
    assert_eq!(read_all(&fs, "/f"), expected);

    // The closure check must still be clean after all the transitions.
    assert!(fs.verify().expect("verify").is_clean());
}

/// Cross the boundary from the blob shape while writing past the end, which
/// also exercises the zero-fill branch together with the shape change.
#[test]
fn crossing_the_boundary_with_a_gap_zero_fills_and_keeps_the_prefix() {
    let block = BLOCK_BYTES as usize;
    let (_root, fs) = fixture("blob-gap");
    let prefix = pattern(1000, 13);

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &prefix).expect("write");

    // Skip to past two blocks and write; everything in between is zeros.
    let offset = (block * 2 + 10) as u64;
    fs.write_at("/f", offset, b"END")
        .expect("write past the boundary");

    let mut expected = prefix.clone();
    expected.resize(offset as usize + 3, 0);
    expected[offset as usize..].copy_from_slice(b"END");
    assert_eq!(read_all(&fs, "/f"), expected);
}
