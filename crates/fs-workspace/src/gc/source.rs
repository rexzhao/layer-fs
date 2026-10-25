//! Reading objects through a **connection the caller already holds** (P2b-6 §19, §23, §24).
//!
//! Maintenance owns `store.sqlite` and every overlay exclusively (ADR-008 §6.1), so the collector
//! cannot open a `cas_store::Store` for marking: a second connection to an exclusively-locked file is
//! refused, and "just open another one" is exactly the mistake §19 names. This type satisfies
//! [`fs_snapshot::ObjectSource`] over the maintenance connection instead, so marking and root
//! enumeration share one connection per database.
//!
//! It deliberately contains **no object-reading logic of its own**: `get` is a call into
//! `cas_store::read_published`, which is the same path `Store::get` uses, including the length,
//! identity and role checks (§24). A second implementation here would be a second set of checks to
//! keep in sync, and the copy that drifts is the one nobody exercises.

use std::path::Path;

use cas_core::ObjectId;
use fs_snapshot::{ObjectSource, SnapshotError, SnapshotResult};

/// An object source backed by a maintenance connection plus the store's layout.
#[derive(Debug)]
pub struct MaintenanceSource<'a> {
    connection: &'a rusqlite::Connection,
    layout: cas_store::Layout,
}

impl<'a> MaintenanceSource<'a> {
    /// Builds a source over `connection`, which must be the connection that holds the store's lock.
    #[must_use]
    pub fn new(connection: &'a rusqlite::Connection, store_dir: impl AsRef<Path>) -> Self {
        Self {
            connection,
            layout: cas_store::Layout::new(store_dir.as_ref().to_path_buf()),
        }
    }
}

impl ObjectSource for MaintenanceSource<'_> {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        cas_store::read_published(self.connection, &self.layout, oid).map_err(SnapshotError::Store)
    }
}
