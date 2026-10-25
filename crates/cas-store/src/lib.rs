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

mod batch;
mod durability;
mod error;
pub mod index;
mod layout;
mod object;
mod objects_scan;
mod open;
mod pack;
pub mod repack;
mod schema;

/// Crash-point helpers for this crate's own crash-injection tests.
///
/// Compiled only with the `crash-points` feature, which the crate enables for
/// its own tests and no consumer receives by default (ADR-002 §13.1).
/// The publication pin, exposed for the test that asserts it denies deletion (ADR-008 §5.2).
///
/// Behind the same feature as the crash points: a production consumer never gets it.
#[cfg(feature = "crash-points")]
pub fn durability_pin_for_test(path: &std::path::Path) -> std::io::Result<PinForTest> {
    durability::Pin::hold(path).map(|pin| PinForTest { _pin: pin })
}

/// A held pin. Dropping it releases the file (ADR-008 §5.2).
#[cfg(feature = "crash-points")]
#[derive(Debug)]
pub struct PinForTest {
    _pin: durability::Pin,
}

/// Reads one published object over a **caller-owned** connection (offline maintenance, ADR-008 §6.1).
///
/// The same code path [`Store::get`] uses, including the length, identity and role checks — a
/// maintenance caller (the offline collector) must not reimplement "find the row, read the frame,
/// verify it", because the copy that drifts is the one nobody exercises (P2b-6 §24).
///
/// # Errors
///
/// [`StoreError::NotFound`] when nothing is published under `oid`, [`StoreError::MissingPack`] when the
/// pack file is gone, or [`StoreError::Corrupt`] when the bytes do not match the index.
pub fn read_published(
    connection: &rusqlite::Connection,
    layout: &Layout,
    oid: ObjectId,
) -> StoreResult<Vec<u8>> {
    object::get_with(connection, layout, oid)
}

/// Validates a store's schema over a caller-owned connection (offline maintenance, ADR-008 §6.1).
///
/// The maintenance open owns **one** connection for the whole store, so it cannot go through
/// [`Store::open`] (which opens its own). This runs the same read-only checks the normal path runs,
/// inside the caller's exclusive lock.
///
/// # Errors
///
/// [`StoreError::UnsupportedSchemaVersion`] / [`StoreError::SchemaMismatch`] as usual.
pub fn validate_schema_over(connection: &rusqlite::Connection) -> StoreResult<()> {
    schema::validate(connection)
}

#[cfg(feature = "crash-points")]
pub mod testing;

/// Arms a one-shot pin failure for the next publication (`staging-audit` builds only).
#[cfg(all(feature = "staging-audit", feature = "crash-points"))]
pub fn fail_next_pin() {
    testing::fail_next_pin();
}

/// The publication probe used by the ADR-008 §5 acceptance tests.
///
/// Compiled only with the `sync-probe` feature.
#[cfg(any(feature = "sync-probe", feature = "lock-probe"))]
pub mod probe;

pub use batch::{Batch, BatchReport, PutOutcome};
pub use object::{
    PackFailureKind, PackVerificationFailure, VerificationFailure, VerificationReport,
};
pub use pack::{NoPackObservation, PackObserver, PackStep};
pub use repack::{RepackFrame, RepackResult};

use std::path::Path;

use cas_core::{Object, ObjectId, Role};

