//! Encoders for the four trees (ADR-005 §8).
//!
//! Each function takes a strictly increasing entry stream and produces the tree's
//! root object id, or `None` when the stream is empty. Children are published before
//! the pages that reference them, so every published object only ever points at
//! objects that already exist (Root review Q3, extended inside the object graph).

use cas_core::{ObjectId, Role};

use crate::bytes::Writer;
use crate::error::{malformed, SnapshotError, SnapshotResult};
use crate::pager::{build_tree, Entry, PageSink, Shape};
use crate::profile::{Profile, TreeDomain};
use crate::record::{DirEntry, Extent, InodeRecord, RootValue};

fn dir_shape() -> Shape {
    Shape {
        domain: TreeDomain::Dir,
        leaf_role: Role::DirectoryLeaf,
        branch_role: Role::DirectoryBranch,
        inode_node_byte: false,
    }
}

fn inode_shape() -> Shape {
    Shape {
        domain: TreeDomain::Inode,
        // One role carries both leaf and branch, told apart by the `node` byte.
        leaf_role: Role::InodeTable,
        branch_role: Role::InodeTable,
        inode_node_byte: true,
    }
}

fn extent_shape() -> Shape {
    Shape {
        domain: TreeDomain::Extent,
        leaf_role: Role::ExtentLeaf,
        branch_role: Role::ExtentBranch,
        inode_node_byte: false,
    }
}

