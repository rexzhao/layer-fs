//! The overlay backend: a per-workspace write layer composed over an immutable
//! snapshot (ADR-006 §5, §7).
//!
//! A workspace does not copy the base. It keeps its changes in its own SQLite file and
//! composes them with the snapshot at read time:
//!
//! ```text
//! read(serial)  = ov_inode row  (including "deleted")  ??  base inode
//! lookup(dir,n) = ov_entry row  (including whiteout)   ??  base directory entry
//! ```
//!
//! Because the overlay is keyed by **serial** and not by path (W5), renaming a
//! directory writes two name rows and leaves the subtree alone, and a file's identity
//! survives a move.
//!
//! # What this crate implements
//!
//! [`OverlayStore`] implements [`fs_namespace::TreeStore`], so `fs_namespace::Rules`
//! runs against it **unchanged** — that is the whole point of P2a-3 and the first thing
//! to check if something here looks wrong: if the rules needed editing to work, the
//! refactor was wrong, not the rules.
//!
//! # Durability
//!
//! Each change is one overlay transaction at `synchronous = NORMAL`: committed, visible
//! to this workspace, not fsynced. [`OverlayStore::flush`] makes everything up to that
//! point durable with a single `FULL` commit (W3, W4). Crash recovery is consequently
//! "a prefix of the operation sequence that contains everything before the last
//! successful flush".
//!
//! # Known cost
//!
//! The first time a workspace modifies a directory that lives in the base, the overlay
//! row needs the directory's visible entry count (so `rmdir` and rename's R6 stay
//! O(1)). Computing it walks the base directory once. Every later change adjusts the
//! count incrementally.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod base;
mod content;
mod error;
mod publish;
pub mod schema;
mod store;
mod store_impl;
mod view;
mod write_view;

pub use content::ContentState;
pub use error::{OverlayError, OverlayResult};
pub use store::{OverlayOptions, OverlayStore};
pub use view::OverlayRead;
pub use write_view::OverlayWrite;
