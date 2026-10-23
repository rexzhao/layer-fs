//! Six holes, poisoned buffers and other read-path edge cases (T4-QA-1).
//!
//! Every buffer here is **poisoned** before the call. A read must write every
//! byte it reports as read, so stale memory can never be mistaken for file
//! content. Tests elsewhere that start from `vec![0u8; n]` cannot see this
//! class of defect because the fixture and the production caller share the same
//! unstated assumption.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, BLOCK_BYTES};

const POISON: u8 = 0xAA;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-holes-{label}-{}-{}-{sequence}",
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

fn poisoned(length: usize) -> Vec<u8> {
    vec![POISON; length]
}

/// QA's reproduction: a hole between two written regions must read as zeros,
/// not as whatever the caller's buffer held.
#[test]
fn a_multi_block_hole_reads_as_zeros_not_stale_buffer() {
    let (_root, fs) = fixture("multi-block");
    let far = 3 * BLOCK_BYTES;

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &[1u8; 100]).expect("write head");
    fs.write_at("/f", far, &[2u8; 50]).expect("write tail");

    // Read starting inside the hole, across the whole gap.
    let mut buffer = poisoned((far - 100 + 50) as usize);
    let read = fs.read_at("/f", 100, &mut buffer).expect("read");
    assert_eq!(read as u64, far - 100 + 50);

    let hole_len = (far - 100) as usize;
    assert!(
        buffer[..hole_len].iter().all(|byte| *byte == 0),
        "the hole must read as zeros; found stale bytes at {:?}",
        buffer[..hole_len]
            .iter()
            .position(|byte| *byte != 0)
            .map(|index| (index, buffer[index]))
    );
    assert!(buffer[hole_len..].iter().all(|byte| *byte == 2));
}

/// A read that stays entirely inside a hole still reports the requested length
/// and writes zeros.
#[test]
fn a_read_inside_a_hole_reports_the_full_length_and_zeroes_it() {
    let (_root, fs) = fixture("inside");
    let far = 4 * BLOCK_BYTES;

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"head").expect("write head");
    fs.write_at("/f", far, b"tail").expect("write tail");

    let mut buffer = poisoned(1000);
    let read = fs.read_at("/f", BLOCK_BYTES, &mut buffer).expect("read");
    assert_eq!(read, 1000);
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "a hole read must write zeros over the whole reported length"
    );
}

/// The part of the buffer beyond EOF must be left untouched, and the count must
/// reflect only what was read.
#[test]
fn bytes_past_eof_are_not_reported_and_not_claimed() {
    let (_root, fs) = fixture("past-eof");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"abc").expect("write");

    let mut buffer = poisoned(16);
    let read = fs.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(read, 3);
    assert_eq!(&buffer[..3], b"abc");
    assert!(
        buffer[3..].iter().all(|byte| *byte == POISON),
        "bytes past EOF are outside the read and must not be written"
    );
}

/// A caller may request a range whose length is larger than the remaining file;
/// the reported count covers exactly the file's part.
#[test]
fn a_request_longer_than_the_file_reports_only_the_file_part() {
    let (_root, fs) = fixture("short");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &[7u8; 10]).expect("write");

    let mut buffer = poisoned(4096);
    let read = fs.read_at("/f", 4, &mut buffer).expect("read");
    assert_eq!(read, 6);
    assert!(buffer[..6].iter().all(|byte| *byte == 7));
    assert!(buffer[6..].iter().all(|byte| *byte == POISON));
}

/// Reusing one buffer for consecutive reads must not leak one read's bytes into
/// the next one's hole.
#[test]
fn reusing_a_buffer_does_not_leak_between_reads() {
    let (_root, fs) = fixture("reuse");
    let far = 2 * BLOCK_BYTES;

    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"HEAD").expect("write head");
    fs.write_at("/f", far, b"TAIL").expect("write tail");

    let mut buffer = poisoned(far as usize + 4);
    fs.read_at("/f", 0, &mut buffer).expect("read head");
    assert_eq!(&buffer[..4], b"HEAD");

    // The same buffer, now full of "HEAD", is used to read across the hole.
    let read = fs.read_at("/f", 2, &mut buffer).expect("read across");
    let expected_len = (far as usize + 4) - 2;
    assert_eq!(read, expected_len);
    assert_eq!(&buffer[..2], b"AD");
    assert!(
        buffer[2..far as usize - 2].iter().all(|byte| *byte == 0),
        "the hole must be zeroed even when the buffer held earlier bytes"
    );
    assert_eq!(&buffer[far as usize - 2..far as usize + 2], b"TAIL");
}

/// A zero-length read must not touch the buffer at all.
#[test]
fn an_empty_request_writes_nothing() {
    let (_root, fs) = fixture("empty-out");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"data").expect("write");

    let mut buffer: [u8; 0] = [];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 0);
    assert!(buffer.is_empty());
}
