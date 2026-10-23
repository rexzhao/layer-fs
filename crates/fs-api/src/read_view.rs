//! Reading a published tree: a commit's files, with no table of ours involved
//! (ADR-006 I1, §13).

use std::path::Path;

use cas_core::ObjectId;
use cas_store::{OpenMode, Store};
use fs_namespace::{
    resolve_in, FileId, InodeView, Kind, NamespaceError, NamespaceResult, Path as FsPath, Stat,
    TreeRead,
};
use fs_snapshot::{Content, InodeRecord, Profile, SnapshotError, SnapshotReader};

use crate::error::{ApiError, ApiResult};

/// A read-only view of one commit's tree.
///
/// Everything it needs is the store and a root id, which is the point: history stays
/// readable after the workspace rows and overlay files are gone (I1).
///
/// The reader is re-opened per call rather than cached, because [`SnapshotReader`] borrows
/// the store and a self-referential handle would need unsafe code. The cost is one root
/// object read per call — a page-cache hit, not a table scan.
#[derive(Debug)]
pub struct ReadView {
    store: Store,
    root: ObjectId,
    profile: Profile,
}

impl ReadView {
    /// Opens the view over `root` in the store at `store_dir`.
    ///
    /// # Errors
    ///
    /// Store failures, or [`ApiError::Snapshot`] when the root is not a snapshot.
    pub(crate) fn open(store_dir: &Path, root: ObjectId) -> ApiResult<Self> {
        let profile = Profile::CURRENT;
        let store = Store::open(store_dir, OpenMode::ReadOnly)?;
        // Fail here rather than on the first read: a root that is not a snapshot should be
        // reported by `read_view`, not by whatever calls it next.
        SnapshotReader::open_any(&store, root)?;
        Ok(Self {
            store,
            root,
            profile,
        })
    }

    /// The snapshot root this view reads.
    #[must_use]
    pub fn root(&self) -> ObjectId {
        self.root
    }

    /// One entry's metadata.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing.
    pub fn stat(&self, path: &str) -> ApiResult<Stat> {
        let reader = self.reader()?;
        let tree = SnapshotTree { reader: &reader };
        let total = parse(path)?;
        let id = resolve_in(&tree, &total)?;
        let inode = tree.inode_record(id)?.ok_or_else(|| missing(&total))?;
        Ok(Stat {
            id,
            kind: namespace_kind(inode.kind),
            size: inode.size,
            // A snapshot's bytes live in a Blob or an extent tree, not in a "content root"
            // the way the retired table backend meant it; the caller reads bytes through
            // `read_at` either way.
            content_root: None,
            created_ns: inode.created_ns,
            modified_ns: inode.modified_ns,
        })
    }

