//! The file API's semantics (ADR-004 §4, §5).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::OpenMode;
use fs_api::{Fs, FsError, BLOCK_BYTES};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-api-test-{label}-{}-{}-{sequence}",
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
fn create_makes_an_empty_file() {
    let (_root, fs) = fixture("create");
    fs.create("/f").expect("create");
    let stat = fs.stat("/f").expect("stat");
    assert_eq!(stat.size, 0);
    let mut buffer = [0u8; 8];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 0);
}

#[test]
fn create_refuses_an_existing_name() {
    let (_root, fs) = fixture("create-twice");
    fs.create("/f").expect("create");
    assert!(matches!(
        fs.create("/f"),
        Err(FsError::AlreadyExists { .. })
    ));
}

#[test]
fn create_requires_an_existing_parent() {
    let (_root, fs) = fixture("create-parent");
    assert!(matches!(fs.create("/no/f"), Err(FsError::NotFound { .. })));
}

/// ADR-004 §4.1: a read shorter than the file returns only what was asked.
#[test]
fn read_at_returns_at_most_the_requested_length() {
    let (_root, fs) = fixture("read-short");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"abcdefghij").expect("write");
    let mut buffer = [0u8; 4];
    assert_eq!(fs.read_at("/f", 2, &mut buffer).expect("read"), 4);
    assert_eq!(&buffer, b"cdef");
}

/// ADR-004 §4.1: a read near EOF is short, and past EOF returns zero.
#[test]
fn read_at_is_short_near_eof_and_zero_past_it() {
    let (_root, fs) = fixture("read-eof");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"abcdefghij").expect("write");

    let mut buffer = [0u8; 8];
    assert_eq!(fs.read_at("/f", 8, &mut buffer).expect("read"), 2);
    assert_eq!(&buffer[..2], b"ij");

    assert_eq!(fs.read_at("/f", 10, &mut buffer).expect("read"), 0);
    assert_eq!(fs.read_at("/f", 999, &mut buffer).expect("read"), 0);
}

/// ADR-004 §4.1: EOF is a hard boundary; no zero padding is returned.
#[test]
fn read_at_does_not_return_bytes_past_eof() {
    let (_root, fs) = fixture("read-boundary");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"abc").expect("write");
    let mut buffer = [0xFFu8; 16];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 3);
    assert_eq!(&buffer[3..], &[0xFF; 13], "the tail is untouched");
}

/// ADR-004 §4.2: a write past EOF zero-fills the gap.
#[test]
fn write_at_past_eof_zero_fills_the_gap() {
    let (_root, fs) = fixture("write-gap");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"ab").expect("write");
    fs.write_at("/f", 5, b"XY").expect("write past eof");

    let stat = fs.stat("/f").expect("stat");
    assert_eq!(stat.size, 7);
    let mut buffer = [0u8; 7];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 7);
    assert_eq!(&buffer, b"ab\0\0\0XY");
}

#[test]
fn write_at_overwrites_in_place() {
    let (_root, fs) = fixture("write-inplace");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"0123456789").expect("write");
    fs.write_at("/f", 3, b"ab").expect("overwrite");
    let mut buffer = [0u8; 10];
    fs.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer, b"012ab56789");
    assert_eq!(fs.stat("/f").expect("stat").size, 10);
}

/// ADR-004 §4.3: shrinking truncates, growing zero-fills, FileId is stable.
#[test]
fn truncate_shrinks_and_grows_without_changing_identity() {
    let (_root, fs) = fixture("truncate");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"0123456789").expect("write");
    let before = fs.file_id("/f").expect("id");

    fs.truncate("/f", 4).expect("shrink");
    assert_eq!(fs.stat("/f").expect("stat").size, 4);
    let mut buffer = [0u8; 4];
    fs.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer, b"0123");

    fs.truncate("/f", 6).expect("grow");
    let mut buffer = [0u8; 6];
    fs.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer, b"0123\0\0");

    assert_eq!(
        fs.file_id("/f").expect("id"),
        before,
        "truncate keeps the id"
    );
}

/// ADR-004 §4.5: `flush` is a documented no-op, not an error.
#[test]
fn flush_is_a_no_op_on_a_file() {
    let (_root, fs) = fixture("flush");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"data").expect("write");
    fs.flush("/f").expect("flush");
    // Nothing was buffered, so the data was already readable and still is.
    let mut buffer = [0u8; 4];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 4);
}

#[test]
fn reading_a_directory_is_refused() {
    let (_root, fs) = fixture("read-dir");
    fs.mkdir("/d").expect("mkdir");
    let mut buffer = [0u8; 4];
    assert!(matches!(
        fs.read_at("/d", 0, &mut buffer),
        Err(FsError::IsADirectory { .. })
    ));
}

#[test]
fn remove_deletes_files_and_empty_directories() {
    let (_root, fs) = fixture("remove");
    fs.create("/f").expect("create");
    fs.remove("/f").expect("remove file");
    assert!(matches!(fs.stat("/f"), Err(FsError::NotFound { .. })));

    fs.mkdir("/d").expect("mkdir");
    fs.remove("/d").expect("remove dir");
    assert!(matches!(fs.stat("/d"), Err(FsError::NotFound { .. })));
}

#[test]
fn remove_refuses_a_non_empty_directory() {
    let (_root, fs) = fixture("remove-nonempty");
    fs.mkdir("/d").expect("mkdir");
    fs.create("/d/f").expect("create");
    assert!(matches!(
        fs.remove("/d"),
        Err(FsError::DirectoryNotEmpty { .. })
    ));
}

#[test]
fn rename_moves_a_file_and_keeps_its_content() {
    let (_root, fs) = fixture("rename");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, b"payload").expect("write");
    fs.rename("/a", "/b").expect("rename");
    let mut buffer = [0u8; 7];
    fs.read_at("/b", 0, &mut buffer).expect("read");
    assert_eq!(&buffer, b"payload");
}

/// A file exactly at the block boundary must round-trip through either path.
#[test]
fn a_file_at_the_block_boundary_round_trips() {
    let (_root, fs) = fixture("boundary");
    let bytes = vec![7u8; BLOCK_BYTES as usize];
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &bytes).expect("write");
    assert_eq!(fs.stat("/f").expect("stat").size, BLOCK_BYTES);

    let mut buffer = vec![0u8; bytes.len()];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), bytes.len());
    assert_eq!(buffer, bytes);
}

#[test]
fn directory_operations_work_through_the_api() {
    let (_root, fs) = fixture("dirs");
    fs.mkdir("/a").expect("mkdir");
    fs.mkdir("/a/b").expect("mkdir nested");
    fs.create("/a/b/f").expect("create");
    let names: Vec<String> = fs
        .read_dir("/a/b")
        .expect("read_dir")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["f"]);
}
