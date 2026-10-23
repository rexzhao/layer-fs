//! File content references: `create`, size/content updates and extent rows
//! (ADR-004 §4, §5).
//!
//! This module never touches object bytes. It records *references* to objects
//! that the caller has already published through [`cas_store::Store`], which is
//! what keeps the ordering invariant: the namespace can only point at an object
//! that exists.

use rusqlite::OptionalExtension;

use cas_core::ObjectId;

use crate::error::{NamespaceError, NamespaceResult};
use crate::namespace::{now_ns, FileId, Kind, Namespace};
use crate::path::Path;

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

impl Namespace {
    /// Creates an empty regular file at `path` (ADR-004 §4.4).
    ///
    /// # Errors
    ///
    /// `AlreadyExists` when the name is taken, `NotFound` when the parent is
    /// missing, `NotADirectory` when the parent is a file.
    pub fn create_file(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::AlreadyExists {
                path: path.display(),
            });
        }
        let parent = self.resolve_parent(path)?;
        let name = path.name().expect("non-root path has a name");
        if self.lookup(parent, name)?.is_some() {
            return Err(NamespaceError::AlreadyExists {
                path: path.display(),
            });
        }
        self.with_write_tx(|_tx| {
            let id = self.next_serial()?;
            let now = now_ns();
            self.insert_entry(parent, name, FileId::from_serial(id))?;
            self.conn().execute(
                "INSERT INTO file
                 (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
                 VALUES (?1, ?2, ?3, 0, NULL, ?4, ?4)",
                (&self.name, id as i64, Kind::File as i64, now),
            )?;
            self.publish_tx("create", None)?;
            Ok(())
        })
    }

    /// Reads a regular file's content description.
    ///
    /// # Errors
    ///
    /// `NotFound` when absent, `IsADirectory` when the path is a directory.
    pub fn content_state(&self, path: &Path) -> NamespaceResult<ContentState> {
        let id = self.resolve(path)?;
        match self.kind_of(id)? {
            Some(Kind::Directory) => {
                return Err(NamespaceError::IsADirectory {
                    path: path.display(),
                })
            }
            None => {
                return Err(NamespaceError::NotFound {
                    path: path.display(),
                })
            }
            Some(Kind::File) => {}
        }
        self.content_state_of(id)
    }

    /// [`content_state`](Self::content_state) by identity.
    ///
    /// # Errors
    ///
    /// `NotFound` when the row is missing.
    pub fn content_state_of(&self, id: FileId) -> NamespaceResult<ContentState> {
        let conn = self.conn();
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT size, content_root FROM file WHERE namespace = ?1 AND file_id = ?2",
                (&self.name, id.serial() as i64),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((size, content)) = row else {
            return Err(NamespaceError::NotFound {
                path: format!("{}", id.serial()),
            });
        };
        let content_root =
            match content {
                Some(text) => Some(text.parse::<ObjectId>().map_err(|error| {
                    NamespaceError::InvalidRecord {
                        field: "file.content_root",
                        detail: error.to_string(),
                    }
                })?),
                None => None,
            };
        let mut statement = conn.prepare(
            "SELECT logical_start, length, object_id, source_offset
             FROM file_extent WHERE namespace = ?1 AND file_id = ?2
             ORDER BY logical_start",
        )?;
        let rows = statement.query_map((&self.name, id.serial() as i64), |row| {
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
            let object_id =
                object
                    .parse::<ObjectId>()
                    .map_err(|error| NamespaceError::InvalidRecord {
                        field: "file_extent.object_id",
                        detail: error.to_string(),
                    })?;
            extents.push(Extent {
                logical_start: start as u64,
                length: length as u64,
                object_id,
                source_offset: source as u64,
            });
        }
        Ok(ContentState {
            size: size as u64,
            content_root,
            extents,
        })
    }

    /// Replaces a file's content description in one transaction (ADR-004 §5.4).
    ///
    /// The caller must already have published every object referenced here; this
    /// function only records references, so the namespace can never point at a
    /// missing object.
    ///
    /// # Errors
    ///
    /// `NotFound` when the file is missing, `IsADirectory` for a directory, and
    /// store errors from the transaction.
    pub fn replace_content(
        &self,
        path: &Path,
        op: &str,
        state: &ContentState,
    ) -> NamespaceResult<()> {
        let id = self.resolve_file(path)?;
        self.with_write_tx(|_tx| {
            let conn = self.conn();
            Self::write_file_row(conn, &self.name, id, state)?;
            conn.execute(
                "DELETE FROM file_extent WHERE namespace = ?1 AND file_id = ?2",
                (&self.name, id.serial() as i64),
            )?;
            for extent in &state.extents {
                Self::insert_extent(conn, &self.name, id, extent)?;
            }
            self.publish_tx(op, None)?;
            Ok(())
        })
    }

    /// Updates only the blocks a write replaced, leaving the rest untouched.
    ///
    /// `replaced` holds the new extent rows for the blocks that changed;
    /// `removed_from` drops every extent at or past that logical offset (used on
    /// truncate). Extents below `removed_from` that are not in `replaced` keep
    /// their rows, so a one-block write touches one row instead of rewriting the
    /// whole file's extent list (ADR-004 §5.4).
    ///
    /// # Errors
    ///
    /// `NotFound`, `IsADirectory`, store errors from the transaction.
    pub fn replace_extents(
        &self,
        path: &Path,
        op: &str,
        size: u64,
        content_root: Option<ObjectId>,
        replaced: &[Extent],
        removed_from: Option<u64>,
    ) -> NamespaceResult<()> {
        let id = self.resolve_file(path)?;
        let state = ContentState {
            size,
            content_root,
            extents: Vec::new(),
        };
        self.with_write_tx(|_tx| {
            let conn = self.conn();
            Self::write_file_row(conn, &self.name, id, &state)?;
            for extent in replaced {
                conn.execute(
                    "DELETE FROM file_extent
                     WHERE namespace = ?1 AND file_id = ?2 AND logical_start = ?3",
                    (&self.name, id.serial() as i64, extent.logical_start as i64),
                )?;
                Self::insert_extent(conn, &self.name, id, extent)?;
            }
            if let Some(from) = removed_from {
                conn.execute(
                    "DELETE FROM file_extent
                     WHERE namespace = ?1 AND file_id = ?2 AND logical_start >= ?3",
                    (&self.name, id.serial() as i64, from as i64),
                )?;
            }
            self.publish_tx(op, None)?;
            Ok(())
        })
    }

    /// Resolves `path` to a regular file's identity.
    fn resolve_file(&self, path: &Path) -> NamespaceResult<FileId> {
        let id = self.resolve(path)?;
        match self.kind_of(id)? {
            Some(Kind::Directory) => Err(NamespaceError::IsADirectory {
                path: path.display(),
            }),
            None => Err(NamespaceError::NotFound {
                path: path.display(),
            }),
            Some(Kind::File) => Ok(id),
        }
    }

    fn write_file_row(
        conn: &rusqlite::Connection,
        namespace: &str,
        id: FileId,
        state: &ContentState,
    ) -> NamespaceResult<()> {
        conn.execute(
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
        Ok(())
    }

    fn insert_extent(
        conn: &rusqlite::Connection,
        namespace: &str,
        id: FileId,
        extent: &Extent,
    ) -> NamespaceResult<()> {
        conn.execute(
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

    /// A file's size and single-object root, without reading extent rows.
    ///
    /// Used by the write path, which only needs the extents its range touches
    /// and must not load the whole file's extent list (ADR-004 §5.4).
    ///
    /// # Errors
    ///
    /// `NotFound` when the file is missing.
    pub fn shape_of(&self, path: &Path) -> NamespaceResult<(u64, Option<ObjectId>)> {
        let id = self.resolve_file(path)?;
        let conn = self.conn();
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT size, content_root FROM file WHERE namespace = ?1 AND file_id = ?2",
                (&self.name, id.serial() as i64),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((size, content)) = row else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        let content_root =
            match content {
                Some(text) => Some(text.parse::<ObjectId>().map_err(|error| {
                    NamespaceError::InvalidRecord {
                        field: "file.content_root",
                        detail: error.to_string(),
                    }
                })?),
                None => None,
            };
        Ok((size as u64, content_root))
    }

    /// The extents overlapping `[from, to)`, in logical order.
    ///
    /// Only rows that can overlap the range come back, so a small read or a
    /// one-block write costs O(rows returned), not O(file).
    ///
    /// # Errors
    ///
    /// `NotFound`, or a stored object id that is not a valid identity.
    pub fn extents_in_range(
        &self,
        path: &Path,
        from: u64,
        to: u64,
    ) -> NamespaceResult<Vec<Extent>> {
        let id = self.resolve_file(path)?;
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT logical_start, length, object_id, source_offset
             FROM file_extent
             WHERE namespace = ?1 AND file_id = ?2
               AND logical_start < ?4
               AND logical_start + length > ?3
             ORDER BY logical_start",
        )?;
        let rows = statement.query_map(
            (&self.name, id.serial() as i64, from as i64, to as i64),
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
            let object_id =
                object
                    .parse::<ObjectId>()
                    .map_err(|error| NamespaceError::InvalidRecord {
                        field: "file_extent.object_id",
                        detail: error.to_string(),
                    })?;
            extents.push(Extent {
                logical_start: start as u64,
                length: length as u64,
                object_id,
                source_offset: source as u64,
            });
        }
        Ok(extents)
    }

    /// Every object this namespace references, deduplicated (ADR-004 §7).
    ///
    /// # Errors
    ///
    /// Store errors from reading the tables.
    pub fn referenced_objects(&self) -> NamespaceResult<Vec<ObjectId>> {
        let conn = self.conn();
        let mut found = Vec::new();
        let mut statement = conn.prepare(
            "SELECT content_root FROM file
             WHERE namespace = ?1 AND content_root IS NOT NULL",
        )?;
        let rows = statement.query_map([&self.name], |row| row.get::<_, String>(0))?;
        for row in rows {
            let text = row?;
            found.push(text.parse::<ObjectId>().map_err(|error| {
                NamespaceError::InvalidRecord {
                    field: "file.content_root",
                    detail: error.to_string(),
                }
            })?);
        }
        let mut statement =
            conn.prepare("SELECT object_id FROM file_extent WHERE namespace = ?1")?;
        let rows = statement.query_map([&self.name], |row| row.get::<_, String>(0))?;
        for row in rows {
            let text = row?;
            found.push(text.parse::<ObjectId>().map_err(|error| {
                NamespaceError::InvalidRecord {
                    field: "file_extent.object_id",
                    detail: error.to_string(),
                }
            })?);
        }
        found.sort_by_key(|oid| *oid.as_bytes());
        found.dedup();
        Ok(found)
    }
}