    /// Lists a directory, in name order.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is not a directory.
    pub fn read_dir(&self, path: &str) -> ApiResult<Vec<fs_namespace::Entry>> {
        let reader = self.reader()?;
        let tree = SnapshotTree { reader: &reader };
        let total = parse(path)?;
        let id = resolve_in(&tree, &total)?;
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = tree.read_dir(id, after.as_deref(), 256)?;
            if page.is_empty() {
                return Ok(out);
            }
            after = page.last().map(|(name, _)| name.clone());
            for (name, child) in page {
                let inode = tree.inode_record(child)?.ok_or_else(|| missing(&total))?;
                out.push(fs_namespace::Entry {
                    name: String::from_utf8_lossy(&name).into_owned(),
                    id: child,
                    kind: namespace_kind(inode.kind),
                });
            }
        }
    }

    /// Reads at most `out.len()` bytes at `offset`, returning how many were read.
    ///
    /// # Errors
    ///
    /// [`ApiError::Namespace`] when the path is missing or is a directory.
    pub fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> ApiResult<usize> {
        let file = self.file_record(path)?;
        if out.is_empty() || offset >= file.size {
            return Ok(0);
        }
        let want = usize::try_from(file.size - offset)
            .unwrap_or(usize::MAX)
            .min(out.len());
        let bytes = self.read_range(&file, offset, want)?;
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    /// The whole file, for callers that know they want all of it.
    ///
    /// # Errors
    ///
    /// As [`ReadView::read_at`].
    pub fn read_all(&self, path: &str) -> ApiResult<Vec<u8>> {
        let size = self.file_record(path)?.size;
        let mut out = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
        let read = self.read_at(path, 0, &mut out)?;
        out.truncate(read);
        Ok(out)
    }

    fn file_record(&self, path: &str) -> ApiResult<InodeRecord> {
        let reader = self.reader()?;
        let tree = SnapshotTree { reader: &reader };
        let total = parse(path)?;
        let id = resolve_in(&tree, &total)?;
        let inode = tree.inode_record(id)?.ok_or_else(|| missing(&total))?;
        if inode.kind != fs_snapshot::Kind::File {
            return Err(ApiError::Namespace(NamespaceError::IsADirectory {
                path: total.display(),
            }));
        }
        Ok(inode)
    }

    fn read_range(&self, file: &InodeRecord, offset: u64, want: usize) -> ApiResult<Vec<u8>> {
        let mut out = vec![0u8; want];
        match file.content {
            Content::Empty => Ok(out),
            Content::Blob(oid) => {
                let value = self.object_value(oid)?;
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                let available = value.len().saturating_sub(start).min(want);
                out[..available].copy_from_slice(&value[start..start + available]);
                Ok(out)
            }
            Content::Extents(_) => {
                let reader = self.reader()?;
                let extents = reader.extents_in_range(file, offset, offset + want as u64)?;
                for extent in extents {
                    let extent_end = extent.logical_start + extent.length;
                    let from = offset.max(extent.logical_start);
                    let to = (offset + want as u64).min(extent_end);
                    if from >= to {
                        continue;
                    }
                    let value = self.object_value(extent.object)?;
                    let source =
                        usize::try_from(extent.source_offset + (from - extent.logical_start))
                            .unwrap_or(usize::MAX);
                    let count = usize::try_from(to - from).unwrap_or(usize::MAX);
                    let available = value.len().saturating_sub(source).min(count);
                    let at = usize::try_from(from - offset).unwrap_or(usize::MAX);
                    out[at..at + available].copy_from_slice(&value[source..source + available]);
                }
                Ok(out)
            }
            Content::Dir(_) => Err(ApiError::Namespace(NamespaceError::IsADirectory {
                path: format!("serial {}", file.serial),
            })),
        }
    }

    fn object_value(&self, oid: ObjectId) -> ApiResult<Vec<u8>> {
        let object = self.store.get(oid)?;
        let decoded = cas_core::Object::decode(&object)
            .map_err(|error| ApiError::Store(cas_store::StoreError::from(error)))?;
        Ok(decoded.value().to_vec())
    }

    fn reader(&self) -> ApiResult<SnapshotReader<'_>> {
        Ok(SnapshotReader::open(&self.store, self.root, self.profile)?)
    }
}

/// [`TreeRead`] over a snapshot reader, so the path rules can resolve a path in history
/// without knowing what a snapshot is.
struct SnapshotTree<'a> {
    reader: &'a SnapshotReader<'a>,
}

impl TreeRead for SnapshotTree<'_> {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>> {
        let Some(record) = self.inode_record(parent)? else {
            return Ok(None);
        };
        Ok(self
            .reader
            .lookup(&record, name)
            .map_err(into_namespace)?
            .map(FileId::from_serial))
    }

    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>> {
        Ok(self
            .reader
            .inode(id.serial())
            .map_err(into_namespace)?
            .map(|record| InodeView {
                id,
                kind: namespace_kind(record.kind),
                size: record.size,
                created_ns: record.created_ns,
                modified_ns: record.modified_ns,
            }))
    }

    fn read_dir(
        &self,
        dir: FileId,
        after: Option<&[u8]>,
        limit: usize,
    ) -> NamespaceResult<Vec<(Vec<u8>, FileId)>> {
        let Some(record) = self.inode_record(dir)? else {
            return Ok(Vec::new());
        };
        let entries = self
            .reader
            .read_dir(&record, after, limit)
            .map_err(into_namespace)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.name, FileId::from_serial(entry.child_serial)))
            .collect())
    }

    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool> {
        Ok(self.read_dir(dir, None, 1)?.is_empty())
    }
}

impl SnapshotTree<'_> {
    fn inode_record(&self, id: FileId) -> NamespaceResult<Option<InodeRecord>> {
        self.reader.inode(id.serial()).map_err(into_namespace)
    }
}

fn namespace_kind(kind: fs_snapshot::Kind) -> Kind {
    match kind {
        fs_snapshot::Kind::Directory => Kind::Directory,
        fs_snapshot::Kind::File => Kind::File,
    }
}

fn into_namespace(error: SnapshotError) -> NamespaceError {
    NamespaceError::Store(cas_store::StoreError::InvalidRecord {
        field: "snapshot",
        detail: error.to_string(),
    })
}

fn missing(path: &FsPath) -> NamespaceError {
    NamespaceError::NotFound {
        path: path.display(),
    }
}

fn parse(text: &str) -> ApiResult<FsPath> {
    Ok(FsPath::parse(text)?)
}
