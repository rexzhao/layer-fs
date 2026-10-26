//! The `fmt` acceptance matrix (P3-2 card §3.1), driven through a **forgeable** source so the bytes can be made wrong on
//! purpose.
//!
//! Row 0 is the fixture proving itself: the same harness is required to read a sound v2 root **and** a sound v3 root
//! successfully before any rejection row is believed. Without that control, a harness that mis-wired its objects would
//! make every rejection row pass for the wrong reason — and "it was rejected" is exactly what a broken harness produces.
//!
//! Rows 1–4 change **one byte** at a time from a sound build, so each rejection is attributable to that byte.

use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use fs_snapshot::record::Content;
use fs_snapshot::{
    encode_inode_table, encode_root, InodeRecord, Kind, ObjectSource, PageSink, Profile, RootValue,
    SnapshotError, SnapshotReader, SnapshotResult,
};

#[derive(Default, Clone)]
struct Memory {
    objects: BTreeMap<ObjectId, (Role, Vec<u8>)>,
}

impl PageSink for Memory {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        let oid = ObjectId::from_bytes(blake3::hash(value).into());
        self.objects.insert(oid, (role, value.to_vec()));
        Ok(oid)
    }
}

impl ObjectSource for Memory {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        // The source hands back a **framed object**, not a bare page: the reader decodes the envelope and checks the
        // role. Storing bare payloads here would make every row fail with "wrong magic" — which is exactly what the
        // first version of this fixture did, and why row 0 exists.
        self.objects
            .get(&oid)
            .map(|(role, value)| {
                cas_core::Object::new(*role, value)
                    .expect("framed object")
                    .to_vec()
            })
            .ok_or_else(|| SnapshotError::Store(cas_store::StoreError::NotFound { oid }))
    }
}

fn record(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 0,
        created_ns: 1,
        modified_ns: 2,
        accessed_ns: 3,
        changed_ns: 4,
        attributes: 0x20,
        security: Vec::new(),
        content: Content::Empty,
    }
}

/// A complete, sound snapshot (root + inode table) for `profile`.
fn sound(profile: Profile) -> (Memory, ObjectId) {
    let mut memory = Memory::default();
    let table =
        encode_inode_table(&profile, vec![record(1)].into_iter(), &mut memory).expect("table");
    let root = encode_root(
        &profile,
        &RootValue {
            scope_id: [9u8; 32],
            inode_table: table,
        },
        &mut memory,
    )
    .expect("root");
    (memory, root)
}

/// Rewrites every object whose value is an inode table page (the v3 builder marks them by `fmt` + role).
fn forge_inode_fmt(memory: &mut Memory, new_fmt: u8) {
    for (_, (role, value)) in memory.objects.iter_mut() {
        if *role == Role::InodeTable {
            value[0] = new_fmt;
        }
    }
}

/// Rewrites `fmt` on every **non**-inode page (directory/extent/root), which is how row 4 is built.
fn forge_other_fmt(memory: &mut Memory, new_fmt: u8) {
    for (_, (role, value)) in memory.objects.iter_mut() {
        if *role != Role::InodeTable {
            value[0] = new_fmt;
        }
    }
}

fn read(memory: &Memory, root: ObjectId, profile: Profile) -> SnapshotResult<Option<InodeRecord>> {
    let reader = SnapshotReader::open(memory, root, profile)?;
    reader.inode(1)
}

