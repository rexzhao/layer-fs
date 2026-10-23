//! Directory and path operations (ADR-003 §3.2, §3.3, §5).

use rusqlite::OptionalExtension;

use crate::error::{NamespaceError, NamespaceResult};
use crate::namespace::{now_ns, Entry, FileId, Kind, Namespace, Stat};
use crate::path::Path;

impl Namespace {
    /// Creates a directory.
    ///
    /// # Errors
    ///
    /// `AlreadyExists` when the name is taken by a directory **or** a file
    /// (ADR-003 §3.2 T1), `NotFound` when the parent is missing, and
    /// `NotADirectory` when the parent is a file.
    pub fn mkdir(&self, path: &Path) -> NamespaceResult<()> {
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
        let id = self.with_write_tx(|_tx| {
            let id = self.next_serial()?;
            let now = now_ns();
            // Same race as `create_file`: the pre-check runs outside the write
            // lock, so a concurrent mkdir can win. The primary key decides, and a
            // constraint violation means the name is taken.
            self.insert_entry(parent, name, FileId::from_serial(id))
                .map_err(|error| already_exists_if_constraint(error, path))?;
            self.conn().execute(
                "INSERT INTO file (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
                 VALUES (?1, ?2, ?3, 0, NULL, ?4, ?4)",
                (&self.name, id as i64, Kind::Directory as i64, now),
            )?;
            self.publish_tx("mkdir", None)?;
            Ok(id)
        })?;
        let _ = id;
        Ok(())
    }

