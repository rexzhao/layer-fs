//! Mark: the reachable set, and the temporary SQLite that holds it (ADR-008 §6.3, P2b-6 §13).
//!
//! The marked set is the evidence a sweep is judged by, so it must not live in the process: a
//! collector over a large store would grow with the store, which §5.4 forbids. It is therefore a
//! temporary SQLite database **under the store root** — not under `tmp/`, because `tmp/` is swept
//! wholesale (§6.3), so a marker there could be deleted by the very run that is reading it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use cas_core::ObjectId;
use fs_snapshot::SnapshotReader;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::gc::roots::RootSet;
use crate::gc::{GcHooks, GcReport};

/// The marked set, stored in its own SQLite file so it does not grow the process heap.
///
/// One table, one column, the primary key doing the de-duplication: "already marked?" is a key
/// lookup, not a scan, which is what makes the mark loop's `O(1)` per edge claim true.
#[derive(Debug)]
pub struct Marker {
    path: PathBuf,
    connection: rusqlite::Connection,
}

impl Marker {
    /// The file name used under the store root.
    pub const FILE_NAME: &'static str = "gc-mark.sqlite";

    /// Creates the marker beside the store's own databases.
    ///
    /// # Errors
    ///
    /// A SQLite failure, or a store directory that does not exist.
    pub fn create_in(store_dir: impl AsRef<Path>) -> WorkspaceResult<Self> {
        let store_dir = store_dir.as_ref();
        if !store_dir.is_dir() {
            return Err(WorkspaceError::NotFound {
                what: format!("store directory {}", store_dir.display()),
            });
        }
        let path = store_dir.join(Self::FILE_NAME);
        // A leftover from a crashed run must not be mistaken for this run's marks.
        for suffix in ["", "-wal", "-shm"] {
            let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
            if candidate.exists() {
                std::fs::remove_file(&candidate).map_err(|error| WorkspaceError::NotFound {
                    what: format!("a stale marker at {} ({error})", candidate.display()),
                })?;
            }
        }
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = OFF;
             PRAGMA synchronous = OFF;
             CREATE TABLE marked (oid BLOB PRIMARY KEY) WITHOUT ROWID;",
        )?;
        Ok(Self { path, connection })
    }

    /// Where the marker lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records `oid`; returns `true` when it was **not** already there.
    ///
    /// This is the whole of the mark loop's de-duplication: "walk a shared subtree once" is a
    /// property of this set, not of the traversal (P2b-6 §11).
    ///
    /// # Errors
    ///
    /// A SQLite failure.
    pub fn insert(&mut self, oid: ObjectId) -> WorkspaceResult<bool> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO marked (oid) VALUES (?1)",
            [&oid.as_bytes()[..]],
        )?;
        Ok(changed == 1)
    }

    /// Whether `oid` is marked.
    ///
    /// # Errors
    ///
    /// A SQLite failure.
    pub fn contains(&self, oid: ObjectId) -> WorkspaceResult<bool> {
        let found: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM marked WHERE oid = ?1",
            [&oid.as_bytes()[..]],
            |row| row.get(0),
        )?;
        Ok(found == 1)
    }

    /// How many objects are marked.
    ///
    /// # Errors
    ///
    /// A SQLite failure.
    pub fn count(&self) -> WorkspaceResult<usize> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM marked", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    /// Every marked id, ordered (the report carries them; a `Vec` here is the report's own shape).
    ///
    /// # Errors
    ///
    /// A SQLite failure.
    pub fn all(&self) -> WorkspaceResult<Vec<ObjectId>> {
        let mut statement = self
            .connection
            .prepare("SELECT oid FROM marked ORDER BY oid")?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            out.push(object_id_from_bytes(&bytes)?);
        }
        Ok(out)
    }

    /// Closes and deletes the marker, including its `-wal`/`-shm` companions.
    ///
    /// # Errors
    ///
    /// A filesystem failure.
    pub fn remove(self) -> WorkspaceResult<()> {
        let path = self.path.clone();
        drop(self.connection);
        for suffix in ["", "-wal", "-shm"] {
            let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
            if candidate.exists() {
                std::fs::remove_file(&candidate).map_err(|error| WorkspaceError::NotFound {
                    what: format!("the marker at {} ({error})", candidate.display()),
                })?;
            }
        }
        Ok(())
    }
}

fn object_id_from_bytes(bytes: &[u8]) -> WorkspaceResult<ObjectId> {
    // The column is written only by `insert`, always from a real `ObjectId`, so a wrong length
    // means something else edited the marker file: an inconsistency, not a parse failure.
    let Ok(array) = <[u8; 32]>::try_from(bytes) else {
        return Err(WorkspaceError::Inconsistent {
            detail: format!("a marked id of {} bytes is not an object id", bytes.len()),
        });
    };
    Ok(ObjectId::from_bytes(array))
}

/// Walks every root the source yields and every object reachable from them.
///
/// The loop is the ADR's literal algorithm — enqueue roots, pop, skip when already marked, otherwise
/// mark and enqueue the object's direct references:
///
/// ```text
/// for each root: mark(root); push(root)          // roots are marked *before* expansion (§13.1)
/// while let Some(oid) = pop():
///     if already marked: continue
///     mark(oid)
///     push(references(oid))
/// ```
///
/// Two details are load-bearing rather than stylistic:
///
/// - **A root enters the marked set before its children are walked.** An `Empty` root, or a root that
///   references nothing, would otherwise never be inserted, so a criterion that asks "was every
///   declared root reachable?" would pass on a run that visited nothing (G3 false green).
/// - **Nothing is written to objects, packs or index rows.** A decode failure returns immediately
///   with a typed error: a skipped subtree is a deleted subtree (§12), so the only safe response is
///   to abort before any sweep.
///
/// # Errors
///
/// Whatever [`SnapshotReader::references`] reported (a damaged object is an error, never "no
/// children"), or a marker failure.
pub fn mark_roots(
    declared: &RootSet,
    walked: &RootSet,
    reader: &SnapshotReader<'_>,
    marker: &mut Marker,
    hooks: &mut GcHooks<'_>,
) -> WorkspaceResult<GcReport> {
    let mut report = GcReport::skeleton(declared);
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    for entry in walked.entries() {
        // The skip seam answers "should this root be skipped?" and runs **before** traversal, so a
        // skipped root is absent from `roots_visited` and none of its subtree is marked (F).
        #[cfg(feature = "gc-test-hooks")]
        if let Some(skip) = hooks.skip_root.as_mut() {
            if skip(entry) {
                continue;
            }
        }
        report.record_visit(entry);
        if let Some(sink) = hooks.on_root_visited.as_mut() {
            sink(entry.kind, entry.root);
        }
        // §13.1: the root is marked even if it references nothing.
        if marker.insert(entry.root)? {
            queue.push_back(entry.root);
        }
    }

    while let Some(oid) = queue.pop_front() {
        let children = reader.references(oid)?;
        for child in children {
            if marker.insert(child)? {
                queue.push_back(child);
            }
        }
    }

    report.reachable = marker.all()?;
    report.reachable_count = report.reachable.len();
    Ok(report)
}
