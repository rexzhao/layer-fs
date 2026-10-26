//! The workspace filesystem: repositories, workspaces and readable history
//! (ADR-006 §13).
//!
//! `fs-api` is the crate a consumer talks to. It is path-addressed and stateless
//! (ADR-004 §3): nothing is held open between calls, and every call resolves its path
//! inside the transaction that uses it.
//!
//! ```no_run
//! use fs_api::Repo;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let repo = Repo::create("my-store")?;
//! let mut workspace = repo.workspace_create("w1", "main")?;
//! workspace.create("/hello.txt")?;
//! workspace.write_at("/hello.txt", 0, b"hello, world")?;
//! let commit = workspace.commit("first", false)?;
//!
//! // History is a snapshot root, so it needs no table of ours to read (I1).
//! let view = repo.read_view(&commit)?;
//! assert_eq!(view.read_all("/hello.txt")?, b"hello, world");
//! # Ok(())
//! # }
//! ```
//!
//! # The three objects
//!
//! - [`Repo`] — one store: its branches, its history, and the workspaces registered in it.
//! - [`Workspace`] — a mutable view: edits go into an overlay file, `commit` publishes
//!   them, and two workspaces on one branch cannot silently overwrite each other
//!   (ADR-006 §10.3).
//! - [`ReadView`] — any commit's tree, read straight from the CAS with the path rules and
//!   nothing else.
//!
//! # What this crate does not do
//!
//! No open handles, no memory mapping (P3), no merge, no garbage collection, no CDC or
//! pack (P2b). It does not decide when a snapshot is taken; that is the caller's
//! `snapshot`/`commit`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod error;
mod read_view;
mod repo;
mod stats;
mod verify;
mod workspace;

pub use error::{ApiError, ApiResult};
pub use fs_namespace::disposition;
pub use read_view::ReadView;
pub use repo::{
    gc, BranchInfo, CommitInfo, GcSummary, Repo, WorkspaceStatus, WorkspaceSummary,
    DEFAULT_REPACK_THRESHOLD,
};
pub use stats::Stats;
pub use verify::{Dangling, VerifyReport};
pub use workspace::Workspace;

// Re-exported so entry points (the CLI) can name the types they handle without taking a
// dependency on the crates this layer is built from (ADR-004 §3: entry points depend on
// the API, not on what it is built from).
pub use fs_namespace::{Entry, Kind, Stat};
