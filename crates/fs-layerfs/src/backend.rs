//! `fs_winfsp::WorkspaceOps` implemented directly over upstream LayerFS
//! (`layerfs_store_service::store::BoundWorkspace` + `layerfs_workspace`).
//!
//! Design properties:
//! - **Zero core format changes**: the upstream POSIX/CAS format stays 100%
//!   compatible with Linux/macOS LayerFS stores.
//! - **O(1) Windows case-insensitive lookup with exact case preservation**:
//!   each touched directory keeps an in-memory `folded_key -> (stored_name, serial)`
//!   map, lazily populated from `SourceView::list` and maintained on mutations
//!   (invalidated on commit/base switch).
//! - **No 4096 truncation**: `read_dir` paginates `SourceView::list` to exhaustion.
//! - **Windowed I/O**: `read`/`write` slice cleanly to `READ_WINDOW`/`WRITE_WINDOW`.
//! - **Publication gate**: an `RwLock` keeps live `commit_captured` mutually
//!   exclusive with in-flight data mutations so no write is lost across capture/install.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use fs_semantics::attributes::report as report_attributes;
use fs_semantics::disposition::{decide, Action, Disposition, Exists, Failure, Outcome as DispOutcome};
use fs_semantics::name::fold_bytes;

/// `FILE_ATTRIBUTE_READONLY` (mapped to POSIX write-bit absence `mode & 0o222 == 0`).
const READONLY: u32 = 0x01;
use fs_winfsp::ops::{
    Attributes, DirEntry, DirOutcome, Kind, NodeInfo, OpenOutcome, OpenTarget, OpsError, OpsResult,
    Path, RenameOutcome, Times, WorkspaceOps,
};
use layerfs_content::filesystem::PathName;
use layerfs_content::object::inode_leaf::InodeKind;
use layerfs_content::ContentError;
use layerfs_overlay::{READ_WINDOW, WRITE_WINDOW};
use layerfs_store_service::store::{BoundWorkspace, StoreOperation};
use layerfs_workspace::{
    Operation, Outcome, OverlayJobs, Position, Refusal, SourceView, Time, ViewStat, WorkspaceError,
};

const DEFAULT_FILE_MODE: u32 = 0o644;
const DEFAULT_DIR_MODE: u32 = 0o755;
const SERIAL_POOL_BATCH: u64 = 256;

/// Folded child name -> `(exact_stored_name, child_serial)`.
type DirTable = HashMap<Vec<u8>, (Vec<u8>, u64)>;

/// Concurrent-reader directory lookup cache protected by `RwLock`.
/// `mut_gen` increments on every namespace mutation so a concurrent lock-free directory
/// scan never publishes a stale snapshot into `dirs`.
#[derive(Default)]
struct DirCache {
    dirs: HashMap<u64, Arc<DirTable>>,
    mut_gen: u64,
}

/// Persistent retained `StoreOperation` + `SourceView` shared across all concurrent WinFsp
/// callbacks until `commit()` or `close()` explicitly releases it (eliminating 2 overlay SQLite
/// write transactions per read/stat/write callback).
struct CachedView {
    route: layerfs_workspace::Route,
    op: StoreOperation,
    view: SourceView,
}

impl Drop for CachedView {
    fn drop(&mut self) {
        let _ = self
            .op
            .overlay()
            .release_base_source(self.route, self.view.source());
    }
}

/// WinFsp `WorkspaceOps` backend backed by an upstream LayerFS `BoundWorkspace`.
///
/// Read operations (`read`, `read_into`, `get_info`, `lookup_info`, `open_entry(Open)`,
/// `open_dir`, `dir_binding`, `create_or_open(Open)`, `read_dir`, `read_dir_with_info`,
/// `is_dir_empty`) run in parallel across all WinFsp dispatcher threads using a persistent
/// cached `SourceView` without holding any global write mutex during I/O.
pub struct WinFspBackend {
    workspace: Arc<BoundWorkspace>,
    gate: Arc<RwLock<()>>,
    /// Persistent `StoreOperation` + `SourceView` released only on `commit()` / `close()`.
    cached_view: Arc<RwLock<Option<CachedView>>>,
    /// Pre-reserved inode serial range `[next, end)` from the store's allocator.
    serial_pool: Arc<Mutex<(u64, u64)>>,
    /// Concurrent-read case-folded directory lookup cache.
    dir_cache: Arc<RwLock<DirCache>>,
    /// Serializes mutating operations so namespace & overlay updates stay consistent.
    write_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for WinFspBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinFspBackend")
            .field("workspace", &self.workspace.identity())
            .finish()
    }
}

impl Clone for WinFspBackend {
    fn clone(&self) -> Self {
        Self {
            workspace: Arc::clone(&self.workspace),
            gate: Arc::clone(&self.gate),
            cached_view: Arc::clone(&self.cached_view),
            serial_pool: Arc::clone(&self.serial_pool),
            dir_cache: Arc::clone(&self.dir_cache),
            write_lock: Arc::clone(&self.write_lock),
        }
    }
}

