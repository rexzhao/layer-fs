//! The repository: one store, its branches, its commits and its workspaces
//! (ADR-006 §4, §10).
//!
//! Everything shared lives here. A [`Repo`] owns the main-database handle and the CAS
//! store; a [`Workspace`](crate::Workspace) borrows both through it. That is deliberate —
//! one main-database connection per process means the short publication transactions
//! cannot deadlock against each other, only against another process.
//!
//! # What is *not* here
//!
//! Reading a commit's tree. That is `fs-snapshot`'s job and it needs nothing from this
//! crate: given a root id and a store, the snapshot reader walks it (ADR-006 I1). The
//! repository only records which roots exist and why.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cas_core::ObjectId;
use cas_store::{OpenMode, Store};
use fs_snapshot::{
    encode_inode_table, encode_root, Content, InodeRecord, Profile, RootValue, SnapshotReader,
};
use rusqlite::{Connection, OptionalExtension};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::ids::{self, commit_id, parse_oid, SerialSegment};
use crate::main_db::{hex, now_ns, unhex_32, MainDb};
use crate::schema;
use crate::workspace::{Workspace, WorkspaceSummary};

/// One commit, as the history reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// The commit id (ADR-006 §10.1).
    pub id: String,
    /// The snapshot root it publishes.
    pub root: String,
    /// Its parent, absent only for the first commit.
    pub parent: Option<String>,
    /// When it was created, Unix epoch nanoseconds.
    pub created_ns: i64,
    /// Its message.
    pub message: String,
}

/// One branch: a name, where it points, and the scope its workspaces allocate in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    /// The branch name.
    pub name: String,
    /// The commit it points at.
    pub head: String,
    /// The serial scope shared by every workspace on this branch (ADR-006 §6).
    pub scope_id: String,
}

/// Counts the repository keeps, reported by `stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoStats {
    /// Branches that exist.
    pub branches: u64,
    /// Commits recorded.
    pub commits: u64,
    /// Workspaces registered.
    pub workspaces: u64,
    /// Named snapshot roots recorded.
    pub snapshots: u64,
    /// Reflog entries: every head move, including forced ones.
    pub branch_log_entries: u64,
}
/// A store plus the branches, commits and workspaces recorded in it.
pub struct Repo {
    store_dir: PathBuf,
    db: MainDb,
    /// The CAS handle. Behind a mutex because cas_store::Store owns a connection and is
    /// Send but not Sync, and a repository must be usable from several threads
    /// (ADR-006 §7.4). Lock order is always main database, then CAS store.
    store: std::sync::Mutex<Store>,
}

impl fmt::Debug for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Repo")
            .field("store_dir", &self.store_dir)
            .finish_non_exhaustive()
    }
}

impl Repo {
    /// Opens the repository in `store_dir`, creating it when `create`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotInitialised`] when the workspace tables are absent and
    /// `create` is false; store or schema failures.
    pub fn open(store_dir: impl AsRef<Path>, create: bool) -> WorkspaceResult<Self> {
        let store_dir = store_dir.as_ref().to_path_buf();
        let mode = if create {
            OpenMode::Create
        } else {
            OpenMode::ReadWrite
        };
        // The busy timeout that should configure the CAS connection lives in the store's
        // own policy, so the store has to be opened once to find that file and read the
        // value, and then again with it. Two opens at startup, both cheap; the alternative
        // would be two sources for one number (ADR-006 §11.1).
        let probe = Store::open(&store_dir, mode)?;
        let metadata_path = probe.metadata_path().to_path_buf();
        drop(probe);
        let db = MainDb::open(&metadata_path, create)?;
        let busy_timeout = db.busy_timeout();
        let store = Store::open_with(&store_dir, mode, cas_store::StoreOptions { busy_timeout })?;
        let repo = Self {
            store_dir,
            db,
            store: std::sync::Mutex::new(store),
        };
        // §9.3: a directory without a row is a `discard` that crashed between deleting the
        // row and deleting the files.
        repo.sweep_orphans()?;
        Ok(repo)
    }

    /// Where this repository lives.
    #[must_use]
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    /// The directory one workspace's files live in.
    #[must_use]
    pub fn workspace_dir(&self, id: &str) -> PathBuf {
        self.store_dir.join("workspaces").join(id)
    }

