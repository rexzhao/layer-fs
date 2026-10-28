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

/// The per-mount configuration the callbacks need: what a path resolves to, what an entry looks like, how to create one, and
/// what a directory holds.
///
/// These used to be process-wide `OnceLock`s. That worked for exactly one mount and made every test share one configuration —
/// and worse, a lookup with no configuration returned **the previous configuration** instead of nothing (352: a process-wide
/// static invents an answer where `None` is the truth). They live here now because they belong to the mount.
#[derive(Clone, Copy, Default)]
pub struct AdapterConfig {
    /// The volume answer (`GetVolumeInfo`). Configuration, not state: it is decided before the mount starts and does not
    /// change while it runs — which is why it belongs to the mount rather than to the process.
    pub volume: Option<crate::abi::VolumeInfo>,
    /// `path -> FileId`, for `Open`/`Create`/`GetSecurityByName`.
    pub resolve: Option<fn(&str) -> Option<u64>>,
    /// `FileId -> metadata`, for `GetFileInfo`.
    pub lookup: Option<fn(u64) -> Option<NodeInfo>>,
    /// `path -> FileId`, creating the entry: the one path→id entry point (`create_or_open`), not a second translation.
    pub make: Option<fn(&str) -> Option<u64>>,
    /// `dir id -> entries`, for `ReadDirectory`.
    pub list: Option<fn(u64) -> Vec<ListEntry>>,
}

impl std::fmt::Debug for AdapterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterConfig")
            .field("resolve", &self.resolve.is_some())
            .field("lookup", &self.lookup.is_some())
            .field("make", &self.make.is_some())
            .field("list", &self.list.is_some())
            .finish()
    }
}

/// One directory entry as the callbacks see it.
pub type ListEntry = (Vec<u16>, NodeInfo);

/// A backend plus the handle table that gives its operations Windows handle semantics.
pub struct Adapter<B: WorkspaceOps> {
    ops: B,
    /// Behind a `Mutex` so every adapter method can take `    handles: HandleTable,self`: the registry holds `Arc<Adapter>` and the callbacks arrive on
    /// several dispatcher threads at once. A `RefCell` here would be a data race with a tidy-looking API.
    handles: std::sync::Mutex<HandleTable>,
    config: AdapterConfig,
    /// `FileContext` allocations and frees **for this mount**.
    ///
    /// Process-wide counters answered the question "did anything leak anywhere"; per-mount counters answer "did *this* mount
    /// balance", which is the question a teardown actually asks — and the reason 300 moves them here.
    contexts: (
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    ),
}

impl<B: WorkspaceOps> std::fmt::Debug for Adapter<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapter")
            .field(
                "handles",
                &self
                    .handles
                    .lock()
                    .expect("handle table")
                    .handles_of(0)
                    .len(),
            )
            .finish_non_exhaustive()
    }
}

impl<B: WorkspaceOps> Adapter<B> {
    /// Wraps a backend. One adapter per mounted volume.
    pub fn new(ops: B) -> Self {
        Self {
            ops,
            handles: std::sync::Mutex::new(HandleTable::new()),
            config: AdapterConfig::default(),
            contexts: (
                std::sync::atomic::AtomicUsize::new(0),
                std::sync::atomic::AtomicUsize::new(0),
            ),
        }
    }

    /// Wraps a backend together with this mount's configuration.
    pub fn with_config(ops: B, config: AdapterConfig) -> Self {
        Self {
            ops,
            handles: std::sync::Mutex::new(HandleTable::new()),
            config,
            contexts: (
                std::sync::atomic::AtomicUsize::new(0),
                std::sync::atomic::AtomicUsize::new(0),
            ),
        }
    }

