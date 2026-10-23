//! [`TreeStore`] for the overlay backend, plus `flush` (ADR-006 §7.1, §8).

use fs_namespace::{NamespaceResult, OpLabel, TreeStore};

use crate::error::{OverlayError, OverlayResult};
use crate::store::{OverlayStore, ScopeGuard};
use crate::view::{into_namespace, OverlayRead};
use crate::write_view::OverlayWrite;

impl TreeStore for OverlayStore {
    type Read<'t> = OverlayRead<'t>;
    type Write<'t> = OverlayWrite<'t>;

    fn read<T>(
        &self,
        body: impl FnOnce(&Self::Read<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        let connection = self.lock();
        // TX3: one consistent state across the body. The overlay is exclusive and has
        // no other writer, so `BEGIN DEFERRED` costs nothing and makes a multi-step
        // rule read unable to see its own half-finished state.
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        let view = OverlayRead::new(&connection, &self.base);
        let outcome = body(&view);
        let _ = connection.execute_batch("COMMIT");
        outcome
    }

    fn write<T>(
        &self,
        op: OpLabel,
        body: impl FnOnce(&mut Self::Write<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        let _guard = ScopeGuard::enter().map_err(into_namespace)?;
        // The label is audit data; the overlay has no chain of publications to record
        // it in (that is the table backend's D+ representation). Accepted and dropped.
        let _ = op;
        let mut connection = self.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        let mut view = OverlayWrite {
            transaction,
            base: &self.base,
        };
        // TX1: an `Err` from the body drops the transaction, rolling it back; a panic
        // unwinds through the same drop.
        let value = body(&mut view)?;
        view.transaction
            .commit()
            .map_err(|error| into_namespace(OverlayError::Sqlite(error)))?;
        Ok(value)
    }
}

impl OverlayStore {
    /// Makes every change that has returned so far durable (ADR-006 §8).
    ///
    /// The overlay commits each change with `synchronous = NORMAL`, which is what keeps
    /// an unflushed change visible to this workspace without an fsync. `flush` raises
    /// the level for **one** commit and checkpoints, so everything committed before it
    /// is on disk; then it restores `NORMAL` so the next change is cheap again.
    ///
    /// A trivial meta write is what makes "one commit at `FULL`" a real event: without
    /// it, a `COMMIT` with no changes would not push the earlier WAL frames out.
    ///
    /// # Errors
    ///
    /// SQLite failures while syncing or checkpointing.
    pub fn flush(&self) -> OverlayResult<()> {
        let _guard = ScopeGuard::enter()?;
        let connection = self.lock();
        connection.execute_batch("PRAGMA synchronous = FULL")?;
        let sequence: i64 = connection
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM ov_meta WHERE key = 'flush_seq'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        connection.execute(
            "INSERT INTO ov_meta (key, value) VALUES ('flush_seq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [(sequence + 1).to_string()],
        )?;
        connection.execute_batch("PRAGMA wal_checkpoint(FULL)")?;
        connection.execute_batch("PRAGMA synchronous = NORMAL")?;
        Ok(())
    }

    /// How many times this workspace has been flushed, for tests and diagnostics.
    ///
    /// # Errors
    ///
    /// SQLite failures.
    pub fn flush_count(&self) -> OverlayResult<u64> {
        let connection = self.lock();
        let value: Option<i64> = connection
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM ov_meta WHERE key = 'flush_seq'",
                [],
                |row| row.get(0),
            )
            .ok();
        Ok(value.unwrap_or(0).max(0) as u64)
    }
}
