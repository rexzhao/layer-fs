//! The reader: object graph back to records, one page at a time (ADR-005 §6).
//!
//! Every lookup costs O(tree depth) object fetches and never materialises the whole
//! tree. That bound is the reason a snapshot can be read without a table: the
//! structure lives entirely in the objects, and the depth is ⌈log₆₄ N⌉.

use cas_core::{Object, ObjectId, Role};
use cas_store::Store;

use crate::bytes::Reader;
use crate::error::{malformed, SnapshotError, SnapshotResult};
use crate::profile::Profile;
use crate::record::{Content, DirEntry, Extent, InodeRecord, Kind, RootValue};

/// Reads a snapshot from a store.
#[derive(Debug)]
pub struct SnapshotReader<'a> {
    store: &'a Store,
    profile: Profile,
    root: RootValue,
}

impl<'a> SnapshotReader<'a> {
    /// Opens a snapshot by its root object id.
    ///
    /// Checks the role, the value format and the profile id before anything else, so
    /// a root from another format is refused rather than misread.
    ///
    /// # Errors
    ///
    /// `WrongRole`, `UnsupportedFormat`, `UnsupportedProfile`, `Malformed`, or a store
    /// failure.
    pub fn open(store: &'a Store, root: ObjectId, profile: Profile) -> SnapshotResult<Self> {
        let value = fetch(store, root, Role::FilesystemRoot, "filesystem root")?;
        let mut reader = Reader::new(&value);
        reader.format(profile.fmt)?;
        let found = reader.take(32)?;
        let expected = profile.id();
        if found != expected {
            return Err(SnapshotError::UnsupportedProfile { found: hex(found) });
        }
        let scope = reader.take(32)?;
        let mut scope_id = [0u8; 32];
        scope_id.copy_from_slice(scope);
        let _root_serial = reader.u64()?;
        let inode_table = reader.oid()?;
        reader.finish()?;
        Ok(Self {
            store,
            profile,
            root: RootValue {
                scope_id,
                inode_table,
            },
        })
    }

    /// The snapshot's scope id (ADR-005 §3.1).
    #[must_use]
    pub fn scope_id(&self) -> [u8; 32] {
        self.root.scope_id
    }

    /// The root directory's serial, which is always 0.
    #[must_use]
    pub fn root_serial(&self) -> u64 {
        0
    }

    /// Looks up one inode by serial.
    ///
    /// # Errors
    ///
    /// Decode failures, or a store failure.
    pub fn inode(&self, serial: u64) -> SnapshotResult<Option<InodeRecord>> {
        let mut pointer = self.root.inode_table;
        // Descend until a leaf, then search it.
        loop {
            let page = fetch(self.store, pointer, Role::InodeTable, "inode table")?;
            let mut reader = Reader::new(&page);
            reader.format(self.profile.fmt)?;
            // The fourth and last caller: `lookup`, `read_dir` and `extents_in_range`
            // already bound the page, and this one was missed (P2a-1-QA-2).
            check_page_size(&page, &self.profile)?;
            let node = reader.u8()?;
            match node {
                0 => return self.find_inode_in_leaf(&mut reader, serial),
                1 => {
                    let height = reader.u8()?;
                    if height == 0 {
                        return Err(malformed("an inode branch height must be at least 1"));
                    }
                    pointer = self.descend_inode_branch(&mut reader, serial)?;
                }
                other => {
                    return Err(malformed(format!("unknown inode page node byte {other}")));
                }
            }
        }
    }

    fn find_inode_in_leaf(
        &self,
        reader: &mut Reader<'_>,
        serial: u64,
    ) -> SnapshotResult<Option<InodeRecord>> {
        let count = reader.u32()?;
        let mut previous: Option<u64> = None;
        for _ in 0..count {
            let record = decode_inode(reader, self.profile)?;
            if let Some(previous) = previous {
                if record.serial <= previous {
                    return Err(malformed(format!(
                        "inode serials are not strictly increasing: {} after {previous}",
                        record.serial
                    )));
                }
            }
            previous = Some(record.serial);
            if record.serial == serial {
                return Ok(Some(record));
            }
            if record.serial > serial {
                return Ok(None);
            }
        }
        reader.finish()?;
        Ok(None)
    }

