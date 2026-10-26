//! The pager: a sorted entry stream becomes a tree of pages (ADR-005 §4).
//!
//! The boundary rule is the whole design: a page ends after entry `e` when either
//! the hash of `e`'s key says so (and the page already holds `min_entries`) or the
//! page would exceed its byte budget. Because the hash depends only on the **key**,
//! the same set of entries always produces the same pages, whatever order they were
//! inserted into the source tree — that is what makes a snapshot's root id history
//! independent.
//!
//! The alternative — cutting every N entries — looks equivalent and is not: inserting
//! one entry shifts every later page, so re-encoding would rewrite the whole tree.

use cas_core::{ObjectId, Role};

use crate::error::SnapshotResult;
use crate::profile::{Profile, TreeDomain};

/// Where pages are published (ADR-005 §8).
///
/// The encoder never touches the store itself, so a caller can collect pages, write
/// them immediately, or batch them.
pub trait PageSink {
    /// Publishes one page value under `role` and returns its object id.
    ///
    /// # Errors
    ///
    /// Whatever the sink cannot do; the encoder only propagates.
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId>;
}

/// One leaf entry: its sort key, and the bytes that follow the key in the page.
///
/// `key` is what the page-split hash sees and what ordering compares. `leaf` is what
/// the page stores after the key — for directories the key *is* the `name` field, so
/// `leaf` holds only `child_serial`; for inodes and extents the page stores the key
/// again, so `leaf` repeats it. Keeping the choice with the caller avoids the pager
/// knowing four different value layouts.
pub(crate) struct Entry {
    /// The raw key bytes used for ordering and for the split hash.
    ///
    /// For directories this is the bare name, **not** the length-prefixed form: the
    /// page-split hash and ordering both work on the key as ADR-005 §3.3 defines it.
    pub(crate) key: Vec<u8>,
    /// The key as a leaf page writes it. Directories store a `name` (length prefix
    /// included); inodes and extents store their fixed-width key, so this equals
    /// `key`.
    pub(crate) page_key: Vec<u8>,
    /// The bytes following the key inside the page.
    pub(crate) leaf: Vec<u8>,
    /// How the key appears in a branch entry at the level above (ADR-005 §3.2–§3.4:
    /// `first_name` is a `name`, `first_serial` and `first_start` are fixed-width
    /// integers).
    pub(crate) branch_key: Vec<u8>,
}

impl Entry {
    /// Bytes this entry adds to a leaf page: its page key plus its payload.
    pub(crate) fn leaf_len(&self) -> usize {
        self.page_key.len() + self.leaf.len()
    }
}

/// How a tree's pages are framed (ADR-005 §3.2–§3.4).
pub(crate) struct Shape {
    pub(crate) domain: TreeDomain,
    pub(crate) leaf_role: Role,
    pub(crate) branch_role: Role,
    /// The inode table carries a `node` byte in both roles; the others do not.
    pub(crate) inode_node_byte: bool,
}

/// A published page at one level, waiting to become an entry at the level above.
struct Child {
    /// The key encoded the way a branch entry needs it.
    branch_key: Vec<u8>,
    /// The same key's raw bytes, for the split hash.
    raw_key: Vec<u8>,
    oid: ObjectId,
    subtree_count: u64,
}

