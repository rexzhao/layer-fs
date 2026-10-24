//! Batch publication: one `fsync` per pack, one short transaction per pack (ADR-008 §5).
//!
//! ```text
//! let mut batch = store.batch();            // a PageSink for the materialiser
//! batch.put(role, value)?;                  // in-memory dedupe; nothing on disk yet
//! batch.commit()?;                          // per pack: tmp → fsync → rename → fsync dir → BEGIN..COMMIT
//! ```
//!
//! The order is the contract, exactly as it was for one object (ADR-002 §5) but batched:
//!
//! 1. the pack's bytes are durable in `tmp/`;
//! 2. the name exists in `packs/` and that directory entry is durable;
//! 3. **then** one short `BEGIN IMMEDIATE` inserts the `pack` row and its
//!    `object_location` rows;
//! 4. step 1–3 repeat per pack, in order, so a child object's pack row is committed before
//!    the parent that references it.
//!
//! Every `fsync` happens **outside** the transaction (KI-7): the transaction holds only
//! INSERTs, so the write lock's duration does not grow with the number of objects. The
//! probe in `testing` measures exactly that, and `docs/adr/ADR-008-pack-gc.md` §5.1 lists the
//! four crash points this order creates.

use std::collections::HashSet;

use cas_core::{Object, ObjectId, Role};

use crate::error::{StoreError, StoreResult};
use crate::object;
use crate::open::Database;
use crate::pack::{self, PackObserver, PackedFrame};
use crate::schema;
use crate::Layout;

/// What one `put` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was already published; nothing was written.
    AlreadyPresent,
    /// The object was written into a pack and its index row committed.
    Inserted,
}

impl PutOutcome {
    /// Whether this call wrote the object's frame itself.
    #[must_use]
    pub fn wrote_file(self) -> bool {
        matches!(self, PutOutcome::Inserted)
    }
}

/// What one `commit` published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReport {
    /// Objects handed to `put`, after in-memory and index dedupe.
    pub new_objects: usize,
    /// Objects skipped because the index already had them.
    pub already_present: usize,
    /// Packs written by this commit.
    pub packs: usize,
}

/// One object waiting for the batch to commit.
#[derive(Debug)]
struct Pending {
    oid: ObjectId,
    role: Role,
    frame: Vec<u8>,
}

/// A batch of objects published together (ADR-008 §5).
///
/// Constructed by [`Store::batch`](crate::Store::batch); dropping it without `commit`
/// discards everything, which is what a failed materialisation wants.
#[derive(Debug)]
pub struct Batch<'a> {
    database: &'a Database,
    layout: &'a Layout,
    limit: u64,
    read_only: bool,
    pending: Vec<Pending>,
    seen: HashSet<ObjectId>,
}