impl WinFspBackend {
    /// Creates a WinFsp backend over `workspace`, sharing `gate` with `commit`.
    #[must_use]
    pub fn new(workspace: Arc<BoundWorkspace>, gate: Arc<RwLock<()>>) -> Self {
        Self {
            workspace,
            gate,
            cached_view: Arc::new(RwLock::new(None)),
            serial_pool: Arc::new(Mutex::new((0, 0))),
            dir_cache: Arc::new(RwLock::new(DirCache::default())),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Releases the cached `BaseSource` / `SourceView` (must be called under `gate.write()` before
    /// `commit_captured` or `workspace.close()` so overlay base transitions see zero retained sources).
    pub fn release_cached_view(&self) {
        let mut slot = self
            .cached_view
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = slot.take();
    }

    /// Drops cached directory tables (called after `commit` or base-root switch).
    pub fn invalidate_cache(&self) {
        self.release_cached_view();
        let mut cache = self
            .dir_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.mut_gen = cache.mut_gen.wrapping_add(1);
        cache.dirs.clear();
    }

    fn acquire_cached_view(&self) -> OpsResult<CachedView> {
        let route = self.workspace.route();
        let op = self.workspace.operation().map_err(map_workspace_err)?;
        let source = op
            .overlay()
            .acquire_diagnostic_source(route)
            .map_err(|e| OpsError::Other(e.to_string()))?;
        match op.workspace().view_for_source(source) {
            Ok(view) => Ok(CachedView { route, op, view }),
            Err(err) => {
                let _ = op.overlay().release_base_source(route, source);
                Err(map_workspace_err(err))
            }
        }
    }

    /// Runs `body` against the persistent cached `(StoreOperation, SourceView)`, lazily
    /// acquiring it once under `cached_view.write()` on cold start or after `commit()`.
    fn run_with_cached_view<T>(
        &self,
        body: impl FnOnce(&StoreOperation, &SourceView) -> OpsResult<T>,
    ) -> OpsResult<T> {
        {
            let guard = self
                .cached_view
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ref cv) = *guard {
                return body(&cv.op, &cv.view);
            }
        }
        {
            let mut slot = self
                .cached_view
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(self.acquire_cached_view()?);
            }
        }
        let guard = self
            .cached_view
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cv = guard
            .as_ref()
            .ok_or_else(|| OpsError::Other("cached view cleared unexpectedly".to_owned()))?;
        body(&cv.op, &cv.view)
    }

    /// Runs a **concurrent read-only** closure against the persistent cached `SourceView`
    /// WITHOUT acquiring `write_lock`, so multiple WinFsp dispatcher threads run I/O in parallel
    /// with zero per-call overlay source acquisition overhead.
    fn with_read_view<T>(
        &self,
        body: impl FnOnce(&StoreOperation, &SourceView) -> OpsResult<T>,
    ) -> OpsResult<T> {
        let _read_gate = self
            .gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.run_with_cached_view(body)
    }

    /// Runs a **mutating** closure serialized by `write_lock` under `gate.read()`, reusing the
    /// persistent cached `SourceView`.
    fn with_write_view<T>(
        &self,
        body: impl FnOnce(&StoreOperation, &SourceView) -> OpsResult<T>,
    ) -> OpsResult<T> {
        let _read_gate = self
            .gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _write_guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.run_with_cached_view(body)
    }

    fn next_serial(&self, op: &StoreOperation) -> OpsResult<u64> {
        let mut pool = self
            .serial_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pool.0 == pool.1 {
            let (start, count) = op
                .ports()
                .reserve_pooled(SERIAL_POOL_BATCH)
                .map_err(map_workspace_err)?;
            *pool = (start, start.saturating_add(count));
        }
        let serial = pool.0;
        pool.0 += 1;
        Ok(serial)
    }

    fn apply_op(
        &self,
        op: &StoreOperation,
        view: &SourceView,
        operation: Operation,
        now: Time,
    ) -> OpsResult<Option<ViewStat>> {
        let fresh = if operation.creates() {
            Some(self.next_serial(op)?)
        } else {
            None
        };
        let mut plan = op
            .workspace()
            .prepare_mutation(view, operation, now, fresh)
            .map_err(|e| map_workspace_err(e.error))?;
        loop {
            let job = plan
                .job()
                .ok_or_else(|| OpsError::Other("mutation plan missing job".to_owned()))?;
            let result = OverlayJobs::namespace(op.overlay(), job);
            match plan.accept(result).map_err(map_workspace_err)? {
                Some(Outcome::Applied { publication, stat }) => {
                    op.overlay()
                        .reply_attempted(publication)
                        .map_err(|e| OpsError::Other(e.to_string()))?;
                    return Ok(stat);
                }
                Some(Outcome::Unchanged { stat }) => return Ok(stat),
                None => plan.supply(view).map_err(map_workspace_err)?,
            }
        }
    }

    /// Returns a snapshot of `dir_serial`'s folded lookup table without holding any lock during I/O.
    fn get_dir_table(
        &self,
        op: &StoreOperation,
        view: &SourceView,
        dir_serial: u64,
    ) -> OpsResult<Arc<DirTable>> {
        let start_gen = {
            let cache = self
                .dir_cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(hit) = cache.dirs.get(&dir_serial) {
                return Ok(Arc::clone(hit));
            }
            cache.mut_gen
        };
        // Lock-free directory scan on cache miss.
        let stat = view.stat(op.overlay(), dir_serial).map_err(map_workspace_err)?;
        if stat.kind != InodeKind::Directory {
            return Err(OpsError::NotADirectory);
        }
        let mut map = DirTable::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = view
                .list(op.overlay(), dir_serial, after.as_deref())
                .map_err(map_workspace_err)?;
            for (name, serial) in page.entries {
                map.insert(fold_bytes(&name), (name, serial));
            }
            match page.continuation {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let arc = Arc::new(map);
        {
            let mut cache = self
                .dir_cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache.mut_gen == start_gen {
                cache.dirs.entry(dir_serial).or_insert_with(|| Arc::clone(&arc));
            }
        }
        Ok(arc)
    }

    fn dir_cache_insert(&self, dir_serial: u64, folded: Vec<u8>, stored: Vec<u8>, child: u64) {
        let mut cache = self
            .dir_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.mut_gen = cache.mut_gen.wrapping_add(1);
        if let Some(arc) = cache.dirs.get_mut(&dir_serial) {
            Arc::make_mut(arc).insert(folded, (stored, child));
        }
    }

    fn dir_cache_remove_entry(&self, dir_serial: u64, folded: &[u8]) {
        let mut cache = self
            .dir_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.mut_gen = cache.mut_gen.wrapping_add(1);
        if let Some(arc) = cache.dirs.get_mut(&dir_serial) {
            Arc::make_mut(arc).remove(folded);
        }
    }

    fn dir_cache_insert_empty_dir(&self, dir_serial: u64) {
        let mut cache = self
            .dir_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.mut_gen = cache.mut_gen.wrapping_add(1);
        cache.dirs.insert(dir_serial, Arc::new(HashMap::new()));
    }

    fn dir_cache_remove_dir(&self, dir_serial: u64) {
        let mut cache = self
            .dir_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.mut_gen = cache.mut_gen.wrapping_add(1);
        cache.dirs.remove(&dir_serial);
    }

    /// Resolves a directory path (all components) using case-folded lookup,
    /// returning `(dir_serial, canonical_path_names_from_root)`.
    fn resolve_dir_chain(
        &self,
        op: &StoreOperation,
        view: &SourceView,
        components: &[Vec<u8>],
    ) -> OpsResult<(u64, Vec<PathName>)> {
        let mut current = view.root_serial();
        let mut chain = Vec::with_capacity(components.len());
        for comp in components {
            let folded = fold_bytes(comp);
            let found = self
                .get_dir_table(op, view, current)?
                .get(&folded)
                .cloned();
            let Some((stored, child)) = found else {
                return Err(OpsError::NotFound);
            };
            let stat = view.stat(op.overlay(), child).map_err(map_workspace_err)?;
            if stat.kind != InodeKind::Directory {
                return Err(OpsError::NotADirectory);
            }
            chain.push(to_path_name(&stored)?);
            current = child;
        }
        Ok((current, chain))
    }

    /// Resolves an arbitrary path (file or directory), returning
    /// `(parent_serial, stored_name_opt, target_serial, target_stat)`.
    fn resolve_entry(
        &self,
        op: &StoreOperation,
        view: &SourceView,
        path: &Path<'_>,
    ) -> OpsResult<(u64, Option<Vec<u8>>, u64, ViewStat)> {
        let segments = parse_segments(path)?;
        if segments.is_empty() {
            let root = view.root_serial();
            let stat = view.stat(op.overlay(), root).map_err(map_workspace_err)?;
            return Ok((root, None, root, stat));
        }
        let (parent, _) = self.resolve_dir_chain(op, view, &segments[..segments.len() - 1])?;
        let leaf = &segments[segments.len() - 1];
        let folded = fold_bytes(leaf);
        let found = self
            .get_dir_table(op, view, parent)?
            .get(&folded)
            .cloned();
        let Some((stored, serial)) = found else {
            return Err(OpsError::NotFound);
        };
        let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
        Ok((parent, Some(stored), serial, stat))
    }
}

fn now_time() -> Time {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Time {
        seconds: elapsed.as_secs() as i64,
        nanoseconds: elapsed.subsec_nanos(),
    }
}

fn time_to_unix_ns(seconds: i64, nanoseconds: u32) -> i64 {
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(nanoseconds))
}

fn unix_ns_to_time(ns: i64) -> Time {
    let seconds = ns.div_euclid(1_000_000_000);
    let nanoseconds = ns.rem_euclid(1_000_000_000) as u32;
    Time {
        seconds,
        nanoseconds,
    }
}

fn kind_of(kind: InodeKind) -> Kind {
    match kind {
        InodeKind::Directory => Kind::Directory,
        InodeKind::RegularFile | InodeKind::Symlink => Kind::File,
    }
}

/// Maps `fs_winfsp::ROOT` (0) <-> upstream LayerFS `view.root_serial()` (typically 1).
#[inline]
fn to_serial(view: &SourceView, file_id: u64) -> u64 {
    if file_id == fs_winfsp::ROOT {
        view.root_serial()
    } else {
        file_id
    }
}

#[inline]
fn from_serial(view: &SourceView, serial: u64) -> u64 {
    if serial == view.root_serial() {
        fs_winfsp::ROOT
    } else {
        serial
    }
}

fn node_info_of(view: &SourceView, stat: &ViewStat) -> NodeInfo {
    let is_dir = stat.kind == InodeKind::Directory;
    let is_symlink = stat.kind == InodeKind::Symlink;
    let stored_bits = if is_symlink || (!is_dir && (stat.metadata.mode & 0o222) == 0) {
        READONLY
    } else {
        0
    };
    let ns = time_to_unix_ns(stat.metadata.mtime_seconds, stat.metadata.mtime_nanoseconds);
    NodeInfo {
        file_id: from_serial(view, stat.serial),
        kind: kind_of(stat.kind),
        size: if is_dir { 0 } else { stat.logical_len },
        times: Times {
            created: ns,
            accessed: ns,
            written: ns,
            changed: ns,
        },
        attributes: Attributes(report_attributes(is_dir, stored_bits)),
    }
}

fn map_workspace_err(err: WorkspaceError) -> OpsError {
    match err {
        WorkspaceError::Refused(refusal) => match refusal {
            Refusal::Exists => OpsError::NameCollision,
            Refusal::Missing => OpsError::NotFound,
            Refusal::NotDirectory => OpsError::NotADirectory,
            Refusal::IsDirectory => OpsError::IsADirectory,
            Refusal::NotEmpty => OpsError::NotEmpty,
            Refusal::NotPermitted => OpsError::AccessDenied,
            Refusal::Invalid
            | Refusal::Unsupported
            | Refusal::AncestryRequired
            | Refusal::AncestryMismatch => OpsError::InvalidParameter,
            Refusal::TooLarge | Refusal::TooManyLinks => OpsError::OutOfRange,
        },
        WorkspaceError::Content(ContentError::PathNotFound) => OpsError::NotFound,
        WorkspaceError::Content(ContentError::WrongLogicalRole) => OpsError::NotADirectory,
        WorkspaceError::Content(ContentError::InvalidPath | ContentError::PathLimitExceeded) => {
            OpsError::InvalidParameter
        }
        other => OpsError::Other(other.to_string()),
    }
}

fn map_failure(failure: Failure) -> OpsError {
    match failure {
        Failure::LeafMissing | Failure::ParentMissing => OpsError::NotFound,
        Failure::NameCollision => OpsError::NameCollision,
        Failure::NotADirectory => OpsError::NotADirectory,
        Failure::IsADirectory => OpsError::IsADirectory,
        Failure::DirectoryNotEmpty => OpsError::NotEmpty,
        Failure::AccessDenied => OpsError::AccessDenied,
        Failure::InvalidParameter | Failure::InvalidName => OpsError::InvalidParameter,
        Failure::DeletePending => OpsError::DeletePending,
        Failure::SharingViolation => OpsError::SharingViolation,
        other => OpsError::Other(format!("{other:?}")),
    }
}

fn parse_segments(path: &Path<'_>) -> OpsResult<Vec<Vec<u8>>> {
    let raw = path.display();
    let trimmed = raw
        .strip_prefix(b"/")
        .or_else(|| raw.strip_prefix(b"\\"))
        .unwrap_or(raw);
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for part in trimmed.split(|&b| b == b'/' || b == b'\\') {
        if part.is_empty() {
            continue;
        }
        if part == b"." || part == b".." || part.len() > 255 || part.contains(&0) {
            return Err(OpsError::InvalidParameter);
        }
        segments.push(part.to_vec());
    }
    Ok(segments)
}

fn to_path_name(bytes: &[u8]) -> OpsResult<PathName> {
    PathName::from_bytes(bytes).map_err(|_| OpsError::InvalidParameter)
}

impl WorkspaceOps for WinFspBackend {
    fn create_or_open(&self, path: &Path<'_>, disposition: Disposition) -> OpsResult<OpenOutcome> {
        // Fast concurrent read-only path for `Disposition::Open` (no `write_lock` held!).
        if disposition == Disposition::Open {
            return self.with_read_view(|op, view| {
                let segments = parse_segments(path)?;
                if segments.is_empty() {
                    let root = view.root_serial();
                    let stat = view.stat(op.overlay(), root).map_err(map_workspace_err)?;
                    return match decide(disposition, Exists::Directory) {
                        DispOutcome::Opened(action) => {
                            let info = node_info_of(view, &stat);
                            Ok(OpenOutcome {
                                file_id: info.file_id,
                                kind: info.kind,
                                action,
                                size: info.size,
                                times: info.times,
                                attributes: info.attributes,
                                binding: (root, Vec::new()),
                                replaced: None,
                            })
                        }
                        DispOutcome::Failed(f) => Err(map_failure(f)),
                    };
                }
                let (parent, _) =
                    self.resolve_dir_chain(op, view, &segments[..segments.len() - 1])?;
                let leaf = &segments[segments.len() - 1];
                let folded = fold_bytes(leaf);
                let Some((stored, serial)) = self
                    .get_dir_table(op, view, parent)?
                    .get(&folded)
                    .cloned()
                else {
                    return Err(OpsError::NotFound);
                };
                let stat = match view.stat(op.overlay(), serial) {
                    Ok(stat) => stat,
                    Err(WorkspaceError::Content(ContentError::PathNotFound)) => {
                        self.dir_cache_remove_entry(parent, &folded);
                        return Err(OpsError::NotFound);
                    }
                    Err(e) => return Err(map_workspace_err(e)),
                };
                let exists = if stat.kind == InodeKind::Directory {
                    Exists::Directory
                } else {
                    Exists::File
                };
                let action = match decide(disposition, exists) {
                    DispOutcome::Opened(action) => action,
                    DispOutcome::Failed(failure) => return Err(map_failure(failure)),
                };
                let info = node_info_of(view, &stat);
                Ok(OpenOutcome {
                    file_id: info.file_id,
                    kind: info.kind,
                    action,
                    size: info.size,
                    times: info.times,
                    attributes: info.attributes,
                    binding: (from_serial(view, parent), stored),
                    replaced: None,
                })
            });
        }

        self.with_write_view(|op, view| {
            let segments = parse_segments(path)?;
            if segments.is_empty() {
                let root = view.root_serial();
                let stat = view.stat(op.overlay(), root).map_err(map_workspace_err)?;
                return match decide(disposition, Exists::Directory) {
                    DispOutcome::Opened(action) => {
                        let info = node_info_of(view, &stat);
                        Ok(OpenOutcome {
                            file_id: info.file_id,
                            kind: info.kind,
                            action,
                            size: info.size,
                            times: info.times,
                            attributes: info.attributes,
                            binding: (root, Vec::new()),
                            replaced: None,
                        })
                    }
                    DispOutcome::Failed(f) => Err(map_failure(f)),
                };
            }

            let (parent, _) = self.resolve_dir_chain(op, view, &segments[..segments.len() - 1])?;
            let leaf = &segments[segments.len() - 1];
            let folded = fold_bytes(leaf);
            let existing = self
                .get_dir_table(op, view, parent)?
                .get(&folded)
                .cloned();
            let (exists, existing_stat) = match &existing {
                None => (Exists::Absent, None),
                Some((_, serial)) => match view.stat(op.overlay(), *serial) {
                    Ok(stat) => {
                        let ex = if stat.kind == InodeKind::Directory {
                            Exists::Directory
                        } else {
                            Exists::File
                        };
                        (ex, Some(stat))
                    }
                    Err(WorkspaceError::Content(ContentError::PathNotFound)) => {
                        self.dir_cache_remove_entry(parent, &folded);
                        (Exists::Absent, None)
                    }
                    Err(e) => return Err(map_workspace_err(e)),
                },
            };

            let action = match decide(disposition, exists) {
                DispOutcome::Opened(action) => action,
                DispOutcome::Failed(failure) => return Err(map_failure(failure)),
            };

            let now = now_time();
            let (final_stat, stored_name) = match action {
                Action::Open => {
                    let (stored, _) = existing.expect("Open requires existing");
                    let stat = existing_stat.expect("Open requires existing stat");
                    (stat, stored)
                }
                Action::CreateNew => {
                    let name = to_path_name(leaf)?;
                    let stat = self
                        .apply_op(
                            op,
                            view,
                            Operation::Create {
                                parent,
                                name,
                                mode: DEFAULT_FILE_MODE,
                            },
                            now,
                        )?
                        .ok_or_else(|| OpsError::Other("Create returned no stat".to_owned()))?;
                    self.dir_cache_insert(parent, folded, leaf.clone(), stat.serial);
                    (stat, leaf.clone())
                }
                Action::TruncateExisting => {
                    let (stored, serial) = existing.expect("TruncateExisting requires existing");
                    let mode = if disposition == Disposition::Supersede {
                        Some(DEFAULT_FILE_MODE)
                    } else {
                        None
                    };
                    let stat = self
                        .apply_op(
                            op,
                            view,
                            Operation::SetAttributes {
                                serial,
                                mode,
                                mtime: Some(now),
                                size: Some(0),
                            },
                            now,
                        )?
                        .ok_or_else(|| OpsError::Other("Truncate returned no stat".to_owned()))?;
                    (stat, stored)
                }
            };

            let info = node_info_of(view, &final_stat);
            Ok(OpenOutcome {
                file_id: info.file_id,
                kind: info.kind,
                action,
                size: info.size,
                times: info.times,
                attributes: info.attributes,
                binding: (from_serial(view, parent), stored_name),
                replaced: None,
            })
        })
    }

