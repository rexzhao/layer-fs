//! The materialiser: a workspace's view becomes a snapshot object graph (ADR-006 §9.2).
//!
//! **Reachability, not tables** (I2). The traversal starts at serial 0 and walks downwards
//! through directory listings; it never scans `ov_inode` to decide which files exist. That
//! is what makes the inode table's key set equal the reachable set *by construction*
//! (ADR-005 V4) rather than by a repair pass.
//!
//! # Two passes, on purpose
//!
//! Pass 1 walks the tree inside one read scope and records, for every reachable inode, its
//! serial, kind, size, times, path and — for a directory — its child names. Pass 2 reads
//! file bytes and publishes objects.
//!
//! They are separate because the overlay's content API opens its own scope, and entering
//! it inside the traversal's scope would be re-entry (TX4). Reading structure first also
//! means the whole walk sees one consistent view while the (slower, publishing) second
//! pass does not hold a read transaction open.
//!
//! # Shape of a file
//!
//! ADR-005 §3.2 fixes exactly one representation per byte content, and the materialiser
//! must produce that one or two equal files would hash differently:
//!
//! | condition | representation |
//! | --- | --- |
//! | `size == 0`, or every block is zero | `Empty` |
//! | `size < block_bytes` | `Blob` (one object) |
//! | otherwise | `Extents`, all-zero blocks omitted |
//!
//! Content is read one block at a time, so peak memory is one block plus one object, not
//! one file.
//!
//! # Known costs (not optimised here)
//!
//! - **Unchanged directories are re-encoded** rather than reusing the base's tree oid, and
//!   **unchanged file blocks are re-read** rather than reusing the base's extents. Content
//!   addressing makes the re-encoding produce the same oids, so the *result* is right and
//!   no extra object is stored (`AlreadyPresent`), but the walk still touches everything.
//!   ADR-006 §9.2 describes both reuses as the intended optimisation; they are reported as
//!   not done rather than quietly claimed.

use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use cas_store::Store;
use fs_namespace::{FileId, Kind, Path, TreeRead};
use fs_snapshot::{
    encode_directory, encode_extents, encode_inode_table, encode_root, Content, DirEntry, Extent,
    InodeRecord, PageSink, Profile, RootValue, SnapshotError, SnapshotResult,
};

use crate::error::{WorkspaceError, WorkspaceResult};

/// How many directory entries one listing page holds.
const PAGE_LIMIT: usize = 256;

/// Publishes pages into a CAS store.
///
/// [`PageSink`] forces the snapshot error type, but a store failure is not a malformed
/// tree: flattening it into a message would erase the one thing ADR-006 §11.1 promises the
/// caller — a typed `Busy`. So the failure is kept aside and substituted by [`publish`],
/// which is what every encoder call goes through.
pub(crate) struct StoreSink<'a> {
    pub(crate) store: &'a Store,
    failure: Option<cas_store::StoreError>,
}

impl<'a> StoreSink<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self {
            store,
            failure: None,
        }
    }

    /// The store failure that stopped this sink, if any.
    fn take_failure(&mut self) -> Option<cas_store::StoreError> {
        self.failure.take()
    }
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        match self.store.put(role, value) {
            Ok(published) => Ok(published.oid),
            Err(error) => {
                self.failure = Some(error);
                Err(SnapshotError::Malformed {
                    detail: "the store refused an object; the cause is reported separately"
                        .to_owned(),
                })
            }
        }
    }
}

/// Runs one encoder step, preferring the store's own failure over the encoder's sentinel.
pub(crate) fn publish<T>(
    sink: &mut StoreSink<'_>,
    body: impl FnOnce(&mut StoreSink<'_>) -> SnapshotResult<T>,
) -> WorkspaceResult<T> {
    match body(sink) {
        Ok(value) => Ok(value),
        Err(error) => match sink.take_failure() {
            Some(store) => Err(WorkspaceError::from(store)),
            None => Err(WorkspaceError::from(error)),
        },
    }
}

/// One reachable inode, as pass 1 recorded it.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    serial: u64,
    is_dir: bool,
    size: u64,
    created_ns: i64,
    modified_ns: i64,
    path: Path,
    /// For a directory, its entries in name order. Recorded during pass 1 so pass 2 needs
    /// no view at all — which is what keeps the content reads from nesting inside a read
    /// scope (TX4).
    children: Vec<(Vec<u8>, u64)>,
}

/// Walks the view from the root and records every reachable inode.
///
/// # Errors
///
/// A reachable serial without an inode is an error: skipping it would silently drop a
/// subtree, which is worse than refusing to snapshot.
pub(crate) fn collect(view: &impl TreeRead) -> WorkspaceResult<Vec<Node>> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut seen: BTreeMap<u64, ()> = BTreeMap::new();
    let mut pending: Vec<(FileId, Path)> = vec![(FileId::ROOT, root_path())];
    while let Some((id, path)) = pending.pop() {
        if seen.insert(id.serial(), ()).is_some() {
            continue;
        }
        let Some(inode) = view.inode(id)? else {
            return Err(WorkspaceError::Inconsistent {
                detail: format!("serial {} is reachable but has no inode", id.serial()),
            });
        };
        let is_dir = inode.kind == Kind::Directory;
        let mut children = Vec::new();
        if is_dir {
            for (name, child) in children_of(view, id)? {
                let child_path = child_path(&path, &name)?;
                pending.push((child, child_path));
                children.push((name, child.serial()));
            }
        }
        nodes.push(Node {
            serial: id.serial(),
            is_dir,
            size: inode.size,
            created_ns: inode.created_ns,
            modified_ns: inode.modified_ns,
            path,
            children,
        });
    }
    nodes.sort_by_key(|node| node.serial);
    Ok(nodes)
}

