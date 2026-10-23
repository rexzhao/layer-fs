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

/// The result of a closure check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Distinct objects the namespace references.
    pub references: usize,
    /// References that could not be read back, in object order.
    pub dangling: Vec<DanglingReference>,
}

impl VerifyReport {
    /// Whether every reference resolved to a readable object.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dangling.is_empty()
    }
}

/// Checks every object the namespace references.
///
/// References are deduplicated first, so a block shared by ten files is read
/// once. Nothing is repaired: a dangling reference is reported, not removed.
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
    Ok(VerifyReport {
        references: referenced.len(),
        dangling,
    })
}
