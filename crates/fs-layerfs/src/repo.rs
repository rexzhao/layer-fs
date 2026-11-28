//! Repository, branch, merge, diff, publish, and workspace lifecycle facade
//! over the upstream LayerFS v1.0.0 storage and history crates.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

use layerfs_bridge::control::SdkValue;
use layerfs_content::filesystem::attributes::{
    build_attribute_tree, emit_value, AttributeEntry, AttributeKey, PortableMetadata,
};
use layerfs_content::filesystem::{
    build_filesystem, profile_id, scope_for_seed, statistics::logical_size, DirectoryUpdate,
    FilesystemInput, FilesystemObjects, FilesystemResources, InodeUpdate,
};
use layerfs_content::object::inode_leaf::{InodeKind, InodeMetadata, InodeValue};
use layerfs_content::ObjectId;
use layerfs_history::{
    BranchId, CommitMeta, CommitOutcome, CommitRequest, CommitStatistics, CommitVia, ForkRequest,
    ForkSource, HistoryCatalog, HistoryCatalogConfig, HistoryName, LayerStackId, LayerStackRecord,
    NodeId, ReserveRequest, StackInitialization, WorkspaceId,
};

/// Tag byte of a `LayerStackId` (`layerfs_history::contract::identity::LAYER_STACK_TAG`).
const LAYER_STACK_TAG: u8 = 0x31;
use layerfs_overlay::ProfileConfig;
use layerfs_persistence::{
    Handles, PersistenceConfig, SqliteAcquisitionSchema, SqlitePersistenceProfile,
    RECLAMATION_PAGE_LIMIT,
};
use layerfs_project::{import_tree, ImportTreeRequest, ProjectError};
use layerfs_storage::{ReservationBlocks, Storage, StoragePolicy};
use layerfs_store_service::bootstrap::{open_store_observed, OpenedStore};
use layerfs_store_service::history::{branch, lookup};
use layerfs_store_service::read::{compare, snapshot};
use layerfs_store_service::store::{
    BindRequest, BoundWorkspace, CommitOptions, Store,
};
use layerfs_store_service::{kernel_guard, Owner, OwnerConfig};
use layerfs_telemetry::timer::Timing;
use serde::{Deserialize, Serialize};

use crate::backend::WinFspBackend;
use crate::error::{Error, Result};

/// Default advertised virtual volume size (128 GiB).
pub const DEFAULT_VOLUME_SIZE_BYTES: u64 = 128 * 1024 * 1024 * 1024;

/// Default writable branch created on `init` (since `"main"` is the published layer line).
pub const DEFAULT_BRANCH: &str = "work";

/// Default immutable object read cache budget (128 MiB).
pub const DEFAULT_STORE_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Minimum allowed immutable object read cache budget (4 MiB).
pub const MIN_STORE_CACHE_BYTES: usize = 4 * 1024 * 1024;
/// Environment variable overriding the default object read cache size (e.g. `256M`, `1G`).
pub const CACHE_SIZE_ENV: &str = "LAYERFS_CACHE_SIZE";
/// Overlay SQLite page cache (4 MiB — metadata only, payloads live in loose sparse files).
const OVERLAY_PAGER_KIB: u32 = 4096;
/// Max concurrent workspaces bound per process.
const MAX_WORKSPACES: usize = 32;

/// Parses a human-readable cache size string (e.g. `128M`, `512MiB`, `1G`, or byte count).
pub fn parse_cache_size(text: &str) -> Result<usize> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidInput("cache size cannot be empty".into()));
    }
    let split = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let value: u64 = number
        .parse()
        .map_err(|_| Error::InvalidInput(format!("invalid cache size {text:?}")))?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(Error::InvalidInput(format!(
                "invalid cache size unit {other:?} in {text:?} (use K, M, G, or bytes)"
            )))
        }
    };
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| Error::InvalidInput(format!("cache size too large: {text:?}")))?;
    let bytes = usize::try_from(bytes)
        .map_err(|_| Error::InvalidInput(format!("cache size overflows usize: {text:?}")))?;
    if bytes < MIN_STORE_CACHE_BYTES {
        return Err(Error::InvalidInput(format!(
            "cache size {text:?} is below minimum ({} MiB)",
            MIN_STORE_CACHE_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

/// Resolves the active object read cache budget from explicit argument, `LAYERFS_CACHE_SIZE`,
/// or [`DEFAULT_STORE_CACHE_BYTES`].
pub fn resolve_store_cache_bytes(explicit: Option<usize>) -> Result<usize> {
    if let Some(bytes) = explicit {
        if bytes < MIN_STORE_CACHE_BYTES {
            return Err(Error::InvalidInput(format!(
                "cache size {bytes} bytes is below minimum ({} MiB)",
                MIN_STORE_CACHE_BYTES / (1024 * 1024)
            )));
        }
        return Ok(bytes);
    }
    if let Some(raw) = std::env::var_os(CACHE_SIZE_ENV) {
        let text = raw.to_string_lossy();
        if !text.trim().is_empty() {
            return parse_cache_size(&text);
        }
    }
    Ok(DEFAULT_STORE_CACHE_BYTES)
}

/// Durability profile for the underlying SQLite Store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersistenceProfile {
    /// WAL + `synchronous = FULL` (power-loss durable).
    #[default]
    Durable,
    /// WAL + `synchronous = OFF` (fast, survives process exit, matches upstream 1.0.0 default).
    Disposable,
}

impl PersistenceProfile {
    fn to_sqlite(self) -> SqlitePersistenceProfile {
        match self {
            Self::Durable => SqlitePersistenceProfile::Durable,
            Self::Disposable => SqlitePersistenceProfile::Disposable,
        }
    }

    /// Human-readable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Disposable => "disposable",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RepoMeta {
    version: u32,
    profile: PersistenceProfile,
    volume_size_bytes: u64,
    stack_name: String,
    binding_key: String,
    cursor_key_hex: String,
    stack_id_hex: String,
}

/// Summary of one branch or `main`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchInfo {
    pub name: String,
    pub base: String,
    pub head: Option<String>,
    pub forked_from: Option<String>,
}

/// Summary of one commit in `log`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitInfo {
    pub id: String,
    pub root: String,
    pub message: Option<String>,
    pub created_ms: Option<i64>,
    pub branch: Option<String>,
}

/// Summary of a workspace commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSummary {
    pub kind: String,
    pub commit: Option<String>,
    pub root: String,
    pub conflicts: Vec<String>,
}

