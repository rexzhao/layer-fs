//! The immutable namespace layer over the content-addressed Store.
//!
//! This crate owns logical file identity ([`FileId`]), directory entries, path
//! operations and root publication. It knows nothing about where object bytes
//! live: objects go through [`cas_store::Store`], and the namespace tables live
//! in that Store's metadata file on this crate's own connection (ADR-003
//! §4.2.1).
//!
//! ```no_run
//! use cas_store::{OpenMode, Store};
//! use fs_namespace::{Namespace, Path};
//!
//! let store = Store::open("my-store", OpenMode::Create)?;
//! let namespace = Namespace::open(&store, "default", OpenMode::Create)?;
//! namespace.mkdir(&Path::parse("/docs")?)?;
//! namespace.mkdir(&Path::parse("/docs/api")?)?;
//! assert_eq!(namespace.read_dir(&Path::parse("/docs")?)?.len(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Roots (ADR-003 §4.1, D+)
//!
//! The current tree is mutable; every mutating operation appends exactly one row
//! to an append-only publication chain. **P1 does not provide historical tree
//! views** — reading an old root's contents is P2 Snapshot work. What P1
//! guarantees is: the chain is append-only, publication is atomic with the
//! operation, and the chain's parent links are unbroken.
//!
//! # What this crate does not do
//!
//! No file content operations (`create`/`read_at`/`write_at`/`truncate` — T4), no
//! CLI (T4), no recursive delete, no hard or symbolic links, no branches or
//! snapshots (P2), and no Windows semantics (P3).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod content;
mod db;
mod error;
/// A deterministic mid-operation failure for this crate's atomicity tests.
///
/// Compiled only with the `failure-injection` feature, which the crate enables
/// for its own tests and no consumer receives by default.
#[cfg(feature = "failure-injection")]
pub mod inject;
/// The inode view the path rules see (ADR-006 §7.1).
mod inode_view;
mod namespace;

mod path;
mod rules;
mod schema;
mod sql_tree;
mod sql_tree_store;
mod tree_store;

use rusqlite::Connection;

pub use content::{ContentState, Extent};
pub use db::NamespaceDb;
pub use error::{InvalidPath, NamespaceError, NamespaceResult, MAX_DEPTH, MAX_NAME_BYTES};
pub use inode_view::InodeView;
pub use namespace::{Entry, FileId, Kind, Namespace, RootRecord, Stat};
pub use path::Path;
pub use sql_tree::{SqlRead, SqlTree, SqlWrite};
pub use tree_store::{OpLabel, TreeRead, TreeStore, TreeWrite};

/// Applies the connection settings the namespace connection needs.
///
/// Mirrors the choices `cas-store` makes for its own connection (ADR-002 §9):
/// WAL so both connections coexist, `synchronous = FULL` so a committed root
/// publication is durable, and no busy handler so contention is a typed failure
/// rather than a wait (ADR-003 §7).
fn configure(connection: &Connection) -> Result<(), NamespaceError> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "busy_timeout", 0)?;
    Ok(())
}
