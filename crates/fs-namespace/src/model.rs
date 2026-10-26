//! The value types a tree is described with: identities, kinds, entries and stats.
//!
//! These outlive any one backend. The table backend that originally defined them is gone
//! (ADR-006 §14); the overlay and the snapshot reader speak the same vocabulary, and the
//! path rules are written against it.

/// What kind of entry a [`FileId`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Directory = 1,
    /// A regular file.
    File = 2,
}

/// The logical identity of a file or directory within a namespace.
///
/// Serial 0 is the root directory; allocated serials start at 1 and are never
/// reused (ADR-003 §4.3, ADR-006 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u64);

impl FileId {
    /// The root directory's identity.
    pub const ROOT: FileId = FileId(0);

    /// The raw serial.
    #[must_use]
    pub const fn serial(self) -> u64 {
        self.0
    }

    /// Wraps a raw serial.
    #[must_use]
    pub const fn from_serial(serial: u64) -> Self {
        Self(serial)
    }
}

/// A directory entry as `read_dir` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's name, byte-for-byte as stored.
    pub name: String,
    /// What the name refers to.
    pub id: FileId,
    /// Whether it is a directory or a file.
    pub kind: Kind,
}

/// What `stat` reports about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// The logical identity.
    pub id: FileId,
    /// Directory or file.
    pub kind: Kind,
    /// Logical size in bytes (0 for directories).
    pub size: u64,
    /// The content root, for a table-backend file that has one.
    ///
    /// Always `None` from the overlay and from the snapshot reader: neither represents a
    /// file's bytes as a single content root (ADR-006 §7.1). Kept in the type because a
    /// reader that *does* have one (a table-backed history) would need somewhere to say so.
    pub content_root: Option<cas_core::ObjectId>,
    /// Creation time, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Last modification time, Unix epoch nanoseconds.
    pub modified_ns: i64,
    /// Last access time, Unix epoch nanoseconds (v3; 0 from a v2 backend, where the field does not exist).
    pub accessed_ns: i64,
    /// Metadata change time, Unix epoch nanoseconds (v3; 0 from a v2 backend).
    pub changed_ns: i64,
    /// **Derived** attribute bits, ready for the Windows frontend.
    ///
    /// The store keeps only the four writable bits (`READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE`); `DIRECTORY` and `NORMAL`
    /// are computed here from `kind` and from "nothing else is set". Deriving them in one place is what lets the stored
    /// form stay minimal without the frontend having to re-derive a fact the core already knows — and it is why writing
    /// a derived bit is `Malformed` at the record layer.
    pub attributes: u32,
}

/// `FILE_ATTRIBUTE_DIRECTORY` (derived, never stored).
pub const ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_NORMAL` (derived: meaningful only when no other bit is set).
pub const ATTRIBUTE_NORMAL: u32 = 0x80;

/// The attribute bits `stat` reports: `stored` plus whatever `kind` and emptiness imply.
#[must_use]
pub fn derived_attributes(kind: Kind, stored: u32) -> u32 {
    match kind {
        Kind::Directory => stored | ATTRIBUTE_DIRECTORY,
        Kind::File => {
            if stored == 0 {
                ATTRIBUTE_NORMAL
            } else {
                stored
            }
        }
    }
}