    /// Removes an empty directory.
    ///
    /// # Errors
    ///
    /// `IsRoot` for `/`, `NotFound` when absent, `NotADirectory` when the target
    /// is a file, and `DirectoryNotEmpty` when it still has entries
    /// (ADR-003 §3.2 T5).
    pub fn rmdir(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        let parent = self.resolve_parent(path)?;
        let name = path.name().expect("non-root path has a name");
        let Some(id) = self.lookup(parent, name)? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        if self.kind_of(id)? != Some(Kind::Directory) {
            return Err(NamespaceError::NotADirectory {
                path: path.display(),
            });
        }
        let children: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM directory_entry
                     WHERE namespace = ?1 AND parent_id = ?2",
            (&self.name, id.serial() as i64),
            |row| row.get(0),
        )?;
        if children > 0 {
            return Err(NamespaceError::DirectoryNotEmpty {
                path: path.display(),
            });
        }
        self.with_write_tx(|_tx| {
            self.delete_entry(parent, name)?;
            self.delete_inode(id)?;
            self.publish_tx("rmdir", None)?;
            Ok(())
        })
    }

    /// Removes a regular file.
    ///
    /// # Errors
    ///
    /// `NotFound` when absent and `IsADirectory` when the target is a directory
    /// (ADR-003 §3.2 T4).
    pub fn unlink(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        let parent = self.resolve_parent(path)?;
        let name = path.name().expect("non-root path has a name");
        let Some(id) = self.lookup(parent, name)? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
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
        self.with_write_tx(|_tx| {
            self.delete_entry(parent, name)?;
            self.delete_inode(id)?;
            self.publish_tx("unlink", None)?;
            Ok(())
        })
    }

    /// Renames `from` to `to` (ADR-003 §3.3 R1–R12).
    ///
    /// # Errors
    ///
    /// The matching row of ADR-003 §3.3. A byte-identical `from`/`to` is a
    /// successful no-op that publishes nothing (R8); a case-only change is a real
    /// rename and falls through to the normal rules.
    pub fn rename(&self, from: &Path, to: &Path) -> NamespaceResult<()> {
        if from.is_root() || to.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        // R8: byte-identical paths are a no-op, and publish nothing.
        if from.segments() == to.segments() {
            self.resolve(from)?;
            return Ok(());
        }
        let source_parent = self.resolve_parent(from)?;
        let from_name = from.name().expect("non-root path has a name");
        let Some(source) = self.lookup(source_parent, from_name)? else {
            return Err(NamespaceError::NotFound {
                path: from.display(),
            });
        };
        let source_kind = self
            .kind_of(source)?
            .ok_or_else(|| NamespaceError::NotFound {
                path: from.display(),
            })?;

        let target_parent = self.resolve_parent(to)?;
        let to_name = to.name().expect("non-root path has a name");

        // R9: a directory may not move inside itself. Compare the resolved
        // target parent with the source: if the target parent is the source or
        // lives under it, the move would detach the subtree.
        if source_kind == Kind::Directory && self.is_within(target_parent, source)? {
            return Err(NamespaceError::IntoOwnSubtree {
                from: from.display(),
                to: to.display(),
            });
        }

        self.with_write_tx(|_tx| {
            if let Some(existing) = self.lookup(target_parent, to_name)? {
                let existing_kind = self
                    .kind_of(existing)?
                    .ok_or_else(|| NamespaceError::NotFound { path: to.display() })?;
                match (source_kind, existing_kind) {
                    // R3: file over file replaces it. The replaced inode and its
                    // extent rows go with the name, or they stay behind as rows
                    // nothing points at (P2-0a A1).
                    (Kind::File, Kind::File) => {}
                    // R4/R5: a directory is never replaced by a file, and a file
                    // is never replaced by a directory.
                    (Kind::File, Kind::Directory) | (Kind::Directory, Kind::File) => {
                        return Err(NamespaceError::AlreadyExists { path: to.display() });
                    }
                    // R6: a non-empty directory blocks the move.
                    (Kind::Directory, Kind::Directory) => {
                        let children: i64 = self.conn().query_row(
                            "SELECT COUNT(*) FROM directory_entry
                     WHERE namespace = ?1 AND parent_id = ?2",
                            (&self.name, existing.serial() as i64),
                            |row| row.get(0),
                        )?;
                        if children > 0 {
                            return Err(NamespaceError::DirectoryNotEmpty { path: to.display() });
                        }
                    }
                }
                // R3 and R7 both replace the destination: remove its name and its
                // inode, and any extent rows the inode owned.
                self.delete_entry(target_parent, to_name)?;
                self.delete_inode(existing)?;
            }

            self.delete_entry(source_parent, from_name)?;
            self.insert_entry(target_parent, to_name, source)?;
            self.publish_tx("rename", None)?;
            Ok(())
        })
    }

    /// Reports one entry.
    ///
    /// # Errors
    ///
    /// `NotFound` when the path does not resolve.
    pub fn stat(&self, path: &Path) -> NamespaceResult<Stat> {
        let id = self.resolve(path)?;
        let row: Option<(i64, i64, Option<String>, i64, i64)> = self
            .conn()
            .query_row(
                "SELECT kind, size, content_root, created_ns, modified_ns
                 FROM file WHERE namespace = ?1 AND file_id = ?2",
                (&self.name, id.serial() as i64),
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind_tag, size, content, created_ns, modified_ns)) = row else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        let kind = Kind::from_tag(kind_tag).ok_or_else(|| NamespaceError::NotFound {
            path: path.display(),
        })?;
        let content_root = match content {
            Some(text) => Some(text.parse::<cas_core::ObjectId>().map_err(|error| {
                NamespaceError::InvalidRecord {
                    field: "file.content_root",
                    detail: error.to_string(),
                }
            })?),
            None => None,
        };
        Ok(Stat {
            id,
            kind,
            size: size as u64,
            content_root,
            created_ns,
            modified_ns,
        })
    }

    /// Lists a directory's entries, sorted by name bytes.
    ///
    /// # Errors
    ///
    /// `NotFound` when absent and `NotADirectory` when the target is a file
    /// (ADR-003 §3.2 T3).
    pub fn read_dir(&self, path: &Path) -> NamespaceResult<Vec<Entry>> {
        let id = self.resolve(path)?;
        if self.kind_of(id)? != Some(Kind::Directory) {
            return Err(NamespaceError::NotADirectory {
                path: path.display(),
            });
        }
        let mut statement = self.conn().prepare(
            "SELECT e.name, e.child_id, f.kind
             FROM directory_entry e
             JOIN file f ON f.namespace = e.namespace AND f.file_id = e.child_id
             WHERE e.namespace = ?1 AND e.parent_id = ?2
             ORDER BY e.name",
        )?;
        let rows = statement.query_map((&self.name, id.serial() as i64), |row| {
            let name: Vec<u8> = row.get(0)?;
            let child: i64 = row.get(1)?;
            let kind: i64 = row.get(2)?;
            Ok((name, child, kind))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (name, child, kind) = row?;
            let name = String::from_utf8(name).map_err(|_| NamespaceError::InvalidRecord {
                field: "directory_entry.name",
                detail: "name is not valid UTF-8".to_owned(),
            })?;
            let kind = Kind::from_tag(kind).ok_or(NamespaceError::InvalidRecord {
                field: "file.kind",
                detail: format!("unknown kind tag {kind}"),
            })?;
            entries.push(Entry {
                name,
                id: FileId::from_serial(child as u64),
                kind,
            });
        }
        Ok(entries)
    }

    pub(crate) fn insert_entry(
        &self,
        parent: FileId,
        name: &str,
        child: FileId,
    ) -> NamespaceResult<()> {
        self.conn().execute(
            "INSERT INTO directory_entry (namespace, parent_id, name, child_id)
             VALUES (?1, ?2, ?3, ?4)",
            (
                &self.name,
                parent.serial() as i64,
                name.as_bytes(),
                child.serial() as i64,
            ),
        )?;
        Ok(())
    }

    fn delete_entry(&self, parent: FileId, name: &str) -> NamespaceResult<()> {
        self.conn().execute(
            "DELETE FROM directory_entry WHERE namespace = ?1 AND parent_id = ?2 AND name = ?3",
            (&self.name, parent.serial() as i64, name.as_bytes()),
        )?;
        Ok(())
    }

    /// Removes an inode and everything that hangs off it.
    ///
    /// Removing an inode without its `file_extent` rows leaves block records
    /// pointing at an id nothing references: the bytes stay in the store and the
    /// namespace can never reach them again. Every removal path goes through here
    /// so the cascade cannot be forgotten in one of them (P2-0a A1).
    ///
    /// Callers must already have deleted the inode's directory entries; this does
    /// not check for them, because `rmdir` and `rename` verify emptiness first and
    /// `unlink` only ever sees files.
    pub(crate) fn delete_inode(&self, id: FileId) -> NamespaceResult<()> {
        self.conn().execute(
            "DELETE FROM file_extent WHERE namespace = ?1 AND file_id = ?2",
            (&self.name, id.serial() as i64),
        )?;
        self.conn().execute(
            "DELETE FROM file WHERE namespace = ?1 AND file_id = ?2",
            (&self.name, id.serial() as i64),
        )?;
        Ok(())
    }

    /// Whether `candidate` is `ancestor` or lies underneath it.
    fn is_within(&self, candidate: FileId, ancestor: FileId) -> NamespaceResult<bool> {
        if candidate == ancestor {
            return Ok(true);
        }
        let mut current = candidate;
        loop {
            let parent: Option<i64> = self
                .conn()
                .query_row(
                    "SELECT parent_id FROM directory_entry
                     WHERE namespace = ?1 AND child_id = ?2",
                    (&self.name, current.serial() as i64),
                    |row| row.get(0),
                )
                .optional()?;
            let Some(parent) = parent else {
                return Ok(false);
            };
            let parent = FileId::from_serial(parent as u64);
            if parent == ancestor {
                return Ok(true);
            }
            current = parent;
        }
    }
}

/// Turns a primary-key violation into `AlreadyExists`.
///
/// The existence pre-checks in `mkdir` and `create_file` run before the write lock,
/// so two concurrent creators can both pass them. The table's primary key is the
/// real arbiter; without this translation the loser surfaces a raw SQLite
/// constraint message instead of the documented `AlreadyExists` (S5 concurrency).
pub(crate) fn already_exists_if_constraint(error: NamespaceError, path: &Path) -> NamespaceError {
    match error {
        NamespaceError::Store(cas_store::StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            inner,
            _,
        ))) if inner.code == rusqlite::ErrorCode::ConstraintViolation => {
            NamespaceError::AlreadyExists {
                path: path.display(),
            }
        }
        other => other,
    }
}
