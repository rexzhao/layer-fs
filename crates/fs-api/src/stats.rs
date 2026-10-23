//! What the store and the repository hold, for `layerfs stats`.

use cas_store::{OpenMode as StoreMode, Store};

use crate::error::ApiResult;
use crate::repo::Repo;

/// Object and repository counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Objects in the CAS index.
    pub objects: u64,
    /// Branches that exist.
    pub branches: u64,
    /// Commits recorded.
    pub commits: u64,
    /// Workspaces registered.
    pub workspaces: u64,
    /// Named snapshot roots recorded.
    pub snapshots: u64,
    /// Reflog entries: every head move, including forced ones.
    pub branch_log_entries: u64,
}

impl Repo {
    /// Reads the counts.
    ///
    /// # Errors
    ///
    /// Store or main-database failures.
    pub fn stats(&self) -> ApiResult<Stats> {
        let store = Store::open(self.store_dir(), StoreMode::ReadOnly)?;
        let repo = self.inner.stats()?;
        Ok(Stats {
            objects: store.object_count()?,
            branches: repo.branches,
            commits: repo.commits,
            workspaces: repo.workspaces,
            snapshots: repo.snapshots,
            branch_log_entries: repo.branch_log_entries,
        })
    }
}
