//! The path rules, written once against [`TreeStore`] (ADR-006 §7.1).
//!
//! Every rule here — path resolution, R1–R12, the violation forms of ADR-003 §3.2 —
//! talks only to the trait, so the SQLite backend and the overlay backend share the
//! same implementation. What differs between backends lives behind the trait or on
//! the backend itself.
//!
//! # Atomicity
//!
//! Each public mutating operation calls [`TreeStore::write`] **exactly once** (TX5).
//! That is what makes "a rejected operation changes nothing" true by construction
//! rather than by care: the body either commits whole or rolls back whole, and no
//! rule can accidentally split its changes across two transactions.

// `TreeRead` must be in scope to call its methods on a `TreeWrite`: a supertrait's
// methods are callable only when that trait is imported, and the rules call them
// through the write view.
use crate::tree_store::{OpLabel, TreeRead, TreeStore, TreeWrite};
use crate::{FileId, Kind, NamespaceError, NamespaceResult, Path};

/// The path-addressed rules over any tree backend.
///
/// Named `Rules` rather than `Namespace` so the concrete `Namespace` (which binds
/// these rules to a specific backend and adds that backend's own capabilities)
/// can keep its P1 name and signature.
#[derive(Debug)]
pub struct Rules<S: TreeStore> {
    store: S,
}

/// What [`Rules::create_or_open`] produced (P3-2 G1).
///
/// `kind` is the node's kind, not a variant of the outcome: the same shape answers "which entry" and "is it a
/// directory", which is exactly the third column the eighteen-cell table has to assert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutcome {
    /// The entry that now carries the name.
    pub file_id: FileId,
    /// Whether that entry is a file or a directory.
    pub kind: Kind,
    /// What the call had to do to satisfy the disposition.
    pub action: fs_semantics::disposition::Action,
    /// Where the name now lives: `(parent, name)`.
    ///
    /// Returned because the caller may have created the entry through a *different* path than the one it will use
    /// next (an adapter that resolved a parent handle once), and re-deriving it means a second lookup that can race.
    pub binding: (FileId, Vec<u8>),
    /// The entry that lost the name, for `SUPERSEDE`.
    ///
    /// `None` for every other disposition. The old row is **not** destroyed (it may still be open elsewhere); this is
    /// the caller's handle on it, which is what makes "unbind the name, keep the entry" observable rather than implied.
    pub replaced: Option<FileId>,
}

/// The disposition table's failures, translated to this layer's vocabulary.
///
/// Deliberately lossy in one place: the frontend keeps `Failure::AccessDenied` vs `InvalidParameter` apart for
/// NTSTATUS, while the tree only needs "this was refused". Anything the tree cannot name becomes `ReadOnly`, which
/// is the closest thing it has to "not offered here".
fn into_namespace_failure(
    failure: fs_semantics::disposition::Failure,
    path: &Path,
) -> NamespaceError {
    use fs_semantics::disposition::Failure;
    match failure {
        Failure::NameCollision => NamespaceError::AlreadyExists {
            path: path.display(),
        },
        Failure::LeafMissing | Failure::ParentMissing => NamespaceError::NotFound {
            path: path.display(),
        },
        Failure::NotADirectory => NamespaceError::NotADirectory {
            path: path.display(),
        },
        Failure::IsADirectory => NamespaceError::IsADirectory {
            path: path.display(),
        },
        Failure::DirectoryNotEmpty => NamespaceError::DirectoryNotEmpty {
            path: path.display(),
        },
        // Everything else is "this layer does not offer it", which is what `ReadOnly` means here.
        _ => NamespaceError::ReadOnly,
    }
}

impl<S: TreeStore> Rules<S> {
    /// Wraps a backend.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The backend, for the capabilities that are not part of the tree contract.
    ///
    /// The backing store's content API is reached through here; it is storage
    /// representation, not tree rules.
    pub fn backend(&self) -> &S {
        &self.store
    }

