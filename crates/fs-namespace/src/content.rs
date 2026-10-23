//! File content references: `create`, size/content updates and extent rows
//! (ADR-004 §4, §5).
//!
//! This module never touches object bytes. It records *references* to objects that
//! the caller has already published through [`cas_store::Store`], which is what keeps
//! the ordering invariant: the namespace can only point at an object that exists.
//!
//! # Why these are inherent methods, not part of [`crate::TreeStore`]
//!
//! The overlay backend represents a file's bytes as pages over a base with a
//! `base_limit`; it has no `content_root` and no extent rows. A trait demanding them
//! would force the overlay to imitate P1's tables, which is the thing it exists to
//! avoid (ADR-006 §7.1). So content stays on the table backend and disappears with it
//! in P2a-7.
//!
//! The path rules copy of ADR-003 §3 change only *name bindings* and *inode
//! existence*; nothing here is reachable from [`crate::Rules`], which is what lets the
//! rules compile for a backend without any content concept at all.

use rusqlite::{Connection, OptionalExtension};

use cas_core::ObjectId;

use crate::error::{NamespaceError, NamespaceResult};
use crate::namespace::{now_ns, FileId, Kind};
use crate::path::Path;
use crate::rules::Rules;
use crate::sql_tree::{SqlRead, SqlTree};

/// One extent row: a run of the file's logical bytes held by one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    /// First logical byte covered by this extent.
    pub logical_start: u64,
    /// How many logical bytes it covers.
    pub length: u64,
    /// The object holding them.
    pub object_id: ObjectId,
    /// Where in the object's value the run starts.
    pub source_offset: u64,
}

/// One file's content description, as the namespace records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentState {
    /// The file's logical size.
    pub size: u64,
    /// A single whole-file object, or `None` when the file uses extents.
    pub content_root: Option<ObjectId>,
    /// Extent rows in logical order (empty when `content_root` is set).
    pub extents: Vec<Extent>,
}

