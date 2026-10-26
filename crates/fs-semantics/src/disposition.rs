//! Open dispositions, shared by the core and the Windows frontend (P3-2 §12).
//!
//! The table lives **here** rather than in `fs-windows` because both sides need the same eighteen cells: the
//! frontend classifies a request before calling the core, and the core performs the action atomically. Two copies
//! would drift, and the one that drifts is always the one without tests.
//!
//! Windows-specific material (the NTSTATUS mapping) stays in `fs-windows`; only the *semantics* move. So does the
//! failure vocabulary: [`Failure`] is defined here and re-exported there, which keeps a single enum rather than a
//! frontend copy that has to be kept in step.

/// A failure of the model, before it is an NTSTATUS.
///
/// Deliberately **finer** than an NTSTATUS: "the parent is missing" and "the leaf is missing" are one status
/// apart on Windows and are the difference between its two not-found codes, so the vocabulary carries the
/// distinction and the frontend's mapping decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// A path or name violates Windows rules.
    InvalidName,
    /// A folded name collides with an existing entry.
    NameCollision,
    /// The final component does not exist.
    LeafMissing,
    /// A directory along the path does not exist.
    ParentMissing,
    /// A path component that must be a directory is a file.
    NotADirectory,
    /// The target is a directory; the operation needs a file.
    IsADirectory,
    /// The entry is delete-pending.
    DeletePending,
    /// An existing handle's share mode forbids this access.
    SharingViolation,
    /// The caller may not do this (read-only attributes, security descriptors).
    AccessDenied,
    /// The directory is not empty.
    DirectoryNotEmpty,
    /// An argument is out of range.
    InvalidParameter,
    /// The read is at or past EOF.
    EndOfFile,
    /// The write would exceed the maximum size.
    FileTooLarge,
    /// No space.
    NoSpace,
    /// Valid but unimplemented in this model (P3-1 §2.4).
    Unsupported,
    /// Anything else.
    Internal,
}

/// The six dispositions `CreateFile` can ask for (ADR-009 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Disposition {
    /// Fail if the target exists.
    Create,
    /// Fail if the target does not exist.
    Open,
    /// Open if it exists, create if not.
    OpenIf,
    /// Always create a fresh entry, replacing whatever was there.
    OverwriteIf,
    /// Require an existing file and truncate it.
    Overwrite,
    /// Replace the entry with an empty one (or create it).
    Supersede,
}

impl Disposition {
    /// Every disposition, for completeness walks.
    pub const ALL: [Disposition; 6] = [
        Disposition::Create,
        Disposition::Open,
        Disposition::OpenIf,
        Disposition::OverwriteIf,
        Disposition::Overwrite,
        Disposition::Supersede,
    ];
}

/// What the target is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exists {
    /// Nothing at the path.
    Absent,
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

impl Exists {
    /// Every state, for completeness walks.
    pub const ALL: [Exists; 3] = [Exists::Absent, Exists::File, Exists::Directory];
}

/// What the create must do to satisfy the disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Create a new file (the entry did not exist).
    CreateNew,
    /// Open as-is.
    Open,
    /// Truncate the existing file to zero length and open it.
    TruncateExisting,
    /// Delete the existing entry and create a new one.
    ReplaceExisting,
}

/// How the call ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Opened, with the action above.
    Opened(Action),
    /// Refused with this failure.
    Failed(Failure),
}

