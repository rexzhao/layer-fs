//! The adapter's handle table: sharing, reference counts, delete-pending and `DELETE_ON_CLOSE`.
//!
//! These are **adapter policy**, not core operations (P3-3a card §2): the core has no notion of a handle, so everything
//! here is about which caller may act on which `FileId` while other callers hold it. Two consequences run through the
//! whole module:
//!
//! - a handle is bound to a **`FileId`**, never to a path. A rename therefore cannot invalidate a handle, and no method
//!   here takes a path at all;
//! - the removal itself is **not** performed here. [`HandleTable::close`] reports `Remove` when the last handle of a
//!   delete-pending entry goes away, and the adapter then calls `WorkspaceOps::remove_at` once. If this module deleted
//!   anything itself, there would be two deletion paths and the "exactly once" criterion would be untestable.
//!
//! Sharing is **not** re-derived: [`fs_windows::share::admit`] is the one implementation of the 3×3 rule (card §3
//! criterion 8). A second copy here would drift, and the drift would be invisible until a real mount.

use fs_windows::share::{admit, Admission, Mask};

use crate::ops::OpsError;

/// Which handle, as an opaque token. Not a `FileId`: two handles on the same file are common and must be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandleId(u64);

impl HandleId {
    /// The raw token, for assertion messages.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Rebuilds a handle token from its raw value.
    ///
    /// Needed by the callbacks' direct tests, which build an `EntryContext` by hand. **A forged token cannot do any harm**: it names
    /// no open handle, so every table lookup refuses it — the token is an index, not an authority.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// One open handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handle {
    /// The entry this handle refers to. Stable across rename, never reused.
    pub file_id: u64,
    /// What this caller asked to do.
    pub desired: Mask,
    /// What this caller allows others to do.
    pub share: Mask,
    /// `FILE_FLAG_DELETE_ON_CLOSE`: the entry goes away when the **last** handle closes.
    pub delete_on_close: bool,
}

/// What the adapter must do after a [`HandleTable::close`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseEffect {
    /// Other handles on the entry remain; nothing to do.
    Nothing,
    /// This was the last handle and the entry is delete-pending: call `remove_at` **once**.
    ///
    /// The `FileId` is reported so the caller can use it in the guard (`expected_id`), which is what makes a
    /// delete-after-rename remove the right object rather than whatever is now at the old path.
    Remove(u64),
}

/// The table of open handles.
#[derive(Debug, Default)]
pub struct HandleTable {
    handles: std::collections::BTreeMap<HandleId, Handle>,
    /// `FileId -> (parent, name)`: where each open entry currently lives.
    ///
    /// Keyed by **FileId**, not by handle: `remove_at` guards with an id, so the binding's lifetime has to follow the id.
    /// Per-handle copies would disagree with each other after a rename, and the disagreement would surface as "the entry
    /// cannot be deleted" (the guard refuses a stale name) rather than as an obvious error.
    bindings: std::collections::BTreeMap<u64, (u64, Vec<u8>)>,
    next: u64,
    /// `FileId`s that have been marked delete-pending. Kept per **id**, not per name: the entry may be renamed
    /// between `set_delete(true)` and the last close, and the deletion must follow the object.
    pending: std::collections::BTreeSet<u64>,
}

/// The whole of criterion 3's third case, as one expression.
///
/// **Criterion aid only.** This is not the adapter's policy entry point: the adapter decides when to call
/// `remove_at`, and it must not route that decision through this function to "reuse the tested rule" — a single call
/// site is what makes the "exactly once" criterion observable in `calls()`. It is `pub` so the criterion can run the
/// real rule and two wrong policies side by side in one process; production code has exactly one caller, [`HandleTable::close`].
///
/// Extracted so the criterion can be **falsified in-process**: the test runs the same table of `(remaining, pending)`
/// through this function and through two wrong policies, and requires the wrong ones to disagree. A version of this
/// criterion that could only be checked by editing the source and re-running is a criterion that will not be re-checked.
#[must_use]
pub fn close_removes(remaining: usize, pending: bool) -> bool {
    remaining == 0 && pending
}

impl HandleTable {
    /// Records where `file_id` currently lives (from `OpenOutcome.binding`).
    pub fn remember_binding(&mut self, file_id: u64, parent: u64, name: Vec<u8>) {
        self.bindings.insert(file_id, (parent, name));
    }

    /// Where `file_id` currently lives, if a handle ever recorded it.
    ///
    /// `None` is **not** "guess from the last path": an unrecorded id must fail loudly, because a guessed name would name
    /// some other object and the guard would then refuse the removal — a silent non-deletion.
    #[must_use]
    pub fn binding_of(&self, file_id: u64) -> Option<(u64, Vec<u8>)> {
        self.bindings.get(&file_id).cloned()
    }