pub use error::{CorruptCause, StoreError, StoreResult};
pub use layout::Layout;
pub use objects_scan::BadObjectName;
pub use open::{OpenMode, Opened, StoreOptions};

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
    metadata_path: std::path::PathBuf,
    read_only: bool,
    /// How large one pack may grow before a batch starts another (ADR-008 §3).
    pack_max_bytes: u64,
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
        Self::open_with(root, mode, StoreOptions::default())
    }

    /// [`Store::open`] with an explicit connection profile.
    ///
    /// The only knob today is the bounded busy wait (ADR-006 §11.1). `open` is this call
    /// with [`StoreOptions::default`], so ADR-002 §8's "no busy handler" remains the
    /// default behaviour and the existing callers are unchanged.
    ///
    /// # Errors
    ///
    /// The same failures as [`Store::open`].
    pub fn open_with(
        root: impl AsRef<Path>,
        mode: OpenMode,
        options: StoreOptions,
    ) -> StoreResult<Self> {
        let (database, layout, opened) = open::open(root.as_ref(), mode, options)?;
        let metadata_path = layout.database();
        let pack_max_bytes = schema::pack_max_bytes(&database.connection)?;
        Ok(Self {
            database,
            layout,
            metadata_path,
            read_only: mode == OpenMode::ReadOnly,
            pack_max_bytes,
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

    /// The path of this Store's metadata database.
    ///
    /// Exposed so that a layer above this crate can open the same file on its
    /// own connection without knowing the file name itself: which file holds the
    /// metadata is this crate's knowledge, not theirs (ADR-003 §4.2.1).
    #[must_use]
    pub fn metadata_path(&self) -> &std::path::Path {
        // The database path is derived from the layout, so a borrowed return
        // needs it cached; store it at open time instead of recomputing.
        &self.metadata_path
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

    /// The pack size limit this Store was created with (ADR-008 §3).
    #[must_use]
    pub fn pack_max_bytes(&self) -> u64 {
        self.pack_max_bytes
    }

    /// Starts a batch: many objects, one `fsync` per pack and one short transaction
    /// per pack (ADR-008 §5).
    #[must_use]
    pub fn batch(&self) -> Batch<'_> {
        Batch::new(
            &self.database,
            &self.layout,
            self.pack_max_bytes,
            self.read_only,
        )
    }

    /// Publishes `value` under `role`, returning its identity and frame.
    ///
    /// Idempotent: a second `put` of the same role and value writes nothing and
    /// reports [`PutOutcome::AlreadyPresent`]. This is a batch of one object
    /// (ADR-008 §5): the object lands in a one-frame pack.
    ///
    /// # Errors
    ///
    /// [`StoreError::ReadOnly`] on a read-only handle, [`StoreError::InvalidObject`]
    /// or [`StoreError::TooLarge`] when the object cannot be framed,
    /// [`StoreError::Busy`] under contention, [`StoreError::Corrupt`] when a
    /// leftover file for this object does not match what would be written, and
    /// [`StoreError::Io`]/[`StoreError::Sqlite`] for storage failures.
    pub fn put(&self, role: Role, value: &[u8]) -> StoreResult<Published> {
        let mut observer = pack::NoPackObservation;
        self.put_observed(role, value, &mut observer)
    }

    /// [`Store::put`], reporting each pack's publication steps to `observer`.
    ///
    /// This is the one implementation of the publication sequence: [`Store::put`]
    /// is this call with a no-op observer, and the crash-injection tests pass an
    /// observer that aborts (ADR-002 §13.1). The observer cannot change what is
    /// written, or in what order.
    ///
    /// # Errors
    ///
    /// The same failures as [`Store::put`].
    pub fn put_observed(
        &self,
        role: Role,
        value: &[u8],
        observer: &mut dyn PackObserver,
    ) -> StoreResult<Published> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let mut batch = self.batch();
        let oid = batch.put(role, value)?;
        let report = batch.commit_observed(observer)?;
        let outcome = if report.packs == 0 {
            PutOutcome::AlreadyPresent
        } else {
            PutOutcome::Inserted
        };
        let frame = Object::new(role, value)?.to_vec();
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
    /// Also reports entries under `objects/` whose name is not a valid object id
    /// (ADR-002 §3). Correctly named files with no index row are orphans and are
    /// allowed, so they are not failures (ADR-002 §10).
    ///
    /// Read-only: it repairs, deletes and quarantines nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError::Sqlite`] when the index cannot be read, [`StoreError::Io`]
    /// when `objects/` cannot be walked.
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
