//! Copy-repacking: move a pack's surviving frames into a fresh pack (ADR-008 §3, §6.3).
//!
//! The order is the whole design, and it is the same one a publication uses: **write the new pack
//! durably first, then move the index in one transaction, and only then unlink the old files.**
//!
//! - The new pack is staged in `tmp/`, fsynced, renamed into `packs/`, and the directory is fsynced
//!   — after that the file exists and is durable.
//! - The pin is held until the transaction that names it has committed, so the window where the file
//!   exists but no row names it is protected (ADR-008 §5.2). A pin failure is `MissingPack` and the
//!   transaction never runs.
//! - The old pack rows are removed **in the same transaction** that points the survivors at the new
//!   pack, so no reader can observe a frame at two locations or at none.
//! - The old files are unlinked last: a crash before that leaves a pack file with no row, which the
//!   next sweep reclaims as an orphan. The opposite order would leave rows pointing at nothing.

use cas_core::{ObjectId, Role};

use crate::error::{StoreError, StoreResult};
use crate::layout::Layout;
use crate::pack::{self, PackObserver};

/// The steps of a repack, in order (P2b-6 §28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackStep {
    /// The replacement pack is durable in `packs/`; the index still points at the old pack.
    Staged = 3,
    /// The switch transaction has committed; the old pack's file is still there.
    Switched = 4,
}

impl RepackStep {
    /// The number a crash report prints (I6).
    #[must_use]
    pub fn number(self) -> u8 {
        self as u8
    }
}

/// Receives each [`RepackStep`], and may **abort** the repack by returning an error.
///
/// Returning an error rather than panicking is the whole point (P2b-6 §28): an abort must not unwind
/// through a transaction boundary, and a panic would disguise "the test stopped here" as a bug.
pub trait RepackObserver {
    /// Called immediately after `step` has completed.
    ///
    /// # Errors
    ///
    /// Whatever the observer wants to report; the repack stops there.
    fn step(&mut self, step: RepackStep) -> StoreResult<()>;
}

/// The production observer: does nothing and never aborts.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRepackObservation;

impl RepackObserver for NoRepackObservation {
    fn step(&mut self, _step: RepackStep) -> StoreResult<()> {
        Ok(())
    }
}

impl<F: FnMut(RepackStep) -> StoreResult<()>> RepackObserver for F {
    fn step(&mut self, step: RepackStep) -> StoreResult<()> {
        self(step)
    }
}

/// One frame destined for the replacement pack.
#[derive(Debug, Clone)]
pub struct RepackFrame {
    /// The object the frame holds.
    pub oid: ObjectId,
    /// The object's role (carried because the envelope already does; kept for the index row).
    pub role: Role,
    /// The frame's bytes, exactly as they sit in the old pack.
    pub frame: Vec<u8>,
}

/// What a repack produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackResult {
    /// The replacement pack's id (its file is `<id>.pack`).
    pub pack_id: String,
    /// The replacement pack's size in bytes.
    pub bytes: u64,
    /// How many frames it holds.
    pub frames: usize,
}

/// Rewrites `frames` into a new pack and retires `retired` in one transaction.
///
/// `retired` are the pack ids whose rows must go: the caller has already decided that their surviving
/// frames are exactly `frames`, so removing the rows here keeps "two locations for one object" from
/// ever being visible.
///
/// Returns the new pack's identity; **the caller unlinks the old files** after this returns, because a
/// file with no row is the safe direction.
///
/// # Errors
///
/// A staging/rename/fsync failure, [`StoreError::MissingPack`] when the pin cannot be taken, or a
/// SQLite failure (in which case the new pack file is left behind as an orphan, not as a row).
pub fn repack(
    connection: &mut rusqlite::Connection,
    layout: &Layout,
    frames: &mut [RepackFrame],
    retired: &[String],
    observer: &mut dyn PackObserver,
) -> StoreResult<RepackResult> {
    repack_observed(
        connection,
        layout,
        frames,
        retired,
        observer,
        &mut NoRepackObservation,
    )
}

/// [`repack`], with a step observer that may abort (P2b-6 §28).
///
/// # Errors
///
/// As [`repack`], plus whatever the observer returns.
pub fn repack_observed(
    connection: &mut rusqlite::Connection,
    layout: &Layout,
    frames: &mut [RepackFrame],
    retired: &[String],
    observer: &mut dyn PackObserver,
    steps: &mut dyn RepackObserver,
) -> StoreResult<RepackResult> {
    let mut packed: Vec<pack::PackedFrame> = frames
        .iter()
        .map(|frame| pack::PackedFrame {
            oid: frame.oid,
            role: frame.role,
            frame: frame.frame.clone(),
            offset: 0,
        })
        .collect();
    let encoded = pack::encode(&mut packed);
    let pack_id = pack::new_pack_id();
    let size = encoded.bytes.len() as u64;

    // Durable before the index names it; the pin spans the "file exists, no row yet" window.
    let pin = pack::write(layout, &pack_id, &encoded.bytes, observer)?;
    // Point 3: the file is durable, the switch transaction has not opened. An abort here leaves a pack no
    // row names — the direction the next sweep reclaims.
    steps.step(RepackStep::Staged)?;

    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO pack (pack_id, bytes, objects, created_ns) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            &pack_id,
            i64::try_from(size).unwrap_or(i64::MAX),
            i64::try_from(packed.len()).unwrap_or(i64::MAX),
            now_ns(),
        ],
    )?;
    for frame in &packed {
        let updated = transaction.execute(
            "UPDATE object_location SET location = ?1, offset = ?2, kind = ?3 WHERE oid = ?4",
            rusqlite::params![
                &pack_id,
                i64::try_from(frame.offset).unwrap_or(i64::MAX),
                crate::index::kind_pack(),
                frame.oid.to_string(),
            ],
        )?;
        if updated != 1 {
            // A frame we were told to keep has no row: the plan and the index disagree, and
            // guessing which one is right is how data gets lost.
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("object {} has no index row to repoint", frame.oid),
            )));
        }
    }
    for old in retired {
        transaction.execute("DELETE FROM pack WHERE pack_id = ?1", [old])?;
    }
    transaction.commit()?;
    // Point 4: the switch committed, the old pack's file is still there but no row names it. Every object
    // has exactly one location at this instant — the new pack.
    steps.step(RepackStep::Switched)?;
    drop(pin);

    Ok(RepackResult {
        pack_id,
        bytes: size,
        frames: packed.len(),
    })
}

/// Reads one frame out of a pack, for the collector to copy into a replacement.
///
/// # Errors
///
/// [`StoreError::MissingPack`] when the file is gone, or an IO/decode failure.
pub fn read_frame(
    layout: &Layout,
    pack_id: &str,
    offset: u64,
    length: u64,
) -> StoreResult<Vec<u8>> {
    pack::read_frame(layout, pack_id, offset, length)
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