    fn mkdir(&self, path: &Path<'_>) -> OpsResult<u64> {
        self.with_write_view(|op, view| {
            let segments = parse_segments(path)?;
            if segments.is_empty() {
                return Err(OpsError::NameCollision);
            }
            let (parent, _) = self.resolve_dir_chain(op, view, &segments[..segments.len() - 1])?;
            let leaf = &segments[segments.len() - 1];
            let folded = fold_bytes(leaf);
            if self.get_dir_table(op, view, parent)?.contains_key(&folded) {
                return Err(OpsError::NameCollision);
            }
            let name = to_path_name(leaf)?;
            let stat = self
                .apply_op(
                    op,
                    view,
                    Operation::Mkdir {
                        parent,
                        name,
                        mode: DEFAULT_DIR_MODE,
                    },
                    now_time(),
                )?
                .ok_or_else(|| OpsError::Other("Mkdir returned no stat".to_owned()))?;
            self.dir_cache_insert(parent, folded, leaf.clone(), stat.serial);
            self.dir_cache_insert_empty_dir(stat.serial);
            Ok(from_serial(view, stat.serial))
        })
    }

    fn open_dir(&self, path: &Path<'_>) -> OpsResult<u64> {
        self.with_read_view(|op, view| {
            let (_, _, serial, stat) = self.resolve_entry(op, view, path)?;
            if stat.kind != InodeKind::Directory {
                return Err(OpsError::NotADirectory);
            }
            Ok(from_serial(view, serial))
        })
    }

