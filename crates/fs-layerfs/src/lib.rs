//! Repository, branch/merge/history facade, and WinFsp `WorkspaceOps` bridge
//! directly over the upstream LayerFS v1.0.0 core crates.

#![forbid(unsafe_code)]

pub mod backend;
pub mod error;
pub mod repo;

pub use backend::WinFspBackend;
pub use error::{Error, Result};
pub use repo::{
    BranchInfo, CommitInfo, CommitSummary, DiffEntry, ImportSummary, MergeOutcome,
    PersistenceProfile, PublishOutcome, ReclaimSummary, Repo, StoreStats, WorkspaceHandle,
    WorkspaceStatus, DEFAULT_BRANCH, DEFAULT_VOLUME_SIZE_BYTES,
};
