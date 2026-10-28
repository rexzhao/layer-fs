//! Publication: a workspace's view becomes a snapshot, then a commit (ADR-006 §9.1).
//!
//! The four steps, in the order that makes a crash survivable:
//!
//! ```text
//! 0. flush the overlay, so what is about to be materialised is on disk
//! 1. materialise into CAS objects      — nothing references them yet
//! 2. overlay: pending_root = R          — "I intend to publish R"
//! 3. main db, one transaction: publish `R` (commit row + branch head + branch_log,
//!    or a snapshot row)
//! 4. overlay: adopt R as the new base, clearing everything it held
//! ```
//!
//! **Step 3 is the single publication point.** Before it, a crash leaves unreferenced
//! objects and an overlay that still holds the work (recovered by §9.3). After it, a crash
//! leaves a committed tree whose overlay has not been reset — also recovered by §9.3,
//! because `pending_root` still names R. There is no window in which a reference exists
//! without its object (I5).

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::ids::commit_id;
use crate::main_db::now_ns;
use crate::materialize;
use crate::workspace::Workspace;

/// The environment variable that names a crash-injection point (ADR-006 §9.4).
pub(crate) const CRASH_AT: &str = "WCAS_WORKSPACE_CRASH_AT";

/// The environment variable that names a **pause** point (ADR-010 S2).
pub(crate) const PAUSE_AT: &str = "LAYERFS_PAUSE_AT";

/// Aborts the process when the harness asked for a crash here.
///
/// ADR-006 §9.4 lists the points where a publication can be interrupted, and each one is
/// only meaningful if the process really dies: no unwinding, no Drop, no SQLite close.
/// The hook is inert unless the variable names this exact point, so a normal run pays one
/// environment lookup.
/// **Stops inside a publication until someone lets it continue** (ADR-010 S2, G1/G2/G3).
///
/// With `LAYERFS_PAUSE_AT=<point>` set, the named point blocks until its release file appears (or 30 s pass). That turns "a write arriving during a commit"
/// from a race nobody can schedule into an experiment: the test starts a commit, waits for the `.here` marker, and only then writes.
///
/// Deliberately **not** a crash point: crashing tests recovery, pausing tests **ordering**, and a pause that could abort would conflate the two.
pub(crate) fn pause_point(name: &str) {
    let Ok(requested) = std::env::var(PAUSE_AT) else {
        return;
    };
    if requested != name {
        return;
    }
    let directory = std::env::temp_dir();
    // **The tag keeps two paused tests apart**: the marker is a file in `%TEMP%`, and two processes paused at the same point would otherwise read each other's
    // release file — one test's `go` would let the other's commit continue, which is the kind of interference that makes a concurrency criterion lie.
    let tag = std::env::var("LAYERFS_PAUSE_TAG").unwrap_or_default();
    let release = directory.join(format!("layerfs-pause-{name}{tag}.go"));
    let marker = directory.join(format!("layerfs-pause-{name}{tag}.here"));
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::write(&marker, std::process::id().to_string());
    let started = std::time::Instant::now();
    while !release.exists() && started.elapsed() < std::time::Duration::from_secs(30) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = std::fs::remove_file(&marker);
}

pub(crate) fn crash_point(name: &str) {
    if let Ok(requested) = std::env::var(CRASH_AT) {
        if requested == name {
            std::process::abort();
        }
    }
}

/// What a publication did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    /// The commit the branch now points at, for a `commit`.
    pub commit: Option<String>,
    /// The snapshot root that was published.
    pub root: String,
}

impl Workspace {
    /// Materialises the view and records a snapshot root without moving the branch.
    ///
    /// A snapshot is a **GC root**: the tree stays readable through the `snapshot` table
    /// even after the branch moves on (ADR-006 §15).
    ///
    /// # Errors
    ///
    /// Materialisation, overlay or main-database failures.
    pub fn snapshot(&self) -> WorkspaceResult<PublishOutcome> {
        let gate = std::sync::Arc::clone(&self.gate);
        let _gate = gate.write();
        let root = self.materialise()?;
        crash_point("after-materialise");
        // Step 2, then step 3, then step 4.
        self.overlay.set_pending_root(Some(root))?;
        crash_point("after-intent");
        if let Err(error) = self.record_snapshot(root) {
            // Nothing was published, so the intent must not outlive the call: §9.3 would
            // otherwise read it as "step 3 may have committed".
            let _ = self.overlay.set_pending_root(None);
            return Err(error);
        }
        crash_point("after-publish");
        self.adopt(root, None)?;
        Ok(PublishOutcome {
            commit: None,
            root: root.to_string(),
        })
    }

