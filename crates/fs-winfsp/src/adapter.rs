//! The adapter: a handle table in front of a `WorkspaceOps` backend (P3-4 step 0b).
//!
//! P3-3a built the two halves and deliberately did not wire them: the handle table and the backend were tested separately,
//! and criterion 3's **adapter half** (`DeletePending`) was left to this card. Wiring them here — still in pure Rust, still
//! without FFI — keeps the FFI layer to "plumbing" instead of "plumbing + first semantic verification of delete-pending",
//! which is the part that cannot be debugged through a real mount.
//!
//! **One mount = one `Adapter`.** It owns the backend and the table; an FFI callback borrows it. A callback that built a
//! fresh adapter would re-open the workspace per operation: two tables, two views of delete-pending, and a cost.

use fs_windows::share::Mask;

use crate::handles::{CloseEffect, HandleId, HandleTable};
use crate::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpsError, OpsResult, RenameOutcome, WorkspaceOps,
};

/// A backend plus the handle table that gives its operations Windows handle semantics.
pub struct Adapter<B: WorkspaceOps> {
    ops: B,
    handles: HandleTable,
}

impl<B: WorkspaceOps> std::fmt::Debug for Adapter<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapter")
            .field("handles", &self.handles.handles_of(0).len())
            .finish_non_exhaustive()
    }
}

impl<B: WorkspaceOps> Adapter<B> {
    /// Wraps a backend. One adapter per mounted volume.
    pub fn new(ops: B) -> Self {
        Self {
            ops,
            handles: HandleTable::new(),
        }
    }

    /// The backend, for the operations that are path-based by nature.
    pub fn ops(&self) -> &B {
        &self.ops
    }

    /// Opens `path` and registers a handle.
    ///
    /// The delete-pending check has to happen **before** the backend is asked to create or truncate anything, or a
    /// refused open would leave a truncated file behind. The trait has no "resolve only" call, so this probes with
    /// `Disposition::Open` — the one value that cannot mutate — and refuses if the entry it names is pending. That costs
    /// one extra backend call per open, which is the price of "refused means unchanged"; the alternative is to let the
    /// mutation happen and then refuse the handle, which is a log that says "denied" over a file that was already emptied.
    ///
    /// # Errors
    ///
    /// [`OpsError::DeletePending`] when the entry is pending, [`OpsError::SharingViolation`] per the share matrix, or
    /// whatever the backend reports.
    pub fn open(
        &mut self,
        path: &[u8],
        disposition: fs_semantics::disposition::Disposition,
        desired: Mask,
        share: Mask,
        delete_on_close: bool,
    ) -> OpsResult<(HandleId, u64, Kind)> {
        if let Ok(existing) = self
            .ops
            .create_or_open(path, fs_semantics::disposition::Disposition::Open)
        {
            if self.handles.is_pending(existing.file_id) {
                return Err(OpsError::DeletePending);
            }
        }

        let outcome = self.ops.create_or_open(path, disposition)?;
        self.handles.remember_binding(
            outcome.file_id,
            outcome.binding.0,
            outcome.binding.1.clone(),
        );
        let handle = self
            .handles
            .open(outcome.file_id, desired, share, delete_on_close)?;
        Ok((handle, outcome.file_id, outcome.kind))
    }

    /// Reads through a handle.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle, or whatever the backend reports.
    pub fn read(&self, handle: HandleId, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        self.ops.read(self.file_of(handle)?, offset, len)
    }

    /// Writes through a handle, requiring write access on it.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle, [`OpsError::AccessDenied`] without write access.
    pub fn write(&self, handle: HandleId, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        let bound = self.handles.get(handle).ok_or(OpsError::NotFound)?;
        if !bound.desired.has(fs_windows::share::Term::Write) {
            return Err(OpsError::AccessDenied);
        }
        self.ops.write(bound.file_id, offset, bytes)
    }

    /// `get_info` through a handle.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle.
    pub fn get_info(&self, handle: HandleId) -> OpsResult<NodeInfo> {
        self.ops.get_info(self.file_of(handle)?)
    }

    /// `read_dir` through a handle.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle.
    pub fn read_dir(&self, handle: HandleId) -> OpsResult<Vec<DirEntry>> {
        self.ops.read_dir(self.file_of(handle)?)
    }

    /// `set_attributes` through a handle.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle.
    pub fn set_attributes(&self, handle: HandleId, attributes: Attributes) -> OpsResult<()> {
        self.ops.set_attributes(self.file_of(handle)?, attributes)
    }

    /// Marks the handle's entry delete-pending (phase one of two-phase deletion).
    ///
    /// # Errors
    ///
    /// [`OpsError::AccessDenied`] without `DELETE` access, [`OpsError::NotFound`] for an unknown handle.
    pub fn set_delete(&mut self, handle: HandleId) -> OpsResult<()> {
        self.handles.set_delete(handle)
    }

    /// Closes a handle; on the last one of a pending entry the entry is removed **once**, here.
    ///
    /// The removal is the adapter's single call to `remove_at` — the table decides *when*, this performs it *once*. The
    /// name passed is the **current** one from the binding table, which is why that table exists: a stale name would make
    /// the guard refuse, and a refused removal reads as "cannot delete" rather than "wrong name".
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle or a missing binding, or whatever the backend reports.
    pub fn close(&mut self, handle: HandleId) -> OpsResult<()> {
        match self.handles.close(handle)? {
            CloseEffect::Nothing => Ok(()),
            CloseEffect::Remove(file_id) => {
                let (parent, name) = self
                    .handles
                    .binding_of(file_id)
                    .ok_or_else(|| OpsError::InvalidRecord(format!("no binding for {file_id}")))?;
                self.ops.remove_at(parent, &name, file_id)?;
                self.handles.removed(file_id);
                Ok(())
            }
        }
    }

    /// Renames by path and moves the recorded binding: the handle keeps naming the entry, the entry keeps being findable
    /// by the adapter's deletion path.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports.
    pub fn rename(&mut self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome> {
        let outcome = self.ops.rename(from, to, replace)?;
        // The backend reports which id moved (P3-4 added that field): deriving it from the path would be guessing, and a
        // wrong guess means the binding keeps the old name, so the eventual deletion is refused by the guard.
        self.handles.rebind(
            outcome.file_id,
            outcome.binding.0,
            outcome.binding.1.clone(),
        );
        Ok(outcome)
    }

    fn file_of(&self, handle: HandleId) -> OpsResult<u64> {
        Ok(self.handles.get(handle).ok_or(OpsError::NotFound)?.file_id)
    }
}