    /// Records one `FileContext` allocation for this mount.
    pub fn note_context_allocated(&self) {
        self.contexts
            .0
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Records one `FileContext` free for this mount.
    pub fn note_context_freed(&self) {
        self.contexts
            .1
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `(allocated, freed)` for this mount: equal after a clean teardown.
    #[must_use]
    pub fn context_counts(&self) -> (usize, usize) {
        (
            self.contexts.0.load(std::sync::atomic::Ordering::Relaxed),
            self.contexts.1.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// How many file objects this mount currently has open.
    ///
    /// Read from the **handle table**, not from the context counters: the counters answer "was every context released", which is a teardown question, while
    /// this answers "is a file open right now", which is the question `status.handles` reports and the one a remote `umount` refuses on (ADR-010 §7.2).
    #[must_use]
    pub fn open_handles(&self) -> usize {
        self.handles.lock().expect("handle table").len()
    }

    /// **File** handles open on this adapter, and directory handles (S3 card §3.2).
    ///
    /// Exposed as a pair on purpose: `restore`/`switch` block on the first and merely report the second, and a caller that could ask for only one of them
    /// would be free to forget the other half of the rule.
    #[must_use]
    pub fn open_handle_kinds(&self) -> (usize, usize) {
        self.handles.lock().expect("handle table").handle_kinds()
    }

    /// The backend, borrowed: the callbacks need `read_dir`/`get_info` directly, and going through the adapter's handle semantics
    /// would be the wrong question for a listing (there is no handle — the directory id is already known).
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.ops
    }

    /// This mount's configuration.
    #[must_use]
    pub fn config(&self) -> AdapterConfig {
        self.config
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
        if let Ok(existing) = self.ops.create_or_open(
            &crate::ops::Path::new(path),
            fs_semantics::disposition::Disposition::Open,
        ) {
            if self
                .handles
                .lock()
                .expect("handle table")
                .is_pending(existing.file_id)
            {
                return Err(OpsError::DeletePending);
            }
        }

        let outcome = self
            .ops
            .create_or_open(&crate::ops::Path::new(path), disposition)?;
        self.handles.lock().expect("handle table").remember_binding(
            outcome.file_id,
            outcome.binding.0,
            outcome.binding.1.clone(),
        );
        let handle = self.handles.lock().expect("handle table").open(
            outcome.file_id,
            desired,
            share,
            delete_on_close,
            // The kind comes from the **backend's own answer**, not from the disposition: a `FILE_OPEN` can land on either, and this field is what
            // `restore`/`switch` count when deciding whether a file handle is in the way (S3 §3.2).
            outcome.kind == crate::ops::Kind::Directory,
        )?;
        Ok((handle, outcome.file_id, outcome.kind))
    }

    /// Registers a handle on an entry **without touching the backend**.
    ///
    /// `open` cannot be used for this: it resolves the path again (`create_or_open`), and a second resolution is a second answer to a
    /// question already answered. The callbacks therefore ask the backend once and register the result here.
    ///
    /// **Sharing is not enforced yet** (`ALL_BITS` on both sides): the table has the entry, so `Cleanup`/`Close` can act on an
    /// **identity**, but the 9-cell share rule belongs to its own delivery (card U3) and arrives with its own criterion. Saying so here
    /// is what keeps "the table is wired" from being read as "sharing is implemented".
    ///
    /// # Errors
    ///
    /// [`OpsError::DeletePending`] when the entry is already delete-pending.
    pub fn open_handle(
        &self,
        file_id: u64,
        binding: Option<(u64, Vec<u8>)>,
        is_dir: bool,
    ) -> OpsResult<HandleId> {
        let mut table = self.handles.lock().expect("handle table");
        if table.is_pending(file_id) {
            return Err(OpsError::DeletePending);
        }
        // The binding is what `remove_at` will need at the last close. Without one the entry can still be opened and closed, but a
        // pending deletion would have no name to remove — so it is recorded here, from the backend's own answer, never guessed.
        if let Some((parent, name)) = binding {
            table.remember_binding(file_id, parent, name);
        }
        table.open(
            file_id,
            fs_windows::share::Mask::ALL_BITS,
            fs_windows::share::Mask::ALL_BITS,
            false,
            is_dir,
        )
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
        let file_id = {
            let table = self.handles.lock().expect("handle table");
            let bound = table.get(handle).ok_or(OpsError::NotFound)?;
            if !bound.desired.has(fs_windows::share::Term::Write) {
                return Err(OpsError::AccessDenied);
            }
            bound.file_id
        };
        // Outside the lock: the backend call must not run under the table lock.
        self.ops.write(file_id, offset, bytes)
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
    pub fn set_delete(&self, handle: HandleId) -> OpsResult<()> {
        self.handles
            .lock()
            .expect("handle table")
            .set_delete(handle)
    }

    /// Closes a handle; on the last one of a pending entry the entry is removed **once**, here.
    ///
    /// The removal is the adapter's single call to `remove_at` — the table decides *when*, this performs it *once*. The name passed
    /// is the **current** one from the binding table, which is why that table exists: a stale name would make the guard refuse, and
    /// a refused removal reads as "cannot delete" rather than "wrong name".
    ///
    /// **The lock is released before the backend is called.** Holding it across `remove_at` would turn a slow backend into lock
    /// contention on every other handle operation, and would make any future backend-to-adapter call a deadlock rather than a bug.
    ///
    /// # Errors
    ///
    /// [`OpsError::NotFound`] for an unknown handle or a missing binding, or whatever the backend reports.
    pub fn close(&self, handle: HandleId) -> OpsResult<()> {
        let removal = {
            let mut table = self.handles.lock().expect("handle table");
            match table.close(handle)? {
                CloseEffect::Nothing => None,
                CloseEffect::Remove(file_id) => {
                    let (parent, name) = table.binding_of(file_id).ok_or_else(|| {
                        OpsError::InvalidRecord(format!("no binding for {file_id}"))
                    })?;
                    Some((parent, name, file_id))
                }
            }
        };
        match removal {
            None => Ok(()),
            Some((parent, name, file_id)) => {
                // Outside the lock, deliberately.
                self.ops.remove_at(parent, &name, file_id)?;
                self.handles.lock().expect("handle table").removed(file_id);
                Ok(())
            }
        }
    }

    /// Renames by path and moves the recorded binding: the handle keeps naming the entry, the entry keeps being findable by the
    /// adapter's deletion path.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports.
    pub fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome> {
        let outcome = self.ops.rename(
            &crate::ops::Path::new(from),
            &crate::ops::Path::new(to),
            replace,
        )?;
        // The backend reports which id moved (P3-4 added that field): deriving it from the path would be guessing, and a wrong guess
        // means the binding keeps the old name, so the eventual deletion is refused by the guard.
        self.handles.lock().expect("handle table").rebind(
            outcome.file_id,
            outcome.binding.0,
            outcome.binding.1.clone(),
        );
        Ok(outcome)
    }

    fn file_of(&self, handle: HandleId) -> OpsResult<u64> {
        Ok(self
            .handles
            .lock()
            .expect("handle table")
            .get(handle)
            .ok_or(OpsError::NotFound)?
            .file_id)
    }
}