/// Summary of a branch merge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeOutcome {
    pub kind: String,
    pub branch: String,
    pub fast_forward: bool,
    pub head: Option<String>,
    pub root: Option<String>,
    pub conflicts: Vec<String>,
}

/// Summary of publishing a branch onto `main`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishOutcome {
    pub kind: String,
    pub layer: String,
    pub overwrote_count: i64,
}

/// One changed path in `diff`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffEntry {
    pub path: String,
    pub kind: String,
}

/// Store-level statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreStats {
    pub profile: PersistenceProfile,
    pub volume_size_bytes: u64,
    pub store_file_bytes: u64,
    pub branches: usize,
    pub head_layer: String,
    pub logical_bytes: u64,
    pub canonical_bytes: u64,
    pub stored_bytes: u64,
}

/// One host directory source for `init --from`, with an optional explicit branch name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportSource {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Parses a `--from` source spec of the form `<path_to_dir>` or `<path_to_dir>:<branch_name>`.
///
/// Preserves Windows drive-letter colons (`C:\foo`, `C:`, `\\?\C:\foo`) while splitting a
/// trailing `:branch_name` suffix that contains no path separators.
pub fn parse_import_source(spec: &str) -> Result<ImportSource> {
    if spec.is_empty() {
        return Err(Error::InvalidInput("`--from` source cannot be empty".into()));
    }
    if let Some(colon_idx) = spec.rfind(':') {
        let is_drive_colon = colon_idx == 1 && spec.as_bytes()[0].is_ascii_alphabetic();
        let after = &spec[colon_idx + 1..];
        let after_has_sep = after.contains('\\') || after.contains('/');
        if !is_drive_colon && !after_has_sep {
            let raw_path = &spec[..colon_idx];
            if raw_path.is_empty() {
                return Err(Error::InvalidInput(format!(
                    "missing source directory before `:` in `{spec}`"
                )));
            }
            if after.is_empty() {
                return Err(Error::InvalidInput(format!(
                    "missing branch name after `:` in `{spec}`"
                )));
            }
            return Ok(ImportSource {
                path: PathBuf::from(raw_path),
                branch: Some(after.to_owned()),
            });
        }
    }
    Ok(ImportSource {
        path: PathBuf::from(spec),
        branch: None,
    })
}

/// Summary of one branch imported during `init --from`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedBranchSummary {
    pub branch: String,
    pub source: PathBuf,
    pub entries: u64,
    pub root: String,
    pub commit: Option<String>,
}

/// Result of importing one or more host directories into a new repository (`init --from`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportSummary {
    /// Total entries imported across all source directories (including root directories).
    pub entries: u64,
    /// Hex-encoded root object ID of the last imported tree (or empty genesis).
    pub root: String,
    /// Per-directory imported branch details, in input order.
    pub branches: Vec<ImportedBranchSummary>,
}

/// Result of SQLite incremental space reclamation (`compact`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReclaimSummary {
    /// Free pages observed before reclamation.
    pub free_pages_before: u64,
    /// Total free pages reclaimed back to the host filesystem.
    pub reclaimed_pages: u64,
    /// Remaining free pages after reclamation.
    pub remaining_free_pages: u64,
}

/// Live workspace status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceStatus {
    pub branch: String,
    pub base_layer: String,
    pub head_commit: Option<String>,
    pub effective_root: String,
    pub dirty_inodes: u64,
    pub dirty_entries: u64,
    pub dirty: bool,
}

/// An opened LayerFS repository backed by upstream `layerfs-store-service`.
pub struct Repo {
    dir: PathBuf,
    meta: RepoMeta,
    opened: OpenedStore,
    store: Arc<Store>,
    owner: Option<Owner>,
    stack_id: LayerStackId,
    overlay_dir: PathBuf,
    cache_bytes: usize,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo")
            .field("dir", &self.dir)
            .field("profile", &self.meta.profile)
            .finish()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            let _ = owner.stop();
        }
        let _ = std::fs::remove_dir_all(&self.overlay_dir);
    }
}

impl Repo {
    /// Creates a new empty repository directory containing a standard upstream LayerFS
    /// `store.sqlite`, an empty genesis LayerStack, and an initial writable branch (`work`).
    pub fn init(
        dir: impl AsRef<Path>,
        profile: PersistenceProfile,
        volume_size_bytes: u64,
    ) -> Result<Self> {
        let (repo, _) = Self::init_inner(dir.as_ref(), &[], profile, volume_size_bytes)?;
        Ok(repo)
    }

    /// Creates a new repository directory initialized from one existing host directory,
    /// automatically creating a branch named after the directory's basename.
    pub fn init_from(
        dir: impl AsRef<Path>,
        source: impl AsRef<Path>,
        profile: PersistenceProfile,
        volume_size_bytes: u64,
    ) -> Result<(Self, ImportSummary)> {
        let one = ImportSource {
            path: source.as_ref().to_path_buf(),
            branch: None,
        };
        Self::init_from_many(dir, &[one], profile, volume_size_bytes)
    }

    /// Creates a new repository directory initialized from one or more host directories.
    ///
    /// Each source directory creates a branch (named by explicit `source.branch` or inferred
    /// from the directory basename) with its imported tree committed on top of the shared
    /// empty genesis layer. All inputs and branch names are validated upfront before creating
    /// any directory or SQLite file so duplicate branch names or invalid sources leave zero
    /// side effects on disk.
    pub fn init_from_many(
        dir: impl AsRef<Path>,
        sources: &[ImportSource],
        profile: PersistenceProfile,
        volume_size_bytes: u64,
    ) -> Result<(Self, ImportSummary)> {
        if sources.is_empty() {
            return Err(Error::InvalidInput(
                "at least one `--from` source directory is required".into(),
            ));
        }
        Self::init_inner(dir.as_ref(), sources, profile, volume_size_bytes)
    }

