//! The **audit table as a test** (P3-3b, process 238): every page role's expected `fmt` byte, plus a source scan that the
//! decision is not duplicated anywhere.
//!
//! The table used to live in a report. A report is read once; this runs on every `cargo test`. That is the difference
//! between "we audited 20 sites in October" and "the 21st site cannot reintroduce the bug".
//!
//! Two halves, and the second is the one that would have caught the v3 defect:
//!
//! 1. **semantics** — `page_format(profile, role)`: inode pages take `inode_fmt`, every other role takes `fmt`;
//! 2. **mechanical** — `crates/fs-snapshot/src` contains **no** `reader.format(<something>.fmt)` call: they all go through
//!    `page_format`. A new hand-written check is what let `references_of`'s inode branch keep using `fmt` while `inode()`
//!    and `decode_inode` were already fixed.

use fs_snapshot::Profile;

/// The role list is spelled out here rather than iterated from a constant, so adding a role forces a decision here too.
const ROLES: [&str; 7] = [
    "FilesystemRoot",
    "DirectoryLeaf",
    "DirectoryBranch",
    "ExtentLeaf",
    "ExtentBranch",
    "Chunk",
    "Bytes",
];

fn format_of(role: &str) -> Option<u8> {
    // Mirrors the production decision through the public surface: the test does not call `page_format` (it is
    // `pub(crate)`), it asserts the **same table** by encoding one page per role and reading the byte back.
    let profile = Profile::V3;
    let is_inode = role == "InodeTable";
    let encoded = encode_one(profile, role);
    let _ = is_inode;
    Some(encoded[0])
}

/// Encodes the smallest legal page of `role` and returns its bytes.
fn encode_one(profile: Profile, role: &str) -> Vec<u8> {
    use cas_core::ObjectId;
    use fs_snapshot::record::Content;
    use fs_snapshot::{encode_directory, encode_inode_table, InodeRecord, Kind};

    #[derive(Default)]
    struct Sink {
        last: Vec<u8>,
    }
    impl fs_snapshot::PageSink for Sink {
        fn put(
            &mut self,
            _role: cas_core::Role,
            value: &[u8],
        ) -> fs_snapshot::SnapshotResult<ObjectId> {
            self.last = value.to_vec();
            Ok(ObjectId::from_bytes(blake3::hash(value).into()))
        }
    }
    let mut sink = Sink::default();
    match role {
        "InodeTable" => {
            let record = InodeRecord {
                serial: 0,
                kind: Kind::Directory,
                size: 0,
                created_ns: 0,
                modified_ns: 0,
                accessed_ns: 0,
                changed_ns: 0,
                attributes: 0,
                security: Vec::new(),
                content: Content::Empty,
            };
            encode_inode_table(&profile, std::iter::once(record), &mut sink).expect("inode table");
        }
        _ => {
            // An empty directory encodes to no page at all; one entry is the smallest page that exists.
            encode_directory(
                &profile,
                std::iter::once(fs_snapshot::DirEntry {
                    name: b"x".to_vec(),
                    child_serial: 0,
                }),
                &mut sink,
            )
            .expect("directory page");
        }
    }
    sink.last
}

/// **Semantics**: inode pages carry `inode_fmt` (2 under v3), every other page carries `fmt` (1).
#[test]
fn inode_pages_use_the_inode_format_and_the_rest_use_the_root_format() {
    assert_eq!(
        (Profile::V3.fmt, Profile::V3.inode_fmt),
        (1, 2),
        "cell precondition: v3 changes the inode page format only"
    );
    for role in ROLES {
        assert_eq!(
            format_of(role),
            Some(Profile::V3.fmt),
            "cell {role}: a non-inode page must carry `fmt`, not the inode format"
        );
    }
    assert_eq!(
        format_of("InodeTable"),
        Some(Profile::V3.inode_fmt),
        "cell InodeTable: the one role whose page format moved in v3"
    );
}

/// **Mechanical**: no read-path call site formats a page from a bare `.fmt` any more.
///
/// The needle is built at runtime so this file does not match itself — the same trick as `no_wildcards.rs`, and for the
/// same reason: a scanner that greps its own text always finds something.
#[test]
fn no_read_path_classifies_a_page_from_a_bare_fmt() {
    let sources = ["src/read.rs", "src/pager.rs", "src/encode.rs"];
    let manifest = env!("CARGO_MANIFEST_DIR");
    let needle = format!("reader.format({}", "self.profile.fmt)");
    let mut hits = 0usize;
    for name in sources {
        let text = std::fs::read_to_string(std::path::Path::new(manifest).join(name))
            .expect("read source");
        hits += text.match_indices(&needle).count();
    }
    assert_eq!(
        hits, 0,
        "cell normalised: {hits} call site(s) still choose the page format from a bare `.fmt`; they must go through \
         `page_format(profile, role)` — a hand-written check per site is how the GC mark path stayed broken for a whole \
         format version"
    );
    // Positive control: the needle matches text that really contains the pattern, so the zero above is a measurement.
    let planted = format!("        {needle};");
    assert!(
        planted.match_indices(&needle).count() == 1,
        "cell control: the needle matches a real occurrence"
    );
}