    fn descend_inode_branch(
        &self,
        reader: &mut Reader<'_>,
        serial: u64,
    ) -> SnapshotResult<ObjectId> {
        let count = reader.u32()?;
        if count == 0 {
            return Err(malformed("an inode branch may not be empty"));
        }
        let mut chosen: Option<ObjectId> = None;
        let mut previous: Option<u64> = None;
        for _ in 0..count {
            let first_serial = reader.u64()?;
            let child = reader.oid()?;
            let _subtree_count = reader.u64()?;
            if let Some(previous) = previous {
                if first_serial <= previous {
                    return Err(malformed(format!(
                        "branch first keys are not increasing: {first_serial} after {previous}"
                    )));
                }
            }
            previous = Some(first_serial);
            // The child covering `serial` is the last one whose first key is <= it.
            if first_serial <= serial {
                chosen = Some(child);
            }
        }
        reader.finish()?;
        chosen.ok_or_else(|| {
            malformed(format!(
                "serial {serial} precedes the first key of every branch entry"
            ))
        })
    }

    /// Looks up a name in a directory.
    ///
    /// # Errors
    ///
    /// Decode failures, or a store failure.
    pub fn lookup(&self, dir: &InodeRecord, name: &[u8]) -> SnapshotResult<Option<u64>> {
        let root = match dir.content {
            Content::Dir(root) => root,
            // An empty directory is a legal inode (`Directory + Empty`, ADR-005 §3.2 —
            // "不为空目录生成空页"), so looking into one finds nothing rather than being a
            // malformed value. Only a non-directory content here is a real error.
            Content::Empty if dir.kind == Kind::Directory => return Ok(None),
            _ => return Err(malformed("lookup needs an inode whose content is Dir")),
        };
        let mut pointer = root;
        loop {
            // Resolve the role **before** fetching. A directory page is a leaf or a
            // branch and the two carry different roles, so a `fetch` pinned to
            // `DirectoryLeaf` rejects every branch page and leaves the branch arm
            // below unreachable (P2a-1-QA-1). `read_dir` and `extents_in_range`
            // already worked this way; this one did not.
            let role = role_of(self.store, pointer)?;
            let page = fetch(self.store, pointer, role, "directory")?;
            let mut reader = Reader::new(&page);
            reader.format(self.profile.fmt)?;
            check_page_size(&page, &self.profile)?;
            match role {
                Role::DirectoryLeaf => {
                    let count = reader.u32()?;
                    let mut previous: Option<Vec<u8>> = None;
                    for _ in 0..count {
                        let entry_name = reader.name(self.profile.name_max)?;
                        let child = reader.u64()?;
                        if let Some(previous) = &previous {
                            if entry_name <= *previous {
                                return Err(malformed(
                                    "directory names are not strictly increasing",
                                ));
                            }
                        }
                        previous = Some(entry_name.clone());
                        if entry_name == name {
                            return Ok(Some(child));
                        }
                        if entry_name.as_slice() > name {
                            return Ok(None);
                        }
                    }
                    reader.finish()?;
                    return Ok(None);
                }
                Role::DirectoryBranch => {
                    let height = reader.u8()?;
                    if height == 0 {
                        return Err(malformed("a directory branch height must be at least 1"));
                    }
                    // A name that sorts before the branch's first key simply is not in
                    // this directory — the leaf arm already answers `Ok(None)` for that
                    // case, and a lookup of a missing name must not depend on which level
                    // of the tree happened to be reached.
                    match self.descend_name_branch(&mut reader, name)? {
                        Some(child) => pointer = child,
                        None => return Ok(None),
                    }
                }
                other => {
                    return Err(SnapshotError::WrongRole {
                        expected: "directory",
                        found: other.name(),
                    })
                }
            }
        }
    }

    /// The child page that would hold `name`, or `None` when `name` sorts before every
    /// entry of this branch.
    fn descend_name_branch(
        &self,
        reader: &mut Reader<'_>,
        name: &[u8],
    ) -> SnapshotResult<Option<ObjectId>> {
        let count = reader.u32()?;
        if count == 0 {
            return Err(malformed("a directory branch may not be empty"));
        }
        let mut chosen: Option<ObjectId> = None;
        let mut previous: Option<Vec<u8>> = None;
        for _ in 0..count {
            let first_name = reader.name(self.profile.name_max)?;
            let child = reader.oid()?;
            let _subtree_count = reader.u64()?;
            if let Some(previous) = &previous {
                if first_name <= *previous {
                    return Err(malformed("branch first names are not increasing"));
                }
            }
            previous = Some(first_name.clone());
            if first_name.as_slice() <= name {
                chosen = Some(child);
            }
        }
        reader.finish()?;
        Ok(chosen)
    }

