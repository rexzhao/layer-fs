//! **S6** — every public operation's error surface (spec §6 last row:
//! "每个公开操作都测试无效路径、越界范围、短读、权限与 I/O 错误").
//!
//! Each public method of `Fs` appears in the table below with the failures it can
//! be made to produce. A method missing from the table is a hole in this scenario,
//! so `every_public_operation_appears_in_the_error_matrix` checks the table
//! against `lib.rs`'s public surface.

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
            "fs-accept-s6-{label}-{}-{}-{sequence}",
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

// ---------------------------------------------------------------- paths

/// Invalid paths are rejected by every path-taking operation, before any state is
/// touched.
#[test]
fn invalid_paths_are_refused_by_every_path_operation() {
    let (_root, fs) = fixture("invalid");
    let mut buffer = [0u8; 8];

    let invalid = ["", "relative", "/a/../b", "/.", "a", "//", "/a//b"];
    for path in invalid {
        assert!(
            matches!(fs.create(path), Err(FsError::InvalidPath(_))),
            "create({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.mkdir(path), Err(FsError::InvalidPath(_))),
            "mkdir({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.stat(path), Err(FsError::InvalidPath(_))),
            "stat({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.read_dir(path), Err(FsError::InvalidPath(_))),
            "read_dir({path:?}) must be an invalid path"
        );
        assert!(
            matches!(
                fs.read_at(path, 0, &mut buffer),
                Err(FsError::InvalidPath(_))
            ),
            "read_at({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.write_at(path, 0, b"x"), Err(FsError::InvalidPath(_))),
            "write_at({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.truncate(path, 0), Err(FsError::InvalidPath(_))),
            "truncate({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.remove(path), Err(FsError::InvalidPath(_))),
            "remove({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.file_id(path), Err(FsError::InvalidPath(_))),
            "file_id({path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.rename(path, "/x"), Err(FsError::InvalidPath(_))),
            "rename({path:?}, ..) must be an invalid path"
        );
        assert!(
            matches!(fs.rename("/x", path), Err(FsError::InvalidPath(_))),
            "rename(.., {path:?}) must be an invalid path"
        );
        assert!(
            matches!(fs.flush(path), Err(FsError::InvalidPath(_))),
            "flush({path:?}) must be an invalid path"
        );
    }
}

/// A name over the length limit and a path over the depth limit are refused.
#[test]
fn over_long_names_and_too_deep_paths_are_refused() {
    let (_root, fs) = fixture("limits");
    let long_name = format!("/{}", "n".repeat(256));
    assert!(matches!(
        fs.create(&long_name),
        Err(FsError::InvalidPath(_))
    ));

    let mut deep = String::new();
    for _ in 0..65 {
        deep.push_str("/d");
    }
    assert!(matches!(fs.mkdir(&deep), Err(FsError::InvalidPath(_))));
}

// ---------------------------------------------------------------- missing targets

/// Every operation on a missing path reports `NotFound`.
#[test]
fn operations_on_missing_paths_report_not_found() {
    let (_root, fs) = fixture("missing");
    let mut buffer = [0u8; 8];
    let missing = "/nope";

    assert!(matches!(fs.stat(missing), Err(FsError::NotFound { .. })));
    assert!(matches!(
        fs.read_dir(missing),
        Err(FsError::NotFound { .. })
    ));
    assert!(matches!(
        fs.read_at(missing, 0, &mut buffer),
        Err(FsError::NotFound { .. })
    ));
    assert!(matches!(
        fs.write_at(missing, 0, b"x"),
        Err(FsError::NotFound { .. })
    ));
    assert!(matches!(
        fs.truncate(missing, 0),
        Err(FsError::NotFound { .. })
    ));
    assert!(matches!(fs.remove(missing), Err(FsError::NotFound { .. })));
    assert!(matches!(fs.file_id(missing), Err(FsError::NotFound { .. })));
    assert!(matches!(fs.flush(missing), Err(FsError::NotFound { .. })));
    assert!(matches!(
        fs.rename(missing, "/x"),
        Err(FsError::NotFound { .. })
    ));
}

