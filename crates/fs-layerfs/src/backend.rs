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
    Attributes, DirEntry, DirOutcome, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, Path,
    RenameOutcome, Times, WorkspaceOps,
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

/// Mutable coordinator state protected by a mutex inside `WinFspBackend`.
#[derive(Default)]
struct BackendState {
    /// Pre-reserved inode serial range `[next, end)` from the store's allocator.
    serial_pool: (u64, u64),
    /// Per-directory case-folded lookup table:
    /// `dir_serial -> { fold_bytes(stored_name) -> (stored_name, child_serial) }`.
    dir_cache: HashMap<u64, DirTable>,
}

/// WinFsp `WorkspaceOps` backend backed by an upstream LayerFS `BoundWorkspace`.
pub struct WinFspBackend {
    workspace: Arc<BoundWorkspace>,
    gate: Arc<RwLock<()>>,
    state: Arc<Mutex<BackendState>>,
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
            state: Arc::clone(&self.state),
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
            state: Arc::new(Mutex::new(BackendState::default())),
        }
    }

    /// Drops cached directory tables (called after `commit` or base-root switch).
    pub fn invalidate_cache(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dir_cache
            .clear();
    }

    /// Runs a closure with a fresh `StoreOperation` + acquired `SourceView`,
    /// releasing the base source before returning.
    fn with_view<T>(
        &self,
        body: impl FnOnce(&StoreOperation, &SourceView, &mut BackendState) -> OpsResult<T>,
    ) -> OpsResult<T> {
        let _read_gate = self
            .gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let op = self.workspace.operation().map_err(map_workspace_err)?;
        let source = op
            .overlay()
            .acquire_diagnostic_source(self.workspace.route())
            .map_err(|e| OpsError::Other(e.to_string()))?;
        let view = op
            .workspace()
            .view_for_source(source)
            .map_err(map_workspace_err);
        let result = match view {
            Ok(view) => {
                let res = body(&op, &view, &mut state);
                let _ = op
                    .overlay()
                    .release_base_source(self.workspace.route(), view.source());
                res
            }
            Err(err) => {
                let _ = op
                    .overlay()
                    .release_base_source(self.workspace.route(), source);
                Err(err)
            }
        };
        result
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
    let stored_bits = if !is_dir && (stat.metadata.mode & 0o222) == 0 {
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

fn next_serial(op: &StoreOperation, state: &mut BackendState) -> OpsResult<u64> {
    if state.serial_pool.0 == state.serial_pool.1 {
        let (start, count) = op
            .ports()
            .reserve_pooled(SERIAL_POOL_BATCH)
            .map_err(map_workspace_err)?;
        state.serial_pool = (start, start.saturating_add(count));
    }
    let serial = state.serial_pool.0;
    state.serial_pool.0 += 1;
    Ok(serial)
}

fn apply_op(
    op: &StoreOperation,
    view: &SourceView,
    state: &mut BackendState,
    operation: Operation,
    now: Time,
) -> OpsResult<Option<ViewStat>> {
    let fresh = if operation.creates() {
        Some(next_serial(op, state)?)
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

/// Ensures `dir_serial`'s folded lookup map is loaded in `state.dir_cache`.
fn ensure_dir_cached<'a>(
    op: &StoreOperation,
    view: &SourceView,
    state: &'a mut BackendState,
    dir_serial: u64,
) -> OpsResult<&'a mut DirTable> {
    if let std::collections::hash_map::Entry::Vacant(entry) = state.dir_cache.entry(dir_serial) {
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
        entry.insert(map);
    }
    Ok(state.dir_cache.get_mut(&dir_serial).expect("just inserted"))
}

/// Resolves a directory path (all components) using case-folded lookup,
/// returning `(dir_serial, canonical_path_names_from_root)`.
fn resolve_dir_chain(
    op: &StoreOperation,
    view: &SourceView,
    state: &mut BackendState,
    components: &[Vec<u8>],
) -> OpsResult<(u64, Vec<PathName>)> {
    let mut current = view.root_serial();
    let mut chain = Vec::with_capacity(components.len());
    for comp in components {
        let folded = fold_bytes(comp);
        let found = ensure_dir_cached(op, view, state, current)?
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
    op: &StoreOperation,
    view: &SourceView,
    state: &mut BackendState,
    path: &Path<'_>,
) -> OpsResult<(u64, Option<Vec<u8>>, u64, ViewStat)> {
    let segments = parse_segments(path)?;
    if segments.is_empty() {
        let root = view.root_serial();
        let stat = view.stat(op.overlay(), root).map_err(map_workspace_err)?;
        return Ok((root, None, root, stat));
    }
    let (parent, _) = resolve_dir_chain(op, view, state, &segments[..segments.len() - 1])?;
    let leaf = &segments[segments.len() - 1];
    let folded = fold_bytes(leaf);
    let found = ensure_dir_cached(op, view, state, parent)?
        .get(&folded)
        .cloned();
    let Some((stored, serial)) = found else {
        return Err(OpsError::NotFound);
    };
    let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
    Ok((parent, Some(stored), serial, stat))
}

impl WorkspaceOps for WinFspBackend {
    fn create_or_open(&self, path: &Path<'_>, disposition: Disposition) -> OpsResult<OpenOutcome> {
        self.with_view(|op, view, state| {
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

            let (parent, _) = resolve_dir_chain(op, view, state, &segments[..segments.len() - 1])?;
            let leaf = &segments[segments.len() - 1];
            let folded = fold_bytes(leaf);
            let existing = ensure_dir_cached(op, view, state, parent)?
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
                        // Stale cache entry: drop it and treat as absent.
                        if let Some(map) = state.dir_cache.get_mut(&parent) {
                            map.remove(&folded);
                        }
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
                    let stat = apply_op(
                        op,
                        view,
                        state,
                        Operation::Create {
                            parent,
                            name,
                            mode: DEFAULT_FILE_MODE,
                        },
                        now,
                    )?
                    .ok_or_else(|| OpsError::Other("Create returned no stat".to_owned()))?;
                    if let Some(map) = state.dir_cache.get_mut(&parent) {
                        map.insert(folded, (leaf.clone(), stat.serial));
                    }
                    (stat, leaf.clone())
                }
                Action::TruncateExisting => {
                    let (stored, serial) = existing.expect("TruncateExisting requires existing");
                    let mode = if disposition == Disposition::Supersede {
                        Some(DEFAULT_FILE_MODE)
                    } else {
                        None
                    };
                    let stat = apply_op(
                        op,
                        view,
                        state,
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
        self.with_view(|op, view, state| {
            let segments = parse_segments(path)?;
            if segments.is_empty() {
                return Err(OpsError::NameCollision);
            }
            let (parent, _) = resolve_dir_chain(op, view, state, &segments[..segments.len() - 1])?;
            let leaf = &segments[segments.len() - 1];
            let folded = fold_bytes(leaf);
            if ensure_dir_cached(op, view, state, parent)?.contains_key(&folded) {
                return Err(OpsError::NameCollision);
            }
            let name = to_path_name(leaf)?;
            let stat = apply_op(
                op,
                view,
                state,
                Operation::Mkdir {
                    parent,
                    name,
                    mode: DEFAULT_DIR_MODE,
                },
                now_time(),
            )?
            .ok_or_else(|| OpsError::Other("Mkdir returned no stat".to_owned()))?;
            if let Some(map) = state.dir_cache.get_mut(&parent) {
                map.insert(folded, (leaf.clone(), stat.serial));
            }
            // Newly created directory starts empty in cache.
            state.dir_cache.insert(stat.serial, HashMap::new());
            Ok(from_serial(view, stat.serial))
        })
    }

    fn open_dir(&self, path: &Path<'_>) -> OpsResult<u64> {
        self.with_view(|op, view, state| {
            let (_, _, serial, stat) = resolve_entry(op, view, state, path)?;
            if stat.kind != InodeKind::Directory {
                return Err(OpsError::NotADirectory);
            }
            Ok(from_serial(view, serial))
        })
    }

    fn dir_binding(&self, path: &Path<'_>) -> Option<DirOutcome> {
        self.with_view(|op, view, state| {
            let (parent, stored, serial, _) = resolve_entry(op, view, state, path)?;
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
        self.with_view(|op, view, _state| {
            let serial = to_serial(view, file_id);
            let mut out = Vec::with_capacity(len.min(READ_WINDOW));
            let mut pos = offset;
            let mut remaining = len;
            while remaining > 0 {
                let want = remaining.min(READ_WINDOW) as u32;
                let before = out.len();
                view.read(op.overlay(), serial, pos, want, &mut out)
                    .map_err(map_workspace_err)?;
                let got = out.len() - before;
                if got == 0 {
                    break;
                }
                pos = pos.saturating_add(got as u64);
                remaining -= got;
                if got < want as usize {
                    break;
                }
            }
            // WorkspaceOps::read contract: short reads at EOF are zero-padded to `len`.
            out.resize(len, 0);
            Ok(out)
        })
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.with_view(|op, view, state| {
            let serial = to_serial(view, file_id);
            let now = now_time();
            let mut written = 0usize;
            while written < bytes.len() {
                let end = (written + WRITE_WINDOW).min(bytes.len());
                let chunk = &bytes[written..end];
                apply_op(
                    op,
                    view,
                    state,
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
        self.with_view(|op, view, state| {
            let serial = to_serial(view, file_id);
            apply_op(
                op,
                view,
                state,
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
        self.with_view(|op, view, state| {
            let dir_serial = to_serial(view, dir_id);
            let entries: Vec<(Vec<u8>, u64)> = ensure_dir_cached(op, view, state, dir_serial)?
                .values()
                .cloned()
                .collect();
            let mut out = Vec::with_capacity(entries.len());
            for (name, serial) in entries {
                if let Ok(stat) = view.stat(op.overlay(), serial) {
                    out.push(DirEntry {
                        name,
                        kind: kind_of(stat.kind),
                        file_id: from_serial(view, serial),
                    });
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        self.with_view(|op, view, _state| {
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
        self.with_view(|op, view, state| {
            let serial = to_serial(view, file_id);
            apply_op(
                op,
                view,
                state,
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
        self.with_view(|op, view, state| {
            let serial = to_serial(view, file_id);
            let stat = view.stat(op.overlay(), serial).map_err(map_workspace_err)?;
            if stat.kind == InodeKind::Directory {
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
            apply_op(
                op,
                view,
                state,
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
        self.with_view(|op, view, state| {
            let from_segs = parse_segments(from)?;
            let to_segs = parse_segments(to)?;
            if from_segs.is_empty() || to_segs.is_empty() {
                return Err(OpsError::InvalidParameter);
            }
            let (src_parent, _) =
                resolve_dir_chain(op, view, state, &from_segs[..from_segs.len() - 1])?;
            let (dst_parent, dst_chain) =
                resolve_dir_chain(op, view, state, &to_segs[..to_segs.len() - 1])?;

            let from_leaf = &from_segs[from_segs.len() - 1];
            let to_leaf = &to_segs[to_segs.len() - 1];
            let from_folded = fold_bytes(from_leaf);
            let to_folded = fold_bytes(to_leaf);

            let Some((stored_from, source_serial)) = ensure_dir_cached(op, view, state, src_parent)?
                .get(&from_folded)
                .cloned()
            else {
                return Err(OpsError::NotFound);
            };

            let existing_dst = ensure_dir_cached(op, view, state, dst_parent)?
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
                // Case-only rename in same directory: upstream is case-sensitive so
                // rename directly from `stored_from` to `to_leaf`!
                apply_op(
                    op,
                    view,
                    state,
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
                if let Some(map) = state.dir_cache.get_mut(&dst_parent) {
                    map.insert(to_folded, (to_leaf.clone(), source_serial));
                }
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
                // If the existing target has a different case spelling than `to_leaf`,
                // unlink/rmdir the differently-cased target first so no ghost entry remains.
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
                    apply_op(op, view, state, remove_op, now_time())?;
                    if dst_stat.kind == InodeKind::Directory {
                        state.dir_cache.remove(&dst_serial);
                    }
                }
                replaced_serial = Some(dst_serial);
            }

            let destination_path = if src_parent != dst_parent {
                Some(dst_chain)
            } else {
                None
            };
            apply_op(
                op,
                view,
                state,
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

            if let Some(map) = state.dir_cache.get_mut(&src_parent) {
                map.remove(&from_folded);
            }
            if let Some(map) = state.dir_cache.get_mut(&dst_parent) {
                map.insert(to_folded, (to_leaf.clone(), source_serial));
            }
            if let Some(old_id) = replaced_serial {
                state.dir_cache.remove(&old_id);
            }

            Ok(RenameOutcome {
                file_id: from_serial(view, source_serial),
                binding: (from_serial(view, dst_parent), to_leaf.clone()),
                replaced: replaced_serial.map(|s| from_serial(view, s)),
            })
        })
    }

    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        self.with_view(|op, view, state| {
            let parent_serial = to_serial(view, parent);
            let expected_serial = to_serial(view, expected_id);
            let folded = fold_bytes(name);
            let Some((stored, serial)) = ensure_dir_cached(op, view, state, parent_serial)?
                .get(&folded)
                .cloned()
            else {
                // Idempotent when `expected_serial` is already retired; `NotFound` when it is still live elsewhere.
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
            apply_op(op, view, state, remove_op, now_time())?;
            if let Some(map) = state.dir_cache.get_mut(&parent_serial) {
                map.remove(&folded);
            }
            if stat.kind == InodeKind::Directory {
                state.dir_cache.remove(&serial);
            }
            Ok(())
        })
    }

    fn flush(&self) -> OpsResult<()> {
        // Each upstream overlay mutation job is already committed to the overlay SQLite file.
        Ok(())
    }
}
