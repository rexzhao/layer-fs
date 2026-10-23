//! The base layer: the snapshot this workspace starts from (ADR-006 §7.2).
//!
//! The overlay never copies the base. Every read the overlay does not answer falls
//! through to here, and because a snapshot is an immutable object graph the fall-through
//! needs no coordination with other workspaces.
//!
//! The store sits behind a mutex because `cas_store::Store` owns a `rusqlite::Connection`
//! and is therefore `Send` but not `Sync`; without this the handle could not be shared
//! across threads, which ADR-006 §7.4 requires. Lock order is always *overlay
//! connection, then base store* — never the reverse — so the two cannot deadlock.

use std::sync::Mutex;

use cas_core::ObjectId;
use cas_store::Store;
use fs_snapshot::{InodeRecord, Kind, Profile, SnapshotReader};

use crate::error::OverlayResult;

/// An immutable snapshot plus the store holding its objects.
#[derive(Debug)]
pub(crate) struct BaseLayer {
    store: Mutex<Store>,
    root: Mutex<ObjectId>,
    profile: Profile,
}

impl BaseLayer {
    pub(crate) fn new(store: Store, root: ObjectId, profile: Profile) -> Self {
        Self {
            store: Mutex::new(store),
            root: Mutex::new(root),
            profile,
        }
    }

    pub(crate) fn root(&self) -> ObjectId {
        *self
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Moves the base to a new snapshot (ADR-006 §9.1 step 4).
    ///
    /// Only `OverlayStore::adopt_base` calls this, and only after the overlay rows that
    /// referred to the old base are gone — otherwise a read could see one workspace's
    /// entries applied to a different snapshot.
    pub(crate) fn set_root(&self, root: ObjectId) {
        *self
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = root;
    }

    /// The base inode for `serial`, if the base has one.
    pub(crate) fn inode(&self, serial: u64) -> OverlayResult<Option<InodeRecord>> {
        let root = self.root();
        let store = self.lock();
        let reader = SnapshotReader::open(&store, root, self.profile)?;
        Ok(reader.inode(serial)?)
    }

    /// The base inode for `serial`, requiring it to be a directory.
    pub(crate) fn directory(&self, serial: u64) -> OverlayResult<Option<InodeRecord>> {
        match self.inode(serial)? {
            Some(record) if record.kind == Kind::Directory => Ok(Some(record)),
            _ => Ok(None),
        }
    }

    /// Looks a name up in a base directory.
    pub(crate) fn lookup(&self, dir: &InodeRecord, name: &[u8]) -> OverlayResult<Option<u64>> {
        let root = self.root();
        let store = self.lock();
        let reader = SnapshotReader::open(&store, root, self.profile)?;
        Ok(reader.lookup(dir, name)?)
    }

    /// Lists a base directory's entries.
    pub(crate) fn read_dir(
        &self,
        dir: &InodeRecord,
        after: Option<&[u8]>,
        limit: usize,
    ) -> OverlayResult<Vec<(Vec<u8>, u64)>> {
        let root = self.root();
        let store = self.lock();
        let reader = SnapshotReader::open(&store, root, self.profile)?;
        let entries = reader.read_dir(dir, after, limit)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.name, entry.child_serial))
            .collect())
    }

    /// Whether a base directory is empty.
    pub(crate) fn is_dir_empty(&self, dir: &InodeRecord) -> OverlayResult<bool> {
        Ok(self.read_dir(dir, None, 1)?.is_empty())
    }

    /// Reads `[offset, offset + len)` of a base file, zero-filling past its extents.
    ///
    /// A snapshot file has three shapes (ADR-005 §3.2) and all three can appear here: a
    /// small file is one `Blob`, a large one is an extent tree, and an all-zero file is
    /// `Empty`. Handling only the extent shape would make every base file below one
    /// block unreadable.
    ///
    /// Only the extents overlapping the range are fetched, so a small read costs
    /// O(range), not O(file) — the property ADR-006 §7.2 requires of the composed read.
    pub(crate) fn read_at(
        &self,
        file: &InodeRecord,
        offset: u64,
        len: usize,
    ) -> OverlayResult<Vec<u8>> {
        let mut out = vec![0u8; len];
        if len == 0 || offset >= file.size {
            return Ok(out);
        }
        let want = ((file.size - offset) as usize).min(len);
        match file.content {
            fs_snapshot::Content::Empty => Ok(out),
            fs_snapshot::Content::Blob(oid) => {
                let store = self.lock();
                let object = store.get(oid)?;
                let value = cas_core::Object::decode(&object)?.value().to_vec();
                // The blob's value length equals the file size (ADR-005 §3.2), so the
                // range maps directly.
                let start = offset as usize;
                let available = value.len().saturating_sub(start).min(want);
                out[..available].copy_from_slice(&value[start..start + available]);
                Ok(out)
            }
            fs_snapshot::Content::Extents(_) => {
                let root = self.root();
                let store = self.lock();
                let reader = SnapshotReader::open(&store, root, self.profile)?;
                let extents = reader.extents_in_range(file, offset, offset + want as u64)?;
                for extent in extents {
                    let extent_start = extent.logical_start;
                    let extent_end = extent_start + extent.length;
                    let from = offset.max(extent_start);
                    let to = (offset + want as u64).min(extent_end);
                    if from >= to {
                        continue;
                    }
                    let object = store.get(extent.object)?;
                    let value = cas_core::Object::decode(&object)?.value().to_vec();
                    let source = extent.source_offset + (from - extent_start);
                    let count = (to - from) as usize;
                    let available = value.len().saturating_sub(source as usize).min(count);
                    out[(from - offset) as usize..(from - offset) as usize + available]
                        .copy_from_slice(&value[source as usize..source as usize + available]);
                }
                Ok(out)
            }
            fs_snapshot::Content::Dir(_) => Err(crate::error::OverlayError::InvalidRecord {
                field: "inode.content",
                detail: "a directory has no bytes to read".to_owned(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
