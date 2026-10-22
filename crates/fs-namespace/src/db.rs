//! Opening the namespace layer over a Store (ADR-003 §4.2.1).

use rusqlite::{Connection, OpenFlags};

use cas_store::{OpenMode, Store};

use crate::error::NamespaceError;
use crate::schema;

/// The namespace layer's view of one Store.
///
/// Holds a connection of its own to the Store's metadata file. The object half
/// of an operation goes through the [`Store`] (another connection), which is why
/// the two do not share a transaction — see ADR-003 §4.2.1.
#[derive(Debug)]
pub struct NamespaceDb {
    connection: Connection,
    read_only: bool,
}

impl NamespaceDb {
    /// Opens (or creates) the namespace tables in `store`'s metadata file.
    ///
    /// # Errors
    ///
    /// [`NamespaceError::SchemaMismatch`] when the recorded tables are not the
    /// ones this build creates, and [`NamespaceError::Store`] for connection
    /// failures.
    pub fn open(store: &Store, mode: OpenMode) -> Result<Self, NamespaceError> {
        let read_only = mode == OpenMode::ReadOnly;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
        };
        let mut connection = Connection::open_with_flags(store.metadata_path(), flags)?;
        crate::configure(&connection)?;

        let present = schema::is_present(&connection)?;
        match (present, mode) {
            (true, _) => schema::validate(&connection)?,
            (false, OpenMode::Create) => schema::create(&mut connection)?,
            (false, _) => {
                return Err(NamespaceError::SchemaMismatch {
                    what: "namespace tables are missing",
                })
            }
        }
        Ok(Self {
            connection,
            read_only,
        })
    }

    /// Whether this handle refuses writes.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}