/// The reference table, one row per legal cell of `Disposition × Exists`.
///
/// There are exactly 6 × 3 = 18 cells and all 18 are spelled out: a `match` with a wildcard would be shorter
/// and would also silently accept a new disposition.
#[must_use]
pub fn decide(disposition: Disposition, exists: Exists) -> Outcome {
    use Action::{CreateNew, Open, ReplaceExisting, TruncateExisting};
    use Exists::{Absent, Directory, File};
    use Failure::{AccessDenied, NameCollision, NotADirectory};

    match (disposition, exists) {
        // CREATE: fail if anything is there.
        (Disposition::Create, Absent) => Outcome::Opened(CreateNew),
        (Disposition::Create, File) => Outcome::Failed(NameCollision),
        (Disposition::Create, Directory) => Outcome::Failed(NameCollision),
        // OPEN: fail if nothing is there, and a directory is not a file.
        (Disposition::Open, Absent) => Outcome::Failed(Failure::LeafMissing),
        (Disposition::Open, File) => Outcome::Opened(Open),
        (Disposition::Open, Directory) => Outcome::Failed(NotADirectory),
        // OPEN_IF.
        (Disposition::OpenIf, Absent) => Outcome::Opened(CreateNew),
        (Disposition::OpenIf, File) => Outcome::Opened(Open),
        (Disposition::OpenIf, Directory) => Outcome::Failed(NotADirectory),
        // OVERWRITE_IF: creates or replaces, but a directory is still not a file.
        (Disposition::OverwriteIf, Absent) => Outcome::Opened(CreateNew),
        (Disposition::OverwriteIf, File) => Outcome::Opened(TruncateExisting),
        (Disposition::OverwriteIf, Directory) => Outcome::Failed(NotADirectory),
        // OVERWRITE: only an existing file.
        (Disposition::Overwrite, Absent) => Outcome::Failed(Failure::LeafMissing),
        (Disposition::Overwrite, File) => Outcome::Opened(TruncateExisting),
        (Disposition::Overwrite, Directory) => Outcome::Failed(NotADirectory),
        // SUPERSEDE: replaces the *entry*, which a directory is not allowed to be.
        (Disposition::Supersede, Absent) => Outcome::Opened(CreateNew),
        (Disposition::Supersede, File) => Outcome::Opened(ReplaceExisting),
        (Disposition::Supersede, Directory) => Outcome::Failed(AccessDenied),
    }
}

/// The reference table as rows, for the completeness test and for documentation.
pub const TABLE: [(Disposition, Exists, Outcome); 18] = [
    (
        Disposition::Create,
        Exists::Absent,
        Outcome::Opened(Action::CreateNew),
    ),
    (
        Disposition::Create,
        Exists::File,
        Outcome::Failed(Failure::NameCollision),
    ),
    (
        Disposition::Create,
        Exists::Directory,
        Outcome::Failed(Failure::NameCollision),
    ),
    (
        Disposition::Open,
        Exists::Absent,
        Outcome::Failed(Failure::LeafMissing),
    ),
    (
        Disposition::Open,
        Exists::File,
        Outcome::Opened(Action::Open),
    ),
    (
        Disposition::Open,
        Exists::Directory,
        Outcome::Failed(Failure::NotADirectory),
    ),
    (
        Disposition::OpenIf,
        Exists::Absent,
        Outcome::Opened(Action::CreateNew),
    ),
    (
        Disposition::OpenIf,
        Exists::File,
        Outcome::Opened(Action::Open),
    ),
    (
        Disposition::OpenIf,
        Exists::Directory,
        Outcome::Failed(Failure::NotADirectory),
    ),
    (
        Disposition::OverwriteIf,
        Exists::Absent,
        Outcome::Opened(Action::CreateNew),
    ),
    (
        Disposition::OverwriteIf,
        Exists::File,
        Outcome::Opened(Action::TruncateExisting),
    ),
    (
        Disposition::OverwriteIf,
        Exists::Directory,
        Outcome::Failed(Failure::NotADirectory),
    ),
    (
        Disposition::Overwrite,
        Exists::Absent,
        Outcome::Failed(Failure::LeafMissing),
    ),
    (
        Disposition::Overwrite,
        Exists::File,
        Outcome::Opened(Action::TruncateExisting),
    ),
    (
        Disposition::Overwrite,
        Exists::Directory,
        Outcome::Failed(Failure::NotADirectory),
    ),
    (
        Disposition::Supersede,
        Exists::Absent,
        Outcome::Opened(Action::CreateNew),
    ),
    (
        Disposition::Supersede,
        Exists::File,
        Outcome::Opened(Action::ReplaceExisting),
    ),
    (
        Disposition::Supersede,
        Exists::Directory,
        Outcome::Failed(Failure::AccessDenied),
    ),
];

/// Every [`Failure`] variant, for completeness checks in tests.
pub const ALL_FAILURES: [Failure; 16] = [
    Failure::InvalidName,
    Failure::NameCollision,
    Failure::LeafMissing,
    Failure::ParentMissing,
    Failure::NotADirectory,
    Failure::IsADirectory,
    Failure::DeletePending,
    Failure::SharingViolation,
    Failure::AccessDenied,
    Failure::DirectoryNotEmpty,
    Failure::InvalidParameter,
    Failure::EndOfFile,
    Failure::FileTooLarge,
    Failure::NoSpace,
    Failure::Unsupported,
    Failure::Internal,
];