    fn init_inner(
        dir: &Path,
        sources: &[ImportSource],
        profile: PersistenceProfile,
        volume_size_bytes: u64,
    ) -> Result<(Self, ImportSummary)> {
        let dir = dir.to_path_buf();
        let store_path = dir.join("store.sqlite");
        if store_path.exists() {
            return Err(Error::Conflict(format!(
                "store already exists at {}",
                store_path.display()
            )));
        }

        // Validate every `--from` source and resolve/deduplicate branch names BEFORE creating
        // any directory or SQLite file, guaranteeing zero side effects on validation errors.
        let resolved_target = resolve_future_path(&dir);
        let mut validated_sources: Vec<(PathBuf, String, HistoryName)> =
            Vec::with_capacity(sources.len());
        let mut seen_branches: std::collections::HashMap<String, PathBuf> =
            std::collections::HashMap::with_capacity(sources.len());
        for src in sources {
            if !src.path.is_dir() {
                return Err(Error::InvalidInput(format!(
                    "source directory does not exist or is not a directory: {}",
                    src.path.display()
                )));
            }
            let canon_src = std::fs::canonicalize(&src.path).unwrap_or_else(|_| src.path.clone());
            if path_starts_with_ci(&resolved_target, &canon_src) {
                return Err(Error::InvalidInput(format!(
                    "repository path {} cannot lie inside source directory {}",
                    dir.display(),
                    src.path.display()
                )));
            }
            let branch_name = match &src.branch {
                Some(explicit) => explicit.clone(),
                None => canon_src
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Error::InvalidInput(format!(
                            "cannot infer branch name from directory {}; specify `<path>:<branch>` explicitly",
                            src.path.display()
                        ))
                    })?,
            };
            if branch_name == "main" {
                return Err(Error::InvalidInput(format!(
                    "branch name \"main\" is reserved (from {}); specify `<path>:<branch>` with another name",
                    src.path.display()
                )));
            }
            let history_name = HistoryName::new(&branch_name).map_err(|_| {
                Error::InvalidInput(format!(
                    "invalid branch name {branch_name:?} (from {}); specify `<path>:<branch>` with a valid name",
                    src.path.display()
                ))
            })?;
            if let Some(previous) = seen_branches.insert(branch_name.clone(), src.path.clone()) {
                return Err(Error::Conflict(format!(
                    "duplicate branch name {branch_name:?} (from {} and {}); specify `<path>:<branch>` to disambiguate",
                    previous.display(),
                    src.path.display()
                )));
            }
            validated_sources.push((src.path.clone(), branch_name, history_name));
        }

        let dir_existed = dir.exists();
        std::fs::create_dir_all(&dir)?;
        if let Err(err) = kernel_guard::check(&store_path).map_err(|u| Error::Unsupported(u.0)) {
            if !dir_existed {
                let _ = std::fs::remove_dir(&dir);
            }
            return Err(err);
        }

        let binding_key = format!(
            "layerfs-win:{}",
            dir.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("repo")
        );
        let seed_hash = blake3::hash(
            format!(
                "{}:{}",
                binding_key,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
            .as_bytes(),
        );
        let cursor_key: [u8; 32] = *blake3::hash(b"cursor-key").as_bytes();
        let scope_seed: [u8; 32] = *seed_hash.as_bytes();
        let stack_name = HistoryName::new("default")
            .map_err(|e| Error::InvalidInput(format!("stack name: {e:?}")))?;
        let stack_id = LayerStackId::from_bytes({
            let mut raw = [0u8; 17];
            raw[0] = LAYER_STACK_TAG;
            raw[1..].copy_from_slice(&scope_seed[..16]);
            raw
        })
        .map_err(|e| Error::Other(format!("stack id: {e:?}")))?;

        let history_cfg = HistoryCatalogConfig {
            binding_key: binding_key.as_bytes().to_vec(),
            cursor_key,
            incarnation: 1,
        };
        let config = PersistenceConfig::sqlite(&store_path)
            .with_sqlite_profile(profile.to_sqlite())
            .with_sqlite_acquisition(SqliteAcquisitionSchema::Tables);

        let init_result: Result<ImportSummary> = (|| {
            let handles = Handles::create(config, StoragePolicy::frozen_default(), &history_cfg)
                .map_err(|e| Error::Other(format!("create store: {e:?}")))?;
            let storage = Storage::new(handles.storage.clone())
                .map_err(|e| Error::Other(format!("storage: {e:?}")))?;

            // Always create the shared empty genesis layer on `main` first so all imported
            // branches share a common merge base and root directory inode serial.
            let scope = scope_for_seed(scope_seed);
            let reservation = handles
                .history
                .reserve_inodes(&ReserveRequest {
                    scope: scope.object(),
                    count: 1,
                })
                .map_err(|e| Error::Other(format!("reserve root inode: {e:?}")))?;
            let genesis_root_serial = reservation.start;

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let portable = PortableMetadata {
                mode: 0o755,
                mtime_seconds: now.as_secs() as i64,
                mtime_nanoseconds: now.subsec_nanos(),
            };

            let reader = storage
                .reader()
                .map_err(|e| Error::Other(format!("reader: {e:?}")))?;
            let save = storage
                .begin_save()
                .map_err(|e| Error::Other(format!("begin save: {e:?}")))?;
            let genesis_root = {
                let mut sink = save.sink();
                let mut objects = FilesystemObjects::new(&reader, &mut sink);
                let mode_bytes = portable
                    .mode_bytes(InodeKind::Directory)
                    .map_err(|e| Error::Other(format!("mode bytes: {e:?}")))?;
                let mtime_bytes = portable
                    .mtime_bytes()
                    .map_err(|e| Error::Other(format!("mtime bytes: {e:?}")))?;
                let mode_root = emit_value(&mut objects, &mode_bytes)
                    .map_err(|e| Error::Other(format!("emit mode: {e:?}")))?;
                let mtime_root = emit_value(&mut objects, &mtime_bytes)
                    .map_err(|e| Error::Other(format!("emit mtime: {e:?}")))?;
                let attr_key_mode = AttributeKey::new("portable".into(), b"mode".to_vec())
                    .map_err(|e| Error::Other(format!("attr key: {e:?}")))?;
                let attr_key_mtime = AttributeKey::new("portable".into(), b"mtime".to_vec())
                    .map_err(|e| Error::Other(format!("attr key: {e:?}")))?;
                let (dir_meta, _) = build_attribute_tree(
                    &mut objects,
                    [
                        Ok(AttributeEntry {
                            key: attr_key_mode,
                            value_root: mode_root,
                        }),
                        Ok(AttributeEntry {
                            key: attr_key_mtime,
                            value_root: mtime_root,
                        }),
                    ]
                    .into_iter(),
                )
                .map_err(|e| Error::Other(format!("attr tree: {e:?}")))?;

                let root_inode = InodeUpdate {
                    serial: genesis_root_serial,
                    value: InodeValue {
                        kind: InodeKind::Directory,
                        content_root: ObjectId::for_bytes(b"new-directory"),
                        metadata_root: InodeMetadata::Root(dir_meta),
                        namespace_ref_count: 0,
                    },
                };
                let root_dir = DirectoryUpdate {
                    parent: genesis_root_serial,
                    changes: Vec::new(),
                };
                let input = FilesystemInput {
                    base: None,
                    scope,
                    root_serial: genesis_root_serial,
                    directories: &[root_dir],
                    inodes: &[root_inode],
                    new_inodes: &[genesis_root_serial],
                    resources: FilesystemResources::default(),
                };
                build_filesystem(&mut objects, &input, None)
                    .map_err(|e| Error::Other(format!("build empty root: {e:?}")))?
                    .root
            };
            save.finish()
                .map_err(|e| Error::Other(format!("finish genesis save: {e:?}")))?;

            let reader = storage
                .reader()
                .map_err(|e| Error::Other(format!("reader after save: {e:?}")))?;
            let logical = logical_size(&reader, genesis_root)
                .map_err(|e| Error::Other(format!("logical size: {e:?}")))?;
            let stack = handles
                .history
                .initialize_layerstack(&StackInitialization {
                    stack: stack_id,
                    name: stack_name.clone(),
                    scope: scope.object(),
                    profile: profile_id(),
                    genesis_root: genesis_root.0,
                    logical,
                })
                .map_err(|e| Error::Other(format!("initialize layerstack: {e:?}")))?;

            if validated_sources.is_empty() {
                // Empty init: fork the default writable branch `work` from the genesis layer.
                let default_name = HistoryName::new(DEFAULT_BRANCH)
                    .map_err(|e| Error::InvalidInput(format!("default branch: {e:?}")))?;
                handles
                    .history
                    .fork(&ForkRequest {
                        stack: stack.id,
                        branch: BranchId::named(stack.id, &default_name),
                        name: default_name,
                        source: ForkSource::Layer(stack.head_layer),
                    })
                    .map_err(|e| Error::Other(format!("fork default branch: {e:?}")))?;
                return Ok(ImportSummary {
                    entries: 1,
                    root: encode_hex(genesis_root.0.as_bytes()),
                    branches: Vec::new(),
                });
            }

            let mut total_entries = 0u64;
            let mut last_root = encode_hex(genesis_root.0.as_bytes());
            let mut imported_branches = Vec::with_capacity(validated_sources.len());

            for (src_path, branch_name, history_name) in &validated_sources {
                let (imported, _) = Timing::disabled("layerfs.init_from", |timer| {
                    import_tree(
                        &storage,
                        &handles.history,
                        ImportTreeRequest {
                            source: src_path,
                            acquisition: &handles.acquisition,
                            stack: stack.id,
                            scope_seed,
                            root_serial: genesis_root_serial,
                            deadline: Instant::now() + Duration::from_secs(3600),
                        },
                        timer,
                    )
                });
                let imported = imported.map_err(|e| match e {
                    ProjectError::BackingInsideSource => Error::InvalidInput(format!(
                        "repository path {} cannot lie inside source directory {}",
                        dir.display(),
                        src_path.display()
                    )),
                    ProjectError::InvalidInput => Error::InvalidInput(format!(
                        "invalid source tree at {}",
                        src_path.display()
                    )),
                    ProjectError::Unsupported => Error::Unsupported(format!(
                        "source tree {} contains unsupported entry types",
                        src_path.display()
                    )),
                    other => Error::Other(format!("import {}: {other}", src_path.display())),
                })?;

                let forked = handles
                    .history
                    .fork(&ForkRequest {
                        stack: stack.id,
                        branch: BranchId::named(stack.id, history_name),
                        name: history_name.clone(),
                        source: ForkSource::Layer(stack.head_layer),
                    })
                    .map_err(|e| Error::Other(format!("fork branch {branch_name}: {e:?}")))?;

                let mut meta = CommitMeta::new(CommitVia::Files);
                meta.message = Some(format!("import {}", src_path.display()));
                meta.statistics = CommitStatistics {
                    logical: imported.logical_bytes,
                    new_canonical: imported.canonical_bytes,
                    new_stored: imported.stored_bytes,
                    new_metadata: imported.metadata_bytes,
                };
                let outcome = handles
                    .history
                    .commit_branch(&CommitRequest {
                        branch: forked.branch.id,
                        expected_head: forked.branch.head_commit,
                        expected_base: forked.branch.base_layer,
                        expected_root: forked.effective_root,
                        candidate_root: imported.root,
                        profile: profile_id(),
                        scope: scope.object(),
                        second_parent: None,
                        meta,
                        force: false,
                    })
                    .map_err(|e| Error::Other(format!("commit imported branch {branch_name}: {e:?}")))?;

                let commit_hex = match outcome {
                    CommitOutcome::Committed { record, .. } => {
                        Some(encode_hex(record.id.as_slice()))
                    }
                    CommitOutcome::UpToDate { head, .. } => {
                        head.map(|id| encode_hex(id.as_slice()))
                    }
                };
                let root_hex = encode_hex(imported.root.as_bytes());
                total_entries = total_entries.saturating_add(imported.entries);
                last_root = root_hex.clone();
                imported_branches.push(ImportedBranchSummary {
                    branch: branch_name.clone(),
                    source: src_path.clone(),
                    entries: imported.entries,
                    root: root_hex,
                    commit: commit_hex,
                });
                // Best-effort reclaim of discarded acquisition working pages.
                let _ = handles.reclaim_space(RECLAMATION_PAGE_LIMIT);
            }

            Ok(ImportSummary {
                entries: total_entries,
                root: last_root,
                branches: imported_branches,
            })
        })();

        let summary = match init_result {
            Ok(summary) => summary,
            Err(err) => {
                // Clean up partial store artifacts on init/import failure.
                let _ = std::fs::remove_file(&store_path);
                let _ = std::fs::remove_file(dir.join("store.sqlite-wal"));
                let _ = std::fs::remove_file(dir.join("store.sqlite-shm"));
                if !dir_existed {
                    let _ = std::fs::remove_dir(&dir);
                }
                return Err(err);
            }
        };

        let meta = RepoMeta {
            version: 1,
            profile,
            volume_size_bytes,
            stack_name: "default".to_owned(),
            binding_key,
            cursor_key_hex: encode_hex(&cursor_key),
            stack_id_hex: encode_hex(&stack_id.to_bytes()),
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| Error::Other(format!("serialize repo.json: {e}")))?;
        std::fs::write(dir.join("repo.json"), meta_json)?;

        Ok((Self::open(dir)?, summary))
    }

    /// Reclaims free SQLite pages in `dir`'s `store.sqlite` using upstream bounded
    /// incremental vacuum (`Handles::reclaim_space`). Logical objects are untouched.
    pub fn reclaim_space(dir: impl AsRef<Path>, max_pages: Option<u32>) -> Result<ReclaimSummary> {
        let dir = dir.as_ref();
        let meta_path = dir.join("repo.json");
        let store_path = dir.join("store.sqlite");
        if !meta_path.is_file() || !store_path.is_file() {
            return Err(Error::NotFound(format!(
                "no layerfs repository at {}",
                dir.display()
            )));
        }
        let meta: RepoMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)
            .map_err(|e| Error::InvalidInput(format!("invalid repo.json: {e}")))?;
        let cursor_key: [u8; 32] = decode_hex_array(&meta.cursor_key_hex)?;
        let config =
            PersistenceConfig::sqlite(&store_path).with_sqlite_profile(meta.profile.to_sqlite());
        let handles = Handles::open_writable(config, meta.binding_key.as_bytes(), cursor_key)
            .map_err(|e| Error::Other(format!("open store for space reclamation: {e:?}")))?;

        let mut remaining_budget = max_pages.unwrap_or(u32::MAX);
        let mut free_pages_before = None;
        let mut total_reclaimed = 0u64;
        let mut remaining_free_pages = 0u64;
        while remaining_budget > 0 {
            let chunk = remaining_budget.min(RECLAMATION_PAGE_LIMIT);
            let rec = handles
                .reclaim_space(chunk)
                .map_err(|e| Error::Unsupported(format!("reclaim_space refused: {e:?}")))?;
            if free_pages_before.is_none() {
                free_pages_before = Some(rec.free_pages_before);
            }
            total_reclaimed = total_reclaimed.saturating_add(rec.reclaimed_pages);
            remaining_free_pages = rec.remaining_free_pages;
            if rec.reclaimed_pages == 0 || rec.remaining_free_pages == 0 {
                break;
            }
            remaining_budget = remaining_budget.saturating_sub(chunk);
        }
        handles.close();
        Ok(ReclaimSummary {
            free_pages_before: free_pages_before.unwrap_or(0),
            reclaimed_pages: total_reclaimed,
            remaining_free_pages,
        })
    }

    /// Opens an existing repository directory using `LAYERFS_CACHE_SIZE` or the default cache budget.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_cache(dir, None)
    }

    /// Opens an existing repository directory with an optional explicit object read cache budget.
    pub fn open_with_cache(dir: impl AsRef<Path>, cache_bytes: Option<usize>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let meta_path = dir.join("repo.json");
        let store_path = dir.join("store.sqlite");
        if !meta_path.is_file() || !store_path.is_file() {
            return Err(Error::NotFound(format!(
                "no layerfs repository at {}",
                dir.display()
            )));
        }
        kernel_guard::check(&store_path).map_err(|u| Error::Unsupported(u.0))?;

        let active_cache = resolve_store_cache_bytes(cache_bytes)?;
        let meta: RepoMeta = serde_json::from_str(&std::fs::read_to_string(&meta_path)?)
            .map_err(|e| Error::InvalidInput(format!("invalid repo.json: {e}")))?;
        let cursor_key: [u8; 32] = decode_hex_array(&meta.cursor_key_hex)?;
        let stack_id_bytes: [u8; 17] = decode_hex_array(&meta.stack_id_hex)?;
        let stack_id = LayerStackId::from_bytes(stack_id_bytes)
            .map_err(|e| Error::InvalidInput(format!("invalid stack id in repo.json: {e:?}")))?;

        let config =
            PersistenceConfig::sqlite(&store_path).with_sqlite_profile(meta.profile.to_sqlite());
        let opened = open_store_observed(
            config,
            meta.binding_key.as_bytes(),
            cursor_key,
            MAX_WORKSPACES,
            active_cache,
            ReservationBlocks::default(),
        )
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("Busy") || msg.contains("locked") {
                Error::Busy(format!("store open: {msg}"))
            } else {
                Error::Other(format!("store open: {msg}"))
            }
        })?;

        let seq = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let overlay_dir = dir.join(".overlay-runtime").join(format!(
            "s-{}-{seq}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&overlay_dir)?;
        let template_path = overlay_dir.join("template.sqlite");
        let owner = Owner::start(
            &template_path,
            ProfileConfig {
                pager_kib: OVERLAY_PAGER_KIB,
                max_pages: None,
            },
            OwnerConfig {
                namespaces: MAX_WORKSPACES,
            },
        )
        .map_err(|e| Error::Other(format!("start overlay owner: {e}")))?;

        let store = Arc::clone(&opened.store);
        Ok(Self {
            dir,
            meta,
            opened,
            store,
            owner: Some(owner),
            stack_id,
            overlay_dir,
            cache_bytes: active_cache,
        })
    }

    /// Active immutable object read cache size in bytes.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    /// Repository directory path.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Resolves a branch name when the caller omitted `--branch`:
    /// uses `requested` if given, otherwise `"work"` if present, otherwise the sole branch
    /// when exactly one branch exists.
    pub fn resolve_default_branch(&self, requested: Option<&str>) -> Result<String> {
        if let Some(name) = requested {
            return Ok(name.to_owned());
        }
        let branches = self.branches()?;
        if branches.iter().any(|b| b.name == DEFAULT_BRANCH) {
            return Ok(DEFAULT_BRANCH.to_owned());
        }
        match branches.as_slice() {
            [] => Err(Error::NotFound("repository has no branches".into())),
            [only] => Ok(only.name.clone()),
            many => {
                let names = many
                    .iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(Error::InvalidInput(format!(
                    "multiple branches exist ({names}); specify `--branch <name>`"
                )))
            }
        }
    }

    /// Configured virtual volume size.
    #[must_use]
    pub fn volume_size_bytes(&self) -> u64 {
        self.meta.volume_size_bytes
    }

    /// Persistence profile (`durable` or `disposable`).
    #[must_use]
    pub fn profile(&self) -> PersistenceProfile {
        self.meta.profile
    }

    /// Underlying SQLite version.
    #[must_use]
    pub fn sqlite_version(&self) -> &str {
        &self.opened.sqlite_version
    }

    fn stack(&self) -> Result<LayerStackRecord> {
        self.store
            .history()
            .layer_stack(self.stack_id)
            .map_err(|e| Error::Other(format!("read layerstack: {e:?}")))?
            .ok_or_else(|| Error::NotFound("repository layerstack missing".to_owned()))
    }

    /// Lists `main` and all branches.
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let stack = self.stack()?;
        let val = branch::list(&self.store, &stack, 4096, 0, None).map_err(Error::from_sdk)?;
        let Some(SdkValue::List(items)) = val.get("items") else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(BranchInfo {
                name: item.get_text("name").unwrap_or_default().to_owned(),
                base: item.get_text("base").unwrap_or_default().to_owned(),
                head: item.get_text("head").map(str::to_owned),
                forked_from: item.get_text("forkedFrom").map(str::to_owned),
            });
        }
        Ok(out)
    }

    /// Creates a new branch forked from `from` (defaults to `"main"`).
    pub fn fork(&self, name: &str, from: Option<&str>) -> Result<BranchInfo> {
        let stack = self.stack()?;
        let val = branch::fork(&self.store, &stack, name, from).map_err(Error::from_sdk)?;
        Ok(BranchInfo {
            name: val.get_text("branch").unwrap_or(name).to_owned(),
            base: val.get_text("base").unwrap_or_default().to_owned(),
            head: val.get_text("head").map(str::to_owned),
            forked_from: from.map(str::to_owned),
        })
    }

    /// Three-way merges `left` and `right` into a new branch `into`.
    pub fn merge(
        &self,
        left: &str,
        right: &str,
        into: &str,
        message: Option<&str>,
    ) -> Result<MergeOutcome> {
        let stack = self.stack()?;
        let val = match branch::merge(&self.store, &stack, left, right, into, message, &[]) {
            Ok(val) => val,
            Err(refusal) if refusal.code == "Conflict" => {
                let conflicts = match refusal.detail.get("conflicts") {
                    Some(SdkValue::List(list)) => list
                        .iter()
                        .filter_map(|c| c.get_text("path").map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                };
                return Ok(MergeOutcome {
                    kind: "Conflict".to_owned(),
                    branch: into.to_owned(),
                    fast_forward: false,
                    head: None,
                    root: None,
                    conflicts,
                });
            }
            Err(refusal) => return Err(Error::from_sdk(refusal)),
        };
        let conflicts = match val.get("conflicts") {
            Some(SdkValue::List(list)) => list
                .iter()
                .filter_map(|c| c.get_text("path").map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        let fast_forward = matches!(val.get("fastForward"), Some(SdkValue::Bool(true)));
        Ok(MergeOutcome {
            kind: val.get_text("kind").unwrap_or("Merged").to_owned(),
            branch: val.get_text("branch").unwrap_or(into).to_owned(),
            fast_forward,
            head: val.get_text("head").map(str::to_owned),
            root: val.get_text("root").map(str::to_owned),
            conflicts,
        })
    }

    /// Publishes a branch's head (or a specific commit) onto `main` as a new Layer.
    pub fn publish(&self, branch_name: &str, commit: Option<&str>) -> Result<PublishOutcome> {
        let stack = self.stack()?;
        let record = lookup::branch(&self.store, &stack, branch_name).map_err(Error::from_sdk)?;
        let node = match commit {
            Some(hex) => {
                let bytes = decode_hex_vec(hex)?;
                Some(
                    NodeId::from_slice(&bytes)
                        .map_err(|e| Error::InvalidInput(format!("invalid commit id: {e:?}")))?,
                )
            }
            None => None,
        };
        let val = branch::publish(&self.store, &stack, &record, node).map_err(Error::from_sdk)?;
        let kind = val.get_text("kind").unwrap_or("Unknown").to_owned();
        let layer = val
            .get("publishedLayer")
            .and_then(|v| v.get_text("id"))
            .or_else(|| val.get_text("layer"))
            .or_else(|| val.get_text("headLayer"))
            .unwrap_or_default()
            .to_owned();
        let overwrote_count = match val.get("overwroteCount") {
            Some(SdkValue::Int(n)) => *n,
            _ => 0,
        };
        Ok(PublishOutcome {
            kind,
            layer,
            overwrote_count,
        })
    }

    /// Lists commits on a branch, newest first.
    pub fn log(&self, branch_name: &str, limit: usize) -> Result<Vec<CommitInfo>> {
        let stack = self.stack()?;
        let record = lookup::branch(&self.store, &stack, branch_name).map_err(Error::from_sdk)?;
        let val = branch::list_commits(&self.store, &record, limit, 0).map_err(Error::from_sdk)?;
        let Some(SdkValue::List(items)) = val.get("items") else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let created_ms = match item.get("createdMs") {
                Some(SdkValue::Int(ms)) => Some(*ms),
                _ => None,
            };
            out.push(CommitInfo {
                id: item.get_text("commit").unwrap_or_default().to_owned(),
                root: item.get_text("root").unwrap_or_default().to_owned(),
                message: item.get_text("message").map(str::to_owned),
                created_ms,
                branch: item.get_text("branch").map(str::to_owned),
            });
        }
        Ok(out)
    }

    /// Diffs two branches (or `"main"`).
    pub fn diff_branches(&self, left: &str, right: &str) -> Result<Vec<DiffEntry>> {
        let stack = self.stack()?;
        let reader = snapshot::reader(&self.store, &stack);
        let left_snap = snapshot::resolve(&self.store, &stack, &reader, left).map_err(Error::from_sdk)?;
        let right_snap = snapshot::resolve(&self.store, &stack, &reader, right).map_err(Error::from_sdk)?;
        let req = compare::Request {
            limit: 4096,
            pos: 0,
            cursor: None,
            hunks: false,
        };
        let val = compare::diff_branches(
            &self.store,
            &reader,
            (&left_snap, left),
            (&right_snap, right),
            Ok(req),
        )
        .map_err(Error::from_sdk)?;
        let Some(SdkValue::List(items)) = val.get("items") else {
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .map(|item| DiffEntry {
                path: item.get_text("path").unwrap_or_default().to_owned(),
                kind: item.get_text("kind").unwrap_or_default().to_owned(),
            })
            .collect())
    }

    /// Repository and store statistics.
    pub fn stats(&self) -> Result<StoreStats> {
        let stack = self.stack()?;
        let branches = self.branches()?.len();
        let store_file_bytes = std::fs::metadata(self.dir.join("store.sqlite"))
            .map(|m| m.len())
            .unwrap_or(0);
        let stats = self
            .store
            .history()
            .statistics()
            .map_err(|e| Error::Other(format!("store statistics: {e:?}")))?;
        Ok(StoreStats {
            profile: self.meta.profile,
            volume_size_bytes: self.meta.volume_size_bytes,
            store_file_bytes,
            branches,
            head_layer: encode_hex(stack.head_layer.as_slice()),
            logical_bytes: stats.logical,
            canonical_bytes: stats.canonical,
            stored_bytes: stats.stored,
        })
    }

    /// Binds a writable workspace onto `branch_name` (ready to be mounted via WinFsp
    /// or mutated programmatically).
    pub fn open_workspace(&self, branch_name: &str) -> Result<WorkspaceHandle> {
        if branch_name == branch::MAIN {
            return Err(Error::InvalidInput(format!(
                "\"{}\" is the published layer line; fork or use a writable branch (e.g. \"{}\")",
                branch::MAIN,
                DEFAULT_BRANCH
            )));
        }
        let stack = self.stack()?;
        let record = lookup::branch(&self.store, &stack, branch_name).map_err(Error::from_sdk)?;
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| Error::Other("overlay owner is closed".to_owned()))?
            .client();
        let ws_seed = blake3::hash(
            format!(
                "ws:{}:{}:{}",
                branch_name,
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
            .as_bytes(),
        );
        let workspace_id = WorkspaceId::from_authority(*ws_seed.as_bytes())
            .map_err(|e| Error::Other(format!("workspace id: {e:?}")))?;
        let bound = self
            .store
            .bind(
                owner.clone(),
                BindRequest {
                    workspace: workspace_id,
                    branch: record.id,
                },
            )
            .map_err(|e| Error::Other(format!("bind workspace on {branch_name}: {e}")))?;

        let gate = Arc::new(RwLock::new(()));
        let workspace = Arc::new(bound.workspace);
        let backend = WinFspBackend::new(Arc::clone(&workspace), Arc::clone(&gate));
        Ok(WorkspaceHandle {
            branch_name: branch_name.to_owned(),
            store: Arc::clone(&self.store),
            workspace,
            owner,
            gate,
            backend,
            logical_cache: Mutex::new(None),
        })
    }
}

/// An active mounted/open writable workspace bound to a branch.
pub struct WorkspaceHandle {
    branch_name: String,
    store: Arc<Store>,
    workspace: Arc<BoundWorkspace>,
    owner: layerfs_store_service::OwnerClient,
    gate: Arc<RwLock<()>>,
    backend: WinFspBackend,
    logical_cache: Mutex<Option<(Instant, u64)>>,
}

impl std::fmt::Debug for WorkspaceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceHandle")
            .field("branch", &self.branch_name)
            .finish()
    }
}