/// Encodes a strictly increasing entry stream into a page tree.
///
/// Returns `None` for an empty stream: emptiness is represented by
/// `Content::Empty`, not by an empty page (ADR-005 §4).
///
/// # Errors
///
/// [`crate::SnapshotError::Unsorted`] when keys are not strictly increasing, or
/// whatever the sink reports.
pub(crate) fn build_tree(
    profile: &Profile,
    shape: &Shape,
    entries: Vec<Entry>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<Option<ObjectId>> {
    if entries.is_empty() {
        return Ok(None);
    }
    check_sorted(&entries)?;

    // ---- leaf level
    let mut level: Vec<Child> = Vec::new();
    let mut page: Vec<Entry> = Vec::new();
    let mut page_bytes = leaf_overhead(profile, shape);
    // ADR-005 §4 condition 2 is a guard *before* appending: a page is closed once
    // adding one more maximal entry could exceed the limit. Checking after the
    // append would let the page overshoot by one entry — which is exactly what the
    // first scale run showed (16,440 bytes against a 16,384 limit).
    let max_entry_bytes = profile.name_max + 8 + 32 + 8 + 8;
    let guard = profile
        .max_page_bytes
        .saturating_sub(max_entry_bytes)
        .max(leaf_overhead(profile, shape));

    for entry in entries {
        let key_says_split = page.len() as u32 >= profile.min_entries
            && profile.is_split_point(shape.domain, 0, &entry.key);

        if key_says_split || page_bytes >= guard {
            level.push(publish_leaf(profile, shape, &mut page, sink)?);
            page_bytes = leaf_overhead(profile, shape);
        }

        page_bytes += entry.leaf_len();
        page.push(entry);
    }
    if !page.is_empty() {
        level.push(publish_leaf(profile, shape, &mut page, sink)?);
    }

    // ---- upper levels, until one page remains: that page is the root
    let mut height: u8 = 1;
    while level.len() > 1 {
        let mut next: Vec<Child> = Vec::new();
        let mut upper: Vec<Child> = Vec::new();
        let mut upper_bytes = branch_overhead(shape);
        // Same pre-append guard as the leaf level: a branch entry is at most a
        // name-length key, its id, and a subtree count.
        let guard = profile
            .max_page_bytes
            .saturating_sub(max_entry_bytes)
            .max(branch_overhead(shape));

        for child in level {
            let entry_bytes = child.branch_key.len() + 32 + 8;
            let key_says_split = upper.len() as u32 >= profile.min_entries
                && profile.is_split_point(shape.domain, height, &child.raw_key);

            if key_says_split || upper_bytes >= guard {
                next.push(publish_branch(profile, shape, height, &mut upper, sink)?);
                upper_bytes = branch_overhead(shape);
            }

            upper_bytes += entry_bytes;
            upper.push(child);
        }
        if !upper.is_empty() {
            next.push(publish_branch(profile, shape, height, &mut upper, sink)?);
        }
        level = next;
        height += 1;
    }

    Ok(Some(level[0].oid))
}

fn check_sorted(entries: &[Entry]) -> SnapshotResult<()> {
    for pair in entries.windows(2) {
        if pair[0].key >= pair[1].key {
            return Err(crate::SnapshotError::Unsorted {
                detail: format!(
                    "key {:?} is not strictly less than the following key {:?}",
                    String::from_utf8_lossy(&pair[0].key),
                    String::from_utf8_lossy(&pair[1].key)
                ),
            });
        }
    }
    Ok(())
}

fn leaf_overhead(profile: &Profile, shape: &Shape) -> usize {
    let _ = profile;
    1 + usize::from(shape.inode_node_byte) + 4
}

fn branch_overhead(shape: &Shape) -> usize {
    1 + usize::from(shape.inode_node_byte) + 1 + 4
}

/// A page's `fmt` byte: inode pages carry `inode_fmt`, every other page carries `fmt` (ADR-005-v3 draft §2.1).
///
/// One function so the writer cannot say "inode page" in one place and "generic page" in another; the reader mirrors it
/// with the same predicate (`shape.inode_node_byte` ⇔ inode table page).
fn page_fmt(profile: &Profile, shape: &Shape) -> u8 {
    if shape.inode_node_byte {
        profile.inode_fmt
    } else {
        profile.fmt
    }
}

fn publish_leaf(
    profile: &Profile,
    shape: &Shape,
    page: &mut Vec<Entry>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<Child> {
    let mut value = Vec::new();
    value.push(page_fmt(profile, shape));
    if shape.inode_node_byte {
        value.push(0); // node = leaf
    }
    value.extend_from_slice(&(page.len() as u32).to_be_bytes());
    for entry in page.iter() {
        value.extend_from_slice(&entry.page_key);
        value.extend_from_slice(&entry.leaf);
    }
    let first = &page[0];
    let child = Child {
        branch_key: first.branch_key.clone(),
        raw_key: first.key.clone(),
        oid: sink.put(shape.leaf_role, &value)?,
        subtree_count: page.len() as u64,
    };
    page.clear();
    Ok(child)
}

fn publish_branch(
    profile: &Profile,
    shape: &Shape,
    height: u8,
    page: &mut Vec<Child>,
    sink: &mut dyn PageSink,
) -> SnapshotResult<Child> {
    let mut value = Vec::new();
    value.push(page_fmt(profile, shape));
    if shape.inode_node_byte {
        value.push(1); // node = branch
    }
    value.push(height);
    value.extend_from_slice(&(page.len() as u32).to_be_bytes());
    let mut total = 0u64;
    for child in page.iter() {
        value.extend_from_slice(&child.branch_key);
        value.extend_from_slice(child.oid.as_bytes());
        value.extend_from_slice(&child.subtree_count.to_be_bytes());
        total += child.subtree_count;
    }
    let child = Child {
        branch_key: page[0].branch_key.clone(),
        raw_key: page[0].raw_key.clone(),
        oid: sink.put(shape.branch_role, &value)?,
        subtree_count: total,
    };
    page.clear();
    Ok(child)
}