    /// Updates the binding of `file_id` after a rename moved it to `(parent, name)`.
    pub fn rebind(&mut self, file_id: u64, parent: u64, name: Vec<u8>) {
        if let Some(binding) = self.bindings.get_mut(&file_id) {
            *binding = (parent, name);
        }
    }

    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a handle on `file_id` if sharing allows it.
    ///
    /// The 9-cell table is `fs_windows::share::admit`; this only collects the existing handles of the **same** file,
    /// which is the part the rule needs and the part a bit table cannot supply.
    ///
    /// # Errors
    ///
    /// [`OpsError::SharingViolation`] when either the new request or an existing handle forbids it;
    /// [`OpsError::DeletePending`] when the entry is already delete-pending and this is not the handle that marked it.
    pub fn open(
        &mut self,
        file_id: u64,
        desired: Mask,
        share: Mask,
        delete_on_close: bool,
    ) -> Result<HandleId, OpsError> {
        if self.pending.contains(&file_id) {
            // Two-phase deletion (card §3 criterion 3): once an entry is delete-pending, a **new** open is refused.
            // The handles that were already open stay usable — that is the other half of the criterion.
            return Err(OpsError::DeletePending);
        }
        let existing: Vec<(Mask, Mask)> = self
            .handles
            .values()
            .filter(|handle| handle.file_id == file_id)
            .map(|handle| (handle.desired, handle.share))
            .collect();
        if let Admission::Refuse(_) = admit(desired, share, &existing) {
            return Err(OpsError::SharingViolation);
        }
        let id = HandleId(self.next);
        self.next += 1;
        self.handles.insert(
            id,
            Handle {
                file_id,
                desired,
                share,
                delete_on_close,
            },
        );
        if delete_on_close {
            // `DELETE_ON_CLOSE` *is* delete-pending from the moment of open: new opens must be refused immediately, not
            // only after the flag is observed at close time.
            self.pending.insert(file_id);
        }
        Ok(id)
    }

    /// Marks the entry behind `handle_id` delete-pending.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle, [`OpsError::AccessDenied`] when the handle lacks `DELETE` access.
    pub fn set_delete(&mut self, handle_id: HandleId) -> Result<(), OpsError> {
        let handle = self.handles.get(&handle_id).ok_or(OpsError::NotFound)?;
        if !handle.desired.has(fs_windows::share::Term::Delete) {
            return Err(OpsError::AccessDenied);
        }
        self.pending.insert(handle.file_id);
        Ok(())
    }

    /// The entry behind `handle_id`, if the handle is open.
    #[must_use]
    pub fn get(&self, handle_id: HandleId) -> Option<&Handle> {
        self.handles.get(&handle_id)
    }

    /// How many handles are open right now.
    ///
    /// This is the number `status.handles` reports and the number a **remote** `umount` refuses on (ADR-010 §7.2): the caller that asks over the pipe cannot
    /// see who has a file open, so the answer has to come from the mount that can.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether no handle is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Every open handle of `file_id`, in creation order.
    #[must_use]
    pub fn handles_of(&self, file_id: u64) -> Vec<HandleId> {
        self.handles
            .iter()
            .filter(|(_, handle)| handle.file_id == file_id)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether `file_id` is delete-pending (a new open must be refused).
    #[must_use]
    pub fn is_pending(&self, file_id: u64) -> bool {
        self.pending.contains(&file_id)
    }

    /// Closes a handle and reports whether the adapter must now remove the entry.
    ///
    /// Removing the last handle of a **non**-pending entry returns [`CloseEffect::Nothing`]: closing is not deleting.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle (a double close is a caller bug, not an idempotent no-op: the second
    /// close would otherwise be able to "close" a handle that a later open reused, if ids were ever reused).
    pub fn close(&mut self, handle_id: HandleId) -> Result<CloseEffect, OpsError> {
        let handle = self.handles.remove(&handle_id).ok_or(OpsError::NotFound)?;
        let remaining = self.handles_of(handle.file_id).len();
        let should_remove = close_removes(remaining, self.pending.contains(&handle.file_id));
        if should_remove {
            // Leave `pending` set: the entry is still pending until the adapter's `remove_at` succeeds, and a second
            // close of the same id cannot get here (the handle is already gone).
            return Ok(CloseEffect::Remove(handle.file_id));
        }
        Ok(CloseEffect::Nothing)
    }

    /// Clears the pending flag once the adapter has actually removed the entry.
    ///
    /// Separate from [`Self::close`] on purpose: "the last handle closed" and "the entry was removed" are two events,
    /// and a failure between them must not leave the adapter believing the entry is gone.
    pub fn removed(&mut self, file_id: u64) {
        self.pending.remove(&file_id);
    }
}