/// **Row 0 — the fixture proves itself.** Both sound builds must read, otherwise every rejection row below is noise.
#[test]
fn row_0_the_harness_reads_sound_v2_and_v3_snapshots() {
    let (v2, v2_root) = sound(Profile::V2);
    let record = read(&v2, v2_root, Profile::V2)
        .expect("cell v2: a sound v2 snapshot reads")
        .expect("cell v2: the record is really there");
    assert_eq!(record.serial, 1, "cell v2: serial");
    // v2 has no v3 fields: they come back as absences, not as values.
    assert_eq!(
        record.accessed_ns, 0,
        "cell v2: accessed_ns is absent, i.e. 0"
    );

    let (v3, v3_root) = sound(Profile::V3);
    let record = read(&v3, v3_root, Profile::V3).expect("cell v3: a sound v3 snapshot reads");
    let record = record.expect("cell v3: the record is really there");
    assert_eq!(record.serial, 1, "cell v3: serial");
    assert_eq!(
        record.accessed_ns, 3,
        "cell v3: accessed_ns is a value, not an absence"
    );
    assert_eq!(record.changed_ns, 4, "cell v3: changed_ns");
}

/// **Row 1** — a v3 snapshot whose inode page says `fmt = 1`: a version difference, so `UnsupportedFormat`.
#[test]
fn row_1_v3_with_v2_inode_page_is_unsupported_format() {
    let (mut memory, root) = sound(Profile::V3);
    forge_inode_fmt(&mut memory, 1);
    let error = read(&memory, root, Profile::V3).expect_err("cell row 1: must be refused");
    assert!(
        matches!(error, SnapshotError::UnsupportedFormat { .. }),
        "cell row 1: UnsupportedFormat (the page is v2-shaped), not Malformed: {error:?}"
    );
}

/// **Row 2** — a v2 snapshot whose inode page says `fmt = 2`: `UnsupportedFormat`.
#[test]
fn row_2_v2_with_v3_inode_page_is_unsupported_format() {
    let (mut memory, root) = sound(Profile::V2);
    forge_inode_fmt(&mut memory, 2);
    let error = read(&memory, root, Profile::V2).expect_err("cell row 2: must be refused");
    assert!(
        matches!(error, SnapshotError::UnsupportedFormat { .. }),
        "cell row 2: UnsupportedFormat: {error:?}"
    );
}

/// **Row 3** — a v3 snapshot whose **directory/extent/root** pages say `fmt = 2`: those layouts did not change, so a
/// changed byte there is a version difference too.
#[test]
fn row_3_v3_with_v3_shaped_non_inode_pages_is_unsupported_format() {
    let (mut memory, root) = sound(Profile::V3);
    forge_other_fmt(&mut memory, 2);
    let error =
        SnapshotReader::open(&memory, root, Profile::V3).expect_err("cell row 3: must be refused");
    assert!(
        matches!(error, SnapshotError::UnsupportedFormat { .. }),
        "cell row 3: UnsupportedFormat: {error:?}"
    );
}

/// **Row 4 — the two rejections must be distinguishable.** A truncated inode page is `Malformed`, a wrong-format page is
/// `UnsupportedFormat`; a single `is_err()` would swallow the difference and let a reader that reports "unsupported" for
/// corruption (or the reverse) pass every row above.
#[test]
fn row_4_corruption_and_version_mismatch_are_different_variants() {
    let (mut truncated, root) = sound(Profile::V3);
    for (_, (role, value)) in truncated.objects.iter_mut() {
        if *role == Role::InodeTable {
            value.truncate(6); // keep fmt/node/count, drop the record
        }
    }
    let corrupt =
        read(&truncated, root, Profile::V3).expect_err("cell row 4: truncated page must fail");
    assert!(
        matches!(corrupt, SnapshotError::Malformed { .. }),
        "cell row 4a: a truncated record is Malformed: {corrupt:?}"
    );

    let (mut wrong_format, root) = sound(Profile::V3);
    forge_inode_fmt(&mut wrong_format, 1);
    let version =
        read(&wrong_format, root, Profile::V3).expect_err("cell row 4: version mismatch must fail");
    assert!(
        matches!(version, SnapshotError::UnsupportedFormat { .. }),
        "cell row 4b: a wrong fmt byte is UnsupportedFormat: {version:?}"
    );
    assert_ne!(
        std::mem::discriminant(&corrupt),
        std::mem::discriminant(&version),
        "cell row 4c: the two failures are different variants — the matrix distinguishes corruption from a version gap"
    );
}