impl<'a> Batch<'a> {
    pub(crate) fn new(
        database: &'a Database,
        layout: &'a Layout,
        limit: u64,
        read_only: bool,
    ) -> Self {
        Self {
            database,
            layout,
            limit,
            read_only,
            pending: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Adds one object to the batch.
    ///
    /// Nothing is written: the frame is kept in memory until `commit`, so a materialiser
    /// can put a tree's pages and then discover it has nothing to publish.
    ///
    /// # Errors
    ///
    /// [`StoreError::ReadOnly`] on a read-only handle, and the object-format failures
    /// [`Store::put`](crate::Store::put) reports.
    pub fn put(&mut self, role: Role, value: &[u8]) -> StoreResult<ObjectId> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        let object = Object::new(role, value)?;
        let oid = object.id()?;
        // In-batch dedupe: the same page written twice in one materialisation is common
        // (a file's second write re-puts its page), and it must cost one frame, not two.
        if self.seen.insert(oid) {
            self.pending.push(Pending {
                oid,
                role,
                frame: object.to_vec(),
            });
        }
        Ok(oid)
    }

    /// Objects handed to `put` so far, before the index dedupe `commit` does.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing has been put.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Publishes the batch and reports what it did.
    ///
    /// # Errors
    ///
    /// [`StoreError::Busy`] under a competing writer, [`StoreError::Io`] /
    /// [`StoreError::Sqlite`] for storage failures. A failure leaves either an
    /// unpublished pack file (harmless, GC's business) or a fully published batch: the
    /// index commit is the publication point, per pack.
    pub fn commit(self) -> StoreResult<BatchReport> {
        self.commit_observed(&mut pack::NoPackObservation)
    }

    /// [`Batch::commit`], reporting each pack's publication steps.
    ///
    /// The crash-injection tests pass an observer that aborts; production passes
    /// [`NoPackObservation`](crate::NoPackObservation). Like `put_observed`, the observer
    /// cannot change what is written or in what order.
    ///
    /// # Errors
    ///
    /// The same failures as [`Batch::commit`].
    pub fn commit_observed(mut self, observer: &mut dyn PackObserver) -> StoreResult<BatchReport> {
        let mut already_present = 0;
        let mut fresh = Vec::new();
        for pending in self.pending.drain(..) {
            if object::published(self.database, pending.oid)? {
                already_present += 1;
            } else {
                fresh.push(pending);
            }
        }
        if fresh.is_empty() {
            return Ok(BatchReport {
                new_objects: 0,
                already_present,
                packs: 0,
            });
        }

        let mut packs = 0;
        let mut new_objects = 0;
        for group in split(fresh, self.limit) {
            let pack_id = pack::new_pack_id();
            let mut frames: Vec<PackedFrame> = group
                .iter()
                .map(|pending| PackedFrame {
                    oid: pending.oid,
                    role: pending.role,
                    frame: pending.frame.clone(),
                    offset: 0,
                })
                .collect();
            let encoded = pack::encode(&mut frames);
            pack::write(self.layout, &pack_id, &encoded.bytes, observer)?;
            // Diagnostic (`staging-audit`): hold the pack open **without** `FILE_SHARE_DELETE`
            // from here until the index commit returns. On Windows a delete of a file with such
            // a handle must fail; if the symptom disappears, something external was deleting it.
            #[cfg(all(feature = "staging-audit", windows))]
            let _pin = {
                use std::os::windows::fs::OpenOptionsExt as _;
                const FILE_SHARE_READ: u32 = 0x0000_0001;
                const FILE_SHARE_WRITE: u32 = 0x0000_0002;
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .open(self.layout.pack_path(&pack_id))
                    .map_err(|error| {
                        eprintln!("staging-audit: cannot pin {pack_id}: {error}");
                        StoreError::Io(error)
                    })?
            };
            let size = encoded.bytes.len() as u64;
            record_pack(
                self.database,
                self.layout,
                &pack_id,
                &frames,
                size,
                observer,
            )?;
            packs += 1;
            new_objects += frames.len();
        }

        Ok(BatchReport {
            new_objects,
            already_present,
            packs,
        })
    }
}

/// Splits the pending objects into packs no larger than `limit` bytes.
///
/// A frame larger than the limit gets a pack of its own rather than being refused: the
/// object format's own limit (1 MiB) is far below the default 64 MiB, so this only happens
/// with a deliberately tiny test limit, and a lone oversized frame still satisfies "at most
/// one pack per group".
fn split(pending: Vec<Pending>, limit: u64) -> Vec<Vec<Pending>> {
    let mut groups: Vec<Vec<Pending>> = Vec::new();
    let mut current: Vec<Pending> = Vec::new();
    let mut bytes = 0u64;
    for item in pending {
        let size = item.frame.len() as u64;
        if !current.is_empty() && bytes + size > limit {
            groups.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += size;
        current.push(item);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// Inserts one pack's index rows in a single short transaction (ADR-008 §5 step 3).
///
/// `INSERT OR IGNORE`, not `INSERT`: two workspaces publishing the same object race, the
/// first committer wins, and the loser's frame becomes dead bytes inside its pack for GC's
/// repack to reclaim (ADR-008 §5, §6.3). Both callers still succeed.
fn record_pack(
    database: &Database,
    _layout: &Layout,
    pack_id: &str,
    frames: &[PackedFrame],
    bytes: u64,
    observer: &mut dyn PackObserver,
) -> StoreResult<()> {
    // `BEGIN IMMEDIATE`, explicit rather than a `Transaction` default: ADR-008 §5 step 3 names
    // the behaviour, and the difference is real if anything inside the transaction ever reads
    // before it writes — a deferred transaction would take a read snapshot first and then fail
    // with `BUSY_SNAPSHOT` instead of the typed `Busy` this crate promises (ADR-006 §11.1).
    //
    // `new_unchecked` keeps that behaviour *and* the rollback guarantee: the value is created
    // with `DropBehavior::Rollback`, so an early return, a failed `COMMIT`, or a panic in the
    // probe leaves no open transaction behind — a connection stuck inside one would turn the
    // next `BEGIN IMMEDIATE` into "cannot start a transaction within a transaction", or worse,
    // quietly enrol later writes in it.
    let transaction = rusqlite::Transaction::new_unchecked(
        &database.connection,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    #[cfg(any(feature = "sync-probe", feature = "lock-probe"))]
    crate::probe::note_transaction_begin();
    #[cfg(feature = "sync-probe")]
    crate::probe::run_injection();

    // Diagnostic checkpoints (`staging-audit`, default off): one immediately after `BEGIN`, so
    // the window is at most one system call wide (rename → sync_dir → BEGIN → INSERT).
    #[cfg(feature = "staging-audit")]
    {
        pack::audit("after-begin", _layout, pack_id, bytes);
    }
    write_rows(&transaction, pack_id, frames, bytes)?;
    transaction.commit()?;
    #[cfg(any(feature = "sync-probe", feature = "lock-probe"))]
    crate::probe::note_transaction_end();

    observer.step(pack::PackStep::Committed);
    Ok(())
}

/// The transaction body: one `pack` row and one `object_location` row per frame.
///
/// Separated from [`record_pack`] so the `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` handling reads
/// as one thing and the statements as another.
fn write_rows(
    connection: &rusqlite::Connection,
    pack_id: &str,
    frames: &[PackedFrame],
    bytes: u64,
) -> StoreResult<()> {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as i64)
        .unwrap_or(0);
    connection.execute(
        "INSERT OR IGNORE INTO pack (pack_id, bytes, objects, created_ns) VALUES (?1, ?2, ?3, ?4)",
        (
            pack_id,
            i64::try_from(bytes).unwrap_or(i64::MAX),
            frames.len() as i64,
            created,
        ),
    )?;
    let mut statement = connection.prepare(
        "INSERT OR IGNORE INTO object_location
         (oid, role, length, kind, location, offset) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for frame in frames {
        statement.execute((
            frame.oid.to_string(),
            frame.role.tag(),
            frame.frame.len() as i64,
            schema::KIND_PACK,
            pack_id,
            i64::try_from(frame.offset).unwrap_or(i64::MAX),
        ))?;
    }
    drop(statement);
    Ok(())
}
