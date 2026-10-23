//! The overlay's half of the publication protocol (ADR-006 §9.1 steps 0, 2 and 4).
//!
//! Publishing a workspace's view is four steps; this module owns the two that touch the
//! overlay (marking the intent with `pending_root`, then adopting the result) and the
//! read scope the materialiser walks the tree in.
//!
//! # Why the intent is recorded at all
//!
//! The overlay and the main database are separate files, so "objects published, rows
//! written, overlay cleared" cannot be one transaction. `pending_root` is the
//! write-ahead record that makes the gap recoverable: if the main database already
//! adopted root `R` and the process dies before the overlay is cleared, the next
//! [`OverlayStore::adopt_base`] finishes the job; if it did not, the next open clears
//! the stale intent and keeps the work (ADR-006 §9.3). Either way the workspace is not
//! left believing its changes are committed when they are not.

use cas_core::ObjectId;
use rusqlite::Connection;

use crate::error::{OverlayError, OverlayResult};
use crate::store::{meta, OverlayStore, ScopeGuard};
use crate::view::OverlayRead;

/// The largest serial the overlay will ever hand out, so a corrupted meta row is caught
/// here rather than as an out-of-range `i64` conversion later.
const MAX_SERIAL: u64 = i64::MAX as u64;

impl OverlayStore {
    /// Runs `body` against one consistent read view of the composed tree.
    ///
    /// The error type is the caller's: fs-workspace has its own error enum and maps both
    /// the overlay's and the namespace's failures into it, so this stays free of either.
    ///
    /// # Errors
    ///
    /// [`OverlayError::NestedTransaction`] if a scope is already open on this thread,
    /// SQLite failures, and whatever `body` returns.
    pub fn read_tree<T, E>(
        &self,
        body: impl FnOnce(&OverlayRead<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<OverlayError>,
    {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        // TX3: the whole walk sees one state, so a directory listing and the inode it
        // names cannot disagree (ADR-006 §7.4).
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(OverlayError::Sqlite)?;
        let view = OverlayRead::new(&connection, &self.base);
        let outcome = body(&view);
        let _ = connection.execute_batch("COMMIT");
        outcome
    }

    /// The workspace this overlay belongs to, if it has been told.
    ///
    /// The row is created empty and stamped by fs-workspace right after it registers the
    /// workspace (ADR-006 §5.2), because the id is the main database's to choose.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn workspace_id(&self) -> OverlayResult<Option<String>> {
        let connection = self.lock();
        let value = meta(&connection, "workspace_id")?;
        Ok(Some(value).filter(|text| !text.is_empty()))
    }

    /// Records which workspace this overlay belongs to.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn set_workspace_id(&self, id: &str) -> OverlayResult<()> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        set_meta(&connection, "workspace_id", id)
    }

    /// The root a publication is currently aiming at, if one is in flight (§9.1 step 2).
    ///
    /// # Errors
    ///
    /// [`OverlayError::InvalidRecord`] when the stored text is not an object id.
    pub fn pending_root(&self) -> OverlayResult<Option<ObjectId>> {
        let connection = self.lock();
        let value = meta(&connection, "pending_root")?;
        if value.is_empty() {
            return Ok(None);
        }
        value
            .parse::<ObjectId>()
            .map(Some)
            .map_err(|error| OverlayError::InvalidRecord {
                field: "ov_meta.pending_root",
                detail: error.to_string(),
            })
    }

