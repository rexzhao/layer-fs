//! Namespace operations over the content-addressed Store (ADR-003).
//!
//! The current tree lives in mutable tables; `root` is an append-only record of
//! publications (D+, ADR-003 §4.1). Path validation happens before any touched
//! state, so an invalid path never opens a transaction.

use std::time::{SystemTime, UNIX_EPOCH};

use cas_core::ObjectId;
use cas_store::Store;

use crate::error::NamespaceResult;
use crate::path::Path;
use crate::{ContentState, Extent};

/// What kind of entry a [`FileId`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Directory = 1,
    /// A regular file.
    File = 2,
}

impl Kind {
    pub(crate) fn from_tag(tag: i64) -> Option<Kind> {
        match tag {
            1 => Some(Kind::Directory),
            2 => Some(Kind::File),
            _ => None,
        }
    }
}

/// The logical identity of a file or directory within a namespace.
///
/// Serial 0 is the root directory; allocated serials start at 1 and are never
/// reused (ADR-003 §4.3).
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
    /// The content root, for a regular file that has content.
    pub content_root: Option<cas_core::ObjectId>,
    /// Creation time, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Last modification time, Unix epoch nanoseconds.
    pub modified_ns: i64,
}

/// One row of the append-only publication chain (ADR-003 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    /// Positional serial, allocated and never reused.
    pub root_id: u64,
    /// The previous publication, or `None` for the first one.
    pub parent_root_id: Option<u64>,
    /// The operation that caused it (`mkdir`, `rmdir`, `rename`, `unlink`, …).
    pub op: String,
    /// When it was published, Unix epoch nanoseconds.
    pub created_ns: i64,
}

/// The namespace layer over one Store.
#[derive(Debug)]
pub struct Namespace {
    rules: crate::rules::Rules<crate::sql_tree::SqlTree>,
}

impl Namespace {
    /// Opens `name` in the namespace tables of `store`, creating the tables and
    /// the namespace when `mode` is [`cas_store::OpenMode::Create`].
    ///
    /// # Errors
    ///
    /// See [`crate::NamespaceDb`]; also [`crate::NamespaceError::NamespaceNotFound`]
    /// when opening an existing database without a matching namespace.
    pub fn open(store: &Store, name: &str, mode: cas_store::OpenMode) -> NamespaceResult<Self> {
        Ok(Self {
            rules: crate::rules::Rules::new(crate::sql_tree::SqlTree::open(store, name, mode)?),
        })
    }

    /// The table backend, for its own capabilities.
    #[must_use]
    pub fn backend(&self) -> &crate::sql_tree::SqlTree {
        self.rules.backend()
    }

    // ---------------------------------------------------------------- rules

    /// Creates an empty directory (ADR-003 §3.2 T1).
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, `NotADirectory`, `InvalidPath`, `AlreadyExists`.
    pub fn mkdir(&self, path: &Path) -> NamespaceResult<()> {
        self.rules.mkdir(path)
    }

    /// Removes an empty directory.
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, `NotADirectory`, `DirectoryNotEmpty`.
    pub fn rmdir(&self, path: &Path) -> NamespaceResult<()> {
        self.rules.rmdir(path)
    }

    /// Removes a file's name.
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, `IsADirectory`.
    pub fn unlink(&self, path: &Path) -> NamespaceResult<()> {
        self.rules.unlink(path)
    }

    /// Moves `from` to `to` (ADR-003 §3.3 R1–R12).
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, `AlreadyExists`, `DirectoryNotEmpty`, `IntoOwnSubtree`.
    pub fn rename(&self, from: &Path, to: &Path) -> NamespaceResult<()> {
        self.rules.rename(from, to)
    }

    /// Resolves a path to an inode.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`.
    pub fn resolve(&self, path: &Path) -> NamespaceResult<FileId> {
        self.rules.resolve(path)
    }

    /// Resolves a path's parent directory, with the final name.
    ///
    /// # Errors
    ///
    /// As [`Namespace::resolve`], plus `IsRoot` when `path` is `/`.
    pub fn resolve_parent(&self, path: &Path) -> NamespaceResult<(FileId, String)> {
        self.rules.resolve_parent(path)
    }

