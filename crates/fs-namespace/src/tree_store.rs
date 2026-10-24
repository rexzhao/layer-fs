//! The storage contract the path rules are written against (ADR-006 §7.1).
//!
//! The rules in [`crate::operations`] — R1–R12, the violation forms, the ordering of
//! checks — know nothing about SQLite. They talk to this trait, and each concrete store
//! supplies its own implementation: the overlay backend (`fs-overlay`, P2a-4) and the
//! snapshot-backed store (`fs-workspace`, P2b).
//!
//! # What is deliberately *not* here
//!
//! Content operations (`update_inode`, the extent methods) are absent by design. The
//! overlay represents a file's bytes as pages plus a `base_limit`, with no
//! `content_root` and no extent rows; a trait that demanded `content_root` would force
//! the overlay to imitate P1's tables — the thing it exists to avoid. Content lives on
//! each store as its own API instead (`fs-overlay`'s page methods, `fs-workspace`'s
//! materialiser).
//! The rules here only ever change *name bindings* and *inode existence*.
//!
//! # Atomicity
//!
//! Atomicity is part of this contract, not an implementation detail of one backend.
//! [`TreeStore::write`] runs its body inside one atomic change: the body either
//! commits whole or leaves nothing. [`TreeStore::read`] shows one consistent state
//! across its body, so a rule cannot read two different worlds within one operation.

use crate::{FileId, InodeView, NamespaceResult};

/// Read-only access to a tree.
///
/// Available both inside and outside a transaction: a [`TreeWrite`] is also a
/// `TreeRead`, so a rule can inspect the state it is about to change.
pub trait TreeRead {
    /// Finds the child bound to `name` in `parent`.
    ///
    /// Never resolves anything but a direct child: `name` must not contain a
    /// separator (the caller has already split the path).
    ///
    /// # Errors
    ///
    /// Backend failures. A missing entry is `Ok(None)`, not an error.
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>>;

    /// Reads one inode, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>>;

    /// Lists a directory's entries in name order, starting strictly after `after`.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>>;

    /// Whether a directory has no entries.
    ///
    /// Separate from `read_dir(dir, None, 1)` because an implementation may answer it
    /// much more cheaply than by scanning: the overlay keeps a per-inode entry count.
    /// Listing is not O(1) there —
    /// it merges the base directory with overlay rows and must skip whiteouts, so
    /// proving a directory is empty can require visiting every tombstone.
    ///
    /// `rmdir` and rename rule R6 both depend on this, so it belongs in the contract.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool>;
}

/// Write access to a tree.
///
/// Only obtainable inside [`TreeStore::write`], so a rule cannot mutate outside a
/// transaction.
pub trait TreeWrite: TreeRead {
    /// Binds `name` in `parent` to `child`.
    ///
    /// # Errors
    ///
    /// [`crate::NamespaceError::AlreadyExists`] when the name is taken — the backend
    /// decides this atomically, so two concurrent creators cannot both succeed.
    fn bind(&mut self, parent: FileId, name: &[u8], child: FileId) -> NamespaceResult<()>;

    /// Removes the binding `name` in `parent`, leaving the inode alone.
    ///
    /// # Errors
    ///
    /// Backend failures. Removing an absent name is not an error.
    fn unbind(&mut self, parent: FileId, name: &[u8]) -> NamespaceResult<()>;

    /// Creates an inode and returns its identity.
    ///
    /// The serial comes from the backend (ADR-003 §4.3: allocated, never reused).
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn create_inode(&mut self, kind: crate::Kind) -> NamespaceResult<FileId>;

    /// Removes an inode **and everything that hangs off it**.
    ///
    /// The cascade is the backend's duty, not the caller's: P1's A1 defect was
    /// exactly a caller forgetting half of it. After this returns, nothing owned by
    /// the inode is reachable.
    ///
    /// # Errors
    ///
    /// Backend failures.
    fn drop_inode(&mut self, id: FileId) -> NamespaceResult<()>;
}

/// A tree backend with transactional read and write scopes.
///
/// # Why the lifetimes are elided
///
/// The views are generic abstractions (GATs), but the `'_` in
/// `FnOnce(&mut Self::Write<'_>)` is **not** tied to `&self`: an elided lifetime in a
/// `Fn` bound becomes a higher-ranked one, i.e. `for<'t> FnOnce(&mut Self::Write<'t>)`.
/// That is what lets an implementation build the view from a locally-borrowed
/// connection and hand it to the body, which a named `&'a self` lifetime would forbid
/// (a `rusqlite::Transaction` must borrow its connection, and the connection lives
/// only for the duration of the call).
///
/// Spelling it `fn write<'a, T>(&'a self, … Self::Write<'a> …)` therefore does not
/// compile for a store whose transaction borrows its connection, and adding
/// `where Self: 'a` to the GATs only moves the error. Keep the elided form.
pub trait TreeStore {
    /// The read view handed to [`TreeStore::read`].
    type Read<'t>: TreeRead;

    /// The write view handed to [`TreeStore::write`].
    type Write<'t>: TreeWrite;

    /// Runs `body` against one consistent snapshot of the tree.
    ///
    /// # Errors
    ///
    /// Whatever the body returns, plus backend failures opening the scope.
    fn read<T>(
        &self,
        body: impl FnOnce(&Self::Read<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T>;

    /// Runs `body` as one atomic change.
    ///
    /// `body` returning `Ok` commits; returning `Err`, or panicking, leaves the tree
    /// as it was. `op` labels the change for the implementation's own bookkeeping — the
    /// overlay only logs it. The label is **not** a publication decision.
    ///
    /// # Errors
    ///
    /// Whatever the body returns, plus backend failures committing.
    fn write<T>(
        &self,
        op: OpLabel,
        body: impl FnOnce(&mut Self::Write<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T>;
}

/// The label describing what a change did (ADR-006 §7.1).
///
/// It is audit data, not semantics: a backend may ignore it, log it, or record it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpLabel {
    /// A directory was created.
    Mkdir,
    /// An empty directory was removed.
    Rmdir,
    /// A file was created.
    Create,
    /// A name was removed.
    Unlink,
    /// A name was moved.
    Rename,
    /// A file's bytes or size changed.
    Content,
}

impl OpLabel {
    /// The label as stored in the P1 root table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            OpLabel::Mkdir => "mkdir",
            OpLabel::Rmdir => "rmdir",
            OpLabel::Create => "create",
            OpLabel::Unlink => "unlink",
            OpLabel::Rename => "rename",
            OpLabel::Content => "content",
        }
    }
}