    /// The main database file, for diagnostics and for tests that need a second
    /// connection.
    #[must_use]
    pub fn metadata_path(&self) -> &Path {
        self.db.path()
    }

    pub(crate) fn db(&self) -> &MainDb {
        &self.db
    }

    /// The bounded wait this repository's main-database connections use (ADR-006 §11.1).
    ///
    /// One number, read from `workspace_policy`, handed to every connection to the main
    /// database that can take the write lane.
    pub(crate) fn busy_timeout(&self) -> std::time::Duration {
        self.db.busy_timeout()
    }

    /// Creates the first commit: an empty tree with only a root directory.
    ///
    /// Serial 0 is the root directory (ADR-003 §4.3), so the empty snapshot is one inode
    /// record and one inode table page.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::AlreadyExists`] when the repository already has branches.
    pub fn init(&self) -> WorkspaceResult<CommitInfo> {
        {
            let connection = self.db.lock();
            let branches: i64 =
                connection.query_row("SELECT COUNT(*) FROM branch", [], |row| row.get(0))?;
            if branches > 0 {
                return Err(WorkspaceError::AlreadyExists {
                    name: "the repository".to_owned(),
                });
            }
        }
        // I5: the objects exist before any row names them.
        let scope = ids::new_scope_id();
        let scope_text = hex(&scope);
        let root = self.empty_root(scope)?;
        let created = now_ns();
        let id = commit_id(root, None, created, schema::INIT_MESSAGE);

        let connection = self.db.lock();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO scope_allocator (scope_id, next_serial) VALUES (?1, 1)",
            [scope_text.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO \"commit\" (commit_id, root_oid, parent_commit, created_ns, message)
             VALUES (?1, ?2, NULL, ?3, ?4)",
            (&id, root.to_string(), created, schema::INIT_MESSAGE),
        )?;
        transaction.execute(
            "INSERT INTO branch (name, scope_id, head_commit) VALUES (?1, ?2, ?3)",
            (schema::DEFAULT_BRANCH, &scope_text, &id),
        )?;
        transaction.execute(
            "INSERT INTO branch_log (seq, branch, old_commit, new_commit, op, created_ns)
             VALUES (0, ?1, NULL, ?2, 'init', ?3)",
            (schema::DEFAULT_BRANCH, &id, created),
        )?;
        transaction.commit()?;
        Ok(CommitInfo {
            id,
            root: root.to_string(),
            parent: None,
            created_ns: created,
            message: schema::INIT_MESSAGE.to_owned(),
        })
    }

