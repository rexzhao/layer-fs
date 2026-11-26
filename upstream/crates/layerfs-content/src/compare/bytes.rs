//! Whether two file roots hold the same bytes.
//!
//! A file's canonical root depends on how it was built, not only on its bytes
//! (handbook §2): a localized edit keeps its predecessor's extents, so a file
//! edited to other bytes and back holds its old bytes under another root
//! (#352 F-073). Unequal roots prove nothing. This compares the bytes,
//! skipping each extent both files take from the same chunk at the same
//! offset, and stops at the first window that differs.

use layerfs_telemetry::timer::{Active, TimingScope};

use crate::error::{ContentError, ContentResult};
use crate::file::{Extent, ExtentCursor, FileView};
use crate::object::inode_leaf::{InodeKind, InodeValue};
use crate::object::{AuthenticatedObjects, ObjectId};

/// Bytes read from each side per comparison.
const WINDOW: u64 = 256 * 1024;

/// Whether the files at `left` and `right` hold the same bytes. Equal roots
/// answer at once; different lengths after one root read each.
pub fn same_file_bytes(
    reader: &dyn AuthenticatedObjects,
    left: ObjectId,
    right: ObjectId,
    scope: TimingScope<'_>,
) -> ContentResult<bool> {
    same_file_bytes_within(reader, left, right, u64::MAX, scope)
}

/// [`same_file_bytes`] for files of at most `limit` bytes. Larger files
/// with different roots answer `false` after the two root reads, as a
/// root comparison would: a caller with a bounded hold (the publication
/// lane) keeps its work bounded.
pub fn same_file_bytes_within(
    reader: &dyn AuthenticatedObjects,
    left: ObjectId,
    right: ObjectId,
    limit: u64,
    scope: TimingScope<'_>,
) -> ContentResult<bool> {
    if left == right {
        return Ok(true);
    }
    scope.run(|scope| {
        let a = FileView::open(reader, left, scope.child("compare.open"))?;
        let b = FileView::open(reader, right, scope.child("compare.open"))?;
        let length = a.logical_len();
        if length != b.logical_len() || length > limit {
            return Ok(false);
        }
        let sides = Sides {
            reader,
            a: &a,
            b: &b,
            scope,
        };
        let (mut x, mut y) = (a.extents(), b.extents());
        let (mut p, mut q) = (sides.step(&mut x)?, sides.step(&mut y)?);
        // `from..at` is read but not yet compared; `at` is where both sides stand.
        let (mut from, mut at) = (0u64, 0u64);
        while at < length {
            let bound = |e: Option<Extent>| {
                e.map_or(length, |e| {
                    if e.offset > at {
                        e.offset
                    } else {
                        e.offset + u64::from(e.length)
                    }
                })
            };
            let end = bound(p).min(bound(q)).min(length);
            if end <= at {
                return Err(ContentError::InvalidRecord("file extents out of order"));
            }
            let shared = matches!((p, q), (Some(e), Some(f)) if e == f && e.offset == at);
            if shared {
                if from < at && !sides.same(from..at)? {
                    return Ok(false);
                }
                from = end;
            } else if end - from >= WINDOW {
                if !sides.same(from..end)? {
                    return Ok(false);
                }
                from = end;
            }
            at = end;
            if p.is_some_and(|e| e.offset + u64::from(e.length) <= at) {
                p = sides.step(&mut x)?;
            }
            if q.is_some_and(|e| e.offset + u64::from(e.length) <= at) {
                q = sides.step(&mut y)?;
            }
        }
        Ok(from >= length || sides.same(from..length)?)
    })
}

/// Whether two inode values hold the same content: the same kind and root,
/// or two regular files of at most `limit` bytes whose different roots hold
/// the same bytes. Other kinds compare their roots.
pub fn same_content(
    reader: &dyn AuthenticatedObjects,
    left: &InodeValue,
    right: &InodeValue,
    limit: u64,
    scope: TimingScope<'_>,
) -> ContentResult<bool> {
    if left.kind != right.kind {
        return Ok(false);
    }
    if left.content_root == right.content_root {
        return Ok(true);
    }
    if left.kind != InodeKind::RegularFile {
        return Ok(false);
    }
    same_file_bytes_within(reader, left.content_root, right.content_root, limit, scope)
}

struct Sides<'a, 's> {
    reader: &'a dyn AuthenticatedObjects,
    a: &'a FileView,
    b: &'a FileView,
    scope: &'a TimingScope<'s, Active>,
}

impl Sides<'_, '_> {
    /// The next extent of a chunked side; a whole-file side has none.
    fn step(&self, cursor: &mut Option<ExtentCursor>) -> ContentResult<Option<Extent>> {
        match cursor {
            Some(cursor) => cursor.next(self.reader, self.scope),
            None => Ok(None),
        }
    }

    /// Whether both sides hold the same bytes in `range`, one window at a time.
    fn same(&self, range: std::ops::Range<u64>) -> ContentResult<bool> {
        let (mut left, mut right) = (Vec::new(), Vec::new());
        let mut start = range.start;
        while start < range.end {
            let end = range.end.min(start + WINDOW);
            left.clear();
            right.clear();
            self.a
                .read_range(self.reader, start..end, &mut left, self.scope)?;
            self.b
                .read_range(self.reader, start..end, &mut right, self.scope)?;
            if left != right {
                return Ok(false);
            }
            start = end;
        }
        Ok(true)
    }
}