impl SqlTree {
    /// Creates an empty regular file at `path` (ADR-004 §4.4).
    ///
    /// # Errors
    ///
    /// `AlreadyExists` when the name is taken, `NotFound` when the parent is missing,
    /// `NotADirectory` when the parent is a file.
    pub(crate) fn create_file(&self, path: &Path, rules: &Rules<SqlTree>) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::AlreadyExists {
                path: path.display(),
            });
        }
        let (parent, name) = rules.resolve_parent(path)?;
        let name = name.into_bytes();
        // The pre-check makes the common case report the documented error without
        // taking the write lock; the insert's own constraint violation covers the
        // case where another process wins the race.
        {
            let connection = self.conn();
            if SqlRead::lookup_impl(&connection, self.name(), parent, &name)?.is_some() {
                return Err(NamespaceError::AlreadyExists {
                    path: path.display(),
                });
            }
        }
        self.with_content_tx(|connection| {
            let id = crate::sql_tree_store::create_inode_on(connection, self.name(), Kind::File)?;
            crate::sql_tree_store::bind_on(connection, self.name(), parent, &name, id).map_err(
                |error| match error {
                    NamespaceError::AlreadyExists { .. } => NamespaceError::AlreadyExists {
                        path: path.display(),
                    },
                    other => other,
                },
            )?;
            self.publish_content(connection, "create")?;
            Ok(())
        })
    }

    /// Reads a regular file's content description.
    ///
    /// # Errors
    ///
    /// `NotFound` when absent, `IsADirectory` when the path is a directory.
    pub(crate) fn content_state(
        &self,
        path: &Path,
        rules: &Rules<SqlTree>,
    ) -> NamespaceResult<ContentState> {
        let id = resolve_file(rules, path)?;
        self.content_state_of(id)
    }

    /// [`content_state`](Self::content_state) by identity.
    ///
    /// # Errors
    ///
    /// `NotFound` when the row is missing.
    pub(crate) fn content_state_of(&self, id: FileId) -> NamespaceResult<ContentState> {
        let connection = self.conn();
        let row: Option<(i64, Option<String>)> = connection
            .query_row(
                "SELECT size, content_root FROM file WHERE namespace = ?1 AND file_id = ?2",
                (self.name(), id.serial() as i64),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((size, content)) = row else {
            return Err(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            });
        };
        let content_root = content.map(parse_object_id).transpose()?;
        let mut statement = connection.prepare(
            "SELECT logical_start, length, object_id, source_offset
             FROM file_extent WHERE namespace = ?1 AND file_id = ?2
             ORDER BY logical_start",
        )?;
        let rows = statement.query_map((self.name(), id.serial() as i64), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut extents = Vec::new();
        for row in rows {
            let (start, length, object, source) = row?;
            extents.push(Extent {
                logical_start: start as u64,
                length: length as u64,
                object_id: parse_object_id(object)?,
                source_offset: source as u64,
            });
        }
        Ok(ContentState {
            size: size as u64,
            content_root,
            extents,
        })
    }

    /// A file's single-object content root, if it has one.
    ///
    /// `stat` needs this because `content_root` is deliberately absent from the tree
    /// contract ([`crate::InodeView`]); the table backend fills it in at the edge.
    ///
    /// # Errors
    ///
    /// `NotFound` when the inode row is missing.
    pub(crate) fn content_root_of(&self, id: FileId) -> NamespaceResult<Option<ObjectId>> {
        let connection = self.conn();
        let row: Option<Option<String>> = connection
            .query_row(
                "SELECT content_root FROM file WHERE namespace = ?1 AND file_id = ?2",
                (self.name(), id.serial() as i64),
                |row| row.get(0),
            )
            .optional()?;
        match row {
            None => Err(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            }),
            Some(content) => content.map(parse_object_id).transpose(),
        }
    }

    /// Replaces a file's content description in one transaction (ADR-004 §5.4).
    ///
    /// The caller must already have published every object referenced here; this
    /// function only records references, so the namespace can never point at a missing
    /// object.
    ///
    /// # Errors
    ///
    /// `NotFound` when the file is missing, `IsADirectory` for a directory, and store
    /// errors from the transaction.
    pub(crate) fn replace_content(
        &self,
        path: &Path,
        op: &str,
        state: &ContentState,
        rules: &Rules<SqlTree>,
    ) -> NamespaceResult<()> {
        let id = resolve_file(rules, path)?;
        self.with_content_tx(|connection| {
            write_file_row(connection, self.name(), id, state)?;
            connection.execute(
                "DELETE FROM file_extent WHERE namespace = ?1 AND file_id = ?2",
                (self.name(), id.serial() as i64),
            )?;
            for extent in &state.extents {
                insert_extent(connection, self.name(), id, extent)?;
            }
            self.publish_content(connection, op)?;
            Ok(())
        })
    }

    /// Updates only the blocks a write replaced, leaving the rest untouched.
    ///
    /// `replaced` holds the new extent rows for the blocks that changed;
    /// `removed_from` drops every extent at or past that logical offset (used on
    /// truncate). Extents below `removed_from` that are not in `replaced` keep their
    /// rows, so a one-block write touches one row instead of rewriting the whole
    /// file's extent list (ADR-004 §5.4).
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`, store errors from the transaction.
    // The eight parameters mirror what the caller has to say about a partial write
    // (which file, which publication label, the new size and root, the rows to
    // replace, where to truncate, and the rules used to resolve the path). Bundling
    // them into a struct would move the same fields one level down for no gain; this
    // signature is the one `fs-api` already calls.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_extents(
        &self,
        path: &Path,
        op: &str,
        size: u64,
        content_root: Option<ObjectId>,
        replaced: &[Extent],
        removed_from: Option<u64>,
        rules: &Rules<SqlTree>,
    ) -> NamespaceResult<()> {
        let id = resolve_file(rules, path)?;
        let state = ContentState {
            size,
            content_root,
            extents: Vec::new(),
        };
        self.with_content_tx(|connection| {
            write_file_row(connection, self.name(), id, &state)?;
            for extent in replaced {
                connection.execute(
                    "DELETE FROM file_extent
                     WHERE namespace = ?1 AND file_id = ?2 AND logical_start = ?3",
                    (self.name(), id.serial() as i64, extent.logical_start as i64),
                )?;
                insert_extent(connection, self.name(), id, extent)?;
            }
            if let Some(from) = removed_from {
                connection.execute(
                    "DELETE FROM file_extent
                     WHERE namespace = ?1 AND file_id = ?2 AND logical_start >= ?3",
                    (self.name(), id.serial() as i64, from as i64),
                )?;
            }
            self.publish_content(connection, op)?;
            Ok(())
        })
    }

    /// A file's size and single-object root, without reading extent rows.
    ///
    /// Used by the write path, which only needs the extents its range touches and must
    /// not load the whole file's extent list (ADR-004 §5.4).
    ///
    /// # Errors
    ///
    /// `NotFound` when the file is missing.
    pub(crate) fn shape_of(
        &self,
        path: &Path,
        rules: &Rules<SqlTree>,
    ) -> NamespaceResult<(u64, Option<ObjectId>)> {
        let id = resolve_file(rules, path)?;
        let connection = self.conn();
        let row: Option<(i64, Option<String>)> = connection
            .query_row(
                "SELECT size, content_root FROM file WHERE namespace = ?1 AND file_id = ?2",
                (self.name(), id.serial() as i64),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((size, content)) = row else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        Ok((size as u64, content.map(parse_object_id).transpose()?))
    }

    /// The extents overlapping `[from, to)`, in logical order.
    ///
    /// Only rows that can overlap the range come back, so a small read or a one-block
    /// write costs O(rows returned), not O(file).
    ///
    /// # Errors
    ///
    /// `NotFound`, or a stored object id that is not a valid identity.
    pub(crate) fn extents_in_range(
        &self,
        path: &Path,
        from: u64,
        to: u64,
        rules: &Rules<SqlTree>,
    ) -> NamespaceResult<Vec<Extent>> {
        let id = resolve_file(rules, path)?;
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT logical_start, length, object_id, source_offset
             FROM file_extent
             WHERE namespace = ?1 AND file_id = ?2
               AND logical_start < ?4
               AND logical_start + length > ?3
             ORDER BY logical_start",
        )?;
        let rows = statement.query_map(
            (self.name(), id.serial() as i64, from as i64, to as i64),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
        let mut extents = Vec::new();
        for row in rows {
            let (start, length, object, source) = row?;
            extents.push(Extent {
                logical_start: start as u64,
                length: length as u64,
                object_id: parse_object_id(object)?,
                source_offset: source as u64,
            });
        }
        Ok(extents)
    }

    /// Inode rows that no directory entry reaches (P2-0a).
    ///
    /// The root inode is reached implicitly, so it is never reported.
    ///
    /// # Errors
    ///
    /// Store errors from reading the tables.
    pub(crate) fn unreachable_inodes(&self) -> NamespaceResult<Vec<u64>> {
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT f.file_id FROM file f
             WHERE f.namespace = ?1
               AND f.file_id != 0
               AND NOT EXISTS (
                   SELECT 1 FROM directory_entry e
                   WHERE e.namespace = f.namespace AND e.child_id = f.file_id
               )
             ORDER BY f.file_id",
        )?;
        let rows = statement.query_map([self.name()], |row| row.get::<_, i64>(0))?;
        let mut found = Vec::new();
        for row in rows {
            found.push(row? as u64);
        }
        Ok(found)
    }

    /// Directory entries whose child has no inode row (P2-0a).
    ///
    /// # Errors
    ///
    /// Store errors from reading the tables.
    pub(crate) fn entries_without_inode(&self) -> NamespaceResult<Vec<(u64, String)>> {
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT e.parent_id, e.name FROM directory_entry e
             WHERE e.namespace = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM file f
                   WHERE f.namespace = e.namespace AND f.file_id = e.child_id
               )
             ORDER BY e.parent_id, e.name",
        )?;
        let rows = statement.query_map([self.name()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut found = Vec::new();
        for row in rows {
            let (parent, name) = row?;
            found.push((parent as u64, String::from_utf8_lossy(&name).into_owned()));
        }
        Ok(found)
    }

    /// Extent rows whose owning inode does not exist (P2-0a).
    ///
    /// # Errors
    ///
    /// Store errors from reading the tables.
    pub(crate) fn extents_without_inode(&self) -> NamespaceResult<Vec<(u64, u64)>> {
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT x.file_id, x.logical_start FROM file_extent x
             WHERE x.namespace = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM file f
                   WHERE f.namespace = x.namespace AND f.file_id = x.file_id
               )
             ORDER BY x.file_id, x.logical_start",
        )?;
        let rows = statement.query_map([self.name()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut found = Vec::new();
        for row in rows {
            let (file, start) = row?;
            found.push((file as u64, start as u64));
        }
        Ok(found)
    }

    /// Every object this namespace references, deduplicated (ADR-004 §7).
    ///
    /// # Errors
    ///
    /// Store errors from reading the tables.
    pub(crate) fn referenced_objects(&self) -> NamespaceResult<Vec<ObjectId>> {
        let connection = self.conn();
        let mut found = Vec::new();
        let mut statement = connection.prepare(
            "SELECT content_root FROM file
             WHERE namespace = ?1 AND content_root IS NOT NULL
             UNION
             SELECT object_id FROM file_extent WHERE namespace = ?1",
        )?;
        let rows = statement.query_map([self.name()], |row| row.get::<_, String>(0))?;
        for row in rows {
            found.push(parse_object_id(row?)?);
        }
        Ok(found)
    }
}

/// Resolves `path` to a regular file's identity.
fn resolve_file(rules: &Rules<SqlTree>, path: &Path) -> NamespaceResult<FileId> {
    // `stat` already resolves the path and reports `NotFound`; only the kind check
    // is left, so this stays a two-line wrapper rather than a second walk.
    let stat = rules.stat(path)?;
    match stat.kind {
        Kind::Directory => Err(NamespaceError::IsADirectory {
            path: path.display(),
        }),
        Kind::File => Ok(stat.id),
    }
}

/// Parses a stored object id, reporting a damaged row rather than panicking.
fn parse_object_id(text: String) -> NamespaceResult<ObjectId> {
    text.parse::<ObjectId>()
        .map_err(|error| NamespaceError::InvalidRecord {
            field: "file.content_root",
            detail: error.to_string(),
        })
}

/// Updates the inode row's content columns, failing when the inode is gone.
///
/// The affected-row count **is** the check. The content paths resolve the path
/// before opening their transaction, so a concurrent `unlink` can remove the inode in
/// between; without this guard the `UPDATE` would silently match nothing and the
/// caller would go on to insert extent rows for an inode that no longer exists,
/// leaving rows `verify` reports as `ExtentWithoutInode`.
///
/// Returning `NotFound` is what ADR-004 §4 already promises for a write to a path
/// that is not there.
fn write_file_row(
    connection: &Connection,
    namespace: &str,
    id: FileId,
    state: &ContentState,
) -> NamespaceResult<()> {
    let updated = connection.execute(
        "UPDATE file SET size = ?1, content_root = ?2, modified_ns = ?3
         WHERE namespace = ?4 AND file_id = ?5",
        (
            state.size as i64,
            state.content_root.map(|oid| oid.to_string()),
            now_ns(),
            namespace,
            id.serial() as i64,
        ),
    )?;
    if updated == 0 {
        return Err(NamespaceError::NotFound {
            path: format!("{}", id.serial()),
        });
    }
    Ok(())
}

fn insert_extent(
    connection: &Connection,
    namespace: &str,
    id: FileId,
    extent: &Extent,
) -> NamespaceResult<()> {
    connection.execute(
        "INSERT INTO file_extent
         (namespace, file_id, logical_start, length, object_id, source_offset)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            namespace,
            id.serial() as i64,
            extent.logical_start as i64,
            extent.length as i64,
            extent.object_id.to_string(),
            extent.source_offset as i64,
        ),
    )?;
    Ok(())
}
