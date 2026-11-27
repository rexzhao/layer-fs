//! `fs-dedup`: streamlined multi-workspace deduplicating WinFsp filesystem for large
//! Unity repositories (`FastCDC-256K` + `Zstd-1`/`LZ4` external 128 MiB packs + lightweight
//! SQLite metadata + NTFS sparse-file CoW overlay + background cold-file dedup).

pub mod backend;
pub mod chunker;
pub mod error;
pub mod filter;
pub mod meta;
pub mod mount;
pub mod overlay;
pub mod pack;
pub mod store;

pub use backend::DedupWorkspaceBackend;
pub use chunker::Codec;
pub use error::{Error, Result};
pub use filter::{UNITY_IMPORT_EXCLUDE_PRESET, UNITY_NO_DEDUP_PRESET};
pub use meta::{StoreConfig, StoreStats, WorkspaceInfo};
pub use mount::{request_umount, run_mount, MountOptions};
pub use pack::{DEFAULT_CACHE_BYTES, DEFAULT_PACK_TARGET_BYTES};
pub use store::{DedupStore, DedupSummary, GcSummary, ImportProgress, ImportSummary};