    /// Creates a branch (ADR-006 §10.2).
    ///
    /// `from` may be a branch or a commit; the new branch inherits the source's serial
    /// scope, so workspaces on both branches allocate from the same range.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::AlreadyExists`] when the name is taken,
    /// [`WorkspaceError::NotFound`] when the source does not exist.
    pub fn branch_create(&self, name: &str, from: Option<&str>) -> WorkspaceResult<BranchInfo> {
        let (scope, head) = match from {
            None => self.branch_row(schema::DEFAULT_BRANCH)?,
            Some(source) => match self.branch_row(source) {
                Ok(found) => found,
                Err(WorkspaceError::NotFound { .. }) => {
                    // Not a branch: accept a commit id, and take the scope from the tree it
                    // publishes — that is where the serials in it were allocated.
                    let root = self.commit_root(source)?;
                    (hex(&self.root_scope(root)?), source.to_owned())
                }
                Err(other) => return Err(other),
            },
        };
        {
            let connection = self.db.lock();
            let taken: bool = connection.query_row(
                "SELECT COUNT(*) > 0 FROM branch WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?;
            if taken {
                return Err(WorkspaceError::AlreadyExists {
                    name: name.to_owned(),
                });
            }
            connection.execute(
                "INSERT INTO branch (name, scope_id, head_commit) VALUES (?1, ?2, ?3)",
                (name, &scope, &head),
            )?;
        }
        Ok(BranchInfo {
            name: name.to_owned(),
            head,
            scope_id: scope,
        })
    }

    /// Every branch, by name.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn branches(&self) -> WorkspaceResult<Vec<BranchInfo>> {
        let connection = self.db.lock();
        let mut statement =
            connection.prepare("SELECT name, scope_id, head_commit FROM branch ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            Ok(BranchInfo {
                name: row.get(0)?,
                scope_id: row.get(1)?,
                head: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Where a branch points.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the branch does not exist.
    pub fn branch_head(&self, name: &str) -> WorkspaceResult<String> {
        Ok(self.branch_row(name)?.1)
    }

    /// One commit.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when it does not exist.
    pub fn commit_info(&self, id: &str) -> WorkspaceResult<CommitInfo> {
        let connection = self.db.lock();
        commit_row(&connection, id)
    }

    /// A branch's history, newest first, following parents.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the branch or one of its commits is missing.
    pub fn log(&self, branch: &str) -> WorkspaceResult<Vec<CommitInfo>> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut current = Some(self.branch_head(branch)?);
        while let Some(id) = current {
            // A commit's parent is fixed when it is created, so a cycle cannot exist; the
            // guard is here so a corrupted database reports rather than hangs.
            if !seen.insert(id.clone()) {
                return Err(WorkspaceError::Inconsistent {
                    detail: format!("commit {id} appears twice in the history of {branch}"),
                });
            }
            let info = self.commit_info(&id)?;
            current = info.parent.clone();
            out.push(info);
        }
        Ok(out)
    }

    /// Moves a branch to an existing commit without creating one (ADR-006 §10.2).
    ///
    /// `expect` is the head the caller believes it is moving; a mismatch is
    /// [`WorkspaceError::BranchMoved`], so a scripted history rewrite cannot clobber a
    /// commit that arrived in between.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::BranchMoved`], [`WorkspaceError::NotFound`].
    pub fn reset(&self, branch: &str, to: &str, expect: Option<&str>) -> WorkspaceResult<()> {
        self.commit_root(to)?;
        let connection = self.db.lock();
        let (_, head) = branch_row(&connection, branch)?;
        if let Some(expect) = expect {
            if expect != head {
                return Err(WorkspaceError::BranchMoved {
                    branch: branch.to_owned(),
                    expected: expect.to_owned(),
                    actual: head,
                });
            }
        }
        let created = now_ns();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE branch SET head_commit = ?1 WHERE name = ?2",
            (to, branch),
        )?;
        let seq = crate::publish::next_branch_log_seq(&transaction)?;
        transaction.execute(
            "INSERT INTO branch_log (seq, branch, old_commit, new_commit, op, created_ns)
             VALUES (?1, ?2, ?3, ?4, 'reset', ?5)",
            (seq, branch, &head, to, created),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Creates a commit that undoes the head's change (ADR-006 §10.2, F3).
    ///
    /// Only the head is undone: the new commit's root is the head's **parent's** root, so
    /// history grows by one instead of the branch moving backwards. Anything the head's
    /// parent already contained stays.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the branch or the head's parent is missing.
    pub fn revert(&self, branch: &str) -> WorkspaceResult<CommitInfo> {
        let head = self.branch_head(branch)?;
        let head_info = self.commit_info(&head)?;
        let Some(parent) = head_info.parent.clone() else {
            return Err(WorkspaceError::NotFound {
                what: format!("the parent of {head}"),
            });
        };
        let parent_info = self.commit_info(&parent)?;
        let root = parse_oid(&parent_info.root)?;
        let created = now_ns();
        let id = commit_id(root, Some(parse_oid(&head)?), created, "revert");
        {
            let connection = self.db.lock();
            let transaction = connection.unchecked_transaction()?;
            transaction.execute(
                "INSERT INTO \"commit\" (commit_id, root_oid, parent_commit, created_ns, message)
                 VALUES (?1, ?2, ?3, ?4, 'revert')",
                (&id, &parent_info.root, &head, created),
            )?;
            transaction.execute(
                "UPDATE branch SET head_commit = ?1 WHERE name = ?2",
                (&id, branch),
            )?;
            let seq = crate::publish::next_branch_log_seq(&transaction)?;
            transaction.execute(
                "INSERT INTO branch_log (seq, branch, old_commit, new_commit, op, created_ns)
                 VALUES (?1, ?2, ?3, ?4, 'revert', ?5)",
                (seq, branch, &head, &id, created),
            )?;
            transaction.commit()?;
        }
        Ok(CommitInfo {
            id,
            root: parent_info.root,
            parent: Some(head),
            created_ns: created,
            message: "revert".to_owned(),
        })
    }

    /// Creates a workspace on `branch` and opens it.
    ///
    /// The row is written first with `state = creating`, the overlay file second, and the
    /// state flipped last: a crash leaves a row that says "this never finished", which
    /// [`Repo::workspace_open`] can rebuild (§9.3).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::AlreadyExists`] when the name is taken,
    /// [`WorkspaceError::NotFound`] when the branch does not exist.
    pub fn workspace_create(
        self: &Arc<Self>,
        name: &str,
        branch: &str,
    ) -> WorkspaceResult<Workspace> {
        let (scope_text, head) = self.branch_row(branch)?;
        let scope = unhex_32(&scope_text).ok_or_else(|| WorkspaceError::Inconsistent {
            detail: format!("branch {branch} has a malformed scope id"),
        })?;
        let commit = self.commit_info(&head)?;
        let base_root = parse_oid(&commit.root)?;
        let id = ids::new_workspace_id();
        let segment = ids::reserve_segment(&self.db, &scope_text)?;
        {
            let connection = self.db.lock();
            let taken: bool = connection.query_row(
                "SELECT COUNT(*) > 0 FROM workspace WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?;
            if taken {
                return Err(WorkspaceError::AlreadyExists {
                    name: name.to_owned(),
                });
            }
            connection.execute(
                "INSERT INTO workspace
                     (name, id, branch, base_commit, base_root, state, created_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    name,
                    &id,
                    branch,
                    &head,
                    &commit.root,
                    schema::STATE_CREATING,
                    now_ns(),
                ),
            )?;
        }
        let overlay = self.open_overlay(&id, base_root, scope, segment)?;
        overlay.set_workspace_id(&id)?;
        self.db.lock().execute(
            "UPDATE workspace SET state = ?1 WHERE name = ?2",
            (schema::STATE_READY, name),
        )?;
        Workspace::open(
            name.to_owned(),
            id,
            branch.to_owned(),
            head,
            base_root,
            Arc::new(overlay),
            Arc::clone(self),
        )
    }

    /// Opens an existing workspace, completing any interrupted publication (§9.3).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the workspace does not exist, plus overlay or
    /// recovery failures.
    pub fn workspace_open(self: &Arc<Self>, name: &str) -> WorkspaceResult<Workspace> {
        let (id, branch, base_commit, base_root_text, state) = {
            let connection = self.db.lock();
            connection
                .query_row(
                    "SELECT id, branch, base_commit, base_root, state FROM workspace
                     WHERE name = ?1",
                    [name],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| WorkspaceError::NotFound {
                    what: format!("workspace {name}"),
                })?
        };
        let (scope_text, _) = self.branch_row(&branch)?;
        let scope = unhex_32(&scope_text).ok_or_else(|| WorkspaceError::Inconsistent {
            detail: format!("branch {branch} has a malformed scope id"),
        })?;
        let base_root = parse_oid(&base_root_text)?;
        let creating = state == schema::STATE_CREATING;
        if creating {
            // The create crashed before it was usable: there is nothing worth keeping, and
            // §9.3 says to start the overlay again rather than adopt a half-made one.
            let _ = std::fs::remove_dir_all(self.workspace_dir(&id));
        }
        let segment = ids::reserve_segment(&self.db, &scope_text)?;
        let overlay = self.open_overlay(&id, base_root, scope, segment)?;
        if creating {
            self.db.lock().execute(
                "UPDATE workspace SET state = ?1 WHERE name = ?2",
                (schema::STATE_READY, name),
            )?;
        }
        // §9.3: the two rows that can disagree about a publication in flight.
        if let Some(pending) = overlay.pending_root()? {
            if pending == base_root {
                // Step 3 committed; finish step 4.
                overlay.adopt_base(pending)?;
            } else {
                // Step 3 did not commit; the overlay keeps the work.
                overlay.set_pending_root(None)?;
            }
        }
        Workspace::open(
            name.to_owned(),
            id,
            branch,
            base_commit,
            base_root,
            Arc::new(overlay),
            Arc::clone(self),
        )
    }

    /// Every workspace, by name.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn workspace_list(&self) -> WorkspaceResult<Vec<WorkspaceSummary>> {
        let connection = self.db.lock();
        let mut statement = connection
            .prepare("SELECT name, id, branch, base_commit, state FROM workspace ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            Ok(WorkspaceSummary {
                name: row.get(0)?,
                id: row.get(1)?,
                branch: row.get(2)?,
                base_commit: row.get(3)?,
                state: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Deletes a workspace's row and then its files.
    ///
    /// The row goes first because it is the record of existence: a crash in between leaves
    /// files without a row, which [`Repo::open`] cleans up. The workspace must not be open.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when it does not exist.
    pub fn workspace_discard(&self, name: &str) -> WorkspaceResult<()> {
        let id = {
            let connection = self.db.lock();
            let id: Option<String> = connection
                .query_row("SELECT id FROM workspace WHERE name = ?1", [name], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(id) = id else {
                return Err(WorkspaceError::NotFound {
                    what: format!("workspace {name}"),
                });
            };
            connection.execute("DELETE FROM workspace WHERE name = ?1", [name])?;
            crate::publish::crash_point("after-discard-row");
            id
        };
        let _ = std::fs::remove_dir_all(self.workspace_dir(&id));
        Ok(())
    }

    /// Moves a workspace onto another branch (ADR-006 §10.3, 出路 1).
    ///
    /// Only allowed when that branch's head is the commit the workspace is already based
    /// on: anything else would be a merge, and P2 has no merge.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::BranchMoved`], [`WorkspaceError::NotFound`].
    pub fn workspace_rebind(&self, name: &str, branch: &str) -> WorkspaceResult<()> {
        let connection = self.db.lock();
        let base_commit: Option<String> = connection
            .query_row(
                "SELECT base_commit FROM workspace WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(base_commit) = base_commit else {
            return Err(WorkspaceError::NotFound {
                what: format!("workspace {name}"),
            });
        };
        let (_, head) = branch_row(&connection, branch)?;
        if head != base_commit {
            return Err(WorkspaceError::BranchMoved {
                branch: branch.to_owned(),
                expected: base_commit,
                actual: head,
            });
        }
        connection.execute(
            "UPDATE workspace SET branch = ?1 WHERE name = ?2",
            (branch, name),
        )?;
        Ok(())
    }

    /// The reflog entry that made `commit` a branch's head: what it did, and what it
    /// displaced.
    ///
    /// A forced commit records the head it replaced, so that commit stays reachable as a GC
    /// root even though it is no longer on the branch's parent chain (ADR-006 §10.3).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the branch never pointed at that commit.
    pub fn branch_log_entry_for(
        &self,
        branch: &str,
        commit: &str,
    ) -> WorkspaceResult<(String, Option<String>)> {
        let connection = self.db.lock();
        connection
            .query_row(
                "SELECT op, old_commit FROM branch_log
                 WHERE branch = ?1 AND new_commit = ?2
                 ORDER BY seq DESC LIMIT 1",
                [branch, commit],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| WorkspaceError::NotFound {
                what: format!("a branch_log entry for {commit}"),
            })
    }
    /// Counts of what the main database records, for `stats`.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn stats(&self) -> WorkspaceResult<RepoStats> {
        let connection = self.db.lock();
        let count = |table: &str| -> WorkspaceResult<u64> {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            let value: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
            Ok(u64::try_from(value).unwrap_or(0))
        };
        Ok(RepoStats {
            branches: count("branch")?,
            commits: count("\"commit\"")?,
            workspaces: count("workspace")?,
            snapshots: count("snapshot")?,
            branch_log_entries: count("branch_log")?,
        })
    }

    /// Every snapshot root the repository records.
    ///
    /// All commits, all named snapshots, and every workspace's current base. `verify` walks
    /// them; P2b's GC will need the same set (ADR-006 §15).
    ///
    /// # Errors
    ///
    /// SQLite failures, or [`WorkspaceError::Inconsistent`] for a stored value that is not
    /// an object id.
    pub fn roots(&self) -> WorkspaceResult<Vec<ObjectId>> {
        let connection = self.db.lock();
        let mut statement = connection.prepare(
            "SELECT root_oid FROM \"commit\"
             UNION SELECT root_oid FROM snapshot
             UNION SELECT base_root FROM workspace",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut roots = Vec::new();
        for row in rows {
            roots.push(parse_oid(&row?)?);
        }
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    /// The root a commit publishes.
    pub(crate) fn commit_root(&self, id: &str) -> WorkspaceResult<ObjectId> {
        let connection = self.db.lock();
        let root: Option<String> = connection
            .query_row(
                "SELECT root_oid FROM \"commit\" WHERE commit_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        match root {
            Some(text) => parse_oid(&text),
            None => Err(WorkspaceError::NotFound {
                what: format!("commit {id}"),
            }),
        }
    }

    /// Moves a workspace's view without touching its base commit (used by `restore`).
    pub(crate) fn set_workspace_base_root(
        &self,
        name: &str,
        root: ObjectId,
    ) -> WorkspaceResult<()> {
        let connection = self.db.lock();
        let changed = connection.execute(
            "UPDATE workspace SET base_root = ?1 WHERE name = ?2",
            (root.to_string(), name),
        )?;
        if changed == 0 {
            return Err(WorkspaceError::NotFound {
                what: format!("workspace {name}"),
            });
        }
        Ok(())
    }

    /// The root directory of an empty tree, encoded and published.
    fn empty_root(&self, scope: [u8; 32]) -> WorkspaceResult<ObjectId> {
        let profile = Profile::CURRENT;
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sink = crate::materialize::StoreSink::new(&store);
        let now = now_ns();
        let records = [InodeRecord {
            serial: 0,
            kind: fs_snapshot::Kind::Directory,
            size: 0,
            created_ns: now,
            modified_ns: now,
            content: Content::Empty,
        }];
        let table = crate::materialize::publish(&mut sink, |sink| {
            encode_inode_table(&profile, records.into_iter(), sink)
        })?;
        crate::materialize::publish(&mut sink, |sink| {
            encode_root(
                &profile,
                &RootValue {
                    scope_id: scope,
                    inode_table: table,
                },
                sink,
            )
        })
    }

    /// The serial scope a published tree was allocated in.
    fn root_scope(&self, root: ObjectId) -> WorkspaceResult<[u8; 32]> {
        let store = Store::open(&self.store_dir, OpenMode::ReadOnly)?;
        Ok(SnapshotReader::open(&store, root, Profile::CURRENT)?.scope_id())
    }

    /// Opens (or creates) one workspace's overlay file.
    fn open_overlay(
        &self,
        id: &str,
        base_root: ObjectId,
        scope: [u8; 32],
        segment: SerialSegment,
    ) -> WorkspaceResult<fs_overlay::OverlayStore> {
        let store = Store::open(&self.store_dir, OpenMode::ReadWrite)?;
        Ok(fs_overlay::OverlayStore::open(
            self.workspace_dir(id).join("overlay.sqlite"),
            store,
            &fs_overlay::OverlayOptions {
                base_root,
                profile: Profile::CURRENT,
                scope_id: scope,
                serial_first: segment.first,
                serial_end: segment.end,
            },
        )?)
    }

    /// A branch's `(scope, head)`.
    fn branch_row(&self, name: &str) -> WorkspaceResult<(String, String)> {
        let connection = self.db.lock();
        branch_row(&connection, name)
    }

    /// Deletes workspace directories that no row claims (§9.3).
    fn sweep_orphans(&self) -> WorkspaceResult<()> {
        let dir = self.store_dir.join("workspaces");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let known: std::collections::BTreeSet<String> = {
            let connection = self.db.lock();
            let mut statement = connection.prepare("SELECT id FROM workspace")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known.contains(&name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }
}

fn branch_row(connection: &Connection, name: &str) -> WorkspaceResult<(String, String)> {
    connection
        .query_row(
            "SELECT scope_id, head_commit FROM branch WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| WorkspaceError::NotFound {
            what: format!("branch {name}"),
        })
}

fn commit_row(connection: &Connection, id: &str) -> WorkspaceResult<CommitInfo> {
    connection
        .query_row(
            "SELECT commit_id, root_oid, parent_commit, created_ns, message
             FROM \"commit\" WHERE commit_id = ?1",
            [id],
            |row| {
                Ok(CommitInfo {
                    id: row.get(0)?,
                    root: row.get(1)?,
                    parent: row.get(2)?,
                    created_ns: row.get(3)?,
                    message: row.get(4)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| WorkspaceError::NotFound {
            what: format!("commit {id}"),
        })
}
