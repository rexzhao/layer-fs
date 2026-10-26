//! The real `WorkspaceOps` backend (P3-3b): the trait P3-3a was written against, implemented over a workspace.
//!
//! It lives in `fs-api` for a dependency reason, not a taste one: `fs-winfsp` defines the trait and must stay free of
//! SQLite, while any implementation needs a workspace. `fs-api` is the only crate that may depend on both, and the arrow
//! points the right way (frontend trait ← api implementation).
//!
//! Two rules this file follows everywhere:
//!
//! - **every content operation goes through a `FileId`**, never a path. `write`/`read`/`set_size`/`get_info` use the
//!   by-id entries added for exactly this reason; a path here would re-introduce the TOCTOU that G1/G2 removed.
//! - **errors are translated once**: `fs_namespace::failure_for` maps a namespace error to the shared failure vocabulary,
//!   and this file only chooses which `OpsError` variant carries it. A second namespace→status table is the defect that
//!   process 206 fixed.

use fs_namespace::{failure_for, FileId, Kind as NamespaceKind, NamespaceError, Path, Rules};
use fs_semantics::disposition::{Action, Failure};
use fs_winfsp::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome, Times,
    WorkspaceOps,
};

fn id(raw: u64) -> FileId {
    FileId::from_serial(raw)
}

/// Translates a namespace error into the adapter's vocabulary, via the **shared** failure table.
fn to_ops(error: NamespaceError) -> OpsError {
    // `InvalidRecord` is checked **before** the failure mapping: the core uses it for "your identity no longer names this
    // name", which the adapter must distinguish from a missing entry (they map to different actions: drop the handle vs
    // report a deletion). `failure_for` maps it to `Internal`, which is right for the *status* and wrong for the variant.
    if let NamespaceError::InvalidRecord { detail, .. } = &error {
        return OpsError::InvalidRecord(detail.clone());
    }
    match failure_for(&error) {
        Failure::LeafMissing | Failure::ParentMissing => OpsError::NotFound,
        Failure::NameCollision => OpsError::NameCollision,
        Failure::NotADirectory => OpsError::NotADirectory,
        Failure::IsADirectory => OpsError::IsADirectory,
        Failure::DirectoryNotEmpty => OpsError::NotEmpty,
        Failure::AccessDenied => OpsError::AccessDenied,
        Failure::InvalidParameter | Failure::InvalidName => OpsError::InvalidParameter,
        Failure::DeletePending => OpsError::DeletePending,
        Failure::SharingViolation => OpsError::SharingViolation,
        // Everything else is "the store or the record is not something this build can interpret"; the detail is kept so a
        // failure message still names the original variant.
        other => OpsError::Other(format!("{other:?}: {error}")),
    }
}

fn kind_of(kind: NamespaceKind) -> Kind {
    match kind {
        NamespaceKind::Directory => Kind::Directory,
        NamespaceKind::File => Kind::File,
    }
}

fn path_of(raw: &[u8]) -> OpsResult<Path> {
    let text = std::str::from_utf8(raw).map_err(|_| OpsError::InvalidParameter)?;
    Path::parse(text).map_err(|_| OpsError::InvalidParameter)
}

/// A workspace seen as the frontend's backend.
///
/// No `Debug`: `Workspace` holds the overlay lock, and a derived `Debug` would try to print through it.
pub struct ApiBackend<'a> {
    workspace: &'a fs_workspace::Workspace,
}

impl<'a> ApiBackend<'a> {
    /// Wraps a workspace.
    #[must_use]
    pub fn new(workspace: &'a fs_workspace::Workspace) -> Self {
        Self { workspace }
    }

    fn rules(&self) -> Rules<fs_workspace::WorkspaceStore> {
        Rules::new(self.workspace.store())
    }

    fn view(&self, raw: u64) -> OpsResult<fs_namespace::InodeView> {
        use fs_namespace::{TreeRead, TreeStore};
        self.workspace
            .store()
            .read(|tree| tree.inode(id(raw)))
            .map_err(to_ops)?
            .ok_or(OpsError::NotFound)
    }
}

impl std::fmt::Debug for ApiBackend<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiBackend")
            .field("workspace", &self.workspace.name())
            .finish()
    }
}

