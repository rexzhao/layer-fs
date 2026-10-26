//! The core-side surface the adapter needs, and nothing more (P3-3a card §2).
//!
//! **One method = one core call.** Anything that is a frontend *policy* — delete-pending bookkeeping, share-bit
//! adjudication, `DELETE_ON_CLOSE`, handle reference counts — lives in the adapter, not here, because the core has no
//! notion of a handle. The two places where that line is thin are documented at the method that straddles it:
//! [`WorkspaceOps::remove_at`] (called only when the last handle closes) and [`WorkspaceOps::flush`].
//!
//! # Why the trait, and not `fs-api` directly
//!
//! The contract tests in P3-3a run against a mock; P3-3b runs **the same tests** against the real backend. If the
//! adapter called `fs-api` directly, the mock would have to fake `fs-api`'s shape instead of the semantics the adapter
//! actually needs, and every test would then be a test of the fake.

use fs_semantics::disposition::Disposition;

/// The kind of a namespace entry, as the adapter needs to report it.
///
/// # Why this is not the core's `Kind`
///
/// The core's `fs_namespace::Kind` would drag `rusqlite` (bundled SQLite, hence a C toolchain and a platform) into a
/// crate that must build and test anywhere — the same reason the disposition table lives in `fs-semantics` rather than
/// in `fs-namespace`. The alternative would be to move `Kind` into `fs-semantics`; it is not moved **yet** because this
/// crate is the only second consumer, `fs-semantics` is zero-dependency by an asserted boundary, and a two-variant
/// vocabulary is not worth a boundary change before P3-3b proves the real adapter needs it.
///
/// The consequence is a **single** conversion point: the P3-3b adapter maps `fs_namespace::Kind` to this one, and a
/// `Kind` variant added on either side fails that conversion to compile (both matches are exhaustive, no `_` arm).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A file: has bytes, has a size.
    File,
    /// A directory: has entries, has no bytes.
    Directory,
}

/// Timestamps in nanoseconds since the Unix epoch, all four of them.
///
/// The adapter must not map these to `FILETIME` itself in this crate: the 1601 offset and its `i128` intermediate live
/// in `fs_windows::time`, so there is exactly one place that can get the epoch wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Times {
    /// Creation time.
    pub created: i64,
    /// Last access time.
    pub accessed: i64,
    /// Last write time.
    pub written: i64,
    /// Metadata change time (NT's `ChangeTime`).
    pub changed: i64,
}

/// File attributes, in the `FILE_ATTRIBUTE_*` sense.
///
/// A newtype rather than a bare `u32`: it keeps attribute bits from being passed where a length or a mask is expected,
/// which is the kind of mistake that compiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes(pub u32);

/// Everything `get_info` reports about one inode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// The identity — stable across rename, never reused.
    pub file_id: u64,
    /// File or directory.
    pub kind: Kind,
    /// Current byte length (0 for directories).
    pub size: u64,
    /// The four timestamps.
    pub times: Times,
    /// Attribute bits.
    pub attributes: Attributes,
}

/// What `create_or_open` produced.
///
/// `size`/`times`/`attributes` are read **at return time**, not cached from the open: the card's §2 note is explicit
/// that a cached copy makes the "a new handle sees size 0 after `SUPERSEDE`" assertion falsely green.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenOutcome {
    /// The entry the name now refers to.
    pub file_id: u64,
    /// File or directory.
    pub kind: Kind,
    /// What the call had to do (core's `Action`, re-exported so the adapter does not keep a second copy).
    pub action: fs_semantics::disposition::Action,
    /// Size at return time.
    pub size: u64,
    /// Timestamps at return time.
    pub times: Times,
    /// Attribute bits at return time.
    pub attributes: Attributes,
    /// Where the name lives: `(parent id, raw name bytes)`.
    pub binding: (u64, Vec<u8>),
    /// The entry that lost the name, for `SUPERSEDE`. Still readable by its own handle.
    pub replaced: Option<u64>,
}

/// What `rename` produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameOutcome {
    /// Where the moved entry now lives: `(parent id, raw destination name bytes)`.
    pub binding: (u64, Vec<u8>),
    /// The entry that lost the destination name, already retired by the core.
    pub replaced: Option<u64>,
}