    /// Materialises the view and commits it to the workspace's branch.
    ///
    /// The branch head is checked **inside** the main-database transaction: if it moved
    /// since this workspace was last published, the call fails with
    /// [`WorkspaceError::BranchMoved`] and **nothing changes** — the overlay keeps the
    /// work, the branch keeps its head (ADR-006 §10.3).
    ///
    /// `force` skips that check. The new commit's parent is still this workspace's base
    /// commit, not the head it displaces, and `branch_log` records what was displaced so
    /// the old commit stays reachable.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NothingToCommit`] when the view already equals `root(base_commit)`,
    /// [`WorkspaceError::BranchMoved`] when the branch moved.
    pub fn commit(&self, message: &str, force: bool) -> WorkspaceResult<PublishOutcome> {
        // **The publication gate, held for writing across the whole sequence** (ADR-010 §10.3): materialise reads the view, adopt moves the base, and a write
        // that slipped between the two would be in neither. Taking it here — before the first step, released after the last — is what makes G1 a property
        // rather than a hope.
        let gate = std::sync::Arc::clone(&self.gate);
        let _gate = gate.write();
        let root = self.materialise()?;
        crash_point("after-materialise");
        // **The window G1 is about**: materialise has read the view, adopt has not moved the base yet. A write here would be in neither tree.
        pause_point("after-materialise");
        if root == self.repo.commit_root(&self.base_commit())? {
            return Err(WorkspaceError::NothingToCommit);
        }
        self.overlay.set_pending_root(Some(root))?;
        crash_point("after-intent");
        let id = match self.record_commit(root, message, force) {
            Ok(id) => id,
            Err(error) => {
                let _ = self.overlay.set_pending_root(None);
                return Err(error);
            }
        };
        crash_point("after-publish");
        self.adopt(root, Some(id.clone()))?;
        Ok(PublishOutcome {
            commit: Some(id),
            root: root.to_string(),
        })
    }

    /// Step 1: walk the view and encode it.
    fn materialise(&self) -> WorkspaceResult<cas_core::ObjectId> {
        // Step 0: make sure what we are about to read is on disk. Without it a snapshot
        // could be taken from data that only exists in the WAL of a process that then
        // crashes.
        self.overlay.flush()?;
        let nodes = self.overlay.read_tree(|view| materialize::collect(view))?;
        let scope = self.overlay.scope_id();
        // The read closure opens its own overlay scope, so it must not be called from
        // inside the traversal's scope — which is why pass 1 recorded the structure and
        // pass 2 only needs paths.
        let store = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root =
            materialize::encode(&store, &nodes, self.profile, scope, |path, offset, len| {
                Ok(self.overlay.read_at(path, offset, len)?)
            })?;
        Ok(root)
    }

    /// Step 3 for a snapshot: record the root as a GC root and move the view's base.
    fn record_snapshot(&self, root: cas_core::ObjectId) -> WorkspaceResult<()> {
        let mut connection = self.repo.db().lock();
        // BEGIN IMMEDIATE, not a deferred transaction that reads first and writes second:
        // under WAL a deferred transaction upgrades its read snapshot to a write lock, and
        // SQLite answers a contested upgrade with SQLITE_BUSY_SNAPSHOT **without running the
        // busy handler** — so the 5 s wait would never be used. ADR-006 §9.1 asks for
        // BEGIN IMMEDIATE precisely because the head is read before it is written.
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO snapshot (root_oid, workspace, created_ns) VALUES (?1, ?2, ?3)",
            (root.to_string(), &self.name, now_ns()),
        )?;
        transaction.execute(
            "UPDATE workspace SET base_root = ?1 WHERE name = ?2",
            (root.to_string(), &self.name),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Step 3 for a commit: the branch head, the commit row and the reflog, atomically.
    fn record_commit(
        &self,
        root: cas_core::ObjectId,
        message: &str,
        force: bool,
    ) -> WorkspaceResult<String> {
        let created = now_ns();
        let branch = self.branch();
        let parent = self.parent_commit();
        let id = commit_id(root, Some(parent), created, message);
        let expected = self.base_commit();
        let mut connection = self.repo.db().lock();
        // BEGIN IMMEDIATE, not a deferred transaction that reads first and writes second:
        // under WAL a deferred transaction upgrades its read snapshot to a write lock, and
        // SQLite answers a contested upgrade with SQLITE_BUSY_SNAPSHOT **without running the
        // busy handler** — so the 5 s wait would never be used. ADR-006 §9.1 asks for
        // BEGIN IMMEDIATE precisely because the head is read before it is written.
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let head: String = transaction
            .query_row(
                "SELECT head_commit FROM branch WHERE name = ?1",
                [branch.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| WorkspaceError::NotFound {
                what: format!("branch {}", branch),
            })?;
        if !force && head != expected {
            // Dropping the transaction rolls it back: no commit row, no head move, no log
            // entry, and the workspace row still points at the old base.
            return Err(WorkspaceError::BranchMoved {
                branch: branch.clone(),
                expected,
                actual: head,
            });
        }
        transaction.execute(
            "INSERT INTO \"commit\" (commit_id, root_oid, parent_commit, created_ns, message)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (&id, root.to_string(), &expected, created, message),
        )?;
        transaction.execute(
            "UPDATE branch SET head_commit = ?1 WHERE name = ?2",
            (&id, branch.as_str()),
        )?;
        // A forced commit displaces whatever the head was, so the log records the displaced
        // commit: it is still a GC root even though it left the parent chain (§10.3).
        let op = if head == expected {
            "commit"
        } else {
            "commit-force"
        };
        let seq = next_branch_log_seq(&transaction)?;
        transaction.execute(
            "INSERT INTO branch_log (seq, branch, old_commit, new_commit, op, created_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (seq, branch.as_str(), &head, &id, op, created),
        )?;
        transaction.execute(
            "UPDATE workspace SET base_commit = ?1, base_root = ?2 WHERE name = ?3",
            (&id, root.to_string(), &self.name),
        )?;
        transaction.commit()?;
        Ok(id)
    }
}

pub(crate) fn next_branch_log_seq(connection: &rusqlite::Connection) -> WorkspaceResult<i64> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM branch_log",
        [],
        |row| row.get(0),
    )?)
}