impl WorkspaceHandle {
    /// Branch name this workspace is bound to.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch_name
    }

    /// Returns a cloneable WinFsp `WorkspaceOps` backend for mounting.
    #[must_use]
    pub fn backend(&self) -> WinFspBackend {
        self.backend.clone()
    }

    /// Current workspace status (head, root, dirty counts).
    pub fn status(&self) -> Result<WorkspaceStatus> {
        let snap = self
            .workspace
            .snapshot()
            .map_err(|e| Error::Other(format!("workspace snapshot: {e}")))?;
        let state = self
            .owner
            .state(self.workspace.route())
            .map_err(|e| Error::Other(format!("overlay state: {e}")))?;
        let dirty = state.dirty_inodes > 0 || state.dirty_directory_entries > 0;
        Ok(WorkspaceStatus {
            branch: self.branch_name.clone(),
            base_layer: encode_hex(snap.branch.base_layer.as_slice()),
            head_commit: snap.branch.head_commit.map(|c| encode_hex(c.as_slice())),
            effective_root: encode_hex(snap.effective_root.as_bytes()),
            dirty_inodes: state.dirty_inodes,
            dirty_entries: state.dirty_directory_entries,
            dirty,
        })
    }

    /// Commits all uncommitted overlay changes incrementally into the Store
    /// (with automatic 3-way merge if the branch head moved, unless `force`).
    pub fn commit(&self, message: Option<&str>, force: bool) -> Result<CommitSummary> {
        let _write_gate = self
            .gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Release the persistent cached `SourceView` before capturing/installing a new base root.
        self.backend.release_cached_view();
        let options = CommitOptions {
            force,
            message: message.map(str::to_owned),
        };
        let success = self.workspace.commit_captured(&options).map_err(|failure| {
            let msg = failure.to_string();
            if msg.contains("Busy") || msg.contains("InFlight") {
                Error::Busy(msg)
            } else if msg.contains("Markers") || msg.contains("Conflict") {
                Error::Conflict(msg)
            } else {
                Error::Other(msg)
            }
        })?;
        self.backend.invalidate_cache();
        *self
            .logical_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        let conflicts: Vec<String> = success
            .merge
            .as_ref()
            .map(|m| m.conflicts.iter().map(|c| c.path.clone()).collect())
            .unwrap_or_default();

        match success.history {
            Some(CommitOutcome::Committed { record, .. }) => Ok(CommitSummary {
                kind: "Committed".to_owned(),
                commit: Some(encode_hex(record.id.as_slice())),
                root: encode_hex(record.root.as_bytes()),
                conflicts,
            }),
            Some(CommitOutcome::UpToDate { head, root }) => Err(Error::Conflict(format!(
                "nothing to commit (up to date at root {}, head {})",
                encode_hex(root.as_bytes()),
                head.map_or_else(|| "none".to_owned(), |h| encode_hex(h.as_slice()))
            ))),
            None => Err(Error::Conflict(format!(
                "merge conflict in {} file(s): {}",
                conflicts.len(),
                conflicts.join(", ")
            ))),
        }
    }

    /// Approximate logical bytes currently used (cached for 250 ms for Explorer `GetVolumeInfo`).
    pub fn logical_bytes_cached(&self) -> u64 {
        const TTL: Duration = Duration::from_millis(250);
        let mut cache = self
            .logical_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, bytes)) = *cache {
            if at.elapsed() < TTL {
                return bytes;
            }
        }
        let bytes = self
            .store
            .history()
            .statistics()
            .map(|s| s.logical)
            .unwrap_or(0);
        *cache = Some((Instant::now(), bytes));
        bytes
    }

    /// Cleanly closes the workspace and retires its overlay namespace (releasing files/locks).
    pub fn close(self) -> Result<()> {
        self.backend.release_cached_view();
        let route = self.workspace.route();
        self.workspace
            .close()
            .map_err(|e| Error::Other(format!("close workspace: {e:?}")))?;
        let _ = self.owner.retire_namespace(route.namespace());
        Ok(())
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn decode_hex_vec(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        return Err(Error::InvalidInput("odd-length hex string".to_owned()));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for i in 0..text.len() / 2 {
        let byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::InvalidInput("invalid hex character".to_owned()))?;
        out.push(byte);
    }
    Ok(out)
}