    /// Creates an empty directory and fails if the name is taken.
    ///
    /// # Errors
    ///
    /// `AlreadyExists` for an occupied name **and for `/`** (the root directory
    /// already exists, so creating it is a collision, not a malformed path — ADR-003
    /// §3.2 T1), `NotFound` for a missing parent, `NotADirectory` when a parent
    /// component is a file, and `InvalidPath`.
    pub fn mkdir(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::AlreadyExists {
                path: path.display(),
            });
        }
        let (parent, name) = self.resolve_parent(path)?;
        self.store.write(OpLabel::Mkdir, |tree| {
            // The check and the insert are in one atomic scope (TX2), so a concurrent
            // creator cannot slip between them. The backend also translates its
            // uniqueness violation into `AlreadyExists`, which covers the case where
            // two *processes* race — a thread-local lock cannot.
            if tree.lookup(parent, name.as_bytes())?.is_some() {
                return Err(NamespaceError::AlreadyExists {
                    path: path.display(),
                });
            }
            let id = tree.create_inode(Kind::Directory)?;
            tree.bind(parent, name.as_bytes(), id)
                .map_err(|error| match error {
                    NamespaceError::AlreadyExists { .. } => NamespaceError::AlreadyExists {
                        path: path.display(),
                    },
                    other => other,
                })?;
            Ok(())
        })
    }

    /// Creates or opens `path` according to an NT disposition, **atomically** (P3-2 G1).
    ///
    /// The existence test and the action run inside one `write` closure, which is one SQLite transaction: a second
    /// creator cannot slip between "it does not exist" and "create it". That is the whole point — an adapter that
    /// resolved first and created second would be a TOCTOU with a race that users see as a lost file.
    ///
    /// `SUPERSEDE` on an existing entry **unbinds the old name and drops nothing else**: the old inode row stays,
    /// because a handle elsewhere may still be reading it (the frontend's L2 proves that separation; this layer only
    /// has to avoid destroying the row).
    ///
    /// # Errors
    ///
    /// The [`Decide`] failure translated to the closest namespace error: `AlreadyExists` (collision), `NotFound`
    /// (missing leaf), `NotADirectory` / `IsADirectory`, and `ReadOnly` for the derived-attribute refusals.
    pub fn create_or_open(
        &self,
        path: &Path,
        disposition: fs_semantics::disposition::Disposition,
    ) -> NamespaceResult<CreateOutcome> {
        use fs_semantics::disposition::{decide, Exists, Outcome};

        if path.is_root() {
            // The root always exists and is a directory; only the file-opening dispositions care.
            return match decide(disposition, Exists::Directory) {
                Outcome::Opened(action) => Ok(CreateOutcome {
                    file_id: FileId::ROOT,
                    kind: Kind::Directory,
                    action,
                    binding: (FileId::ROOT, Vec::new()),
                    replaced: None,
                }),
                Outcome::Failed(failure) => Err(into_namespace_failure(failure, path)),
            };
        }
        let (parent, name) = self.resolve_parent(path)?;
        self.store.write(OpLabel::Create, |tree| {
            let existing = tree.lookup(parent, name.as_bytes())?;
            let exists = match existing {
                None => Exists::Absent,
                Some(id) => match tree.inode(id)? {
                    Some(view) if view.kind == Kind::Directory => Exists::Directory,
                    Some(_) => Exists::File,
                    None => Exists::Absent,
                },
            };
            let action = match decide(disposition, exists) {
                Outcome::Opened(action) => action,
                Outcome::Failed(failure) => return Err(into_namespace_failure(failure, path)),
            };
            use fs_semantics::disposition::Action;
            let mut replaced: Option<FileId> = None;
            let (file_id, kind) = match action {
                // `Open`: nothing to do, including the directory case (no file handle is created).
                Action::Open => {
                    let id = existing.expect("OPEN requires an existing entry");
                    let kind = tree.inode(id)?.map_or(Kind::File, |view| view.kind);
                    (id, kind)
                }
                Action::CreateNew => {
                    let id = tree.create_inode(Kind::File)?;
                    tree.bind(parent, name.as_bytes(), id)?;
                    (id, Kind::File)
                }
                // OVERWRITE_IF / OVERWRITE: the entry is **reused**, so the FileId does not change — that is the
                // one observable difference from SUPERSEDE, and it is why this arm must not create an inode.
                Action::TruncateExisting => {
                    let id = existing.expect("OVERWRITE requires an existing entry");
                    // Truncated **inside** this transaction: the entry keeps its FileId, and no reader can observe the
                    // old bytes after the disposition was granted.
                    tree.truncate(id, 0)?;
                    (id, Kind::File)
                }
                Action::ReplaceExisting => {
                    let old = existing.expect("SUPERSEDE requires an existing entry");
                    replaced = Some(old);
                    // Unbind the name only: the old inode row (and its content) stays for anyone holding it, which
                    // is what lets a frontend keep serving a handle opened before the replace.
                    tree.unbind(parent, name.as_bytes())?;
                    let fresh = tree.create_inode(Kind::File)?;
                    tree.bind(parent, name.as_bytes(), fresh)?;
                    (fresh, Kind::File)
                }
            };
            Ok(CreateOutcome {
                file_id,
                kind,
                action,
                binding: (parent, name.as_bytes().to_vec()),
                replaced,
            })
        })
    }

    /// Removes an empty directory.
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, `NotADirectory` when the target is a file, and
    /// `DirectoryNotEmpty` (ADR-003 §3.2 T5).
    pub fn rmdir(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        self.store.write(OpLabel::Rmdir, |tree| {
            let (parent, name, id) = resolve_existing(tree, path)?;
            match tree.inode(id)? {
                Some(view) if view.kind == Kind::Directory => {}
                Some(_) => {
                    return Err(NamespaceError::NotADirectory {
                        path: path.display(),
                    })
                }
                None => {
                    return Err(NamespaceError::NotFound {
                        path: path.display(),
                    })
                }
            }
            if !tree.is_dir_empty(id)? {
                return Err(NamespaceError::DirectoryNotEmpty {
                    path: path.display(),
                });
            }
            tree.unbind(parent, name.as_bytes())?;
            tree.tombstone(id)?;
            Ok(())
        })
    }

    /// Removes a file's name.
    ///
    /// # Errors
    ///
    /// `IsRoot`, `NotFound`, and `IsADirectory` when the target is a directory.
    pub fn unlink(&self, path: &Path) -> NamespaceResult<()> {
        if path.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        self.store.write(OpLabel::Unlink, |tree| {
            let (parent, name, id) = resolve_existing(tree, path)?;
            match tree.inode(id)? {
                Some(view) if view.kind == Kind::File => {}
                Some(_) => {
                    return Err(NamespaceError::IsADirectory {
                        path: path.display(),
                    })
                }
                None => {
                    return Err(NamespaceError::NotFound {
                        path: path.display(),
                    })
                }
            }
            tree.unbind(parent, name.as_bytes())?;
            tree.tombstone(id)?;
            Ok(())
        })
    }

    /// Moves `from` to `to` (ADR-003 §3.3 R1–R12).
    ///
    /// # Errors
    ///
    /// `IsRoot` when either side is `/`, `NotFound` for a missing source or parent,
    /// `AlreadyExists` when the types disagree, `DirectoryNotEmpty` when replacing a
    /// non-empty directory, and `IntoOwnSubtree` for the R9 case.
    pub fn rename(&self, from: &Path, to: &Path) -> NamespaceResult<()> {
        if from.is_root() || to.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        // R8: byte-identical paths are a no-op that publishes nothing. The source is
        // still resolved first, so renaming a path that does not exist reports
        // `NotFound` rather than pretending to succeed. `/A` to `/a` differs
        // byte-wise and is therefore a real rename.
        if from.segments() == to.segments() {
            self.resolve(from)?;
            return Ok(());
        }
        self.store.write(OpLabel::Rename, |tree| {
            let (source_parent, source_name, source) = resolve_existing(tree, from)?;
            let source_kind = tree
                .inode(source)?
                .ok_or_else(|| NamespaceError::NotFound {
                    path: from.display(),
                })?
                .kind;
            let target_name = to.name().expect("a non-root destination has a name");
            let target_parent = resolve_parent_in(tree, to)?;

            // R9: a directory may not move inside its own subtree. Walk the
            // destination's path from the root and reject when any prefix *is* the
            // directory being moved (ADR-006 §7.3): O(path depth), lookups only. Since
            // this runs inside the same write scope as the move itself (TX2), two
            // cross-directory renames cannot both pass and close a cycle (K4).
            if source_kind == Kind::Directory && path_passes_through(tree, to, source)? {
                return Err(NamespaceError::IntoOwnSubtree {
                    from: from.display(),
                    to: to.display(),
                });
            }

            match tree.lookup(target_parent, target_name.as_bytes())? {
                None => {}
                Some(existing) => {
                    let existing_kind = tree
                        .inode(existing)?
                        .ok_or_else(|| NamespaceError::NotFound { path: to.display() })?
                        .kind;
                    match (source_kind, existing_kind) {
                        // R3: a file replaces a file.
                        (Kind::File, Kind::File) => {}
                        // R4/R5: types never cross.
                        (Kind::File, Kind::Directory) | (Kind::Directory, Kind::File) => {
                            return Err(NamespaceError::AlreadyExists { path: to.display() })
                        }
                        // R6: a non-empty directory blocks the move.
                        (Kind::Directory, Kind::Directory) => {
                            if !tree.is_dir_empty(existing)? {
                                return Err(NamespaceError::DirectoryNotEmpty {
                                    path: to.display(),
                                });
                            }
                        }
                    }
                    // R3 and R7 both replace: remove the destination's name and its
                    // inode, and with it everything the inode owned.
                    tree.unbind(target_parent, target_name.as_bytes())?;
                    tree.tombstone(existing)?;
                }
            }

            tree.unbind(source_parent, source_name.as_bytes())?;
            tree.bind(target_parent, target_name.as_bytes(), source)?;
            Ok(())
        })
    }

    /// Resolves a path to an inode.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`, or a backend failure.
    pub fn resolve(&self, path: &Path) -> NamespaceResult<FileId> {
        self.store.read(|tree| resolve_in(tree, path))
    }

    /// Resolves a path's parent directory, returning it with the final name.
    ///
    /// The name is cloned because the parent id is read inside a store scope while
    /// the name comes from the caller's path; handing back a `&str` would tie the two
    /// lifetimes together for no benefit.
    ///
    /// # Errors
    ///
    /// As [`Rules::resolve`], and `IsRoot` when `path` is `/`.
    pub fn resolve_parent(&self, path: &Path) -> NamespaceResult<(FileId, String)> {
        if path.is_root() {
            return Err(NamespaceError::IsRoot);
        }
        let name = path.name().expect("a non-root path has a name").to_owned();
        self.store
            .read(|tree| Ok((resolve_parent_in(tree, path)?, name)))
    }

    /// Lists a directory.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`, or a backend failure.
    pub fn read_dir(&self, path: &Path) -> NamespaceResult<Vec<crate::Entry>> {
        self.store.read(|tree| {
            let id = resolve_in(tree, path)?;
            let view = tree.inode(id)?.ok_or_else(|| NamespaceError::NotFound {
                path: path.display(),
            })?;
            if view.kind != Kind::Directory {
                return Err(NamespaceError::NotADirectory {
                    path: path.display(),
                });
            }
            let mut out = Vec::new();
            let mut after: Option<Vec<u8>> = None;
            loop {
                let batch = tree.read_dir(id, after.as_deref(), 256)?;
                if batch.is_empty() {
                    break;
                }
                after = batch.last().map(|(name, _)| name.clone());
                for (name, child) in batch {
                    let kind = tree.inode(child)?.map_or(Kind::File, |view| view.kind);
                    out.push(crate::Entry {
                        name: String::from_utf8_lossy(&name).into_owned(),
                        id: child,
                        kind,
                    });
                }
            }
            Ok(out)
        })
    }

    /// Reports one entry.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `InvalidPath`, or a backend failure.
    pub fn stat(&self, path: &Path) -> NamespaceResult<crate::Stat> {
        self.store.read(|tree| {
            let id = resolve_in(tree, path)?;
            let view = tree.inode(id)?.ok_or_else(|| NamespaceError::NotFound {
                path: path.display(),
            })?;
            Ok(crate::Stat {
                id: view.id,
                kind: view.kind,
                size: view.size,
                // The tree contract has no notion of a content root; the table
                // backend fills this in through its own accessor.
                content_root: None,
                created_ns: view.created_ns,
                modified_ns: view.modified_ns,
            })
        })
    }
}

