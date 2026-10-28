//! A workspace handle: identity, the tree it exposes, and the overlay it edits
//! (ADR-006 §3, §9.3).
//!
//! The handle is a **view**, not a copy: it holds one exclusive overlay file and the
//! snapshot the workspace currently starts from. Everything that makes changes durable —
//! `flush`, `snapshot`, `commit` — lives here or in [`crate::publish`].

use std::fmt;
use std::sync::Arc;

use cas_core::ObjectId;
use cas_store::Store;
use fs_overlay::OverlayStore;
use fs_snapshot::Profile;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::ids::parse_oid;
use crate::repo::Repo;
use crate::tree::WorkspaceStore;

/// One workspace's mutable view of a snapshot.
pub struct Workspace {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) branch: String,
    pub(crate) base_commit: String,
    pub(crate) base_root: ObjectId,
    pub(crate) overlay: Arc<OverlayStore>,
    pub(crate) repo: Arc<Repo>,
    pub(crate) store: std::sync::Mutex<Store>,
    pub(crate) profile: Profile,
    /// The publication gate (ADR-010 §10.3): read by every data operation, written by the publication steps.
    pub(crate) gate: std::sync::Arc<crate::gate::Gate>,
    /// The three fields a publication **moves**, behind one lock (S2 step 1).
    ///
    /// They live here as well as in the plain fields above **on purpose, for one commit**: this makes the type exist and be initialised while nothing reads it,
    /// so the change is "added, behaviour unchanged". The plain fields stay the source of truth until step 3 moves the 17 reads over; dropping them first would
    /// mean a single commit that both adds and removes, which is the shape this project keeps refusing.
    #[allow(dead_code, reason = "S2 step 1: initialised now, read from step 3 on")]
    pub(crate) state: std::sync::Mutex<BaseState>,
}

/// What a publication moves: the commit the view is based on, its root, and the branch it publishes to.
///
/// `allow(dead_code)` is the honest marker for one commit: the type exists and is initialised, and nothing reads it yet. Step 2 starts writing it, step 3 moves
/// the reads over, and the attribute disappears with the plain fields.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "S2 step 1: added before the reads move over, so that each step stays behaviour-preserving"
)]
pub struct BaseState {
    /// The commit the workspace's changes sit on.
    pub base_commit: String,
    /// That commit's root.
    pub base_root: ObjectId,
    /// The branch this workspace publishes to.
    pub branch: String,
}