    fn dir_binding(&self, path: &Path<'_>) -> Option<DirOutcome> {
        self.with_read_view(|op, view| {
            let (parent, stored, serial, _) = self.resolve_entry(op, view, path)?;
            let name = stored.unwrap_or_default();
            Ok(DirOutcome {
                id: from_serial(view, serial),
                binding: (from_serial(view, parent), name),
            })
        })
        .ok()
    }

    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut out = vec![0u8; len];
        let _ = self.read_into(file_id, offset, &mut out)?;
        // WorkspaceOps::read contract: short reads at EOF are zero-padded to `len`.
        Ok(out)
    }

    fn read_into(&self, file_id: u64, offset: u64, dst: &mut [u8]) -> OpsResult<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        self.with_read_view(|op, view| {
            let serial = to_serial(view, file_id);
            let mut scratch = Vec::with_capacity(dst.len().min(READ_WINDOW));
            let mut pos = offset;
            let mut written = 0usize;
            while written < dst.len() {
                let remaining = dst.len() - written;
                let want = remaining.min(READ_WINDOW) as u32;
                scratch.clear();
                match view.read(op.overlay(), serial, pos, want, &mut scratch) {
                    Ok(_) => {}
                    Err(WorkspaceError::Refused(Refusal::Invalid))
                        if written == 0
                            && view
                                .stat(op.overlay(), serial)
                                .is_ok_and(|s| s.kind == InodeKind::Symlink) =>
                    {
                        let target = view
                            .readlink(op.overlay(), serial)
                            .map_err(map_workspace_err)?;
                        let raw = target.as_bytes();
                        let start = usize::try_from(offset).unwrap_or(raw.len()).min(raw.len());
                        let end = start.saturating_add(dst.len()).min(raw.len());
                        let n = end - start;
                        dst[..n].copy_from_slice(&raw[start..end]);
                        written = n;
                        break;
                    }
                    Err(err) => return Err(map_workspace_err(err)),
                }
                let got = scratch.len().min(remaining);
                if got == 0 {
                    break;
                }
                dst[written..written + got].copy_from_slice(&scratch[..got]);
                written += got;
                pos = pos.saturating_add(got as u64);
                if got < want as usize {
                    break;
                }
            }
            Ok(written)
        })
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.with_write_view(|op, view| {
            let serial = to_serial(view, file_id);
            let now = now_time();
            let mut written = 0usize;
            while written < bytes.len() {
                let end = (written + WRITE_WINDOW).min(bytes.len());
                let chunk = &bytes[written..end];
                self.apply_op(
                    op,
                    view,
                    Operation::Write {
                        serial,
                        position: Position::At(offset.saturating_add(written as u64)),
                        data: chunk.into(),
                    },
                    now,
                )?;
                written = end;
            }
            Ok(written)
        })
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        self.with_write_view(|op, view| {
            let serial = to_serial(view, file_id);
            self.apply_op(
                op,
                view,
                Operation::SetAttributes {
                    serial,
                    mode: None,
                    mtime: Some(now_time()),
                    size: Some(len),
                },
                now_time(),
            )?;
            Ok(())
        })
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let items = self.read_dir_with_info(dir_id)?;
        Ok(items
            .into_iter()
            .map(|(name, info)| DirEntry {
                name,
                kind: info.kind,
                file_id: info.file_id,
            })
            .collect())
    }

    fn read_dir_with_info(&self, dir_id: u64) -> OpsResult<Vec<(Vec<u8>, NodeInfo)>> {
        self.with_read_view(|op, view| {
            let dir_serial = to_serial(view, dir_id);
            let table = self.get_dir_table(op, view, dir_serial)?;
            let mut out = Vec::with_capacity(table.len());
            for (name, serial) in table.values() {
                if let Ok(stat) = view.stat(op.overlay(), *serial) {
                    out.push((name.clone(), node_info_of(view, &stat)));
                }
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(out)
        })
    }

    fn is_dir_empty(&self, dir_id: u64) -> OpsResult<bool> {
        self.with_read_view(|op, view| {
            let dir_serial = to_serial(view, dir_id);
            Ok(self.get_dir_table(op, view, dir_serial)?.is_empty())
        })
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        self.with_read_view(|op, view| {
            let serial = to_serial(view, file_id);
            let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
            Ok(node_info_of(view, &stat))
        })
    }

    fn set_times(
        &self,
        file_id: u64,
        _accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        _created: Option<i64>,
    ) -> OpsResult<()> {
        let mtime = written.or(changed).map(unix_ns_to_time);
        let Some(mtime) = mtime else {
            return Ok(());
        };
        self.with_write_view(|op, view| {
            let serial = to_serial(view, file_id);
            self.apply_op(
                op,
                view,
                Operation::SetAttributes {
                    serial,
                    mode: None,
                    mtime: Some(mtime),
                    size: None,
                },
                now_time(),
            )?;
            Ok(())
        })
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        fs_semantics::attributes::validate(attributes.0)
            .map_err(|_| OpsError::InvalidParameter)?;
        self.with_write_view(|op, view| {
            let serial = to_serial(view, file_id);
            let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
            if matches!(stat.kind, InodeKind::Directory | InodeKind::Symlink) {
                return Ok(());
            }
            let current = stat.metadata.mode & 0o777;
            let new_mode = if attributes.0 & READONLY != 0 {
                current & !0o222
            } else {
                current | 0o200
            };
            if new_mode == current {
                return Ok(());
            }
            self.apply_op(
                op,
                view,
                Operation::SetAttributes {
                    serial,
                    mode: Some(new_mode),
                    mtime: None,
                    size: None,
                },
                now_time(),
            )?;
            Ok(())
        })
    }

    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        self.with_write_view(|op, view| {
            let from_segs = parse_segments(from)?;
            let to_segs = parse_segments(to)?;
            if from_segs.is_empty() || to_segs.is_empty() {
                return Err(OpsError::InvalidParameter);
            }
            let (src_parent, _) =
                self.resolve_dir_chain(op, view, &from_segs[..from_segs.len() - 1])?;
            let (dst_parent, dst_chain) =
                self.resolve_dir_chain(op, view, &to_segs[..to_segs.len() - 1])?;

            let from_leaf = &from_segs[from_segs.len() - 1];
            let to_leaf = &to_segs[to_segs.len() - 1];
            let from_folded = fold_bytes(from_leaf);
            let to_folded = fold_bytes(to_leaf);

            let Some((stored_from, source_serial)) = self
                .get_dir_table(op, view, src_parent)?
                .get(&from_folded)
                .cloned()
            else {
                return Err(OpsError::NotFound);
            };

            let existing_dst = self
                .get_dir_table(op, view, dst_parent)?
                .get(&to_folded)
                .cloned();

            // Same-entry rename (exact or case-only in same parent).
            if src_parent == dst_parent && from_folded == to_folded {
                if stored_from == *to_leaf {
                    return Ok(RenameOutcome {
                        file_id: source_serial,
                        binding: (dst_parent, stored_from),
                        replaced: None,
                    });
                }
                self.apply_op(
                    op,
                    view,
                    Operation::Rename {
                        parent: src_parent,
                        name: to_path_name(&stored_from)?,
                        new_parent: dst_parent,
                        new_name: to_path_name(to_leaf)?,
                        replace: false,
                        destination_path: None,
                    },
                    now_time(),
                )?;
                self.dir_cache_insert(dst_parent, to_folded, to_leaf.clone(), source_serial);
                return Ok(RenameOutcome {
                    file_id: source_serial,
                    binding: (dst_parent, to_leaf.clone()),
                    replaced: None,
                });
            }

            let mut replaced_serial = None;
            if let Some((stored_dst, dst_serial)) = existing_dst {
                if !replace {
                    return Err(OpsError::NameCollision);
                }
                if stored_dst != *to_leaf {
                    let dst_stat = view
                        .stat(op.overlay(), dst_serial)
                        .map_err(map_workspace_err)?;
                    let remove_op = if dst_stat.kind == InodeKind::Directory {
                        Operation::Rmdir {
                            parent: dst_parent,
                            name: to_path_name(&stored_dst)?,
                        }
                    } else {
                        Operation::Unlink {
                            parent: dst_parent,
                            name: to_path_name(&stored_dst)?,
                        }
                    };
                    self.apply_op(op, view, remove_op, now_time())?;
                    if dst_stat.kind == InodeKind::Directory {
                        self.dir_cache_remove_dir(dst_serial);
                    }
                }
                replaced_serial = Some(dst_serial);
            }

            let destination_path = if src_parent != dst_parent {
                Some(dst_chain)
            } else {
                None
            };
            self.apply_op(
                op,
                view,
                Operation::Rename {
                    parent: src_parent,
                    name: to_path_name(&stored_from)?,
                    new_parent: dst_parent,
                    new_name: to_path_name(to_leaf)?,
                    replace,
                    destination_path,
                },
                now_time(),
            )?;

            self.dir_cache_remove_entry(src_parent, &from_folded);
            self.dir_cache_insert(dst_parent, to_folded, to_leaf.clone(), source_serial);
            if let Some(old_id) = replaced_serial {
                self.dir_cache_remove_dir(old_id);
            }

            Ok(RenameOutcome {
                file_id: from_serial(view, source_serial),
                binding: (from_serial(view, dst_parent), to_leaf.clone()),
                replaced: replaced_serial.map(|s| from_serial(view, s)),
            })
        })
    }

    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        self.with_write_view(|op, view| {
            let parent_serial = to_serial(view, parent);
            let expected_serial = to_serial(view, expected_id);
            let folded = fold_bytes(name);
            let Some((stored, serial)) = self
                .get_dir_table(op, view, parent_serial)?
                .get(&folded)
                .cloned()
            else {
                return match view.stat(op.overlay(), expected_serial) {
                    Ok(stat) if stat.namespace_refs == 0 => Ok(()),
                    Err(
                        WorkspaceError::Refused(Refusal::Missing)
                        | WorkspaceError::Content(ContentError::PathNotFound),
                    ) => Ok(()),
                    Ok(_) => Err(OpsError::NotFound),
                    Err(e) => Err(map_workspace_err(e)),
                };
            };
            if serial != expected_serial {
                return Err(OpsError::InvalidRecord(format!(
                    "expected id {expected_id}, found {}",
                    from_serial(view, serial)
                )));
            }
            let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
            let remove_op = if stat.kind == InodeKind::Directory {
                Operation::Rmdir {
                    parent: parent_serial,
                    name: to_path_name(&stored)?,
                }
            } else {
                Operation::Unlink {
                    parent: parent_serial,
                    name: to_path_name(&stored)?,
                }
            };
            self.apply_op(op, view, remove_op, now_time())?;
            self.dir_cache_remove_entry(parent_serial, &folded);
            if stat.kind == InodeKind::Directory {
                self.dir_cache_remove_dir(serial);
            }
            Ok(())
        })
    }

    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }

    fn lookup_info(&self, path: &Path<'_>) -> OpsResult<NodeInfo> {
        self.with_read_view(|op, view| {
            let (_, _, _, stat) = self.resolve_entry(op, view, path)?;
            Ok(node_info_of(view, &stat))
        })
    }

    fn open_entry(
        &self,
        path: &Path<'_>,
        disposition: Disposition,
        target: OpenTarget,
    ) -> OpsResult<OpenOutcome> {
        // Fast concurrent RO path for `Open` and already-existing `OpenIf` (1 path walk, 0 write lock).
        if matches!(disposition, Disposition::Open | Disposition::OpenIf) {
            let ro_res = self.with_read_view(|op, view| match self.resolve_entry(op, view, path) {
                Ok((parent, stored, _, stat)) => {
                    let is_dir = stat.kind == InodeKind::Directory;
                    if target == OpenTarget::FileOnly && is_dir {
                        return Err(OpsError::IsADirectory);
                    }
                    if target == OpenTarget::DirOnly && !is_dir {
                        return Err(OpsError::NotADirectory);
                    }
                    let info = node_info_of(view, &stat);
                    let binding = match stored {
                        Some(name) => (from_serial(view, parent), name),
                        None => (fs_winfsp::ROOT, Vec::new()),
                    };
                    Ok(Some(OpenOutcome {
                        file_id: info.file_id,
                        kind: info.kind,
                        action: Action::Open,
                        size: info.size,
                        times: info.times,
                        attributes: info.attributes,
                        binding,
                        replaced: None,
                    }))
                }
                Err(OpsError::NotFound) if disposition == Disposition::OpenIf => Ok(None),
                Err(err) => Err(err),
            })?;
            if let Some(outcome) = ro_res {
                return Ok(outcome);
            }
        }

        match target {
            OpenTarget::DirOnly => match disposition {
                Disposition::Create | Disposition::OpenIf => {
                    let allow_existing = disposition == Disposition::OpenIf;
                    self.with_write_view(|op, view| {
                        let segments = parse_segments(path)?;
                        if segments.is_empty() {
                            if allow_existing {
                                let root = view.root_serial();
                                let stat =
                                    view.stat(op.overlay(), root).map_err(map_workspace_err)?;
                                let info = node_info_of(view, &stat);
                                return Ok(OpenOutcome {
                                    file_id: info.file_id,
                                    kind: info.kind,
                                    action: Action::Open,
                                    size: info.size,
                                    times: info.times,
                                    attributes: info.attributes,
                                    binding: (fs_winfsp::ROOT, Vec::new()),
                                    replaced: None,
                                });
                            }
                            return Err(OpsError::NameCollision);
                        }
                        let (parent, _) =
                            self.resolve_dir_chain(op, view, &segments[..segments.len() - 1])?;
                        let leaf = &segments[segments.len() - 1];
                        let folded = fold_bytes(leaf);
                        if let Some((stored, serial)) = self
                            .get_dir_table(op, view, parent)?
                            .get(&folded)
                            .cloned()
                        {
                            if allow_existing {
                                let stat =
                                    view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
                                if stat.kind != InodeKind::Directory {
                                    return Err(OpsError::NotADirectory);
                                }
                                let info = node_info_of(view, &stat);
                                return Ok(OpenOutcome {
                                    file_id: info.file_id,
                                    kind: info.kind,
                                    action: Action::Open,
                                    size: info.size,
                                    times: info.times,
                                    attributes: info.attributes,
                                    binding: (from_serial(view, parent), stored),
                                    replaced: None,
                                });
                            }
                            return Err(OpsError::NameCollision);
                        }
                        let name = to_path_name(leaf)?;
                        let stat = self
                            .apply_op(
                                op,
                                view,
                                Operation::Mkdir {
                                    parent,
                                    name,
                                    mode: DEFAULT_DIR_MODE,
                                },
                                now_time(),
                            )?
                            .ok_or_else(|| OpsError::Other("Mkdir returned no stat".to_owned()))?;
                        self.dir_cache_insert(parent, folded, leaf.clone(), stat.serial);
                        self.dir_cache_insert_empty_dir(stat.serial);
                        let info = node_info_of(view, &stat);
                        Ok(OpenOutcome {
                            file_id: info.file_id,
                            kind: info.kind,
                            action: Action::CreateNew,
                            size: info.size,
                            times: info.times,
                            attributes: info.attributes,
                            binding: (from_serial(view, parent), leaf.clone()),
                            replaced: None,
                        })
                    })
                }
                other => match self.create_or_open(path, other) {
                    Ok(out) if out.kind == Kind::Directory => Ok(out),
                    Ok(_) => Err(OpsError::NotADirectory),
                    Err(err) => Err(err),
                },
            },
            OpenTarget::FileOnly => match self.create_or_open(path, disposition) {
                Ok(out) if out.kind == Kind::Directory => Err(OpsError::IsADirectory),
                Ok(out) => Ok(out),
                Err(OpsError::NotADirectory) => Err(OpsError::IsADirectory),
                Err(err) => Err(err),
            },
            OpenTarget::Any => self.create_or_open(path, disposition),
        }
    }
}