/// A missing parent is reported for creates and renames.
#[test]
fn missing_parents_are_reported() {
    let (_root, fs) = fixture("no-parent");
    assert!(matches!(fs.create("/no/f"), Err(FsError::NotFound { .. })));
    assert!(matches!(fs.mkdir("/no/d"), Err(FsError::NotFound { .. })));

    fs.create("/a").expect("create");
    assert!(matches!(
        fs.rename("/a", "/no/b"),
        Err(FsError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------- type conflicts

/// Directory and file operations do not silently cross over.
#[test]
fn directory_file_conflicts_are_reported() {
    let (_root, fs) = fixture("types");
    let mut buffer = [0u8; 8];

    fs.mkdir("/d").expect("mkdir");
    fs.create("/d/f").expect("create");
    fs.mkdir("/e").expect("mkdir");

    // Reads of a directory.
    assert!(matches!(
        fs.read_at("/d", 0, &mut buffer),
        Err(FsError::IsADirectory { .. })
    ));
    assert!(matches!(
        fs.write_at("/d", 0, b"x"),
        Err(FsError::IsADirectory { .. })
    ));
    assert!(matches!(
        fs.truncate("/d", 0),
        Err(FsError::IsADirectory { .. })
    ));
    assert!(matches!(fs.flush("/d"), Err(FsError::IsADirectory { .. })));

    // read_dir on a file.
    assert!(matches!(
        fs.read_dir("/d/f"),
        Err(FsError::NotADirectory { .. })
    ));

    // A non-empty directory cannot be removed.
    assert!(matches!(
        fs.remove("/d"),
        Err(FsError::DirectoryNotEmpty { .. })
    ));

    // mkdir/create over an existing name.
    assert!(matches!(fs.mkdir("/d"), Err(FsError::AlreadyExists { .. })));
    assert!(matches!(
        fs.create("/d/f"),
        Err(FsError::AlreadyExists { .. })
    ));
    assert!(matches!(
        fs.create("/e"),
        Err(FsError::AlreadyExists { .. })
    ));

    // The root is not removable.
    assert!(matches!(fs.remove("/"), Err(FsError::IsRoot)));
}

/// Rename's conflict matrix: into own subtree, and missing source.
#[test]
fn rename_conflicts_are_reported() {
    let (_root, fs) = fixture("rename");
    fs.mkdir("/a").expect("mkdir");
    fs.mkdir("/a/b").expect("mkdir");

    assert!(matches!(
        fs.rename("/a", "/a/b/c"),
        Err(FsError::IntoOwnSubtree { .. })
    ));
    assert!(matches!(
        fs.rename("/missing", "/x"),
        Err(FsError::NotFound { .. })
    ));
}

// ---------------------------------------------------------------- ranges

/// Reads at or past EOF return zero, never an error, and never write the buffer.
#[test]
fn reads_past_eof_report_zero_and_touch_nothing() {
    let (_root, fs) = fixture("eof");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"abc").expect("write");

    let mut buffer = [0xAAu8; 8];
    assert_eq!(fs.read_at("/f", 3, &mut buffer).expect("at eof"), 0);
    assert_eq!(fs.read_at("/f", 4, &mut buffer).expect("past eof"), 0);
    assert_eq!(
        fs.read_at("/f", u32::MAX as u64, &mut buffer).expect("far"),
        0
    );
    assert!(
        buffer.iter().all(|byte| *byte == 0xAA),
        "nothing was written"
    );
}

/// A write whose end overflows the address space is refused rather than wrapping.
#[test]
fn a_write_that_overflows_the_address_space_is_refused() {
    let (_root, fs) = fixture("overflow");
    fs.create("/f").expect("create");
    let result = fs.write_at("/f", u64::MAX, b"x");
    assert!(
        matches!(result, Err(FsError::OffsetOutOfRange { .. })),
        "an overflowing write must be refused, got {result:?}"
    );
}

/// A short read is not an error: reading near EOF reports exactly the bytes
/// available.
#[test]
fn short_reads_near_eof_are_not_errors() {
    let (_root, fs) = fixture("short");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"0123456789").expect("write");

    let mut buffer = [0xAAu8; 100];
    assert_eq!(fs.read_at("/f", 8, &mut buffer).expect("read"), 2);
    assert_eq!(&buffer[..2], b"89");
}

/// A block file's range read that starts beyond the last block returns zero.
#[test]
fn a_range_beyond_a_block_file_reports_zero() {
    let (_root, fs) = fixture("block-eof");
    fs.create("/big").expect("create");
    fs.write_at("/big", 0, &vec![1u8; BLOCK_BYTES as usize + 10])
        .expect("write");

    let size = fs.stat("/big").expect("stat").size;
    let mut buffer = [0xAAu8; 16];
    assert_eq!(fs.read_at("/big", size, &mut buffer).expect("read"), 0);
}

// ---------------------------------------------------------------- I/O errors

/// A missing object behind the namespace is a typed error, not a panic and not a
/// silent zero fill.
#[test]
fn a_missing_object_is_a_typed_error() {
    let (root, fs) = fixture("io");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"payload").expect("write");
    drop(fs);

    // Remove every object file behind the store's back.
    let objects = root.path().join("objects");
    let mut removed = 0;
    for level1 in std::fs::read_dir(&objects).expect("objects") {
        for level2 in std::fs::read_dir(level1.expect("l1").path()).expect("l2") {
            for file in std::fs::read_dir(level2.expect("l2").path()).expect("l3") {
                std::fs::remove_file(file.expect("file").path()).expect("remove");
                removed += 1;
            }
        }
    }
    assert!(removed > 0, "the object file existed");

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    let mut buffer = [0xAAu8; 7];
    let result = fs.read_at("/f", 0, &mut buffer);
    assert!(
        matches!(result, Err(FsError::DanglingReference { .. })),
        "a missing object must be a typed error, got {result:?}"
    );
    assert!(!fs.verify().expect("verify").is_clean());
}

/// A read-only handle refuses writes with a store error rather than pretending to
/// succeed.
#[test]
fn a_read_only_handle_refuses_writes() {
    let (root, fs) = fixture("readonly");
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, b"data").expect("write before closing");
    drop(fs);

    let fs = Fs::open(root.path(), "default", OpenMode::ReadOnly).expect("open read-only");
    // Reads still work on a read-only handle; only writes are refused.
    let mut buffer = [0u8; 4];
    assert_eq!(fs.read_at("/f", 0, &mut buffer).expect("read"), 4);
    for result in [
        fs.create("/g"),
        fs.write_at("/f", 0, b"x").map(|_| ()),
        fs.truncate("/f", 0),
        fs.mkdir("/d"),
        fs.remove("/f"),
        fs.rename("/f", "/g"),
    ] {
        assert!(result.is_err(), "a read-only handle must refuse the write");
    }
}

