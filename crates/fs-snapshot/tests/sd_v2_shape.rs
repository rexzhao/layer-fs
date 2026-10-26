//! QA's encode-side gap: under **v2**, a descriptor must not change the encoded tree at all.
//!
//! The v2 record has no field for a security descriptor, so encoding the same records with and without one must produce
//! the **same root id** — byte-for-byte the same tree. That is the assertion the switch batch then flips: once
//! `CURRENT` is v3 the descriptor *is* part of the record and the root **must** change.
//!
//! Why the root id and not "the bytes look equal": the root id is the identity of the tree. A `set_security` that leaked
//! into a v2 page would produce a different tree with the same field values, which is exactly the fork nobody notices
//! until a v2 reader opens it.
//!
//! **This file flips with `baseline_reverse` in the `CURRENT = V3` commit — both at once, neither alone.** Two probes with
//! one expectation each: the switch commit must move them together, or one of them is left judging a format that no
//! longer ships.

use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use fs_snapshot::record::Content;
use fs_snapshot::{
    encode_inode_table, encode_root, InodeRecord, Kind, PageSink, Profile, RootValue,
    SnapshotResult,
};

#[derive(Default)]
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

fn records(descriptor: &[u8]) -> Vec<InodeRecord> {
    (1..=8u64)
        .map(|serial| InodeRecord {
            serial,
            kind: Kind::File,
            size: 0,
            created_ns: 1_700_000_000_000_000_000,
            modified_ns: 1_700_000_001_000_000_000,
            accessed_ns: 1_700_000_002_000_000_000,
            changed_ns: 1_700_000_003_000_000_000,
            attributes: 0x21,
            security: descriptor.to_vec(),
            content: Content::Empty,
        })
        .collect()
}

fn root_under(profile: Profile, descriptor: &[u8]) -> ObjectId {
    let mut memory = Memory::default();
    let table =
        encode_inode_table(&profile, records(descriptor).into_iter(), &mut memory).expect("table");
    encode_root(
        &profile,
        &RootValue {
            scope_id: [7u8; 32],
            inode_table: table,
        },
        &mut memory,
    )
    .expect("root")
}

/// **v2 form** — the descriptor is not part of the record, so the encoded root is identical with and without it.
///
/// The same comparison under v3 must *differ* (asserted in `inode_v3_page.rs` via the page length); this file is the other
/// half of the pair, and the switch commit turns it into the same demand.
#[test]
fn under_v2_a_descriptor_does_not_change_the_encoded_root() {
    assert_eq!(
        Profile::V2.inode_fmt,
        1,
        "cell precondition: this test is about the v2 record layout"
    );
    let without = root_under(Profile::V2, &[]);
    let with = root_under(Profile::V2, &[0xA5u8; 4096]);
    assert_eq!(
        without, with,
        "cell v2: a descriptor is **not representable** in a v2 record, so it cannot change the tree — a different root \
         here means somebody wrote it into the page anyway, and a v2 reader would then see a tree whose id nobody \
         recorded"
    );
}

/// Control: the same comparison under v3 **does** change the root, so the assertion above is not measuring "encoding
/// ignores my input".
#[test]
fn under_v3_the_same_input_changes_the_root() {
    let without = root_under(Profile::V3, &[]);
    let with = root_under(Profile::V3, &[0xA5u8; 4096]);
    assert_ne!(
        without, with,
        "cell control: under v3 the descriptor is part of the record, so the root must move — otherwise the v2 assertion \
         above would pass for the wrong reason (an encoder that ignored the field entirely)"
    );
}
