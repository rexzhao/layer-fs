//! Workspaces over a CAS store: lifecycle, materialisation, snapshots, commits and
//! branches (ADR-006).
//!
//! A workspace is a mutable view of an immutable snapshot. It keeps its changes in an
//! overlay file (`fs_overlay`) and composes them with the snapshot its branch points at;
//! publishing that composition as a new snapshot is what makes the changes durable and
//! shareable.
//!
//! ```text
//! store.sqlite   cas-store's objects  +  this crate's workspace/branch/commit tables
//! workspaces/<id>/overlay.sqlite      one per workspace, opened exclusively
//! ```
//!
//! # Where things live, and why
//!
//! The main database holds what is **shared and low-frequency**: which branches exist,
//! where their heads point, the commit log, and the serial allocator. The overlay holds
//! what is **per-workspace and high-frequency**: the trees being edited. ADR-006 W1 splits
//! them that way so that one workspace's writes never contend with another's.
//!
//! # The publication order
//!
//! Objects are written to the CAS **before** any row references them (I5), and the row
//! that makes a tree reachable is the single publication point (ADR-006 §9.1). A crash
//! therefore leaves either an unreferenced object — harmless, and reused by content
//! addressing if the same tree is published again — or nothing, never a reference to an
//! object that does not exist.
//!
//! # What this crate is not
//!
//! It is not the command-line or `fs-api` surface (P2a-7): the operations here are the
//! seam those will call. It does not read a published tree back — `fs-snapshot` does that
//! from a root id and a store, with no table of ours involved (I1).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod busy;
mod error;
pub mod gc;
mod ids;
mod main_db;
mod materialize;
mod publish;
mod repo;
mod schema;
mod tree;
mod workspace;

pub use error::{WorkspaceError, WorkspaceResult};
pub use ids::{commit_id, new_scope_id, new_workspace_id, parse_oid, random_bytes, SerialSegment};
pub use main_db::MainDb;
pub use publish::PublishOutcome;
pub use repo::{BranchInfo, CommitInfo, Repo, RepoStats};
pub use schema::{
    DEFAULT_BRANCH, INIT_MESSAGE, MAIN_BUSY_TIMEOUT_MS, OVERLAY_PAGE_BYTES, SCHEMA_VERSION,
    SERIAL_LOW_WATER, SERIAL_SEGMENT, STATE_CREATING, STATE_READY,
};
pub use tree::WorkspaceStore;
pub use workspace::{Workspace, WorkspaceStatus, WorkspaceSummary};