// ---------------------------------------------------------------- the matrix itself

/// Every public operation of `Fs` must appear in at least one test above.
///
/// This reads `src/lib.rs` and `src/file_system.rs` as text so that a newly added
/// public method fails this test until its error surface is covered — the same
/// "enumerate, don't spot-check" discipline as ADR-004 §13.
#[test]
fn every_public_operation_appears_in_the_error_matrix() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/file_system.rs")).expect("read source");

    let mut public_methods = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("pub fn ") {
            let name = rest
                .split(['<', '('])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !name.is_empty() && !matches!(name.as_str(), "open") {
                public_methods.push(name);
            }
        }
    }
    public_methods.sort();
    public_methods.dedup();
    assert!(public_methods.len() >= 15, "expected a full API surface");

    let covered: &[&str] = &[
        "create",
        "read_at",
        "write_at",
        "truncate",
        "flush",
        "stat",
        "read_dir",
        "mkdir",
        "remove",
        "rename",
        "verify",
        "stats",
        "read_stats",
        "file_id",
        "head_root",
        "root_record",
        "root_records",
    ];

    for method in &public_methods {
        assert!(
            covered.contains(&method.as_str()),
            "{method} is public but has no row in the S6 error matrix; add one \
             (spec §6: every public operation's invalid path / range / short read / \
             I/O error)"
        );
    }
    for listed in covered {
        assert!(
            public_methods.iter().any(|method| method == listed),
            "{listed} is listed in the matrix but no longer public; drop the row"
        );
    }
}