    /// Lists a directory.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`.
    pub fn read_dir(&self, path: &Path) -> NamespaceResult<Vec<Entry>> {
        self.rules.read_dir(path)
    }

    /// Reports one entry, filling in the content root from the backend.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`.
    pub fn stat(&self, path: &Path) -> NamespaceResult<Stat> {
        let mut stat = self.rules.stat(path)?;
        // `content_root` is not part of the tree contract (ADR-006 §7.1), so the
        // rules leave it `None` and the table backend supplies it here.
        stat.content_root = self.backend().content_root_of(stat.id)?;
        Ok(stat)
    }

    // ------------------------------------------------------- table backend

    /// The current publication serial, if any.
    #[must_use]
    pub fn head_root(&self) -> Option<u64> {
        self.backend().head_root()
    }

    /// One publication record by serial.
    #[must_use]
    pub fn root_record(&self, root_id: u64) -> Option<RootRecord> {
        self.backend().root_record(root_id)
    }

    /// Every publication record in this namespace, oldest first.
    ///
    /// # Errors
    ///
    /// Store failures while reading the table.
    pub fn root_records(&self) -> NamespaceResult<Vec<RootRecord>> {
        self.backend().root_records()
    }

    /// How many publication records exist.
    #[must_use]
    pub fn root_count(&self) -> u64 {
        self.backend().root_count()
    }

    /// The raw connection, for planting a fault in tests (ADR-003 §12.2).
    #[cfg(feature = "failure-injection")]
    #[must_use]
    pub fn connection_for_tests(&self) -> std::cell::Ref<'_, rusqlite::Connection> {
        self.backend().connection_for_tests()
    }

    /// Creates an empty file.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `AlreadyExists`, `IsADirectory`.
    pub fn create_file(&self, path: &Path) -> NamespaceResult<()> {
        self.backend().create_file(path, &self.rules)
    }

    /// The content state of a path.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`.
    pub fn content_state(&self, path: &Path) -> NamespaceResult<ContentState> {
        self.backend().content_state(path, &self.rules)
    }

    /// The `(size, content_root)` pair of a path.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`.
    pub fn shape_of(&self, path: &Path) -> NamespaceResult<(u64, Option<ObjectId>)> {
        self.backend().shape_of(path, &self.rules)
    }

    /// The extents overlapping a range.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`.
    pub fn extents_in_range(
        &self,
        path: &Path,
        from: u64,
        to: u64,
    ) -> NamespaceResult<Vec<Extent>> {
        self.backend().extents_in_range(path, from, to, &self.rules)
    }

    /// Replaces a file's single-object content.
    ///
    /// `op` is the publication label (`"write_at"`, `"truncate"`, …); it is audit data
    /// that the root chain records, so the strings P1 wrote are preserved.
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`.
    pub fn replace_content(
        &self,
        path: &Path,
        op: &str,
        state: &ContentState,
    ) -> NamespaceResult<()> {
        self.backend().replace_content(path, op, state, &self.rules)
    }

    /// Replaces a file's extent rows for the affected blocks.
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`.
    pub fn replace_extents(
        &self,
        path: &Path,
        op: &str,
        size: u64,
        content_root: Option<ObjectId>,
        replaced: &[Extent],
        removed_from: Option<u64>,
    ) -> NamespaceResult<()> {
        self.backend().replace_extents(
            path,
            op,
            size,
            content_root,
            replaced,
            removed_from,
            &self.rules,
        )
    }

    /// Inodes no directory entry reaches (P2-0a).
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn unreachable_inodes(&self) -> NamespaceResult<Vec<u64>> {
        self.backend().unreachable_inodes()
    }

    /// Directory entries whose child has no inode.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn entries_without_inode(&self) -> NamespaceResult<Vec<(u64, String)>> {
        self.backend().entries_without_inode()
    }

    /// Extent rows whose owning inode is gone.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn extents_without_inode(&self) -> NamespaceResult<Vec<(u64, u64)>> {
        self.backend().extents_without_inode()
    }

    /// Every object the namespace references, deduplicated.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn referenced_objects(&self) -> NamespaceResult<Vec<ObjectId>> {
        self.backend().referenced_objects()
    }
}

pub(crate) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0)
}