    /// Lists a directory's entries in order, starting after `after`.
    ///
    /// # Errors
    ///
    /// Decode failures, or a store failure.
    pub fn read_dir(
        &self,
        dir: &InodeRecord,
        after: Option<&[u8]>,
        limit: usize,
    ) -> SnapshotResult<Vec<DirEntry>> {
        let root = match dir.content {
            Content::Dir(root) => root,
            // An empty directory has no page tree at all (ADR-005 §3.2), so it lists
            // nothing — not an error.
            Content::Empty if dir.kind == Kind::Directory => return Ok(Vec::new()),
            _ => return Err(malformed("read_dir needs an inode whose content is Dir")),
        };
        let mut out = Vec::new();
        self.collect_dir(root, after, limit, &mut out)?;
        Ok(out)
    }

    fn collect_dir(
        &self,
        page: ObjectId,
        after: Option<&[u8]>,
        limit: usize,
        out: &mut Vec<DirEntry>,
    ) -> SnapshotResult<()> {
        let role = role_of(self.store, page)?;
        let value = fetch(self.store, page, role, "directory")?;
        let mut reader = Reader::new(&value);
        reader.format(self.profile.fmt)?;
        check_page_size(&value, &self.profile)?;
        match role {
            Role::DirectoryLeaf => {
                let count = reader.u32()?;
                for _ in 0..count {
                    if out.len() >= limit {
                        return Ok(());
                    }
                    let name = reader.name(self.profile.name_max)?;
                    let child = reader.u64()?;
                    if let Some(after) = after {
                        if name.as_slice() <= after {
                            continue;
                        }
                    }
                    out.push(DirEntry {
                        name,
                        child_serial: child,
                    });
                }
                Ok(())
            }
            Role::DirectoryBranch => {
                let _height = reader.u8()?;
                let count = reader.u32()?;
                let mut children = Vec::new();
                for _ in 0..count {
                    let _first = reader.name(self.profile.name_max)?;
                    children.push(reader.oid()?);
                    let _subtree = reader.u64()?;
                }
                for child in children {
                    if out.len() >= limit {
                        return Ok(());
                    }
                    self.collect_dir(child, after, limit, out)?;
                }
                Ok(())
            }
            other => Err(SnapshotError::WrongRole {
                expected: "directory",
                found: other.name(),
            }),
        }
    }

    /// Returns the extents overlapping `[from, to)`.
    ///
    /// # Errors
    ///
    /// Decode failures, or a store failure.
    pub fn extents_in_range(
        &self,
        file: &InodeRecord,
        from: u64,
        to: u64,
    ) -> SnapshotResult<Vec<Extent>> {
        let root = match file.content {
            Content::Extents(root) => root,
            // `Empty` covers both a zero-length file and a file whose blocks are all
            // zero (ADR-005 §3.2); either way no extent covers any range.
            Content::Empty if file.kind == Kind::File => return Ok(Vec::new()),
            _ => return Err(malformed("extents_in_range needs Extents content")),
        };
        let mut out = Vec::new();
        self.collect_extents(root, from, to, &mut out)?;
        Ok(out)
    }

