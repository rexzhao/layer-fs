//! Reads and writes with bounded amplification (ADR-004 §4, §5).

use std::sync::atomic::{AtomicU64, Ordering};

use cas_core::{Object, ObjectId, Role};
use cas_store::Store;
use fs_namespace::{ContentState, Extent, FileId, Kind, Namespace, Path as NsPath};

use crate::error::{FsError, FsResult};
use crate::stats::{ReadStats, Stats};

/// The fixed block size for large files (ADR-004 §5.2).
///
/// Must match `fs_namespace::schema::BLOCK_BYTES`, which is frozen in
/// `namespace_policy` and validated on open; this constant is the API layer's
/// copy so it does not have to reach into the schema module.
pub const BLOCK_BYTES: u64 = 64 * 1024;

/// A file-system handle bound to one Store, one namespace and one connection
/// pair.
///
/// Every method is path-addressed and stateless: there are no long-lived open
/// handles (ADR-004 §3). Because a path is resolved and used inside the same
/// namespace transaction, there is no check-then-use window.
#[derive(Debug)]
pub struct Fs {
    store: Store,
    namespace: Namespace,
    object_gets: AtomicU64,
}

impl Fs {
    /// Opens (or creates) `namespace` in the Store at `store_root`.
    ///
    /// # Errors
    ///
    /// Store and namespace open failures.
    pub fn open(
        store_root: impl AsRef<std::path::Path>,
        namespace: &str,
        mode: cas_store::OpenMode,
    ) -> FsResult<Self> {
        let store = Store::open(store_root, mode)?;
        let ns = Namespace::open(&store, namespace, mode)?;
        Ok(Self {
            store,
            namespace: ns,
            object_gets: AtomicU64::new(0),
        })
    }

    /// The read-amplification counters (ADR-004 §6).
    #[must_use]
    pub fn read_stats(&self) -> ReadStats {
        ReadStats {
            object_gets: self.object_gets.load(Ordering::Relaxed),
        }
    }

    /// Storage and namespace totals.
    ///
    /// # Errors
    ///
    /// Store failures while counting.
    pub fn stats(&self) -> FsResult<Stats> {
        Ok(Stats {
            objects: self.store.object_count()?,
            root_records: self.namespace.root_count(),
        })
    }

    /// Creates an empty file (ADR-004 §4.4).
    ///
    /// # Errors
    ///
    /// The matching `FsError` variant for a taken name, missing parent or
    /// non-directory parent.
    pub fn create(&self, path: &str) -> FsResult<()> {
        let path = NsPath::parse(path)?;
        self.namespace.create_file(&path)?;
        Ok(())
    }

