//! Repository, branch/merge/history facade, and WinFsp `WorkspaceOps` bridge
//! directly over the upstream LayerFS v1.0.0 core crates.

#![forbid(unsafe_code)]

pub mod backend;
pub mod error;
pub mod repo;

pub use backend::WinFspBackend;
pub use error::{Error, Result};
pub use repo::{
    parse_cache_size, parse_import_source, resolve_store_cache_bytes, BranchInfo, CommitInfo,
    CommitSummary, DiffEntry, ImportSource, ImportSummary, ImportedBranchSummary, MergeOutcome,
    PersistenceProfile, PublishOutcome, ReclaimSummary, Repo, StoreStats, WorkspaceHandle,
    WorkspaceStatus, CACHE_SIZE_ENV, DEFAULT_BRANCH, DEFAULT_STORE_CACHE_BYTES,
    DEFAULT_VOLUME_SIZE_BYTES, MIN_STORE_CACHE_BYTES,
};
