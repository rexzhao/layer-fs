//! The repository: branches, history and workspaces (ADR-006 §4, §13).

use std::path::Path;
use std::sync::Arc;

pub use fs_workspace::{BranchInfo, CommitInfo, WorkspaceSummary};

use crate::error::ApiResult;
use crate::read_view::ReadView;
use crate::workspace::Workspace;

/// A store plus the branches, commits and workspaces recorded in it.
///
/// This is the entry point: [`Repo::create`] makes a store, [`Repo::open`] uses one. Both
/// hand out [`Workspace`] handles for editing and [`ReadView`]s for reading history.
#[derive(Debug)]
pub struct Repo {
    pub(crate) inner: Arc<fs_workspace::Repo>,
}

/// Runs the offline collector over `store_dir`.
///
/// A **free function**, not a method on [`Repo`], and that is not a style choice: the collector takes
/// the store exclusively (ADR-008 §6.1), while a live `Repo` holds two connections to it — so a
/// `repo.gc()` would refuse its own caller. The policy is read through a short-lived handle that is
/// dropped before the collector opens anything.
///
/// `gc_repack_threshold` is store data (a **policy value**, not a constant in the binary): when unset,
/// [`DEFAULT_REPACK_THRESHOLD`] applies. A value that is not a number, or is outside `[0, 1]`, is
/// refused **before anything moves**.
///
/// # Errors
///
/// A typed refusal when a writer or an open workspace holds the store, when the policy is not a ratio,
/// or whatever the collector reported.
pub fn gc(store_dir: impl AsRef<Path>, dry_run: bool) -> ApiResult<GcSummary> {
    let store_dir = store_dir.as_ref().to_path_buf();
    let threshold = {
        let repo = Repo::open(&store_dir)?;
        match repo.inner.policy_or_none("gc_repack_threshold")? {
            Some(text) => {
                let value: f64 = text.trim().parse().map_err(|_| {
                    fs_workspace::WorkspaceError::Inconsistent {
                        detail: format!("gc_repack_threshold {text:?} is not a number"),
                    }
                })?;
                if !(0.0..=1.0).contains(&value) {
                    return Err(fs_workspace::WorkspaceError::Inconsistent {
                        detail: format!(
                            "gc_repack_threshold {text} is outside [0, 1]: a survivor ratio cannot \
                             exceed one, and a negative one would repack every pack forever"
                        ),
                    }
                    .into());
                }
                value
            }
            None => DEFAULT_REPACK_THRESHOLD,
        }
    };

    let outcome = fs_workspace::gc::run(
        &store_dir,
        fs_workspace::gc::GcOptions {
            dry_run,
            repack_threshold: threshold,
        },
    )?;
    Ok(GcSummary {
        dry_run,
        repack_threshold: threshold,
        marked_objects: outcome.report.marked_objects,
        freed_objects: outcome.report.freed.len(),
        freed_object_bytes: outcome.report.freed_object_bytes,
        reclaimed_dead_bytes: outcome.report.reclaimed_dead_bytes,
        packs_deleted: outcome.report.packs_deleted,
        packs_repacked: outcome.report.packs_repacked,
        planned_objects: outcome.plan.dead_objects.len(),
    })
}

/// The default `gc_repack_threshold`, used when the store's policy does not set one.
pub const DEFAULT_REPACK_THRESHOLD: f64 = 0.5;

/// What one collector run reports, without the workspace's internal types.
///
/// Flat on purpose: the CLI prints it, and `GcReport`/`SweepPlan` are GC internals that must not leak
/// into the CLI. `planned_objects` and `freed_objects` differ only in a dry run — that difference is
/// the whole reason both fields exist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GcSummary {
    /// Whether anything was actually deleted.
    pub dry_run: bool,
    /// The policy value the run used.
    pub repack_threshold: f64,
    /// Objects the marked set held.
    pub marked_objects: usize,
    /// Objects that were deleted (`0` in a dry run).
    pub freed_objects: usize,
    /// Bytes of the deleted objects.
    pub freed_object_bytes: u64,
    /// Bytes reclaimed by repacking: dead frames that never had a row of their own.
    pub reclaimed_dead_bytes: u64,
    /// Packs deleted because nothing survived.
    pub packs_deleted: usize,
    /// Packs copy-repacked.
    pub packs_repacked: usize,
    /// Objects the sweep decided to free (present in a dry run too).
    pub planned_objects: usize,
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

    /// Deletes a branch, keeping its history (and therefore its objects) alive.
    ///
    /// # Errors
    ///
    /// `NotFound` for an unknown branch, `Inconsistent` while a workspace is still on it.
    pub fn branch_delete(&self, name: &str) -> ApiResult<()> {
        Ok(self.inner.branch_delete(name)?)
    }

    /// Every named snapshot root, oldest first.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn snapshots(&self) -> ApiResult<Vec<fs_workspace::SnapshotRow>> {
        Ok(self.inner.snapshots()?)
    }

    /// Drops a snapshot row, returning whether there was one.
    ///
    /// Deletes the **row**, not the objects: they become unreachable and the next collection reclaims
    /// them. Doing it the other way round would lose data if the process died in between.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn snapshot_drop(&self, root: &str) -> ApiResult<bool> {
        Ok(self.inner.snapshot_drop(root)?)
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