    /// Reads up to `out.len()` bytes starting at `offset` (ADR-004 §4.1).
    ///
    /// Returns the number of bytes actually read; zero at or past EOF. Never
    /// reads the whole file to satisfy a small range.
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`, or a dangling/corrupt reference.
    pub fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> FsResult<usize> {
        let path = NsPath::parse(path)?;
        let (size, content_root) = self.namespace.shape_of(&path)?;
        if out.is_empty() || offset >= size {
            return Ok(0);
        }
        let want = ((size - offset) as usize).min(out.len());
        let span = offset as usize..offset as usize + want;
        match content_root {
            Some(root) => {
                let bytes = self.fetch(root)?;
                let value = Object::decode(&bytes)?.value().to_vec();
                let available = value.len().saturating_sub(span.start);
                let copied = available.min(want);
                out[..copied].copy_from_slice(&value[span.start..span.start + copied]);
                Ok(copied)
            }
            None => {
                // Only the extents the range overlaps are read from the table.
                let extents =
                    self.namespace
                        .extents_in_range(&path, offset, offset + want as u64)?;
                self.read_extents(&extents, span, out)
            }
        }
    }

    /// Writes `bytes` at `offset`, extending the file with zeros if needed
    /// (ADR-004 §4.2).
    ///
    /// Objects are published before the namespace references them, so a failure
    /// between the two leaves an unreferenced object, never a dangling one.
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`, store failures.
    pub fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> FsResult<usize> {
        let path = NsPath::parse(path)?;
        let (size, content_root) = self.namespace.shape_of(&path)?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(FsError::OffsetOutOfRange { offset })?;
        let new_size = size.max(end);
        if bytes.is_empty() {
            return Ok(0);
        }

        // A file that stays under one block keeps the single-object form
        // (ADR-004 §5.1), so a later read needs no extent lookup at all.
        if new_size < BLOCK_BYTES {
            let mut block = vec![0u8; new_size as usize];
            self.copy_existing_into(size, content_root, &path, &mut block)?;
            let from = offset as usize;
            block[from..from + bytes.len()].copy_from_slice(bytes);
            let object_id = match content_root {
                Some(root) if self.object_matches(root, &block)? => root,
                _ => self.store.put(Role::Bytes, &block)?.oid,
            };
            self.namespace.replace_content(
                &path,
                "write_at",
                &ContentState {
                    size: new_size,
                    content_root: Some(object_id),
                    extents: Vec::new(),
                },
            )?;
            return Ok(bytes.len());
        }

        // Only the blocks the written range touches are consulted; every other
        // extent row stays in the table untouched. Loading the file's whole
        // extent list (or rewriting the whole file) per write would make
        // building a large file block by block O(n²).
        let first = offset / BLOCK_BYTES;
        let last = (end - 1) / BLOCK_BYTES;
        let touch_from = first * BLOCK_BYTES;
        let touch_to = ((last + 1) * BLOCK_BYTES).min(new_size);
        // Only the rows this write can reach are fetched, so a one-block write
        // stays O(1) rows regardless of file size.
        let mut existing_rows = self
            .namespace
            .extents_in_range(&path, touch_from, touch_to)?;
        // Shape change: a blob has no extent rows, so its bytes live only in
        // `content_root`. Turn it into one row covering [0, size) before the
        // block work, otherwise the bytes are dropped when the transaction
        // clears `content_root` (F-T4-1). A blob is always under one block, so
        // this stays bounded.
        if let Some(root) = content_root {
            if size > 0 && !existing_rows.iter().any(|row| row.logical_start < size) {
                existing_rows.insert(
                    0,
                    Extent {
                        logical_start: 0,
                        length: size,
                        object_id: root,
                        source_offset: 0,
                    },
                );
            }
        }

        // Rebuild every extent row that overlaps the written range: the old rows
        // are sliced around the blocks being replaced, so an untouched prefix
        // (whether it came from a blob or from earlier blocks) survives.
        let mut replaced = Vec::new();
        for index in first..=last {
            let block_start = index * BLOCK_BYTES;
            let block_len = BLOCK_BYTES.min(new_size - block_start) as usize;
            let mut block = vec![0u8; block_len];
            // A block that was inside an existing extent starts from those bytes.
            let cover = existing_rows.iter().find(|extent| {
                extent.logical_start < block_start + block_len as u64
                    && block_start < extent.logical_start + extent.length
            });
            if let Some(cover) = cover {
                block = self.read_block(size, cover, block_start, block_len)?;
            }
            let untouched = block.clone();
            let from = (offset as usize).max(block_start as usize);
            let to = ((end as usize).min(block_start as usize + block_len)).max(from);
            block[from - block_start as usize..to - block_start as usize]
                .copy_from_slice(&bytes[from - offset as usize..to - offset as usize]);

            // A block whose bytes did not change keeps its existing object, so
            // rewriting identical bytes publishes nothing new.
            let reusable = cover.filter(|extent| {
                extent.logical_start == block_start
                    && extent.length == block_len as u64
                    && extent.source_offset == 0
                    && untouched == block
            });
            let object_id = match reusable {
                Some(extent) => extent.object_id,
                None => self.store.put(Role::Bytes, &block)?.oid,
            };
            replaced.push(Extent {
                logical_start: block_start,
                length: block_len as u64,
                object_id,
                source_offset: 0,
            });
        }

        // Rows that end at or before the rewritten range are carried over as they
        // are. That is what preserves an untouched prefix — including a blob's
        // single row — across the shape change (F-T4-1). A row that *straddles*
        // `touch_from` is clipped to the part before it, so the rewritten blocks
        // take over from there without losing the bytes in front.
        for extent in &existing_rows {
            let extent_end = extent.logical_start + extent.length;
            if extent_end <= touch_from {
                replaced.push(extent.clone());
            } else if extent.logical_start < touch_from {
                replaced.push(Extent {
                    logical_start: extent.logical_start,
                    length: touch_from - extent.logical_start,
                    object_id: extent.object_id,
                    source_offset: extent.source_offset,
                });
            }
        }

        replaced.sort_by_key(|extent| extent.logical_start);
        // Rows from `touch_from` upward are replaced by the rewritten blocks;
        // rows above `touch_to` are out of the write's reach and stay put, which
        // keeps a one-block write at O(1) rows.
        self.namespace
            .replace_extents(&path, "write_at", new_size, None, &replaced, None)?;
        Ok(bytes.len())
    }