fn decode_hex_array<const N: usize>(text: &str) -> Result<[u8; N]> {
    if text.len() != N * 2 {
        return Err(Error::InvalidInput(format!(
            "expected {} hex chars, got {}",
            N * 2,
            text.len()
        )));
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::InvalidInput(format!("invalid hex at byte {i}")))?;
    }
    Ok(out)
}

fn resolve_future_path(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut missing = Vec::new();
    let mut cursor = abs.as_path();
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            return abs;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return abs;
        };
        cursor = parent;
    }
    let mut resolved = std::fs::canonicalize(cursor).unwrap_or_else(|_| cursor.to_path_buf());
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    resolved
}

fn path_starts_with_ci(candidate: &Path, ancestor: &Path) -> bool {
    let norm = |p: &Path| -> PathBuf {
        let s = p.to_string_lossy();
        let trimmed = s.strip_prefix("\\\\?\\").unwrap_or(&s);
        #[cfg(windows)]
        {
            PathBuf::from(trimmed.to_ascii_lowercase())
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(trimmed)
        }
    };
    norm(candidate).starts_with(norm(ancestor))
}

#[cfg(test)]
mod tests {
    use super::{parse_import_source, ImportSource};
    use std::path::PathBuf;

    #[test]
    fn parse_import_source_handles_windows_paths_and_branch_suffixes() {
        assert_eq!(
            parse_import_source(r"F:\work\proj").unwrap(),
            ImportSource {
                path: PathBuf::from(r"F:\work\proj"),
                branch: None,
            }
        );
        assert_eq!(
            parse_import_source(r"F:\work\proj:feat-a").unwrap(),
            ImportSource {
                path: PathBuf::from(r"F:\work\proj"),
                branch: Some("feat-a".to_owned()),
            }
        );
        assert_eq!(
            parse_import_source(r"\\?\C:\dir\sub").unwrap(),
            ImportSource {
                path: PathBuf::from(r"\\?\C:\dir\sub"),
                branch: None,
            }
        );
        assert_eq!(
            parse_import_source(r"\\?\C:\dir\sub:v2").unwrap(),
            ImportSource {
                path: PathBuf::from(r"\\?\C:\dir\sub"),
                branch: Some("v2".to_owned()),
            }
        );
        assert_eq!(
            parse_import_source(r".\local\app:custom").unwrap(),
            ImportSource {
                path: PathBuf::from(r".\local\app"),
                branch: Some("custom".to_owned()),
            }
        );
        assert_eq!(
            parse_import_source("proj").unwrap(),
            ImportSource {
                path: PathBuf::from("proj"),
                branch: None,
            }
        );
        assert_eq!(
            parse_import_source("proj:dev").unwrap(),
            ImportSource {
                path: PathBuf::from("proj"),
                branch: Some("dev".to_owned()),
            }
        );
        assert!(parse_import_source("").is_err());
        assert!(parse_import_source(r"F:\work\proj:").is_err());
        assert!(parse_import_source(":only-branch").is_err());
    }

    #[test]
    fn parse_cache_size_validates_units_and_minimum() {
        use super::{parse_cache_size, MIN_STORE_CACHE_BYTES};
        assert_eq!(parse_cache_size("64M").unwrap(), 64 * 1024 * 1024);
        assert_eq!(parse_cache_size("256MiB").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_cache_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_cache_size("4096K").unwrap(), MIN_STORE_CACHE_BYTES);
        assert!(parse_cache_size("1M").is_err());
        assert!(parse_cache_size("").is_err());
        assert!(parse_cache_size("128X").is_err());
    }
}