/// Encodes a collected tree into a snapshot root.
///
/// `read_block` supplies `[offset, offset + len)` of a file's bytes; it is a parameter so
/// the traversal above stays testable against any view and the overlay-specific content
/// API stays out of this module.
///
/// # Errors
///
/// Encoder or store failures.
pub(crate) fn encode(
    store: &Store,
    nodes: &[Node],
    profile: Profile,
    scope_id: [u8; 32],
    read_block: impl Fn(&Path, u64, usize) -> WorkspaceResult<Vec<u8>>,
) -> WorkspaceResult<ObjectId> {
    let mut sink = StoreSink::new(store);
    let mut records: BTreeMap<u64, InodeRecord> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        // ADR-006 §9.4: a crash after some objects are published and before any row names
        // them must leave the store consistent — they are unreferenced orphans.
        if index == 0 {
            crate::publish::crash_point("during-materialise");
        }
        let content = if node.is_dir {
            encode_dir(&mut sink, node, profile)?
        } else {
            encode_file(&mut sink, node, profile, &read_block)?
        };
        records.insert(
            node.serial,
            InodeRecord {
                serial: node.serial,
                kind: if node.is_dir {
                    fs_snapshot::Kind::Directory
                } else {
                    fs_snapshot::Kind::File
                },
                size: node.size,
                created_ns: node.created_ns,
                modified_ns: node.modified_ns,
                content,
            },
        );
    }
    let table = publish(&mut sink, |sink| {
        encode_inode_table(&profile, records.into_values(), sink)
    })?;
    publish(&mut sink, |sink| {
        encode_root(
            &profile,
            &RootValue {
                scope_id,
                inode_table: table,
            },
            sink,
        )
    })
}

fn encode_dir(sink: &mut StoreSink<'_>, node: &Node, profile: Profile) -> WorkspaceResult<Content> {
    if node.children.is_empty() {
        return Ok(Content::Empty);
    }
    let entries = node.children.iter().map(|(name, child)| DirEntry {
        name: name.clone(),
        child_serial: *child,
    });
    Ok(
        match publish(sink, |sink| encode_directory(&profile, entries, sink))? {
            Some(oid) => Content::Dir(oid),
            None => Content::Empty,
        },
    )
}

fn encode_file(
    sink: &mut StoreSink<'_>,
    node: &Node,
    profile: Profile,
    read_block: &impl Fn(&Path, u64, usize) -> WorkspaceResult<Vec<u8>>,
) -> WorkspaceResult<Content> {
    let size = node.size;
    if size == 0 {
        return Ok(Content::Empty);
    }
    let block = profile.block_bytes;
    if size < block {
        // ADR-005 §3.2: a file below one block is a Blob **even when every byte is zero**.
        // Empty means "no content", which is reserved for size 0 here and for the
        // all-zero-block case above `block`. Encoding a zero-filled 64-byte file as Empty
        // makes the snapshot invalid, and the decoder says so.
        let index = usize::try_from(size).unwrap_or(usize::MAX);
        let bytes = read_block(&node.path, 0, index)?;
        let object = publish(sink, |sink| sink.put(Role::Bytes, &bytes))?;
        return Ok(Content::Blob(object));
    }
    let mut extents = Vec::new();
    let mut start = 0u64;
    while start < size {
        let end = (start + block).min(size);
        let count = usize::try_from(end - start).unwrap_or(usize::MAX);
        let bytes = read_block(&node.path, start, count)?;
        // All-zero blocks are omitted: "wrote zeros" and "did not write" must produce the
        // same tree, or history independence fails (ADR-005 §3.4).
        if bytes.iter().any(|byte| *byte != 0) {
            let object = publish(sink, |sink| sink.put(Role::Bytes, &bytes))?;
            extents.push(Extent {
                logical_start: start,
                length: end - start,
                object,
                source_offset: 0,
            });
        }
        start = end;
    }
    if extents.is_empty() {
        return Ok(Content::Empty);
    }
    Ok(
        match publish(sink, |sink| {
            encode_extents(&profile, extents.into_iter(), sink)
        })? {
            Some(oid) => Content::Extents(oid),
            None => Content::Empty,
        },
    )
}

fn children_of(view: &impl TreeRead, dir: FileId) -> WorkspaceResult<Vec<(Vec<u8>, FileId)>> {
    let mut all = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = view.read_dir(dir, after.as_deref(), PAGE_LIMIT)?;
        if page.is_empty() {
            return Ok(all);
        }
        after = page.last().map(|(name, _)| name.clone());
        all.extend(page);
    }
}

fn root_path() -> Path {
    Path::parse("/").expect("the root path parses")
}

fn child_path(parent: &Path, name: &[u8]) -> WorkspaceResult<Path> {
    // Names are UTF-8 by contract (ADR-003's name rules reject anything else), so this is
    // a copy, not a lossy conversion — and `parse` re-validates the result.
    let name = String::from_utf8_lossy(name);
    let text = if parent.is_root() {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.display())
    };
    Path::parse(&text).map_err(|error| WorkspaceError::Inconsistent {
        detail: format!("child path {text:?}: {error}"),
    })
}