impl WorkspaceOps for ApiBackend<'_> {
    fn mkdir(&self, path: &[u8]) -> OpsResult<u64> {
        let rules = self.rules();
        rules.mkdir(&path_of(path)?).map_err(to_ops)?;
        Ok(rules.resolve(&path_of(path)?).map_err(to_ops)?.serial())
    }

    fn create_or_open(
        &self,
        path: &[u8],
        disposition: fs_semantics::disposition::Disposition,
    ) -> OpsResult<OpenOutcome> {
        let rules = self.rules();
        let parsed = path_of(path)?;
        let outcome = rules.create_or_open(&parsed, disposition).map_err(to_ops)?;
        // Metadata is read **after** the call, not taken from it: `size`/`times`/`attributes` must be the current values,
        // and a cached copy is exactly what would make "a new handle after SUPERSEDE sees size 0" pass for the wrong
        // reason.
        let view = self.view(outcome.file_id.serial())?;
        Ok(OpenOutcome {
            file_id: outcome.file_id.serial(),
            kind: kind_of(outcome.kind),
            action: match outcome.action {
                Action::CreateNew => Action::CreateNew,
                Action::Open => Action::Open,
                Action::TruncateExisting => Action::TruncateExisting,
            },
            size: view.size,
            times: Times {
                created: view.created_ns,
                accessed: view.accessed_ns,
                written: view.modified_ns,
                changed: view.changed_ns,
            },
            attributes: Attributes(view.attributes),
            binding: (outcome.binding.0.serial(), outcome.binding.1),
            replaced: outcome.replaced.map(FileId::serial),
        })
    }

    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        self.workspace
            .read_at_id(id(file_id), offset, len)
            .map_err(|error| match error {
                fs_workspace::WorkspaceError::Namespace(error) => to_ops(error),
                other => OpsError::Other(other.to_string()),
            })
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        self.workspace
            .write_at_id(id(file_id), offset, bytes)
            .map_err(|error| match error {
                fs_workspace::WorkspaceError::Namespace(error) => to_ops(error),
                other => OpsError::Other(other.to_string()),
            })?;
        Ok(bytes.len())
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        self.workspace
            .truncate_id(id(file_id), len)
            .map_err(|error| match error {
                fs_workspace::WorkspaceError::Namespace(error) => to_ops(error),
                other => OpsError::Other(other.to_string()),
            })
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let entries = self
            .workspace
            .read_dir_id(id(dir_id), None, 4096)
            .map_err(|error| match error {
                fs_workspace::WorkspaceError::Namespace(error) => to_ops(error),
                other => OpsError::Other(other.to_string()),
            })?;
        let mut out = Vec::with_capacity(entries.len());
        for (name, child) in entries {
            // The kind comes from the entry's own inode: the adapter needs it to fill attributes without a second call
            // per name, and guessing `File` for a directory would be wrong in a way tests would have to encode.
            let view = self.view(child.serial())?;
            out.push(DirEntry {
                name,
                kind: kind_of(view.kind),
            });
        }
        Ok(out)
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        let view = self.view(file_id)?;
        Ok(NodeInfo {
            file_id,
            kind: kind_of(view.kind),
            size: view.size,
            times: Times {
                created: view.created_ns,
                accessed: view.accessed_ns,
                written: view.modified_ns,
                changed: view.changed_ns,
            },
            // Report **derived** attributes, exactly as `Stat` does: the stored bits stay minimal, the frontend sees a
            // complete picture. Same function, so the two cannot disagree.
            attributes: Attributes(fs_namespace::derived_attributes(view.kind, view.attributes)),
        })
    }

    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()> {
        self.workspace
            .overlay()
            .set_times(id(file_id), accessed, written, changed, created)
            .map_err(to_ops)
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        self.workspace
            .overlay()
            .set_attributes(id(file_id), attributes.0)
            .map_err(to_ops)
    }

    fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome> {
        let rules = self.rules();
        let outcome = rules
            .rename_with(&path_of(from)?, &path_of(to)?, replace)
            .map_err(to_ops)?;
        Ok(RenameOutcome {
            file_id: outcome.file_id.serial(),
            binding: (outcome.binding.0.serial(), outcome.binding.1),
            replaced: outcome.replaced.map(FileId::serial),
        })
    }

    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        self.rules()
            .remove_at(id(parent), name, id(expected_id))
            .map_err(to_ops)
    }

    fn flush(&self) -> OpsResult<()> {
        self.workspace
            .flush()
            .map_err(|error| OpsError::Other(error.to_string()))
    }
}
