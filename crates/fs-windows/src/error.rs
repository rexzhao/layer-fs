//! Windows error model: typed failures ↔ NTSTATUS (P3-1 §1 `error`, ADR-009 §6).
//!
//! The mapping is an **exhaustive `match`**: a new [`Failure`] variant does not compile until it is given
//! an NTSTATUS. That is the module's "missing cell is red" mechanism — the compiler is the checker, and the
//! test below only has to prove the numbers and names are the ones the driver will send.

/// The NTSTATUS values this frontend can return.
///
/// Only the ones ADR-009 §6 names. Kept as an enum rather than bare integers so a table can be checked for
/// duplicates and so a test failure prints `ObjectNameCollision`, not `0xC0000035` (card §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NtStatus {
    /// The operation succeeded.
    Success,
    /// A path component is malformed (illegal character, reserved name, too long).
    ObjectNameInvalid,
    /// A name collides with an existing entry after case folding.
    ObjectNameCollision,
    /// The final component does not exist.
    ObjectNameNotFound,
    /// A parent directory along the path does not exist.
    ObjectPathNotFound,
    /// A component in the middle of the path is a file.
    NotADirectory,
    /// The target is a directory where a file was expected (or vice versa).
    FileIsADirectory,
    /// The entry is delete-pending: it exists but cannot be opened.
    DeletePending,
    /// The share modes of an existing handle forbid this access.
    SharingViolation,
    /// The caller may not perform this operation.
    AccessDenied,
    /// The directory is not empty.
    DirectoryNotEmpty,
    /// A parameter is out of range for the operation.
    InvalidParameter,
    /// A read started at or past the end of the file.
    EndOfFile,
    /// A write would exceed the maximum file size.
    FileTooLarge,
    /// The store has no space left.
    DiskFull,
    /// The operation is valid but this model does not implement it (ACL, locks, reparse, …).
    NotSupported,
    /// An unexpected internal failure.
    InternalError,
}

impl NtStatus {
    /// The 32-bit value the driver returns.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Success => 0x0000_0000,
            Self::ObjectNameInvalid => 0xC000_0033,
            Self::ObjectNameCollision => 0xC000_0035,
            Self::ObjectNameNotFound => 0xC000_0034,
            Self::ObjectPathNotFound => 0xC000_003A,
            Self::NotADirectory => 0xC000_010F,
            Self::FileIsADirectory => 0xC000_00BA,
            Self::DeletePending => 0xC000_0051,
            Self::SharingViolation => 0xC000_0043,
            Self::AccessDenied => 0xC000_0022,
            Self::DirectoryNotEmpty => 0xC000_0101,
            Self::InvalidParameter => 0xC000_000D,
            Self::EndOfFile => 0xC000_0011,
            Self::FileTooLarge => 0xC000_0093,
            Self::DiskFull => 0xC000_007F,
            Self::NotSupported => 0xC000_00BB,
            Self::InternalError => 0xC000_0001,
        }
    }

    /// A stable name, for assertions and logs (card §2.5: never assert on a bare number).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "STATUS_SUCCESS",
            Self::ObjectNameInvalid => "STATUS_OBJECT_NAME_INVALID",
            Self::ObjectNameCollision => "STATUS_OBJECT_NAME_COLLISION",
            Self::ObjectNameNotFound => "STATUS_OBJECT_NAME_NOT_FOUND",
            Self::ObjectPathNotFound => "STATUS_OBJECT_PATH_NOT_FOUND",
            Self::NotADirectory => "STATUS_NOT_A_DIRECTORY",
            Self::FileIsADirectory => "STATUS_FILE_IS_A_DIRECTORY",
            Self::DeletePending => "STATUS_DELETE_PENDING",
            Self::SharingViolation => "STATUS_SHARING_VIOLATION",
            Self::AccessDenied => "STATUS_ACCESS_DENIED",
            Self::DirectoryNotEmpty => "STATUS_DIRECTORY_NOT_EMPTY",
            Self::InvalidParameter => "STATUS_INVALID_PARAMETER",
            Self::EndOfFile => "STATUS_END_OF_FILE",
            Self::FileTooLarge => "STATUS_FILE_TOO_LARGE",
            Self::DiskFull => "STATUS_DISK_FULL",
            Self::NotSupported => "STATUS_NOT_SUPPORTED",
            Self::InternalError => "STATUS_INTERNAL_ERROR",
        }
    }

    /// Whether the value is an error (the high bit of a 32-bit NTSTATUS).
    #[must_use]
    pub const fn is_error(self) -> bool {
        self.code() & 0xC000_0000 == 0xC000_0000
    }
}

/// The failure vocabulary, defined once in the core (P3-2 §12) and re-exported here.
///
/// The table that produces these values lives in `fs_namespace::disposition`, because the core performs the
/// action; this crate's job is only to say what Windows should see for each of them.
pub use fs_namespace::disposition::Failure;

/// The mapping. **Exhaustive on purpose**: every `Failure` names its own `NtStatus`, and there is no
/// fallback arm, so a new variant is a compile error until somebody decides what Windows should see
/// (P3-1 §1 `error`, §2.1). `tests/no_wildcards.rs` scans these sources so the fallback cannot come back —
/// not even in a comment, because the scan is textual and a human has to be able to trust it.
#[must_use]
pub const fn to_status(failure: Failure) -> NtStatus {
    match failure {
        Failure::InvalidName => NtStatus::ObjectNameInvalid,
        Failure::NameCollision => NtStatus::ObjectNameCollision,
        Failure::LeafMissing => NtStatus::ObjectNameNotFound,
        Failure::ParentMissing => NtStatus::ObjectPathNotFound,
        Failure::NotADirectory => NtStatus::NotADirectory,
        Failure::IsADirectory => NtStatus::FileIsADirectory,
        Failure::DeletePending => NtStatus::DeletePending,
        Failure::SharingViolation => NtStatus::SharingViolation,
        Failure::AccessDenied => NtStatus::AccessDenied,
        Failure::DirectoryNotEmpty => NtStatus::DirectoryNotEmpty,
        Failure::InvalidParameter => NtStatus::InvalidParameter,
        Failure::EndOfFile => NtStatus::EndOfFile,
        Failure::FileTooLarge => NtStatus::FileTooLarge,
        Failure::NoSpace => NtStatus::DiskFull,
        Failure::Unsupported => NtStatus::NotSupported,
        Failure::Internal => NtStatus::InternalError,
    }
}

/// Every [`Failure`] variant, for completeness checks in tests (re-exported from the core's table).
pub use fs_namespace::disposition::ALL_FAILURES;

/// Every [`NtStatus`] value this build can return.
pub const ALL_STATUSES: [NtStatus; 17] = [
    NtStatus::Success,
    NtStatus::ObjectNameInvalid,
    NtStatus::ObjectNameCollision,
    NtStatus::ObjectNameNotFound,
    NtStatus::ObjectPathNotFound,
    NtStatus::NotADirectory,
    NtStatus::FileIsADirectory,
    NtStatus::DeletePending,
    NtStatus::SharingViolation,
    NtStatus::AccessDenied,
    NtStatus::DirectoryNotEmpty,
    NtStatus::InvalidParameter,
    NtStatus::EndOfFile,
    NtStatus::FileTooLarge,
    NtStatus::DiskFull,
    NtStatus::NotSupported,
    NtStatus::InternalError,
];
