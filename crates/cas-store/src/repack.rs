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
