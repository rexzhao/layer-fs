//! Consistency checking: does the store still hold everything the repository names?
//!
//! ADR-006 §13 gives `Repo::verify` two levels:
//!
//! | level | what it checks |
//! | --- | --- |
//! | default | every object the **index** names is on disk with the right bytes, and every recorded root opens as a snapshot |
//! | `deep` | additionally, every inode reachable from every recorded root has its content objects |
//!
//! The default level is the cheap daily check; `deep` is the one that would notice an
//! object lost after it was published. Both are **read-only**: a verifier that repaired
//! anything would be a second writer race with the one under test.

use std::collections::BTreeSet;

use cas_store::{OpenMode as StoreMode, Store};
use fs_snapshot::{Content, SnapshotReader};

use crate::error::{ApiError, ApiResult};
use crate::repo::Repo;

/// One reference that did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangling {
    /// The object or root that is missing.
    pub object: String,
    /// What it was referenced by.
    pub reason: String,
}

/// The outcome of a verification run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Objects the index names.
    pub objects: u64,
    /// Whether every indexed object was readable and intact.
    pub index_clean: bool,
    /// Snapshot roots walked.
    pub roots_checked: u64,
    /// Inodes visited at `deep` (0 at the default level).
    pub inodes_checked: u64,
    /// References that did not resolve.
    pub dangling: Vec<Dangling>,
}

impl VerifyReport {
    /// Whether nothing was found wrong.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.index_clean && self.dangling.is_empty()
    }
}

impl Repo {
    /// Checks the store against what the repository records.
    ///
    /// `deep` also walks every reachable inode and fetches its content objects.
    ///
    /// # Errors
    ///
    /// Store failures that are not themselves findings (a store that cannot be opened at
    /// all is an error, a missing object is a finding).
    pub fn verify(&self, deep: bool) -> ApiResult<VerifyReport> {
        let store = Store::open(self.store_dir(), StoreMode::ReadOnly)?;
        let index_clean = store.verify()?.is_clean();
        let objects = store.object_count()?;

        let mut dangling = Vec::new();
        let mut roots_checked = 0u64;
        let mut inodes_checked = 0u64;
        for root in self.inner.roots()? {
            roots_checked += 1;
            match SnapshotReader::open_any(&store, root) {
                Ok(reader) => {
                    if deep {
                        walk(&store, &reader, &mut dangling, &mut inodes_checked)?;
                    }
                }
                Err(error) => dangling.push(Dangling {
                    object: root.to_string(),
                    reason: format!("recorded root cannot be opened: {error}"),
                }),
            }
        }

        Ok(VerifyReport {
            objects,
            index_clean,
            roots_checked,
            inodes_checked,
            dangling,
        })
    }
}

/// Visits every inode reachable from the reader and checks that its content is fetchable.
fn walk(
    store: &Store,
    reader: &SnapshotReader<'_>,
    dangling: &mut Vec<Dangling>,
    inodes_checked: &mut u64,
) -> ApiResult<()> {
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut pending = vec![reader.root_serial()];
    while let Some(serial) = pending.pop() {
        if !seen.insert(serial) {
            continue;
        }
        let Some(inode) = reader.inode(serial)? else {
            dangling.push(Dangling {
                object: serial.to_string(),
                reason: "reachable inode is missing from the table".to_owned(),
            });
            continue;
        };
        *inodes_checked += 1;
        match inode.content {
            Content::Empty => {}
            Content::Blob(oid) => check(store, oid, dangling)?,
            Content::Dir(oid) => {
                check(store, oid, dangling)?;
                if let Ok(entries) = reader.read_dir(&inode, None, usize::MAX) {
                    for entry in entries {
                        pending.push(entry.child_serial);
                    }
                }
            }
            Content::Extents(_) => {
                // Fetching the range makes the reader resolve every extent whose bytes
                // overlap it, which is what checks the leaves rather than just the root.
                match reader.extents_in_range(&inode, 0, inode.size) {
                    Ok(extents) => {
                        for extent in extents {
                            check(store, extent.object, dangling)?;
                        }
                    }
                    Err(error) => dangling.push(Dangling {
                        object: inode.serial.to_string(),
                        reason: format!("extent tree cannot be read: {error}"),
                    }),
                }
            }
        }
    }
    Ok(())
}

fn check(store: &Store, oid: cas_core::ObjectId, dangling: &mut Vec<Dangling>) -> ApiResult<()> {
    match store.get(oid) {
        Ok(_) => Ok(()),
        Err(cas_store::StoreError::NotFound { .. }) => {
            dangling.push(Dangling {
                object: oid.to_string(),
                reason: "referenced object is not in the store".to_owned(),
            });
            Ok(())
        }
        Err(error) => Err(ApiError::Store(error)),
    }
}
