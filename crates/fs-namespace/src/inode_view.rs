//! The inode view the path rules see (ADR-006 §7.1).
//!
//! Deliberately narrower than [`crate::Stat`]: no `content_root`. A file's bytes are
//! a backend concern — P1 keeps an object id in the inode row, the overlay keeps
//! pages over a base — and the rules in [`crate::operations`] never look at them.
//! Content lives behind each store's own API; this view only carries inode metadata.

use crate::{FileId, Kind};

/// What the rules need to know about an inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeView {
    /// The logical identity.
    pub id: FileId,
    /// Directory or file.
    pub kind: Kind,
    /// Logical size in bytes; zero for directories.
    ///
    /// The rules **do** change it: [`crate::TreeWrite::truncate`] backs `create_or_open`'s `FILE_OVERWRITE_IF` arm,
    /// which must truncate inside the transaction that decided the disposition. The view therefore has to carry the
    /// size, and a stale value here would make the truncation invisible to every caller that stats the entry.
    pub size: u64,
    /// Creation time, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Last modification time, Unix epoch nanoseconds.
    pub modified_ns: i64,
}