/// One directory entry, as `read_dir` returns it.
///
/// No `.`/`..`: the adapter synthesises those, and a backend that also returned them would make the adapter show them
/// twice. The name is raw bytes on purpose — case is preserved and compared byte-wise (ADR-009 §4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Raw name bytes, without any path prefix.
    pub name: Vec<u8>,
    /// File or directory, so the adapter can fill `FileAttributes` without a second call.
    pub kind: Kind,
}

/// A typed operation failure.
///
/// Deliberately **not** `NTSTATUS`: mapping is the adapter's job (one place, `fs_windows::error`), and a mock that
/// could already produce `NTSTATUS` would let a wrong mapping test pass by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpsError {
    /// The entry or a parent does not exist (or the `remove_at` guard did not match).
    NotFound,
    /// A name is already taken and the call did not ask to replace.
    NameCollision,
    /// Types disagree (file over directory or the reverse).
    TypeMismatch,
    /// A directory that still has entries blocks the operation.
    NotEmpty,
    /// A path component that must be a directory is a file.
    NotADirectory,
    /// A directory operation was applied to a file.
    IsADirectory,
    /// The namespace is read-only.
    ReadOnly,
    /// The request is outside the file.
    OutOfRange,
    /// A malformed path (empty component, embedded separator, reserved name).
    InvalidParameter,
    /// Anything the core reported that has no dedicated variant here.
    Other(String),
}

/// The result type of every operation.
pub type OpsResult<T> = Result<T, OpsError>;

/// Everything the adapter needs from the core.
///
/// Implementors: `mock::MockBackend` (this card) and the real `fs-api` adapter (P3-3b). The contract tests are generic
/// over this trait, so a behaviour that only one backend has is a bug in that backend, not a footnote.
pub trait WorkspaceOps {
    /// Opens or creates `path` according to `disposition`, returning the resulting entry **and its current metadata**.
    ///
    /// # Errors
    ///
    /// [`OpsError::NameCollision`] for `Create` on an existing name, [`OpsError::TypeMismatch`] where the disposition
    /// and the existing kind disagree.
    fn create_or_open(&self, path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome>;

    /// Reads up to `len` bytes at `offset`; a short read at EOF is zero-padded by the caller's contract, not here.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for a retired inode, [`OpsError::IsADirectory`] for a directory.
    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>>;

    /// Writes all of `bytes` at `offset`, extending the file if needed.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`], [`OpsError::IsADirectory`], [`OpsError::ReadOnly`].
    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize>;

    /// Sets the length, truncating or zero-extending.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`], [`OpsError::IsADirectory`].
    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()>;

    /// Lists the entries of a directory **by FileId**, so a directory handle stays valid across a rename.
    ///
    /// Returned order is the backend's; the adapter sorts by its own folded key (§3.6's snapshot, not this call).
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for a retired inode, [`OpsError::NotADirectory`] for a file.
    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>>;

    /// Current metadata for one inode. Never cached by the adapter: `GetFileInformation` reports the current state.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for a retired inode.
    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo>;

    /// Sets the writable timestamps (`accessed`, `written`, `changed`, `created`), leaving `None` fields alone.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`].
    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()>;

    /// Replaces the attribute bits.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`].
    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()>;

    /// Moves `from` to `to`, replacing the destination when `replace` is true.
    ///
    /// # Errors
    ///
    /// [`OpsError::NameCollision`] when `replace` is false and the name is taken, [`OpsError::NotEmpty`] for a
    /// non-empty destination directory, [`OpsError::TypeMismatch`] for a cross-type move.
    fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome>;

    /// Removes one entry, guarded by the identity the caller believes is there.
    ///
    /// The adapter calls this **once**, when the last handle closes — that ordering is the adapter's job, which is why
    /// this signature takes a name pair and an identity rather than a `delete_pending` flag. A guard mismatch is
    /// [`OpsError::NotFound`]: the name no longer refers to the entry the caller meant, and **retiring whatever is
    /// there now would delete the wrong object** (the §3.9 path-reuse case).
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] when the name is gone, belongs to another inode, or the inode is already retired.
    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()>;

    /// Flushes the workspace's pending state to its durable form.
    ///
    /// # Errors
    ///
    /// [`OpsError::Other`] when the backend reports a failure that has no dedicated variant.
    fn flush(&self) -> OpsResult<()>;
}