/// Encodes a directory's entries.
///
/// # Errors
///
/// `Unsorted` when names are not strictly increasing, `Malformed` for an illegal
/// name, or whatever the sink reports.
pub fn encode_directory(
    profile: &Profile,
    mut entries: impl Iterator<Item = DirEntry>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<Option<ObjectId>> {
    let mut collected = Vec::new();
    for entry in entries.by_ref() {
        if entry.name.is_empty() {
            return Err(malformed("a directory entry name may not be empty"));
        }
        if entry.name.len() > profile.name_max {
            return Err(malformed(format!(
                "entry name length {} exceeds the profile limit {}",
                entry.name.len(),
                profile.name_max
            )));
        }
        let mut leaf = Writer::new();
        leaf.u64(entry.child_serial);
        // A directory page stores the key as a `name` (ADR-005 §3.3): the length
        // prefix is part of what is written. Ordering and the page-split hash still
        // use the raw name bytes — comparing length-prefixed forms would sort by
        // length first, which is not the byte order ADR-005 §3.3 defines.
        let encoded_name = encode_name(&entry.name, profile.name_max)?;
        collected.push(Entry {
            key: entry.name.clone(),
            leaf: leaf.finish(),
            page_key: encoded_name.clone(),
            branch_key: encoded_name,
        });
    }
    build_tree(profile, &dir_shape(), collected, sink)
}

/// Encodes an inode table. The table is never empty: serial 0 is the root directory.
///
/// # Errors
///
/// As [`encode_directory`], plus `Malformed` for an illegal kind/content pair.
pub fn encode_inode_table(
    profile: &Profile,
    mut records: impl Iterator<Item = InodeRecord>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<ObjectId> {
    let mut collected = Vec::new();
    for record in records.by_ref() {
        if let Some(reason) = record.illegal_reason(profile) {
            return Err(malformed(format!("inode {}: {reason}", record.serial)));
        }
        let mut key = Writer::new();
        key.u64(record.serial);
        let key_bytes = key.finish();

        // The inode record's first field *is* its key (ADR-005 §3.2), so the page
        // stores nothing before it: `page_key` stays empty and `leaf` carries the
        // whole record. Writing the key twice would make the decoder read the kind
        // byte out of the serial's second half.
        let mut leaf = Writer::new();
        leaf.u64(record.serial);
        leaf.u8(record.kind.tag());
        leaf.u64(record.size);
        leaf.i64(record.created_ns);
        leaf.i64(record.modified_ns);
        leaf.u8(record.content.tag());
        if let Some(oid) = content_object(record) {
            leaf.oid(oid);
        }

        collected.push(Entry {
            key: key_bytes.clone(),
            page_key: Vec::new(),
            leaf: leaf.finish(),
            // In an inode branch the first key is `first_serial`, a bare u64.
            branch_key: key_bytes,
        });
    }
    if collected.is_empty() {
        return Err(malformed("an inode table may not be empty"));
    }
    build_tree(profile, &inode_shape(), collected, sink)?
        .ok_or_else(|| malformed("an inode table may not be empty"))
}

/// Encodes an extent tree.
///
/// # Errors
///
/// `Unsorted` for out-of-order or overlapping runs, `Malformed` for a zero length or
/// an out-of-range value, or whatever the sink reports.
pub fn encode_extents(
    profile: &Profile,
    mut extents: impl Iterator<Item = Extent>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<Option<ObjectId>> {
    let mut collected = Vec::new();
    for extent in extents.by_ref() {
        if extent.length == 0 {
            return Err(malformed("an extent may not have zero length"));
        }
        if extent.length > profile.max_extent_len {
            return Err(malformed(format!(
                "extent length {} exceeds the profile limit {}",
                extent.length, profile.max_extent_len
            )));
        }
        if extent.source_offset.saturating_add(extent.length) > profile.max_object_value {
            return Err(malformed(
                "extent reads past the maximum object value length",
            ));
        }
        let mut key = Writer::new();
        key.u64(extent.logical_start);
        let key_bytes = key.finish();

        // An extent record's first field is `logical_start`, its key (ADR-005 §3.4),
        // so the page stores nothing before it — same as the inode table.
        let mut leaf = Writer::new();
        leaf.u64(extent.logical_start);
        leaf.u64(extent.length);
        leaf.oid(extent.object);
        leaf.u64(extent.source_offset);

        collected.push(Entry {
            key: key_bytes.clone(),
            page_key: Vec::new(),
            leaf: leaf.finish(),
            // In an extent branch the first key is `first_start`, a bare u64.
            branch_key: key_bytes,
        });
    }
    if collected.is_empty() {
        return Ok(None);
    }
    // Overlap and ordering share one check: strictly increasing starts are not enough
    // (two runs could overlap), so the previous run's end bounds the next start.
    for pair in collected.windows(2) {
        let previous_end = {
            let mut reader = crate::bytes::Reader::new(&pair[0].leaf);
            reader
                .u64()
                .map_err(|_| malformed("extent leaf is malformed"))?; // logical_start
            let length = reader
                .u64()
                .map_err(|_| malformed("extent leaf is malformed"))?;
            let start = {
                let mut key_reader = crate::bytes::Reader::new(&pair[0].key);
                key_reader
                    .u64()
                    .map_err(|_| malformed("extent key is malformed"))?
            };
            start.saturating_add(length)
        };
        let next_start = {
            let mut key_reader = crate::bytes::Reader::new(&pair[1].key);
            key_reader
                .u64()
                .map_err(|_| malformed("extent key is malformed"))?
        };
        if next_start < previous_end {
            return Err(SnapshotError::Unsorted {
                detail: format!(
                    "extent at {next_start} starts before the previous extent ends at {previous_end}"
                ),
            });
        }
    }
    build_tree(profile, &extent_shape(), collected, sink)
}

/// Encodes a snapshot root (ADR-005 §3.1): 105 bytes, fixed length.
///
/// # Errors
///
/// Whatever the sink reports.
pub fn encode_root(
    profile: &Profile,
    root: &RootValue,
    sink: &mut dyn PageSink,
) -> SnapshotResult<ObjectId> {
    let mut value = Writer::new();
    value.u8(profile.fmt);
    value.raw(&profile.id());
    value.raw(&root.scope_id);
    value.u64(0); // root_serial: reserved, always 0
    value.oid(root.inode_table);
    debug_assert_eq!(value.len(), 105, "the root value is a fixed 105 bytes");
    sink.put(Role::FilesystemRoot, &value.finish())
}

fn content_object(record: InodeRecord) -> Option<ObjectId> {
    match record.content {
        crate::record::Content::Empty => None,
        crate::record::Content::Blob(oid)
        | crate::record::Content::Extents(oid)
        | crate::record::Content::Dir(oid) => Some(oid),
    }
}

/// Encodes a name as `u8 len | bytes` (ADR-005 §3).
pub(crate) fn encode_name(name: &[u8], name_max: usize) -> SnapshotResult<Vec<u8>> {
    if name.is_empty() || name.len() > name_max {
        return Err(malformed(format!(
            "name length {} is outside 1..={name_max}",
            name.len()
        )));
    }
    let mut bytes = Vec::with_capacity(name.len() + 1);
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name);
    Ok(bytes)
}
