//! Reading and verifying published objects (ADR-002 §6, ADR-008 §3, §7).
//!
//! Objects are published by [`Batch`](crate::Batch): one pack per group of frames, one
//! `fsync` and one short index transaction per pack. This module is the read side, plus the
//! index lookups the write side needs. It never writes.
//!
//! Two storage shapes are readable:
//!
//! - `kind = pack` — the only shape this build writes (ADR-008 §3);
//! - `kind = loose` — one file per object, the P1/P2a shape. Kept readable because a schema
//!   v3 Store can still hold rows written before packs existed, and because the P2a fixtures
//!   are the migration story's evidence (ADR-008 §4). Nothing writes it any more.

use cas_core::{verify_id, ObjectId, Role};

use crate::error::{CorruptCause, StoreError, StoreResult};
use crate::layout::Layout;
use crate::objects_scan;
use crate::open::Database;
use crate::pack;
use crate::schema;

/// Whether the index has a row for `oid`.
pub(crate) fn published(database: &Database, oid: ObjectId) -> StoreResult<bool> {
    let count: i64 = database.connection.query_row(
        "SELECT COUNT(*) FROM object_location WHERE oid = ?1",
        [oid.to_string()],
        |row| row.get(0),
    )?;
    Ok(count == 1)
}

/// One published object as the index describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    pub(crate) oid: ObjectId,
    pub(crate) role: Role,
    pub(crate) length: u64,
    /// [`schema::KIND_LOOSE`] or [`schema::KIND_PACK`].
    pub(crate) kind: i64,
    /// Loose: the path relative to the Store root. Pack: the pack id.
    pub(crate) location: String,
    /// Pack: the frame's offset inside the pack. Loose: 0.
    pub(crate) offset: u64,
}

pub(crate) fn locate(database: &Database, oid: ObjectId) -> StoreResult<Option<Located>> {
    use rusqlite::OptionalExtension;
    let row: Option<(i64, i64, i64, String, i64)> = database
        .connection
        .query_row(
            "SELECT role, length, kind, location, offset FROM object_location WHERE oid = ?1",
            [oid.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((role_tag, length, kind, location, offset)) = row else {
        return Ok(None);
    };
    let role = u8::try_from(role_tag)
        .ok()
        .and_then(Role::from_tag)
        .ok_or_else(|| StoreError::InvalidRecord {
            field: "object_location.role",
            detail: format!("unknown role tag {role_tag}"),
        })?;
    let length = u64::try_from(length).map_err(|_| StoreError::InvalidRecord {
        field: "object_location.length",
        detail: format!("negative length {length}"),
    })?;
    let offset = u64::try_from(offset).map_err(|_| StoreError::InvalidRecord {
        field: "object_location.offset",
        detail: format!("negative offset {offset}"),
    })?;
    if kind != schema::KIND_LOOSE && kind != schema::KIND_PACK {
        return Err(StoreError::InvalidRecord {
            field: "object_location.kind",
            detail: format!("unknown storage kind {kind}"),
        });
    }
    Ok(Some(Located {
        oid,
        role,
        length,
        kind,
        location,
        offset,
    }))
}

/// Reads, decodes and verifies one published object (ADR-002 §6, ADR-008 §3).
///
/// A pack object costs one seek and one frame read: the index knows `(offset, length)`, so
/// neither the pack's tail nor its other frames are touched (ADR-008 §3).
pub(crate) fn get(database: &Database, layout: &Layout, oid: ObjectId) -> StoreResult<Vec<u8>> {
    let located = locate(database, oid)?.ok_or(StoreError::NotFound { oid })?;
    let bytes = if located.kind == schema::KIND_PACK {
        pack::read_frame(layout, &located.location, located.offset, located.length)?
    } else {
        read_loose(layout, &located)?
    };
    if bytes.len() as u64 != located.length {
        return Err(StoreError::Corrupt {
            oid,
            cause: CorruptCause::LengthMismatch {
                expected: located.length,
                actual: bytes.len() as u64,
            },
        });
    }
    let object = verify_id(oid, &bytes).map_err(|error| match error {
        cas_core::EnvelopeError::IdentityMismatch { expected, actual } => StoreError::Corrupt {
            oid,
            cause: CorruptCause::IdentityMismatch { expected, actual },
        },
        other => StoreError::Corrupt {
            oid,
            cause: CorruptCause::Envelope(other),
        },
    })?;
    if object.role() != located.role {
        return Err(StoreError::Corrupt {
            oid,
            cause: CorruptCause::RoleMismatch {
                recorded: located.role.tag() as i64,
                actual: object.role().tag(),
            },
        });
    }
    Ok(bytes)
}

/// Reads an object stored as its own file (P1/P2a shape).
fn read_loose(layout: &Layout, located: &Located) -> StoreResult<Vec<u8>> {
    let path = layout.root().join(&located.location);
    std::fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::Corrupt {
            oid: located.oid,
            cause: CorruptCause::LengthMismatch {
                expected: located.length,
                actual: 0,
            },
        },
        _ => StoreError::Io(error),
    })
}

/// One failed object reported by `verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationFailure {
    /// The object that failed.
    pub oid: ObjectId,
    /// Why it failed.
    pub reason: String,
}

/// One failed pack reported by `verify` (ADR-008 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackVerificationFailure {
    /// The pack that failed.
    pub pack_id: String,
    /// Why it failed.
    pub reason: String,
}

