//! The records a snapshot is built from (ADR-005 §3.2, §3.3, §3.4).
//!
//! These are the intermediate representation the encoder consumes and the decoder
//! produces. They are deliberately free of any storage or path concept: a snapshot
//! is a tree of records plus object references, nothing else.

use cas_core::ObjectId;

/// What an inode is (ADR-005 §3.2; the numeric values match P1's `Kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Directory = 1,
    /// A regular file.
    File = 2,
}

impl Kind {
    /// The tag byte for this kind.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// The kind for a tag, or `None` when the tag is not one of the two.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Kind> {
        match tag {
            1 => Some(Kind::Directory),
            2 => Some(Kind::File),
            _ => None,
        }
    }
}

/// Where an inode's bytes live (ADR-005 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// No object: a zero-length file, or a file whose bytes are all zero, or an
    /// empty directory.
    Empty,
    /// One whole object whose value length equals the file's size.
    Blob(ObjectId),
    /// An extent tree root.
    Extents(ObjectId),
    /// A directory tree root (at least one entry).
    Dir(ObjectId),
}

impl Content {
    /// The tag byte.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Content::Empty => 0,
            Content::Blob(_) => 1,
            Content::Extents(_) => 2,
            Content::Dir(_) => 3,
        }
    }
}

/// One inode (ADR-005 §3.2; v3 adds the four metadata fields — ADR-005-v3 draft §2.2).
///
/// The v3 fields exist on **one** struct rather than on a parallel `InodeRecordV3`, because a caller that has a record
/// nearly always wants to pass it to both encoders, and two structs would mean a conversion whose only job is to forget
/// the fields that v2 cannot store. The v2 codec simply **ignores** them: v2 has nowhere to put them, so "v2 keeps the
/// metadata" is not a representable state.
///
/// `Copy` is gone: `security` is a `Vec<u8>`. That is deliberate — a security descriptor can be 64 KiB, and a type that
/// copies 64 KiB per record assignment invites the very cost this stage is trying to measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeRecord {
    /// The inode's serial within the snapshot's scope.
    pub serial: u64,
    /// Directory or file.
    pub kind: Kind,
    /// Logical size; zero for directories.
    pub size: u64,
    /// Creation time, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Last modification time, Unix epoch nanoseconds.
    pub modified_ns: i64,
    /// Last access time (v3+; v2 stores nothing and reads back 0).
    ///
    /// Zero in v2 is not "accessed at the epoch": it is "this format has no such field". Any code that treats a v2
    /// record's `accessed_ns` as data is reading a default, which is why the update rule for it is "only when explicitly
    /// set" rather than "on every read".
    pub accessed_ns: i64,
    /// Metadata change time (v3+).
    pub changed_ns: i64,
    /// Stored attribute bits (v3+); only the writable four are legal (see [`crate::inode_v3`]).
    pub attributes: u32,
    /// The self-relative security descriptor (v3+); empty means "no explicit SD".
    pub security: Vec<u8>,
    /// Where the bytes are.
    pub content: Content,
}

impl InodeRecord {
    /// Checks the kind/content combination table (ADR-005 §3.2 for v1, ADR-007 §3.2 for v2).
    ///
    /// Returns the reason the combination is illegal, or `None` when it is legal.
    ///
    /// The two profiles differ in exactly one place: v2 decides a file's shape by the number
    /// of **chunks** its content has, not by comparing its size against a fixed block size.
    /// A 100-byte file and a 7 KiB file are both one chunk, so both are `Blob`; a file of
    /// many chunks is `Extents` regardless of how small those chunks are.
    #[must_use]
    pub fn illegal_reason(&self, profile: &crate::Profile) -> Option<&'static str> {
        let chunked = matches!(profile.chunker, crate::Chunker::FastCdc(_));
        match (self.kind, self.content) {
            (Kind::Directory, Content::Empty) | (Kind::Directory, Content::Dir(_)) => {
                if self.size == 0 {
                    None
                } else {
                    Some("a directory's size must be 0")
                }
            }
            (Kind::Directory, _) => Some("a directory's content must be Empty or Dir"),
            (Kind::File, Content::Dir(_)) => Some("a file's content may not be Dir"),
            (Kind::File, Content::Empty) => {
                // Empty is canonical when there is no content to store: size 0, or a file
                // whose every chunk is zero (ADR-007 §3.2, last row).
                if self.size == 0 {
                    None
                } else if chunked {
                    // Above a chunk, "all chunks zero" is legal; below it a non-zero file
                    // must be a Blob. The decoder cannot know which from the record alone,
                    // so it accepts Empty for any size and `verify --deep` catches a lie.
                    None
                } else if self.size >= profile.block_bytes {
                    None
                } else {
                    Some("a file smaller than one block must be a Blob")
                }
            }
            (Kind::File, Content::Blob(_)) => {
                if self.size == 0 {
                    Some("a zero-length file must be Empty")
                } else if chunked {
                    // One chunk, so a Blob is legal at any size a chunk may have.
                    None
                } else if self.size < profile.block_bytes {
                    None
                } else {
                    Some("a file at or above one block must be Extents or Empty")
                }
            }
            (Kind::File, Content::Extents(_)) => {
                if chunked {
                    // Two or more chunks: the size may be anything above zero.
                    if self.size == 0 {
                        Some("a zero-length file must be Empty")
                    } else {
                        None
                    }
                } else if self.size >= profile.block_bytes {
                    None
                } else {
                    Some("a file smaller than one block must be a Blob")
                }
            }
        }
    }
}

/// One directory entry: a name bound to a serial (ADR-005 §3.3).
///
/// Values live in the inode table, not here, so changing a file never rewrites a
/// directory page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The name, raw bytes, strictly increasing within a directory.
    pub name: Vec<u8>,
    /// The child's serial.
    pub child_serial: u64,
}

/// One extent: a run of a file's bytes held by one object (ADR-005 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// First logical byte.
    pub logical_start: u64,
    /// Length in bytes; must be positive.
    pub length: u64,
    /// The object holding them.
    pub object: ObjectId,
    /// Offset within that object's value.
    pub source_offset: u64,
}

/// A snapshot root's contents (ADR-005 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootValue {
    /// The serial scope this tree belongs to.
    pub scope_id: [u8; 32],
    /// The inode table root; never empty.
    pub inode_table: ObjectId,
}
