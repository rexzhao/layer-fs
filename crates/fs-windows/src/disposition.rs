//! Open dispositions (P3-1 §1 `disposition`, ADR-009 §4.2).
//!
//! Six NT dispositions × "does the target exist" × "is it a file or a directory" is a small product, so the
//! card's requirement is a **full table**: every cell has an action and an outcome, and the test walks the
//! whole product so a missing cell is red (§2.1).
//!
//! The action column matters as much as the outcome column: `FILE_OVERWRITE_IF` on an existing file and
//! `FILE_SUPERSEDE` on one both end with "opened with a truncated file", but only one of them *replaces* the
//! entry, and the difference shows up in timestamps and in who else has it open.

use crate::error::Failure;

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