impl fmt::Debug for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("branch", &self.branch)
            .field("base_commit", &self.base_commit)
            .field("base_root", &self.base_root)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    pub(crate) fn open(
        name: String,
        id: String,
        branch: String,
        base_commit: String,
        base_root: ObjectId,
        overlay: Arc<OverlayStore>,
        repo: Arc<Repo>,
    ) -> WorkspaceResult<Self> {
        // A second handle on the store, for publishing objects during materialisation. The
        // overlay owns its own; keeping them separate is what stops a `put` from happening
        // while the overlay holds a read scope (TX4).
        let store = Store::open_with(
            repo.store_dir(),
            cas_store::OpenMode::ReadWrite,
            // The same number as the repository's metadata connection: publishing contends
            // for the CAS index, and ADR-006 §11.1 asks for one bounded wait, not two.
            cas_store::StoreOptions {
                busy_timeout: repo.busy_timeout(),
            },
        )?;
        Ok(Self {
            state: std::sync::Mutex::new(BaseState {
                base_commit: base_commit.clone(),
                base_root,
                branch: branch.clone(),
            }),
            name,
            id,
            branch,
            base_commit,
            base_root,
            overlay,
            repo,
            store: std::sync::Mutex::new(store),
            profile: Profile::CURRENT,
            gate: std::sync::Arc::new(crate::gate::Gate::new()),
        })
    }

    /// The workspace's name in the main database.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace's directory id, which outlives the name (ADR-006 §4).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The branch this workspace publishes to.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The commit this workspace's changes are based on (ADR-006 §10.3's expectation).
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// The snapshot the view is composed over.
    ///
    /// Normally `root(base_commit)`; after [`Workspace::restore`] or [`Workspace::snapshot`]
    /// it can be a different tree without the base commit changing.
    #[must_use]
    pub fn base_root(&self) -> ObjectId {
        self.base_root
    }

    /// The explicit security descriptor of `file_id`, if the store has one.
    ///
    /// **Separate from `stat` on purpose** (ADR-005-v3 draft, V6): a descriptor can be 64 KiB, and `GetFileInformation`
    /// is called far more often than `GetSecurityInfo`. Folding it into `Stat` would copy 64 KiB per stat and make the
    /// read-amplification criterion unmeasurable.
    ///
    /// A v2 overlay has no column for it, so this returns `Ok(None)`: that is the truth about this store ("inherit the
    /// default"), not a placeholder waiting to be filled. A v3 overlay will return the stored bytes verbatim.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Overlay`] when the overlay cannot answer.
    pub fn security(
        &self,
        file_id: fs_namespace::FileId,
    ) -> crate::WorkspaceResult<Option<Vec<u8>>> {
        // Deliberately not a lookup of the record: the point of a separate accessor is that ordinary stat paths never
        // pay for the descriptor. When the v3 column exists, the read happens here and only here.
        // `file_id` is unused until the v3 column exists; naming that explicitly keeps a future reader from thinking the
        // parameter was forgotten.
        let _ = file_id;
        Ok(None)
    }

    /// Reads a file by **FileId** (P3-2 G2): the entry the caller holds, not the name it had.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Overlay`] with `IsADirectory`/`NotFound` as the overlay reports them.
    pub fn read_at_id(
        &self,
        file_id: fs_namespace::FileId,
        offset: u64,
        len: usize,
    ) -> WorkspaceResult<Vec<u8>> {
        let _gate = self.gate.read();
        Ok(self.overlay.read_at_id(file_id, offset, len)?)
    }

    /// Writes by **FileId** (P3-3b): the write-side twin of [`Self::read_at_id`].
    ///
    /// A handle names an object, not a path. Writing through the path version after a rename or a `SUPERSEDE` would write
    /// to whatever now carries that name — the TOCTOU that G1 and G2 removed, re-introduced one layer up.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Overlay`] with `IsADirectory`/`NotFound`.
    pub fn write_at_id(
        &self,
        file_id: fs_namespace::FileId,
        offset: u64,
        bytes: &[u8],
    ) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self.overlay.write_at_id(file_id, offset, bytes)?)
    }

    /// Sets the size by **FileId** (P3-3b) — the twin of [`Self::write_at_id`].
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Overlay`] with `IsADirectory`/`NotFound`.
    pub fn truncate_id(&self, file_id: fs_namespace::FileId, size: u64) -> WorkspaceResult<()> {
        Ok(self.overlay.truncate_id(file_id, size)?)
    }

    /// Lists a directory by **FileId**, resumable after `after` (P3-2 G2).
    ///
    /// A directory handle must survive a rename, so enumeration cannot go through a path. The order is the tree's:
    /// the same call twice returns the same sequence, which is what the card's "stable order" criterion asks for.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Namespace`] with `NotFound`/`NotADirectory` when the id is not a live directory.
    pub fn read_dir_id(
        &self,
        dir_id: fs_namespace::FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> WorkspaceResult<Vec<(Vec<u8>, fs_namespace::FileId)>> {
        use fs_namespace::TreeStore;
        let store = self.store();
        Ok(store.read(|tree| {
            use fs_namespace::{Kind, TreeRead};
            // Liveness first: `read_dir` on a dead id would otherwise return an empty list, which is
            // indistinguishable from "an empty directory" and hides a stale handle instead of reporting it.
            match tree.inode(dir_id)? {
                Some(view) if view.kind == Kind::Directory => {}
                Some(_) => {
                    return Err(fs_namespace::NamespaceError::NotADirectory {
                        path: String::new(),
                    })
                }
                None => {
                    return Err(fs_namespace::NamespaceError::NotFound {
                        path: String::new(),
                    })
                }
            }
            tree.read_dir(dir_id, after, limit)
        })?)
    }

    /// The overlay file's path.
    #[must_use]
    pub fn overlay_path(&self) -> &std::path::Path {
        self.overlay.path()
    }

    /// A handle that drives `fs_namespace::Rules` against this workspace.
    #[must_use]
    pub fn store(&self) -> WorkspaceStore {
        WorkspaceStore::new(Arc::clone(&self.overlay), Arc::clone(&self.repo))
    }

    /// The overlay behind this workspace.
    ///
    /// **No longer a public door** (ADR-010 §10.3, G5): reaching the overlay directly skips the publication gate, so a write arriving during a `commit` would not
    /// be ordered against it. Everything a caller needs is a method on `Workspace` that takes the gate — `set_times` and `set_attributes` were the last two that
    /// did not, and they are here now, which is why nothing outside this crate calls this any more.
    #[must_use]
    #[allow(
        dead_code,
        reason = "kept as the crate-internal accessor the gate-protected methods share"
    )]
    pub(crate) fn overlay(&self) -> &OverlayStore {
        &self.overlay
    }

    /// Sets file times by id, under the gate.
    ///
    /// # Errors
    ///
    /// `NotFound` for an absent inode, or a store failure.
    pub fn set_times(
        &self,
        file_id: fs_namespace::FileId,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self
            .overlay
            .set_times(file_id, accessed, written, changed, created)?)
    }

    /// Sets attribute bits by id, under the gate.
    ///
    /// # Errors
    ///
    /// `NotFound` for an absent inode, or a store failure.
    pub fn set_attributes(
        &self,
        file_id: fs_namespace::FileId,
        attributes: u32,
    ) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self.overlay.set_attributes(file_id, attributes)?)
    }

    /// Creates an empty regular file, reserving serials first if the workspace is low.
    ///
    /// The overlay's own content API cannot reserve: the allocator lives in the main
    /// database, and the overlay's transaction must not be open across another database's
    /// lock. So every content operation goes through this crate's handle, which buys the
    /// serials — otherwise a workspace with more than one segment's worth of new files
    /// would run dry (`ov_meta.serial_next` exhausted).
    ///
    /// # Errors
    ///
    /// Namespace rules or allocator failures.
    pub fn create_file(&self, path: &str) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        self.reserve_for_growth()?;
        Ok(self.overlay.create_file(&parse_path(path)?)?)
    }

    /// Writes `bytes` at `offset`, extending the file when the write runs past its end.
    ///
    /// # Errors
    ///
    /// Namespace rules or allocator failures.
    pub fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self.overlay.write_at(&parse_path(path)?, offset, bytes)?)
    }

    /// Sets a file's size.
    ///
    /// # Errors
    ///
    /// Namespace rules or allocator failures.
    pub fn truncate(&self, path: &str, size: u64) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self.overlay.truncate(&parse_path(path)?, size)?)
    }

    /// Reads `[offset, offset + len)` of a file.
    ///
    /// # Errors
    ///
    /// Namespace rules.
    pub fn read_at(&self, path: &str, offset: u64, len: usize) -> WorkspaceResult<Vec<u8>> {
        Ok(self.overlay.read_at(&parse_path(path)?, offset, len)?)
    }

    /// A file's logical size.
    ///
    /// # Errors
    ///
    /// Namespace rules.
    pub fn content_size(&self, path: &str) -> WorkspaceResult<u64> {
        Ok(self.overlay.content_state(&parse_path(path)?)?.size)
    }

    /// The tree backend's serial top-up, so a caller that drives the overlay directly can
    /// keep the same guarantee.
    fn reserve_for_growth(&self) -> WorkspaceResult<()> {
        self.store().top_up()
    }

    /// Makes every change so far durable (ADR-006 §8).
    ///
    /// # Errors
    ///
    /// Overlay failures.
    pub fn flush(&self) -> WorkspaceResult<()> {
        let _gate = self.gate.read();
        Ok(self.overlay.flush()?)
    }

    /// Whether the workspace has changes the base does not have.
    ///
    /// # Errors
    ///
    /// Overlay failures.
    pub fn is_dirty(&self) -> WorkspaceResult<bool> {
        Ok(self.overlay.is_dirty()?)
    }

    /// A consistent summary of the workspace's state.
    ///
    /// # Errors
    ///
    /// Overlay failures.
    pub fn status(&self) -> WorkspaceResult<WorkspaceStatus> {
        Ok(WorkspaceStatus {
            name: self.name.clone(),
            id: self.id.clone(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
            base_root: self.base_root.to_string(),
            dirty: self.is_dirty()?,
        })
    }

    /// Replaces the view's base with `commit`'s tree, leaving `base_commit` alone
    /// (ADR-006 §10.2).
    ///
    /// A later `commit` therefore records "put the content back the way this commit had
    /// it", which is what makes restore different from `reset`: history grows.
    ///
    /// `discard` allows replacing a workspace that still has uncommitted changes.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::WorkspaceDirty`] when the overlay is not empty and `discard` is
    /// false, [`WorkspaceError::NotFound`] when the commit is unknown.
    pub fn restore(&mut self, commit: &str, discard: bool) -> WorkspaceResult<()> {
        let gate = std::sync::Arc::clone(&self.gate);
        let _gate = gate.write();
        let root = self.repo.commit_root(commit)?;
        if !discard && self.is_dirty()? {
            return Err(WorkspaceError::WorkspaceDirty);
        }
        // The same three-step shape as a publication: record the intent, move the main
        // database (the publication point), then adopt. A crash at any point leaves the
        // §9.3 table with an unambiguous answer.
        self.overlay.set_pending_root(Some(root))?;
        if let Err(error) = self.repo.set_workspace_base_root(&self.name, root) {
            let _ = self.overlay.set_pending_root(None);
            return Err(error);
        }
        self.overlay.adopt_base(root)?;
        self.base_root = root;
        Ok(())
    }

    /// Re-points this handle and the main database at `branch` (ADR-006 §10.3, 出路 1).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::BranchMoved`] when the branch's head is not this workspace's base
    /// commit — rebinding is only allowed onto an unchanged branch, because merging is not
    /// part of P2.
    pub fn rebind(&mut self, branch: &str) -> WorkspaceResult<()> {
        let gate = std::sync::Arc::clone(&self.gate);
        let _gate = gate.write();
        self.repo.workspace_rebind(&self.name, branch)?;
        self.branch = branch.to_owned();
        Ok(())
    }

    /// Closes the handle.
    ///
    /// The overlay file stays; the workspace can be opened again. If an [`WorkspaceStore`]
    /// clone is still alive the SQLite connection stays open until that is dropped too.
    ///
    /// # Errors
    ///
    /// Never in this build; the signature leaves room for a final checkpoint.
    pub fn close(self) -> WorkspaceResult<()> {
        Ok(())
    }

    /// The commit id a publication must have as its parent.
    pub(crate) fn parent_commit(&self) -> ObjectId {
        parse_oid(&self.base_commit).expect("the stored base commit is a commit id")
    }

    /// Records the outcome of a publication in the main database and the handle.
    pub(crate) fn adopt(&mut self, root: ObjectId, commit: Option<String>) -> WorkspaceResult<()> {
        self.overlay.adopt_base(root)?;
        self.base_root = root;
        if let Some(commit) = commit {
            self.base_commit = commit;
        }
        Ok(())
    }
}

fn parse_path(text: &str) -> WorkspaceResult<fs_namespace::Path> {
    fs_namespace::Path::parse(text).map_err(|error| {
        WorkspaceError::Namespace(fs_namespace::NamespaceError::InvalidPath(error))
    })
}

/// What `status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// The workspace's name.
    pub name: String,
    /// Its directory id.
    pub id: String,
    /// The branch it publishes to.
    pub branch: String,
    /// The commit its changes are based on.
    pub base_commit: String,
    /// The snapshot its view is composed over.
    pub base_root: String,
    /// Whether it has changes the base does not have.
    pub dirty: bool,
}

/// One row of `workspace list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSummary {
    /// The workspace's name.
    pub name: String,
    /// Its directory id.
    pub id: String,
    /// The branch it publishes to.
    pub branch: String,
    /// The commit its changes are based on.
    pub base_commit: String,
    /// `creating` or `ready`.
    pub state: String,
}
