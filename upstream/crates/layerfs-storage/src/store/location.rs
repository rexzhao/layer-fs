//! Engine-independent physical locations and stored value rows.

use layerfs_content::{ObjectId, ObjectRole};

/// One stored object's descriptor and physical location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectLocation {
    /// Canonical identity.
    pub object_id: ObjectId,
    /// Logical role code.
    pub role: ObjectRole,
    /// Canonical object length recorded at save time.
    pub canonical_length: usize,
    /// Pack holding the record.
    pub pack_id: i64,
    /// Group ordinal inside the pack.
    pub group_number: usize,
    /// Record ordinal inside the group.
    pub record_number: usize,
}

/// One stored inode value: the row a pooled leaf names by its ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeValueRow {
    /// The ordinal, assigned once from a Save's reserved block.
    pub ordinal: u32,
    /// The 73 value bytes of one inode-leaf row.
    pub value: [u8; layerfs_content::inode_leaf::INODE_VALUE_BYTES],
}

/// One persisted advisory content-signature ring entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignatureRow {
    /// Ring slot overwritten by this entry.
    pub slot: usize,
    /// One-based insertion order.
    pub stamp: u64,
    /// Admitted canonical object identity.
    pub object_id: ObjectId,
    /// Eight folded hashes in their existing little-endian representation.
    pub signature: [u8; 32],
}

/// Physical body destination, determined by its lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackDomain {
    /// Mapping and pooled metadata bytes.
    Metadata,
    /// File-content bytes.
    Payload,
}

impl PackDomain {
    /// Routes a lane without consulting an engine.
    pub const fn for_lane(lane: crate::pack::layout::PackLane) -> Self {
        use crate::pack::layout::PackLane;
        match lane {
            PackLane::Ordinary => Self::Metadata,
            PackLane::Chunk | PackLane::WholeFile | PackLane::Singleton => Self::Payload,
        }
    }
}

/// Persisted descriptor of a sealed immutable pack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackInfo {
    /// Positive pack id, independent of content digest.
    pub pack_id: i64,
    /// Destination dictated by the lane.
    pub domain: PackDomain,
    /// SHA-256 of the complete sealed bytes.
    pub key: crate::port::ObjectKey,
    /// Exact sealed length, without capacity padding.
    pub length: usize,
}

/// One object location together with the descriptor needed to fetch its pack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocatedObject {
    /// Canonical object descriptor and position.
    pub location: ObjectLocation,
    /// Immutable body descriptor.
    pub pack: PackInfo,
}