    /// Sets the file's size, zero-extending or truncating (ADR-004 §4.3).
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`, store failures.
    pub fn truncate(&self, path: &str, size: u64) -> FsResult<()> {
        let path = NsPath::parse(path)?;
        let state = self.namespace.content_state(&path)?;
        if state.size == size {
            return Ok(());
        }
        // Only the block at the new boundary changes; extents below it are kept
        // and extents above it are dropped.
        if size < BLOCK_BYTES {
            let mut block = vec![0u8; size as usize];
            self.copy_existing_into(state.size, state.content_root, &path, &mut block)?;
            if block.len() as u64 != state.size || state.content_root.is_none() {
                let published = self.store.put(Role::Bytes, &block)?;
                self.namespace.replace_content(
                    &path,
                    "truncate",
                    &ContentState {
                        size,
                        content_root: Some(published.oid),
                        extents: Vec::new(),
                    },
                )?;
            }
            return Ok(());
        }

        let boundary = size / BLOCK_BYTES;
        let boundary_start = boundary * BLOCK_BYTES;
        let block_len = (size - boundary_start) as usize;
        // A blob keeps its bytes in `content_root` with no extent rows, so
        // growing one past the block size would read zeros and then clear
        // `content_root` — losing every earlier byte (F-T4-1's sibling). Treat
        // the blob as one row covering [0, old_size) so the boundary block sees
        // real bytes.
        let mut rows = state.extents.clone();
        if let Some(root) = state.content_root {
            if state.size > 0 {
                rows.insert(
                    0,
                    Extent {
                        logical_start: 0,
                        length: state.size,
                        object_id: root,
                        source_offset: 0,
                    },
                );
            }
        }
        let existing = rows.iter().find(|extent| {
            extent.logical_start <= boundary_start
                && boundary_start < extent.logical_start + extent.length
        });
        let block = match existing {
            Some(extent) => self.read_block(state.size, extent, boundary_start, block_len)?,
            None => vec![0u8; block_len],
        };
        let object_id = match existing {
            Some(extent)
                if extent.logical_start == boundary_start
                    && extent.length == block_len as u64
                    && extent.source_offset == 0
                    && self.block_matches(extent, &block)? =>
            {
                extent.object_id
            }
            _ => self.store.put(Role::Bytes, &block)?.oid,
        };
        // Everything below the boundary block is unchanged, so its rows are
        // re-inserted as they are (a blob's single row gets clipped to the part
        // in front of the boundary).
        let mut replaced = vec![Extent {
            logical_start: boundary_start,
            length: block_len as u64,
            object_id,
            source_offset: 0,
        }];
        for extent in &rows {
            let extent_end = extent.logical_start + extent.length;
            if extent_end <= boundary_start {
                replaced.push(extent.clone());
            } else if extent.logical_start < boundary_start {
                replaced.push(Extent {
                    logical_start: extent.logical_start,
                    length: boundary_start - extent.logical_start,
                    object_id: extent.object_id,
                    source_offset: extent.source_offset,
                });
            }
        }
        replaced.sort_by_key(|extent| extent.logical_start);
        self.namespace.replace_extents(
            &path,
            "truncate",
            size,
            None,
            &replaced,
            Some(boundary_start + block_len as u64),
        )?;
        Ok(())
    }

    /// A no-op in P1 (ADR-004 §4.5).
    ///
    /// Each write is its own transaction, so there is no buffered state to
    /// flush. This method provides **no** durability beyond what the write
    /// already had; it exists to satisfy the API shape.
    ///
    /// # Errors
    ///
    /// `NotFound` / `IsADirectory` if the path is not a readable file.
    pub fn flush(&self, path: &str) -> FsResult<()> {
        let path = NsPath::parse(path)?;
        self.namespace.content_state(&path)?;
        Ok(())
    }

    /// Whether an extent's object already holds exactly `block`.
    fn block_matches(&self, extent: &Extent, block: &[u8]) -> FsResult<bool> {
        let stored = self.read_extent_value(extent)?;
        Ok(stored == block)
    }

    /// The value of an extent's object, used by the block-reuse check.
    fn read_extent_value(&self, extent: &Extent) -> FsResult<Vec<u8>> {
        let bytes = self.fetch(extent.object_id)?;
        Ok(Object::decode(&bytes)?.value().to_vec())
    }

    /// Reads the `block_len` bytes at `block_start`, taking them from the
    /// covering extent and zero-filling anything past EOF.
    fn read_block(
        &self,
        file_size: u64,
        extent: &Extent,
        block_start: u64,
        block_len: usize,
    ) -> FsResult<Vec<u8>> {
        if block_len == 0 {
            return Ok(Vec::new());
        }
        let mut block = vec![0u8; block_len];
        let available = (file_size.saturating_sub(block_start)).min(block_len as u64) as usize;
        if available == 0 {
            return Ok(block);
        }
        let bytes = self.fetch(extent.object_id)?;
        let value = Object::decode(&bytes)?.value();
        let source = extent.source_offset as usize + (block_start - extent.logical_start) as usize;
        let count = available.min((value.len().saturating_sub(source)).min(extent.length as usize));
        if source + count > value.len() {
            return Err(FsError::Corrupt {
                detail: format!("extent at {} overruns its object", extent.logical_start),
            });
        }
        block[..count].copy_from_slice(&value[source..source + count]);
        Ok(block)
    }

    /// Copies the file's existing bytes into `target`, stopping at `target`'s end.
    fn copy_existing_into(
        &self,
        size: u64,
        content_root: Option<ObjectId>,
        path: &NsPath,
        target: &mut [u8],
    ) -> FsResult<()> {
        if target.is_empty() || size == 0 {
            return Ok(());
        }
        let want = (size.min(target.len() as u64)) as usize;
        match content_root {
            Some(root) => {
                let bytes = self.fetch(root)?;
                let value = Object::decode(&bytes)?.value();
                let count = value.len().min(want);
                target[..count].copy_from_slice(&value[..count]);
            }
            None => {
                let extents = self.namespace.extents_in_range(path, 0, want as u64)?;
                self.read_extents(&extents, 0..want, target)?;
            }
        }
        Ok(())
    }

    /// Whether an object's value is exactly `expected`.
    fn object_matches(&self, oid: ObjectId, expected: &[u8]) -> FsResult<bool> {
        let bytes = self.fetch(oid)?;
        Ok(Object::decode(&bytes)?.value() == expected)
    }

    /// Reads and decodes one object, counting the fetch (ADR-004 §6).
    ///
    /// Both "the index has no such object" and "the index has it but its bytes
    /// cannot be read back" mean the same thing to a caller: the namespace's
    /// reference cannot be satisfied. Both become `DanglingReference`, which is
    /// what `verify` reports as well.
    fn fetch(&self, oid: ObjectId) -> FsResult<Vec<u8>> {
        self.object_gets.fetch_add(1, Ordering::Relaxed);
        self.store.get(oid).map_err(|error| match error {
            cas_store::StoreError::NotFound { .. } | cas_store::StoreError::Corrupt { .. } => {
                FsError::DanglingReference {
                    object: oid.to_string(),
                }
            }
            other => FsError::Store(other),
        })
    }

    /// Serves a byte span from extent rows.
    ///
    /// Only the extents that intersect `span` are fetched, which is what bounds
    /// the amplification: a 4 KiB read touches at most two 64 KiB blocks.
    ///
    /// Gaps between extents are **allocated zeros**, not absent bytes: the
    /// caller's `out` already starts zeroed and the file's `size` says the range
    /// exists, so the whole span counts as read. Returning only the filled count
    /// would turn a hole into a short read.
    fn read_extents(
        &self,
        extents: &[Extent],
        span: std::ops::Range<usize>,
        out: &mut [u8],
    ) -> FsResult<usize> {
        let mut cursor = span.start;
        for extent in extents {
            let extent_start = extent.logical_start as usize;
            let extent_end = extent_start + extent.length as usize;
            if extent_end <= cursor {
                continue;
            }
            if extent_start >= span.end {
                break;
            }
            let from = cursor.max(extent_start);
            let to = span.end.min(extent_end);
            if from >= to {
                continue;
            }
            let bytes = self.fetch(extent.object_id)?;
            let value = Object::decode(&bytes)?.value();
            let source = extent.source_offset as usize + (from - extent_start);
            let count = to - from;
            if source + count > value.len() {
                return Err(FsError::Corrupt {
                    detail: format!(
                        "extent at {} reads {source}..{} from a {}-byte object",
                        extent.logical_start,
                        source + count,
                        value.len()
                    ),
                });
            }
            out[from - span.start..from - span.start + count]
                .copy_from_slice(&value[source..source + count]);
            cursor = to;
        }
        Ok(span.end - span.start)
    }

    /// Directory listing (delegates to the namespace layer).
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`.
    pub fn read_dir(&self, path: &str) -> FsResult<Vec<fs_namespace::Entry>> {
        Ok(self.namespace.read_dir(&NsPath::parse(path)?)?)
    }

