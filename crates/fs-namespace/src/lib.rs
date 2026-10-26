//! The path rules and the storage contract they are written against (ADR-006 §7.1).
//!
//! This crate is the part of the filesystem that is **independent of how a tree is
//! stored**: the vocabulary ([`FileId`], [`Kind`], [`Entry`], [`Stat`]), the path syntax
//! ([`Path`]), the operations and their rules ([`Rules`], R1–R12), and the two traits a
//! backend implements ([`TreeRead`]/[`TreeWrite`] and [`TreeStore`]).
//!
//! ```no_run
//! use fs_namespace::{Path, Rules};
//!
//! # fn run<S: fs_namespace::TreeStore>(store: S) -> Result<(), Box<dyn std::error::Error>> {
//! let rules = Rules::new(store);
//! rules.mkdir(&Path::parse("/docs")?)?;
//! assert_eq!(rules.read_dir(&Path::parse("/docs")?)?.len(), 0);
//! # Ok(())
//! # }
//! ```
//!
//! # Why there is no storage here
//!
//! There used to be: a SQLite table backend (`namespace`/`file`/`directory_entry`/
//! `file_extent`/`root`) with its own DDL, connection profile and append-only root chain.
//! ADR-006 retired it (§14): a workspace keeps its changes in an overlay file and reads
//! its base from a snapshot, so no table of ours describes a tree any more. What remains
//! is the part that was worth keeping — the rules — and the traits that let both backends
//! run them unchanged. That is what P2a-3 set out to prove, and the overlay backend is
//! now the only implementation.
//!
//! # What this crate does not do
//!
//! No file content operations (the overlay's pages and the snapshot's extents are each
//! backend's own — ADR-006 §7.1), no storage, no branches or snapshots (fs-workspace),
//! and no Windows semantics (P3).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod error;
/// The inode view the path rules see (ADR-006 §7.1).
mod inode_view;
mod model;
mod path;
mod rules;
mod tree_store;

pub use error::{InvalidPath, NamespaceError, NamespaceResult, MAX_DEPTH, MAX_NAME_BYTES};
pub use inode_view::InodeView;
pub use model::{Entry, FileId, Kind, Stat};
pub use path::Path;
/// The path rules, for a backend to run unchanged (ADR-006 W6).
///
/// `Rules` is what makes P2a-3 worth anything: a backend implements [`TreeStore`] and
/// gets every path rule — R1–R12, the violation forms, the ordering of checks — without
/// copying or re-deriving any of it.
pub use rules::Rules;
/// The path walks, for a backend that must resolve inside its own transaction.
///
/// A backend whose content operations live outside [`TreeStore`] (the overlay's pages)
/// has to resolve a path **in the same transaction** that writes, or it reintroduces a
/// check-then-use window. Re-exported so it can do that without duplicating the walk or
/// opening a nested read scope.
pub use rules::{resolve_in, resolve_parent_in};
pub use tree_store::{OpLabel, TreeRead, TreeStore, TreeWrite};