/// Walks a path from the root (ADR-003 §3.1).
///
/// # Errors
///
/// `NotFound`, `NotADirectory`, and `InvalidPath`.
pub fn resolve_in(tree: &impl crate::TreeRead, path: &Path) -> NamespaceResult<FileId> {
    let mut current = FileId::ROOT;
    for component in path.segments() {
        let Some(view) = tree.inode(current)? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        if view.kind != Kind::Directory {
            return Err(NamespaceError::NotADirectory {
                path: path.display(),
            });
        }
        let Some(next) = tree.lookup(current, component.as_bytes())? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        current = next;
    }
    Ok(current)
}

/// Resolves the parent directory of `path` (ADR-003 §3.1).
///
/// # Errors
///
/// As [`resolve_in`], plus `IsRoot` when `path` is `/`.
pub fn resolve_parent_in(tree: &impl crate::TreeRead, path: &Path) -> NamespaceResult<FileId> {
    if path.is_root() {
        return Err(NamespaceError::IsRoot);
    }
    let mut current = FileId::ROOT;
    for segment in path.parent_segments() {
        let Some(view) = tree.inode(current)? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        if view.kind != Kind::Directory {
            return Err(NamespaceError::NotADirectory {
                path: path.display(),
            });
        }
        let Some(next) = tree.lookup(current, segment.as_bytes())? else {
            return Err(NamespaceError::NotFound {
                path: path.display(),
            });
        };
        current = next;
    }
    // The loop validates the kind of each directory *before* descending, so the last name it
    // resolved was never checked. Without this, `rename("/file", "/file/child")` handed back
    // the file as the destination's parent: the move unbound the source and then bound its
    // name into a parent that cannot hold children, so the entry vanished from the tree
    // entirely. Found by the reference-model sequence (ADR-006 §18-1).
    let Some(view) = tree.inode(current)? else {
        return Err(NamespaceError::NotFound {
            path: path.display(),
        });
    };
    if view.kind != Kind::Directory {
        return Err(NamespaceError::NotADirectory {
            path: path.display(),
        });
    }
    Ok(current)
}

