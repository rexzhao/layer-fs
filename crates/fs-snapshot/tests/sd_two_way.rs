//! The security descriptor's **two directions** (P3-2 card §3.1, G3): the stored form grows, the stat path does not.
//!
//! Two separate assertions, deliberately not one:
//!
//! 1. **the object graph changes** — a record carrying a 4 KiB descriptor produces a page 4 KiB larger, which is what
//!    "SD belongs to the record" means;
//! 2. **the read path does not change** — `SnapshotReader::inode` fetches the **same number of objects** whether or not
//!    the record has a descriptor, because the descriptor rides inside the inode page rather than becoming a second
//!    object. A `Stat` implementation that fetched the SD separately would break this, and only a count can see it.
//!
//! The counting source **proves it moves first** (`control` below): a counter that never increments would make both
//! comparisons trivially equal.

use std::cell::RefCell;
use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use fs_snapshot::record::Content;
use fs_snapshot::{
    encode_inode_table, encode_root, InodeRecord, Kind, ObjectSource, PageSink, Profile, RootValue,
    SnapshotReader, SnapshotResult,
};

#[derive(Default)]
struct Memory {
    objects: BTreeMap<ObjectId, (Role, Vec<u8>)>,
}

impl ObjectSource for Memory {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        self.objects
            .get(&oid)
            .map(|(role, value)| {
                cas_core::Object::new(*role, value)
                    .expect("framed object")
                    .to_vec()
            })
            .ok_or_else(|| {
                fs_snapshot::SnapshotError::Store(cas_store::StoreError::NotFound { oid })
            })
    }
}

impl PageSink for Memory {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        let oid = ObjectId::from_bytes(blake3::hash(value).into());
        self.objects.insert(oid, (role, value.to_vec()));
        Ok(oid)
    }
}

/// Counts `get` calls and records their sizes: the counter is the measurement, the sizes are the cross-check.
struct Counting<'a> {
    inner: &'a Memory,
    gets: RefCell<Vec<usize>>,
}

impl ObjectSource for Counting<'_> {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        let bytes = self.inner.get(oid)?;
        self.gets.borrow_mut().push(bytes.len());
        Ok(bytes)
    }
}

fn snapshot(descriptor: &[u8], serials: u64) -> (Memory, ObjectId) {
    let profile = Profile::V3;
    let mut memory = Memory::default();
    let records: Vec<InodeRecord> = (1..=serials)
        .map(|serial| InodeRecord {
            serial,
            kind: Kind::File,
            size: 0,
            created_ns: 1,
            modified_ns: 2,
            accessed_ns: 3,
            changed_ns: 4,
            attributes: 0x20,
            security: descriptor.to_vec(),
            content: Content::Empty,
        })
        .collect();
    let table = encode_inode_table(&profile, records.into_iter(), &mut memory).expect("table");
    let root = encode_root(
        &profile,
        &RootValue {
            scope_id: [4u8; 32],
            inode_table: table,
        },
        &mut memory,
    )
    .expect("root");
    (memory, root)
}

fn page_bytes(memory: &Memory) -> usize {
    memory
        .objects
        .values()
        .filter(|(role, _)| *role == Role::InodeTable)
        .map(|(_, value)| value.len())
        .sum()
}

/// Direction 1 — the stored form grows by exactly the descriptor's length.
#[test]
fn a_descriptor_makes_the_inode_page_grow_by_its_length() {
    let (without, _) = snapshot(&[], 1);
    let (with, _) = snapshot(&vec![0xA5u8; 4096], 1);
    assert_eq!(
        page_bytes(&with),
        page_bytes(&without) + 4096,
        "cell growth: the descriptor is stored inside the record, so the page grows by exactly its length"
    );
}

/// Direction 2 — reading an inode costs the **same number of objects** with and without a descriptor.
#[test]
fn reading_an_inode_fetches_the_same_objects_with_or_without_a_descriptor() {
    let (plain, plain_root) = snapshot(&[], 1);
    let (rich, rich_root) = snapshot(&vec![0xA5u8; 4096], 1);

    let plain_source = Counting {
        inner: &plain,
        gets: RefCell::new(Vec::new()),
    };
    SnapshotReader::open(&plain_source, plain_root, Profile::V3)
        .expect("open")
        .inode(1)
        .expect("read");
    let plain_gets = plain_source.gets.borrow().clone();

    let rich_source = Counting {
        inner: &rich,
        gets: RefCell::new(Vec::new()),
    };
    SnapshotReader::open(&rich_source, rich_root, Profile::V3)
        .expect("open")
        .inode(1)
        .expect("read");
    let rich_gets = rich_source.gets.borrow().clone();

    // Control first: the counter moves, and it sees the descriptor when there is one.
    assert!(
        !plain_gets.is_empty() && !rich_gets.is_empty(),
        "cell control: the counting source really observed reads"
    );
    assert!(
        rich_gets.iter().sum::<usize>() > plain_gets.iter().sum::<usize>(),
        "cell control: the counter sees the larger record — otherwise the equality below would be vacuous \
         (plain {plain_gets:?} vs rich {rich_gets:?})"
    );
    assert_eq!(
        rich_gets.len(),
        plain_gets.len(),
        "cell counts: the same number of objects is fetched — the descriptor rides inside the inode page, it is not a \
         second object (plain {plain_gets:?} vs rich {rich_gets:?})"
    );
}
