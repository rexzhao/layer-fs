//! A bounded memo of canonical objects for one comparison.
//!
//! A comparison looks up many serials and names, and every lookup descends its
//! B-tree from the root. Without a memo each descent re-reads the upper pages,
//! so a lookup costs one more read per level, and the levels grow with the tree.
//! The memo serves a page the comparison already read, so the shared upper
//! pages are read once and a lookup's own work is its leaf. The memo holds at
//! most [`MEMO_PAGES`] objects. When full it evicts the objects used least
//! recently (#352 014.a1: emptying wholesale made a comparison that touches
//! more than [`MEMO_PAGES`] objects re-read every upper page after each reset).
//! The upper pages serve every descent, so they stay. It is a bound, not a
//! store, and a re-read after an eviction is still correct.

use std::cell::RefCell;
use std::collections::HashMap;

use layerfs_telemetry::timer::TimingScope;

use crate::error::{ContentError, ContentResult};
use crate::object::{AuthenticatedObjects, ObjectId};

/// Objects one comparison's memo holds before it empties.
pub const MEMO_PAGES: usize = 256;

/// A provider that serves repeated demands from memory.
pub struct Memo<'a> {
    inner: &'a dyn AuthenticatedObjects,
    /// Each object with the demand that used it last.
    pages: RefCell<HashMap<ObjectId, (Vec<u8>, u64)>>,
    /// Demands served so far, the recency clock.
    clock: RefCell<u64>,
}

impl<'a> Memo<'a> {
    /// A memo over `inner`, empty.
    pub fn new(inner: &'a dyn AuthenticatedObjects) -> Self {
        Self {
            inner,
            pages: RefCell::new(HashMap::new()),
            clock: RefCell::new(0),
        }
    }

    fn serve(
        &self,
        ids: &[ObjectId],
        read: impl FnOnce(&[ObjectId]) -> ContentResult<Vec<Vec<u8>>>,
    ) -> ContentResult<Vec<Vec<u8>>> {
        let mut misses: Vec<ObjectId> = {
            let pages = self.pages.borrow();
            ids.iter()
                .filter(|id| !pages.contains_key(id))
                .copied()
                .collect()
        };
        misses.sort_unstable();
        misses.dedup();
        let mut fetched: HashMap<ObjectId, Vec<u8>> = HashMap::new();
        if !misses.is_empty() {
            let values = read(&misses)?;
            if values.len() != misses.len() {
                return Err(ContentError::BatchCardinality {
                    requested: misses.len(),
                    returned: values.len(),
                });
            }
            fetched = misses.into_iter().zip(values).collect();
        }
        let now = {
            let mut clock = self.clock.borrow_mut();
            *clock += 1;
            *clock
        };
        let mut pages = self.pages.borrow_mut();
        let out = ids
            .iter()
            .map(|id| match fetched.get(id) {
                Some(bytes) => Ok(bytes.clone()),
                None => pages
                    .get_mut(id)
                    .map(|(bytes, used)| {
                        *used = now;
                        bytes.clone()
                    })
                    .ok_or(ContentError::BatchCardinality {
                        requested: ids.len(),
                        returned: 0,
                    }),
            })
            .collect::<ContentResult<Vec<_>>>()?;
        if fetched.len() <= MEMO_PAGES {
            let excess = (pages.len() + fetched.len()).saturating_sub(MEMO_PAGES);
            if excess > 0 {
                let mut by_age: Vec<(u64, ObjectId)> =
                    pages.iter().map(|(id, (_, used))| (*used, *id)).collect();
                by_age.sort_unstable();
                for (_, id) in by_age.into_iter().take(excess) {
                    pages.remove(&id);
                }
            }
            pages.extend(fetched.into_iter().map(|(id, bytes)| (id, (bytes, now))));
        }
        Ok(out)
    }
}

impl AuthenticatedObjects for Memo<'_> {
    fn read_canonical_batch(&self, ids: &[ObjectId]) -> ContentResult<Vec<Vec<u8>>> {
        self.serve(ids, |misses| self.inner.read_canonical_batch(misses))
    }

    fn read_canonical_batch_scoped(
        &self,
        ids: &[ObjectId],
        scope: TimingScope<'_>,
    ) -> ContentResult<Vec<Vec<u8>>> {
        self.serve(ids, |misses| {
            self.inner.read_canonical_batch_scoped(misses, scope)
        })
    }
}