    fn collect_extents(
        &self,
        page: ObjectId,
        from: u64,
        to: u64,
        out: &mut Vec<Extent>,
    ) -> SnapshotResult<()> {
        let role = role_of(self.store, page)?;
        let value = fetch(self.store, page, role, "extent tree")?;
        let mut reader = Reader::new(&value);
        reader.format(self.profile.fmt)?;
        check_page_size(&value, &self.profile)?;
        match role {
            Role::ExtentLeaf => {
                let count = reader.u32()?;
                let mut previous_end: Option<u64> = None;
                for _ in 0..count {
                    let start = reader.u64()?;
                    let length = reader.u64()?;
                    if length == 0 {
                        return Err(malformed("an extent may not have zero length"));
                    }
                    // Syntax-level bounds, checked without reading the object pointed
                    // at, so a forged leaf is refused here rather than handed to a
                    // caller as a real extent (P2a-1-QA-1 ①).
                    if length > self.profile.max_extent_len {
                        return Err(malformed(format!(
                            "extent length {length} exceeds the profile limit {}",
                            self.profile.max_extent_len
                        )));
                    }
                    if let Some(previous_end) = previous_end {
                        if start < previous_end {
                            return Err(SnapshotError::Unsorted {
                                detail: format!(
                                    "extent at {start} overlaps the previous extent ending at {previous_end}"
                                ),
                            });
                        }
                    }
                    previous_end = Some(start + length);
                    let object = reader.oid()?;
                    let source_offset = reader.u64()?;
                    // The companion of the length bound above: a run may not read past
                    // the largest legal object value. Checked against the syntax only,
                    // without fetching the object (ADR-005 §3.4).
                    if source_offset.saturating_add(length) > self.profile.max_object_value {
                        return Err(malformed(format!(
                            "extent at {start} reads {} bytes past the object value limit {}",
                            source_offset.saturating_add(length),
                            self.profile.max_object_value
                        )));
                    }
                    if start < to && start + length > from {
                        out.push(Extent {
                            logical_start: start,
                            length,
                            object,
                            source_offset,
                        });
                    }
                }
                Ok(())
            }
            Role::ExtentBranch => {
                let _height = reader.u8()?;
                let count = reader.u32()?;
                let mut children = Vec::new();
                for _ in 0..count {
                    let first_start = reader.u64()?;
                    let child = reader.oid()?;
                    let covered_end = reader.u64()?;
                    if covered_end > from && first_start < to {
                        children.push(child);
                    }
                }
                for child in children {
                    self.collect_extents(child, from, to, out)?;
                }
                Ok(())
            }
            other => Err(SnapshotError::WrongRole {
                expected: "extent",
                found: other.name(),
            }),
        }
    }
}

fn role_of(store: &Store, oid: ObjectId) -> SnapshotResult<Role> {
    let bytes = store.get(oid)?;
    let object = Object::decode(&bytes)?;
    Ok(object.role())
}

fn fetch(
    store: &Store,
    oid: ObjectId,
    expected: Role,
    what: &'static str,
) -> SnapshotResult<Vec<u8>> {
    let bytes = store.get(oid)?;
    let object = Object::decode(&bytes)?;
    if object.role() != expected {
        return Err(SnapshotError::WrongRole {
            expected: match expected {
                Role::FilesystemRoot => "filesystem root",
                Role::InodeTable => "inode table",
                Role::DirectoryLeaf => "directory leaf",
                Role::DirectoryBranch => "directory branch",
                Role::ExtentLeaf => "extent leaf",
                Role::ExtentBranch => "extent branch",
                other => other.name(),
            },
            found: object.role().name(),
        });
    }
    let _ = what;
    Ok(object.value().to_vec())
}

fn decode_inode(reader: &mut Reader<'_>, profile: Profile) -> SnapshotResult<InodeRecord> {
    let serial = reader.u64()?;
    let kind_tag = reader.u8()?;
    let kind = Kind::from_tag(kind_tag)
        .ok_or_else(|| malformed(format!("unknown inode kind {kind_tag}")))?;
    let size = reader.u64()?;
    let created_ns = reader.i64()?;
    let modified_ns = reader.i64()?;
    let content_tag = reader.u8()?;
    let content = match content_tag {
        0 => Content::Empty,
        1 => Content::Blob(reader.oid()?),
        2 => Content::Extents(reader.oid()?),
        3 => Content::Dir(reader.oid()?),
        other => return Err(malformed(format!("unknown content tag {other}"))),
    };
    let record = InodeRecord {
        serial,
        kind,
        size,
        created_ns,
        modified_ns,
        content,
    };
    if let Some(reason) = record.illegal_reason(profile.block_bytes) {
        return Err(malformed(format!("inode {serial}: {reason}")));
    }
    Ok(record)
}

/// Rejects a page whose encoded bytes exceed the profile's limit (P2a-1-QA-1 ②).
///
/// The encoder enforces this bound, but a reader must not trust its input: a forged
/// page claiming more entries than the format allows is a malformed value, and
/// accepting it would let one object dictate unbounded work.
fn check_page_size(page: &[u8], profile: &Profile) -> SnapshotResult<()> {
    if page.len() > profile.max_page_bytes {
        return Err(malformed(format!(
            "page is {} bytes, over the profile limit of {}",
            page.len(),
            profile.max_page_bytes
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}
