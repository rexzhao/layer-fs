//! Ordered difference of two immutable sorted trees that skips shared pages.
//!
//! Both trees are content-addressed B+trees in one canonical page grammar, so a
//! child page with the same identity on both sides holds the same rows. The
//! walk keeps one ordered frontier per side and expands a page only when its
//! identity differs from the page facing it. Unchanged subtrees are never read:
//! the pages read grow with the changed rows times the tree depth, not with the
//! size of either tree. Resident state is the two frontiers, bounded by depth
//! times page fan-out.

use std::collections::VecDeque;

use crate::error::{ContentError, ContentResult};
use crate::filesystem::limits::MAXIMUM_TREE_LEVEL;
use crate::filesystem::sorted::format::Format;
use crate::object::{AuthenticatedObjects, ObjectId};

/// Work one difference walk performed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DifferenceWork {
    /// Pages read and decoded.
    pub pages_read: u64,
    /// Pages skipped because both sides held the same identity.
    pub pages_shared: u64,
    /// Rows compared.
    pub rows_compared: u64,
}

impl DifferenceWork {
    /// Adds another walk's counts.
    pub fn add(&mut self, other: Self) {
        self.pages_read = self.pages_read.saturating_add(other.pages_read);
        self.pages_shared = self.pages_shared.saturating_add(other.pages_shared);
        self.rows_compared = self.rows_compared.saturating_add(other.rows_compared);
    }
}

enum Item<K, V> {
    Page { id: ObjectId, level: u8 },
    Row { key: K, value: V },
}

/// Calls `visit(key, left, right)` once for every key whose value differs,
/// in key order. An absent tree (`None`) is empty.
pub(crate) fn difference<F: Format>(
    reader: &dyn AuthenticatedObjects,
    left: Option<ObjectId>,
    right: Option<ObjectId>,
    work: &mut DifferenceWork,
    mut visit: impl FnMut(&F::Key, Option<&F::Value>, Option<&F::Value>) -> ContentResult<()>,
) -> ContentResult<()>
where
    F::Value: PartialEq,
{
    let start = |root: Option<ObjectId>| {
        root.map(|id| Item::Page {
            id,
            level: MAXIMUM_TREE_LEVEL,
        })
        .into_iter()
        .collect::<VecDeque<Item<F::Key, F::Value>>>()
    };
    let (mut l, mut r) = (start(left), start(right));
    loop {
        let expand = match (l.front(), r.front()) {
            (None, None) => return Ok(()),
            (Some(Item::Page { id: a, .. }), Some(Item::Page { id: b, .. })) if a == b => {
                l.pop_front();
                r.pop_front();
                work.pages_shared = work.pages_shared.saturating_add(1);
                continue;
            }
            (Some(Item::Page { level: a, .. }), Some(Item::Page { level: b, .. })) => {
                (a >= b, b >= a)
            }
            (Some(Item::Page { .. }), _) => (true, false),
            (_, Some(Item::Page { .. })) => (false, true),
            (Some(Item::Row { key: a, value: x }), Some(Item::Row { key: b, value: y })) => {
                work.rows_compared = work.rows_compared.saturating_add(1);
                match a.cmp(b) {
                    std::cmp::Ordering::Less => {
                        visit(a, Some(x), None)?;
                        l.pop_front();
                    }
                    std::cmp::Ordering::Greater => {
                        visit(b, None, Some(y))?;
                        r.pop_front();
                    }
                    std::cmp::Ordering::Equal => {
                        if x != y {
                            visit(a, Some(x), Some(y))?;
                        }
                        l.pop_front();
                        r.pop_front();
                    }
                }
                continue;
            }
            (Some(Item::Row { key, value }), None) => {
                visit(key, Some(value), None)?;
                l.pop_front();
                continue;
            }
            (None, Some(Item::Row { key, value })) => {
                visit(key, None, Some(value))?;
                r.pop_front();
                continue;
            }
        };
        if expand.0 {
            open::<F>(reader, &mut l, work)?;
        }
        if expand.1 {
            open::<F>(reader, &mut r, work)?;
        }
    }
}

/// Replaces the front page with its rows or child pages, in key order.
fn open<F: Format>(
    reader: &dyn AuthenticatedObjects,
    side: &mut VecDeque<Item<F::Key, F::Value>>,
    work: &mut DifferenceWork,
) -> ContentResult<()> {
    let Some(Item::Page { id, .. }) = side.pop_front() else {
        return Err(ContentError::InvalidRecord("difference frontier"));
    };
    let wire = F::decode(&reader.read_canonical(id)?)?;
    work.pages_read = work.pages_read.saturating_add(1);
    for entry in wire.entries.into_iter().rev() {
        let item = match (entry.child, entry.value) {
            (Some(child), None) if wire.level > 0 => Item::Page {
                id: child,
                level: wire.level - 1,
            },
            (None, Some(value)) if wire.level == 0 => Item::Row {
                key: entry.key,
                value,
            },
            _ => return Err(ContentError::InvalidRecord("sorted page row")),
        };
        side.push_front(item);
    }
    Ok(())
}
