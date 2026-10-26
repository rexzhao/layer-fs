//! The driver-independent half of the WinFsp frontend (P3-3a).
//!
//! This crate is `no FFI` and has no platform dependency: `cargo test -p fs-winfsp` runs anywhere. The FFI surface and
//! the real mount arrive in P3-4; everything that can be tested with ordinary Rust — handle identity across rename,
//! `SUPERSEDE`, two-phase deletion, directory cursors, error mapping — lives here, on a trait the mock and the real
//! backend both implement.

#![deny(missing_docs)]

pub mod dirent;
pub mod error;
pub mod handles;
pub mod mock;
pub mod ops;

pub use dirent::{DirentCursor, SortEntry};
pub use error::{to_failure, to_status_of};
pub use handles::{close_removes, CloseEffect, Handle, HandleId, HandleTable};
pub use mock::{Call, MockBackend, ROOT};
pub use ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome, Times,
    WorkspaceOps,
};
