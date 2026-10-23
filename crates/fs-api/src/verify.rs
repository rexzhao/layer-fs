//! Namespace-to-store closure verification (ADR-004 §7).

use cas_core::ObjectId;
use cas_store::Store;
use fs_namespace::Namespace;

use crate::error::FsResult;

/// One reference that could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingReference {
    /// The object the namespace points at.
    pub object: String,
    /// Why reading it failed.
    pub reason: String,
}

/// One internal inconsistency in the namespace tables (ADR-004 §7).
///
/// These are the ways the three tables can disagree with each other after a bug or
/// a partial write. Each is a distinct variant so a caller can tell "bytes nobody
/// can reach" from "a name pointing at nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inconsistency {
    /// A `file` row that no directory entry reaches and that is not the root.
    ///
    /// Usually an inode whose last name was removed without removing the inode.
    UnreachableInode {
        /// The orphaned inode.
        file_id: u64,
    },
    /// A directory entry whose child has no `file` row.
    EntryWithoutInode {
        /// The directory holding the entry.
        parent_id: u64,
        /// The entry's name.
        name: String,
    },
    /// A `file_extent` row whose owning inode does not exist.
    ///
    /// The blocks are unreachable bytes: the store still holds them and nothing can
    /// name them again.
    ExtentWithoutInode {
        /// The inode the row claims.
        file_id: u64,
        /// The row's logical start.
        logical_start: u64,
    },
}

impl std::fmt::Display for Inconsistency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Inconsistency::UnreachableInode { file_id } => {
                write!(
                    f,
                    "inode {file_id} is not reachable from any directory entry"
                )
            }
            Inconsistency::EntryWithoutInode { parent_id, name } => {
                write!(f, "entry {name:?} in directory {parent_id} has no inode")
            }
            Inconsistency::ExtentWithoutInode {
                file_id,
                logical_start,
            } => write!(
                f,
                "extent at {logical_start} claims inode {file_id}, which does not exist"
            ),
        }
    }
}

/// The result of a closure and consistency check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Distinct objects the namespace references.
    pub references: usize,
    /// References that could not be read back, in object order.
    pub dangling: Vec<DanglingReference>,
    /// Internal inconsistencies between the namespace tables.
    pub inconsistencies: Vec<Inconsistency>,
}

impl VerifyReport {
    /// Whether every reference resolved and the tables agree with each other.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dangling.is_empty() && self.inconsistencies.is_empty()
    }
}

/// Checks every object the namespace references, and the namespace's internal
/// consistency.
///
/// References are deduplicated first, so a block shared by ten files is read
/// once. Nothing is repaired: every problem is reported.
///
/// # Errors
///
/// Store/namespace failures while enumerating the references.
pub(crate) fn run(store: &Store, namespace: &Namespace) -> FsResult<VerifyReport> {
    let referenced: Vec<ObjectId> = namespace.referenced_objects()?;
    let mut dangling = Vec::new();
    for object in &referenced {
        // A reference is satisfied only if the object can actually be read back.
        // An index row whose bytes are missing is as unsatisfiable as a missing
        // row, so both are reported.
        match store.get(*object) {
            Ok(_) => {}
            Err(cas_store::StoreError::NotFound { .. })
            | Err(cas_store::StoreError::Corrupt { .. }) => {
                dangling.push(DanglingReference {
                    object: object.to_string(),
                    reason: "object is not readable".to_owned(),
                });
            }
            Err(other) => return Err(other.into()),
        }
    }
    // Internal consistency: the three tables must agree with each other.
    let mut inconsistencies = Vec::new();
    for file_id in namespace.unreachable_inodes()? {
        inconsistencies.push(Inconsistency::UnreachableInode { file_id });
    }
    for (parent_id, name) in namespace.entries_without_inode()? {
        inconsistencies.push(Inconsistency::EntryWithoutInode { parent_id, name });
    }
    for (file_id, logical_start) in namespace.extents_without_inode()? {
        inconsistencies.push(Inconsistency::ExtentWithoutInode {
            file_id,
            logical_start,
        });
    }

    Ok(VerifyReport {
        references: referenced.len(),
        dangling,
        inconsistencies,
    })
}
