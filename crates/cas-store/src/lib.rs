//! The persistent content-addressed Store for the Workspace CAS filesystem.
//!
//! A Store is one directory holding a SQLite metadata index and one file per
//! object. Object identity and framing come from [`cas_core`]; this crate owns
//! where objects live, how a `put` becomes durable, and what a reader may trust.
//!
//! ```no_run
//! use cas_core::Role;
//! use cas_store::{OpenMode, Store};
//!
//! let store = Store::open("my-store", OpenMode::Create)?;
//! let oid = store.put(Role::Bytes, b"hello")?.oid;
//! let frame = store.get(oid)?;
//! let object = cas_core::Object::decode(&frame)?;
//! assert_eq!(object.value(), b"hello");
//! assert!(store.verify()?.is_clean());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What a `put` guarantees
//!
//! The object's bytes are on stable storage before the index row that publishes
//! it commits, so a crash leaves the object either invisible or fully readable —
//! never half-published. Orphaned object files left by such a crash are ignored
//! by readers and reused by a later `put` of the same object. See
//! `docs/adr/ADR-002-store-layout.md` §5 for the per-step crash table.
//!
//! # What this crate does not do
//!
//! No namespace, directory or path operations, no CLI, no garbage collection
//! (orphans are kept deliberately), no packing, compression, CDC or delta
//! encoding, and no schema migration: an unknown schema is refused, not
//! upgraded.

#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod durability;
mod error;
mod layout;
mod object;
mod open;
mod schema;
pub mod testing;

use std::path::Path;

use cas_core::{Object, ObjectId, Role};

pub use error::{CorruptCause, StoreError, StoreResult};
pub use layout::Layout;
pub use object::{PutOutcome, VerificationFailure, VerificationReport};
pub use open::{OpenMode, Opened};

use open::Database;

/// The identity and canonical frame a `put` published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The object's identity.
    pub oid: ObjectId,
    /// The object's complete canonical frame.
    pub frame: Vec<u8>,
    /// What the put had to do on disk.
    pub outcome: PutOutcome,
}

impl Published {
    /// The identity and frame as a convenience pair.
    #[must_use]
    pub fn into_parts(self) -> (ObjectId, Vec<u8>) {
        (self.oid, self.frame)
    }
}

/// One open Store.
///
/// Writes are serialised by SQLite's own write lock; a contended write returns
/// [`StoreError::Busy`] rather than waiting and retrying (ADR-002 §8).
#[derive(Debug)]
pub struct Store {
    database: Database,
    layout: Layout,
    read_only: bool,
    opened: Opened,
}

impl Store {
    /// Opens (or creates) the Store at `root`.
    ///
    /// For an existing Store this runs the recovery protocol: schema and policy
    /// validation, then removal of stale `tmp/` files. Object files without an
    /// index row are kept (ADR-002 §10).
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] when the directory shape cannot be read or created,
    /// [`StoreError::UnsupportedSchemaVersion`] or [`StoreError::SchemaMismatch`]
    /// when the Store on disk is not one this build understands, and
    /// [`StoreError::Sqlite`] for database failures.
    pub fn open(root: impl AsRef<Path>, mode: OpenMode) -> StoreResult<Self> {
        let (database, layout, opened) = open::open(root.as_ref(), mode)?;
        Ok(Self {
            database,
            layout,
            read_only: mode == OpenMode::ReadOnly,
            opened,
        })
    }

    /// What `open` did: whether it created the Store, and how many stale
    /// temporary files it removed.
    #[must_use]
    pub fn opened(&self) -> Opened {
        self.opened
    }

    /// The Store's on-disk layout.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Whether this handle is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The identity a `put` of `role`/`value` would publish.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidObject`] or [`StoreError::TooLarge`] when the object
    /// cannot be framed.
    pub fn identify(role: Role, value: &[u8]) -> StoreResult<ObjectId> {
        Ok(Object::new(role, value)?.id()?)
    }

    /// Publishes `value` under `role`, returning its identity and frame.
    ///
    /// Idempotent: a second `put` of the same role and value writes nothing and
    /// reports [`PutOutcome::AlreadyPresent`].
    ///
    /// # Errors
    ///
    /// [`StoreError::ReadOnly`] on a read-only handle, [`StoreError::InvalidObject`]
    /// or [`StoreError::TooLarge`] when the object cannot be framed,
    /// [`StoreError::Busy`] under contention, [`StoreError::Corrupt`] when a
    /// leftover file for this object does not match what would be written, and
    /// [`StoreError::Io`]/[`StoreError::Sqlite`] for storage failures.
    pub fn put(&self, role: Role, value: &[u8]) -> StoreResult<Published> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let object = Object::new(role, value)?;
        let oid = object.id()?;
        let frame = object.to_vec();
        let outcome = object::put(&self.database, &self.layout, role, value)?;
        Ok(Published {
            oid,
            frame,
            outcome,
        })
    }

    /// Reads one published object's canonical frame.
    ///
    /// The bytes are decoded and their BLAKE3 identity recomputed; a mismatch is
    /// [`StoreError::Corrupt`], never a partial or zero-filled value.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when the object was never published,
    /// [`StoreError::Corrupt`] when its bytes cannot be trusted.
    pub fn get(&self, oid: ObjectId) -> StoreResult<Vec<u8>> {
        object::get(&self.database, &self.layout, oid)
    }

    /// Whether `oid` is published.
    ///
    /// This consults the index only and does not read the object's file, so a
    /// `true` answer does not by itself prove the file is intact; [`Store::get`]
    /// does that.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] for database failures.
    pub fn exists(&self, oid: ObjectId) -> StoreResult<bool> {
        object::published(&self.database, oid)
    }

    /// Checks every published object and reports what failed.
    ///
    /// Read-only: it repairs, deletes and quarantines nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the index cannot be read.
    pub fn verify(&self) -> StoreResult<VerificationReport> {
        object::verify_all(&self.database, &self.layout)
    }

    /// The number of published objects.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] for database failures.
    pub fn object_count(&self) -> StoreResult<u64> {
        let count: i64 = self.database.connection.query_row(
            "SELECT COUNT(*) FROM object_location",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}