    /// Metadata for one path.
    ///
    /// # Errors
    ///
    /// `NotFound`.
    pub fn stat(&self, path: &str) -> FsResult<fs_namespace::Stat> {
        Ok(self.namespace.stat(&NsPath::parse(path)?)?)
    }

    /// Creates a directory.
    ///
    /// # Errors
    ///
    /// `AlreadyExists`, `NotFound`, `NotADirectory`.
    pub fn mkdir(&self, path: &str) -> FsResult<()> {
        Ok(self.namespace.mkdir(&NsPath::parse(path)?)?)
    }

    /// Removes a file or an empty directory.
    ///
    /// # Errors
    ///
    /// `NotFound`, `DirectoryNotEmpty`, `IsRoot`.
    pub fn remove(&self, path: &str) -> FsResult<()> {
        let parsed = NsPath::parse(path)?;
        match self.namespace.stat(&parsed)?.kind {
            Kind::Directory => Ok(self.namespace.rmdir(&parsed)?),
            Kind::File => Ok(self.namespace.unlink(&parsed)?),
        }
    }

    /// Renames an entry.
    ///
    /// # Errors
    ///
    /// The rename conflict variants.
    pub fn rename(&self, from: &str, to: &str) -> FsResult<()> {
        Ok(self
            .namespace
            .rename(&NsPath::parse(from)?, &NsPath::parse(to)?)?)
    }

    /// Checks every object and every namespace reference (ADR-004 §7).
    ///
    /// # Errors
    ///
    /// Store failures while walking.
    pub fn verify(&self) -> FsResult<crate::verify::VerifyReport> {
        crate::verify::run(&self.store, &self.namespace)
    }

    /// The identity behind a path, for callers that want it.
    ///
    /// # Errors
    ///
    /// `NotFound`.
    pub fn file_id(&self, path: &str) -> FsResult<FileId> {
        let parsed = NsPath::parse(path)?;
        Ok(self.namespace.stat(&parsed)?.id)
    }
}
