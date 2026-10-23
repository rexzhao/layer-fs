//! The snapshot format: a file-system tree as an immutable CAS object graph
//! (ADR-005).
//!
//! A snapshot is a pure object graph — a root object (role 8) pointing at an inode
//! table (role 7), which points at per-directory trees (roles 5/6) and per-file
//! extent trees (roles 3/4). Reading one needs the root's object id and the store;
//! it never consults a namespace table.
//!
//! ```no_run
//! use fs_snapshot::{encode_root, Profile, RootValue};
//!
//! # fn run(sink: &mut dyn fs_snapshot::PageSink, table: cas_core::ObjectId) -> Result<(), Box<dyn std::error::Error>> {
//! let profile = Profile::V1;
//! let root = RootValue { scope_id: [0u8; 32], inode_table: table };
//! let oid = encode_root(&profile, &root, sink)?;
//! # Ok(())
//! # }
//! ```
//!
//! # Why the root id is history independent
//!
//! Page boundaries come from a hash of each key (ADR-005 §4), so a given set of
//! entries produces the same pages no matter what order they were inserted into the
//! tree. Re-encoding an unchanged tree yields byte-identical pages and therefore the
//! same root id — the property a branch's content token depends on.
//!
//! # What this crate is not
//!
//! It does not read or write namespaces, does not know about overlays, workspaces,
//! branches or commits, and never decides *when* a snapshot is taken. It turns an
//! in-memory tree into objects and back.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

pub mod bytes;
mod cdc;
mod encode;
mod error;
pub mod pager;
pub mod profile;
pub mod read;
pub mod record;

pub use cdc::{chunk, gear_table, is_zero, CdcParams, ChunkSpan};
pub use encode::{encode_directory, encode_extents, encode_inode_table, encode_root};
pub use error::{SnapshotError, SnapshotResult};
pub use pager::PageSink;
pub use profile::{Chunker, Profile, TreeDomain};
pub use read::SnapshotReader;
pub use record::{Content, DirEntry, Extent, InodeRecord, Kind, RootValue};