/// The result of checking every published object and every pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// How many published objects were checked.
    pub checked: usize,
    /// How many packs were checked.
    pub packs_checked: usize,
    /// The objects that failed, in id order.
    pub failures: Vec<VerificationFailure>,
    /// The packs whose tail or geometry failed, in id order.
    pub pack_failures: Vec<PackVerificationFailure>,
    /// How many of `pack_failures` are **missing files** rather than damaged ones.
    ///
    /// Separated because the two mean different things to an operator: damage needs a restore,
    /// a missing pack needs to be found (it was deleted or lost after publication).
    pub missing_packs: usize,
    /// Entries under `objects/` whose name is not a valid object id.
    ///
    /// ADR-002 §3: that directory holds object files only, so a stray name is
    /// damaged state. Correctly named files with no index row are orphans and
    /// are allowed (ADR-002 §10), so they are not listed here.
    pub bad_names: Vec<objects_scan::BadObjectName>,
}

impl VerificationReport {
    /// Whether every published object was readable and intact, every pack's tail matched,
    /// and every entry under `objects/` had a valid object name.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.pack_failures.is_empty() && self.bad_names.is_empty()
    }
}

/// Read-only check of every published object and every pack (ADR-002 §6, ADR-008 §7).
///
/// Repairs and deletes nothing: it reports. Per pack it checks the tail's magic, its
/// recorded length, its BLAKE3, and that every index row pointing into it describes a frame
/// that starts and ends inside the body and exactly fills one frame. Orphans (files or packs
/// with no index row) are out of scope because they are not published.
pub(crate) fn verify_all(database: &Database, layout: &Layout) -> StoreResult<VerificationReport> {
    let mut statement = database
        .connection
        .prepare("SELECT oid FROM object_location ORDER BY oid")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut checked = 0;
    let mut failures = Vec::new();
    for text in ids {
        let oid: ObjectId = text.parse()?;
        checked += 1;
        if let Err(error) = get(database, layout, oid) {
            failures.push(VerificationFailure {
                oid,
                reason: error.to_string(),
            });
        }
    }

    let mut statement = database
        .connection
        .prepare("SELECT pack_id FROM pack ORDER BY pack_id")?;
    let pack_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut pack_failures = Vec::new();
    let mut missing_packs = 0;
    let packs_checked = pack_ids.len();
    for pack_id in pack_ids {
        if let Err(error) = verify_pack(database, layout, &pack_id) {
            if matches!(error, StoreError::MissingPack { .. }) {
                missing_packs += 1;
            }
            pack_failures.push(PackVerificationFailure {
                pack_id,
                reason: error.to_string(),
            });
        }
    }

    let bad_names = objects_scan::scan_names(&layout.objects())?;
    Ok(VerificationReport {
        checked,
        packs_checked,
        failures,
        pack_failures,
        missing_packs,
        bad_names,
    })
}

/// Checks one pack: its tail, and that every index row points at a frame inside the body.
///
/// The rows do **not** have to tile the body. Two publishers racing on the same object both
/// write a frame; the loser's `INSERT OR IGNORE` is ignored, so its frame stays in its pack as
/// dead bytes for GC's repack to reclaim (ADR-008 §5, §6.3). What must hold is the weaker,
/// checkable claim of ADR-008 §7: each row's `(offset, length)` lies inside the body (before
/// the tail) and no two rows overlap, so every row addresses exactly one frame.
fn verify_pack(database: &Database, layout: &Layout, pack_id: &str) -> StoreResult<()> {
    let tail = pack::verify(layout, pack_id)?;
    let mut statement = database.connection.prepare(
        "SELECT oid, length, offset FROM object_location
         WHERE kind = ?1 AND location = ?2 ORDER BY offset, length",
    )?;
    let rows = statement
        .query_map((schema::KIND_PACK, pack_id), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // The tail counts *physical* frames, so it may exceed the rows (dead frames from a
    // concurrent publisher), but never fall below them: that would mean the index names a
    // frame the body does not have.
    if (tail.count as usize) < rows.len() {
        return Err(StoreError::CorruptPack {
            pack_id: pack_id.to_owned(),
            cause: CorruptCause::PackTail(format!(
                "tail declares {} frames, the index has {} rows",
                tail.count,
                rows.len()
            )),
        });
    }

    let mut previous_end = 0i64;
    for (oid, length, offset) in rows {
        if offset < 0 || length <= 0 {
            return Err(StoreError::CorruptPack {
                pack_id: pack_id.to_owned(),
                cause: CorruptCause::PackTail(format!(
                    "object {oid} has a non-positive frame {offset}+{length}"
                )),
            });
        }
        let end = offset.saturating_add(length);
        if end as u64 > tail.frames_len {
            return Err(StoreError::CorruptPack {
                pack_id: pack_id.to_owned(),
                cause: CorruptCause::PackTail(format!(
                    "object {oid} ends at {end}, past the pack's {} frame bytes",
                    tail.frames_len
                )),
            });
        }
        if offset < previous_end {
            return Err(StoreError::CorruptPack {
                pack_id: pack_id.to_owned(),
                cause: CorruptCause::PackTail(format!(
                    "object {oid} starts at {offset}, inside the frame that ends at {previous_end}"
                )),
            });
        }
        previous_end = end;
    }
    Ok(())
}
