//! The repository: branches, history and workspaces (ADR-006 §4, §13).

use std::path::Path;
use std::sync::Arc;

pub use fs_workspace::{BranchInfo, CommitInfo, WorkspaceSummary};

use crate::api_error::ApiResult;
use crate::read_view::ReadView;
use crate::workspace::Workspace;

/// A store plus the branches, commits and workspaces recorded in it.
///
/// This is the entry point: [`Repo::create`] makes a store, [`Repo::open`] uses one. Both
/// hand out [`Workspace`] handles for editing and [`ReadView`]s for reading history.
#[derive(Debug)]
pub struct Repo {
    inner: Arc<fs_workspace::Repo>,
}

impl Repo {
    /// Creates the store (or opens it if it already exists) and makes sure it has a
    /// genesis commit on `main`.
    ///
    /// # Errors
    ///
    /// Store, schema or workspace-table failures.
    pub fn create(store_root: impl AsRef<Path>) -> ApiResult<Self> {
        let repo = Self {
            inner: Arc::new(fs_workspace::Repo::open(store_root, true)?),
        };
        if repo.inner.branches()?.is_empty() {
            repo.inner.init()?;
        }
        Ok(repo)
    }

    /// Opens an existing store.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotInitialised` when the store has no
    /// workspace tables — `create` is the way to make them.
    pub fn open(store_root: impl AsRef<Path>) -> ApiResult<Self> {
        Ok(Self {
            inner: Arc::new(fs_workspace::Repo::open(store_root, false)?),
        })
    }

    /// Where this repository lives.
    #[must_use]
    pub fn store_dir(&self) -> &Path {
        self.inner.store_dir()
    }

    /// Every branch, by name.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn branches(&self) -> ApiResult<Vec<BranchInfo>> {
        Ok(self.inner.branches()?)
    }

    /// Creates a branch, optionally from another branch or a commit.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `AlreadyExists` or `NotFound`.
    pub fn branch_create(&self, name: &str, from: Option<&str>) -> ApiResult<BranchInfo> {
        Ok(self.inner.branch_create(name, from)?)
    }

    /// Where a branch points.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn branch_head(&self, name: &str) -> ApiResult<String> {
        Ok(self.inner.branch_head(name)?)
    }

    /// A branch's history, newest first.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn log(&self, branch: &str) -> ApiResult<Vec<CommitInfo>> {
        Ok(self.inner.log(branch)?)
    }

    /// One commit.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn commit_info(&self, id: &str) -> ApiResult<CommitInfo> {
        Ok(self.inner.commit_info(id)?)
    }

    /// Moves a branch to an existing commit; `expect` refuses to move a head the caller
    /// did not read (ADR-006 §10.2).
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `BranchMoved` or `NotFound`.
    pub fn reset(&self, branch: &str, to: &str, expect: Option<&str>) -> ApiResult<()> {
        Ok(self.inner.reset(branch, to, expect)?)
    }

    /// Creates a commit that undoes the head (ADR-006 §10.2, F3) and returns it.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn revert(&self, branch: &str) -> ApiResult<CommitInfo> {
        Ok(self.inner.revert(branch)?)
    }

    /// Creates a workspace on `branch` and opens it.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `AlreadyExists` or `NotFound`.
    pub fn workspace_create(&self, name: &str, branch: &str) -> ApiResult<Workspace> {
        Ok(Workspace::new(self.inner.workspace_create(name, branch)?))
    }

    /// Opens an existing workspace, completing any interrupted publication (ADR-006 §9.3).
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn workspace_open(&self, name: &str) -> ApiResult<Workspace> {
        Ok(Workspace::new(self.inner.workspace_open(name)?))
    }

    /// Every workspace, by name.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn workspaces(&self) -> ApiResult<Vec<WorkspaceSummary>> {
        Ok(self.inner.workspace_list()?)
    }

    /// Deletes a workspace's row and files. The workspace must be closed.
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound`.
    pub fn workspace_discard(&self, name: &str) -> ApiResult<()> {
        Ok(self.inner.workspace_discard(name)?)
    }

    /// Moves a workspace onto another branch whose head is its base commit (§10.3 出路 1).
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `BranchMoved` or `NotFound`.
    pub fn workspace_rebind(&self, name: &str, branch: &str) -> ApiResult<()> {
        Ok(self.inner.workspace_rebind(name, branch)?)
    }

    /// Reads a commit's tree without consulting any table (ADR-006 I1).
    ///
    /// # Errors
    ///
    /// [`crate::ApiError::Workspace`] with `NotFound` when the commit does not exist.
    pub fn read_view(&self, commit: &str) -> ApiResult<ReadView> {
        let info = self.inner.commit_info(commit)?;
        let root = fs_workspace::parse_oid(&info.root)?;
        ReadView::open(self.inner.store_dir(), root)
    }
}

/// What `status` reports about a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// The workspace's name.
    pub name: String,
    /// The branch it publishes to.
    pub branch: String,
    /// The commit its changes are based on.
    pub base_commit: String,
    /// The snapshot its view is composed over.
    pub base_root: String,
    /// Whether it has changes the base does not have.
    pub dirty: bool,
}