/// Resolves `path` to its parent, name and inode, failing when absent.
fn resolve_existing<'a>(
    tree: &impl crate::TreeRead,
    path: &'a Path,
) -> NamespaceResult<(FileId, &'a str, FileId)> {
    let name = path.name().expect("a non-root path has a name");
    let parent = resolve_parent_in(tree, path)?;
    let id = tree
        .lookup(parent, name.as_bytes())?
        .ok_or_else(|| NamespaceError::NotFound {
            path: path.display(),
        })?;
    Ok((parent, name, id))
}

/// Whether resolving `path` walks through `target` (R9).
///
/// The check is a downward walk from the root: if any prefix of `path` resolves to
/// `target`, then moving a directory *to* `path` would place it inside itself. This
/// needs only [`TreeRead::lookup`], which every backend has, and costs O(depth).
///
/// A prefix that does not resolve yet is fine — `rename` may be creating the name —
/// and stops the walk, because nothing further can be reached.
fn path_passes_through(
    tree: &impl crate::TreeRead,
    path: &Path,
    target: FileId,
) -> NamespaceResult<bool> {
    let mut current = FileId::ROOT;
    for component in path.segments() {
        if current == target {
            return Ok(true);
        }
        let Some(view) = tree.inode(current)? else {
            return Ok(false);
        };
        if view.kind != Kind::Directory {
            return Ok(false);
        }
        let Some(next) = tree.lookup(current, component.as_bytes())? else {
            return Ok(false);
        };
        current = next;
    }
    Ok(current == target)
}
