//! Reading objects through a **connection the caller already holds** (P2b-6 §23).
//!
//! Maintenance owns `store.sqlite` and every overlay exclusively (ADR-008 §6.1), so the collector
//! cannot open a `cas_store::Store` for marking: a second connection to an exclusively-locked file is
//! refused, and "just open another one" is exactly the mistake §19 names. This type satisfies
//! [`fs_snapshot::ObjectSource`] over the maintenance connection instead, so marking and root
//! enumeration share one connection per database.

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
        let row: Option<(i64, String, i64, i64)> = self
            .connection
            .query_row(
                "SELECT kind, location, offset, length FROM object_location WHERE oid = ?1",
                [oid.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();
        let Some((kind, location, offset, length)) = row else {
            return Err(SnapshotError::Store(cas_store::StoreError::NotFound {
                oid,
            }));
        };
        let offset = u64::try_from(offset).unwrap_or(0);
        let length = u64::try_from(length).unwrap_or(0);
        let bytes = if kind == cas_store::index::kind_pack() {
            cas_store::repack::read_frame(&self.layout, &location, offset, length)?
        } else {
            // A loose object is its own file; the index row only says where it lives.
            std::fs::read(self.layout.object_path(oid))
                .map_err(|error| SnapshotError::Store(cas_store::StoreError::Io(error)))?
        };
        // The identity check is what makes a wrong offset a *detected* error rather than silent
        // nonsense: the row is trusted only as far as this comparison agrees with it.
        cas_core::verify_id(oid, &bytes).map_err(|error| {
            SnapshotError::Store(cas_store::StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )))
        })?;
        Ok(bytes)
    }
}