    /// Writes (or clears) the publication intent, durably.
    ///
    /// `synchronous = FULL` for this one commit: the intent is the only thing that
    /// distinguishes "the main database may have adopted this root" from "it did not",
    /// so losing it would make recovery guess.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn set_pending_root(&self, root: Option<ObjectId>) -> OverlayResult<()> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        let value = root.map_or_else(String::new, |root| root.to_string());
        connection.execute_batch("PRAGMA synchronous = FULL")?;
        let outcome =
            set_meta(&connection, "pending_root", &value).and_then(|()| checkpoint(&connection));
        connection.execute_batch("PRAGMA synchronous = NORMAL")?;
        outcome
    }

    /// Step 4: everything the overlay held is now part of `root`.
    ///
    /// One transaction removes every overlay row and points the base at `root`, then the
    /// commit is made durable. Doing it in one transaction is what makes recovery a
    /// single question — either the overlay is still dirty over the old base (and
    /// `pending_root` says so) or it is empty over the new one.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn adopt_base(&self, root: ObjectId) -> OverlayResult<()> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        connection.execute_batch("PRAGMA synchronous = FULL")?;
        let outcome = (|| -> OverlayResult<()> {
            connection.execute_batch("BEGIN IMMEDIATE")?;
            connection.execute("DELETE FROM ov_entry", [])?;
            connection.execute("DELETE FROM ov_page", [])?;
            connection.execute("DELETE FROM ov_inode", [])?;
            set_meta(&connection, "base_root", &root.to_string())?;
            set_meta(&connection, "pending_root", "")?;
            connection.execute_batch("COMMIT")?;
            checkpoint(&connection)
        })();
        connection.execute_batch("PRAGMA synchronous = NORMAL")?;
        outcome?;
        // Only after the file says so: a reader sees the new base through the meta row
        // first, and the in-memory copy must not disagree with it.
        self.base.set_root(root);
        Ok(())
    }

    /// Whether the overlay holds any change at all.
    ///
    /// `restore` needs this to refuse replacing a workspace that still has uncommitted
    /// work (ADR-006 §10.2). Any row counts as a change, including a whiteout or a
    /// tombstone: they are edits to the view even though they remove something.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn is_dirty(&self) -> OverlayResult<bool> {
        let connection = self.lock();
        let rows: i64 = connection.query_row(
            "SELECT (SELECT COUNT(*) FROM ov_inode)
                  + (SELECT COUNT(*) FROM ov_entry)
                  + (SELECT COUNT(*) FROM ov_page)",
            [],
            |row| row.get(0),
        )?;
        Ok(rows > 0)
    }

    /// The serial range this workspace has reserved, `[next, end)` (ADR-006 §6).
    ///
    /// # Errors
    ///
    /// [`OverlayError::InvalidRecord`] when the stored bounds are not numbers.
    pub fn serial_range(&self) -> OverlayResult<(u64, u64)> {
        let connection = self.lock();
        Ok((
            serial_meta(&connection, "serial_next")?,
            serial_meta(&connection, "serial_end")?,
        ))
    }

    /// Extends the reserved range to `[_, end)` after fs-workspace reserved more.
    ///
    /// `next` is left alone: the caller tops up *before* the range runs dry, so no
    /// unissued serial is skipped.
    ///
    /// # Errors
    ///
    /// [`OverlayError::InvalidRecord`] when `end` would not move forward, or a SQLite
    /// failure.
    pub fn extend_serials(&self, end: u64) -> OverlayResult<()> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        let current = serial_meta(&connection, "serial_end")?;
        if end <= current {
            return Err(OverlayError::InvalidRecord {
                field: "ov_meta.serial_end",
                detail: format!("cannot shrink the reserved range from {current} to {end}"),
            });
        }
        set_meta(&connection, "serial_end", &end.to_string())
    }
}

fn set_meta(connection: &Connection, key: &str, value: &str) -> OverlayResult<()> {
    connection.execute(
        "INSERT INTO ov_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

fn serial_meta(connection: &Connection, key: &str) -> OverlayResult<u64> {
    let text = meta(connection, key)?;
    text.parse::<u64>()
        .ok()
        .filter(|value| *value <= MAX_SERIAL)
        .ok_or_else(|| OverlayError::InvalidRecord {
            field: "ov_meta",
            detail: format!("{key} is not a serial: {text:?}"),
        })
}

/// Pushes the WAL to disk. Called with `synchronous = FULL` already in force.
fn checkpoint(connection: &Connection) -> OverlayResult<()> {
    connection.execute_batch("PRAGMA wal_checkpoint(FULL)")?;
    Ok(())
}
