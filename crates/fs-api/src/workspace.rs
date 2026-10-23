//! A workspace handle: the mutable view a caller edits (ADR-006 §13).

use std::path::Path as HostPath;

use fs_namespace::{Entry, Kind, Path, Rules, Stat};

use crate::error::{ApiError, ApiResult};
use crate::repo::WorkspaceStatus;

/// One workspace: a mutable view of a snapshot, published with `commit`.
///
/// Every method is path-addressed and stateless in the ADR-004 §3.1 sense: nothing is
/// held open between calls, and each call resolves its path inside the transaction that
/// uses it. The handle is the workspace's exclusivity, not a cursor.
#[derive(Debug)]
pub struct Workspace {
    pub(crate) inner: fs_workspace::Workspace,
}

impl Workspace {
    pub(crate) fn new(inner: fs_workspace::Workspace) -> Self {
        Self { inner }
    }

    /// The workspace's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// The branch it publishes to.
    #[must_use]
    pub fn branch(&self) -> &str {
        self.inner.branch()
    }

    /// The commit its changes are based on.
    #[must_use]
    pub fn base_commit(&self) -> &str {
        self.inner.base_commit()
    }

    /// The workspace's state: branch, base commit, and whether it has changes.
    ///
    /// # Errors
    ///
    /// Overlay failures.
    pub fn status(&self) -> ApiResult<WorkspaceStatus> {
        let status = self.inner.status()?;
        Ok(WorkspaceStatus {
            name: status.name,
            branch: status.branch,
            base_commit: status.base_commit,
            base_root: status.base_root,
            dirty: status.dirty,
        })
    }

    /// The overlay file's path, for diagnostics.
    #[must_use]
    pub fn overlay_path(&self) -> &HostPath {
        self.inner.overlay_path()
    }

    /// Creates a directory.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] for an existing name, a missing parent, or a parent that is
    /// a file.
    pub fn mkdir(&self, path: &str) -> ApiResult<()> {
        Ok(self.rules().mkdir(&parse(path)?)?)
    }

    /// Creates an empty regular file.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] for an existing name or a missing parent.
    pub fn create(&self, path: &str) -> ApiResult<()> {
        Ok(self.inner.overlay().create_file(&parse(path)?)?)
    }

    /// Removes a file or an **empty** directory.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] for a missing path, or a directory that is not empty.
    pub fn remove(&self, path: &str) -> ApiResult<()> {
        let path = parse(path)?;
        match self.rules().stat(&path)?.kind {
            Kind::File => Ok(self.rules().unlink(&path)?),
            Kind::Directory => Ok(self.rules().rmdir(&path)?),
        }
    }

    /// Renames or moves `from` to `to` (R6–R8: a directory cannot move into its own
    /// subtree, and a rename over an existing name is refused).
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] per the rename rules.
    pub fn rename(&self, from: &str, to: &str) -> ApiResult<()> {
        Ok(self.rules().rename(&parse(from)?, &parse(to)?)?)
    }

    /// Lists a directory, in name order.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is not a directory.
    pub fn read_dir(&self, path: &str) -> ApiResult<Vec<Entry>> {
        Ok(self.rules().read_dir(&parse(path)?)?)
    }

    /// One entry's metadata.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing.
    pub fn stat(&self, path: &str) -> ApiResult<Stat> {
        Ok(self.rules().stat(&parse(path)?)?)
    }

    /// Reads at most `out.len()` bytes at `offset`, returning how many were read.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is a directory.
    pub fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> ApiResult<usize> {
        let path = parse(path)?;
        let size = self.inner.overlay().content_state(&path)?.size;
        if out.is_empty() || offset >= size {
            return Ok(0);
        }
        let want = usize::try_from(size - offset)
            .unwrap_or(usize::MAX)
            .min(out.len());
        let bytes = self.inner.overlay().read_at(&path, offset, want)?;
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    /// Writes `bytes` at `offset`, extending the file when the write runs past its end.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is a directory.
    pub fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> ApiResult<usize> {
        self.inner
            .overlay()
            .write_at(&parse(path)?, offset, bytes)?;
        Ok(bytes.len())
    }

    /// Sets the file's size, zero-extending or cutting it.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is a directory.
    pub fn truncate(&self, path: &str, size: u64) -> ApiResult<()> {
        Ok(self.inner.overlay().truncate(&parse(path)?, size)?)
    }

    /// Makes every change so far durable (ADR-006 §8).
    ///
    /// # Errors
    ///
    /// Overlay failures.
    pub fn flush(&self) -> ApiResult<()> {
        Ok(self.inner.flush()?)
    }

    /// Publishes the view as a snapshot root **without** moving the branch (ADR-006 §9.1).
    ///
    /// The root is a GC root: the tree stays readable after the branch moves on.
    ///
    /// # Errors
    ///
    /// Materialisation or main-database failures.
    pub fn snapshot(&mut self) -> ApiResult<String> {
        Ok(self.inner.snapshot()?.root)
    }

    /// Publishes the view as a commit on the workspace's branch.
    ///
    /// Returns the commit id. `force` skips the expected-head check and records the
    /// displaced head in the reflog (ADR-006 §10.3).
    ///
    /// # Errors
    ///
    /// [`ApiError::Workspace`] with `NothingToCommit` when the view equals the base, or
    /// `BranchMoved` when another workspace published first.
    pub fn commit(&mut self, message: &str, force: bool) -> ApiResult<String> {
        let outcome = self.inner.commit(message, force)?;
        outcome
            .commit
            .ok_or_else(|| ApiError::Workspace(fs_workspace::WorkspaceError::NothingToCommit))
    }

    /// Replaces the view with `commit`'s tree, leaving the base commit alone (§10.2).
    ///
    /// # Errors
    ///
    /// [`ApiError::Workspace`] with `WorkspaceDirty` when the overlay is not empty and
    /// `discard` is false.
    pub fn restore(&mut self, commit: &str, discard: bool) -> ApiResult<()> {
        Ok(self.inner.restore(commit, discard)?)
    }

    /// Closes the handle; the workspace and its work stay on disk.
    ///
    /// # Errors
    ///
    /// Never in this build.
    pub fn close(self) -> ApiResult<()> {
        Ok(self.inner.close()?)
    }

    fn rules(&self) -> Rules<fs_workspace::WorkspaceStore> {
        Rules::new(self.inner.store())
    }
}

fn parse(text: &str) -> ApiResult<Path> {
    Ok(Path::parse(text)?)
}
