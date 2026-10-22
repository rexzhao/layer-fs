//! Putting, getting and verifying objects (ADR-002 §5, §6).

use std::fs::File;
use std::io::Write;
use std::path::Path;

use cas_core::{verify_id, Object, ObjectId, Role};

use crate::durability::{rename_replace, sync_directory, sync_file};
use crate::error::{CorruptCause, StoreError, StoreResult};
use crate::layout::{temporary_name, Layout};
use crate::open::Database;

/// What a `put` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was already published; nothing was written.
    AlreadyPresent,
    /// The object was written and its index row committed.
    Inserted,
    /// The object file already existed (a crash between `rename` and the index
    /// commit) but was not published; its index row was committed now.
    AdoptedOrphan,
}

impl PutOutcome {
    /// Whether this call wrote the object file itself.
    #[must_use]
    pub fn wrote_file(self) -> bool {
        matches!(self, PutOutcome::Inserted)
    }
}

/// Publishes `role`/`value` under its identity (ADR-002 §5).
///
/// Idempotent: putting an already-published object writes nothing and reports
/// [`PutOutcome::AlreadyPresent`]. The order of operations is the contract —
/// object bytes are durable before any index row can point at them — so a
/// crash at any step leaves either "not published" or "fully published".
pub(crate) fn put(
    database: &Database,
    layout: &Layout,
    role: Role,
    value: &[u8],
) -> StoreResult<PutOutcome> {
    let object = Object::new(role, value)?;
    let oid = object.id()?;
    let frame = object.to_vec();

    if published(database, oid)? {
        return Ok(PutOutcome::AlreadyPresent);
    }

    let relative = layout.object_relative(oid);
    let directory = layout.ensure_object_directory(oid)?;
    let target = directory.join(oid.to_string());

    let outcome = if target.is_file() {
        // An earlier attempt reached `rename` but not the index commit.
        // Re-verify before trusting it: a corrupt leftover must be reported,
        // never overwritten, or the disk problem it signals would be hidden
        // (ADR-002 §5.2).
        verify_leftover(oid, &target, &frame)?;
        PutOutcome::AdoptedOrphan
    } else {
        match stage_and_publish(layout, &frame, &target) {
            Ok(()) => PutOutcome::Inserted,
            // A concurrent writer of the same object raced our rename. On
            // Windows the loser sees either NotFound (its staging file was
            // consumed) or PermissionDenied (the target is briefly held by the
            // winner's rename). Either way the bytes at the target are
            // necessarily identical, because the identity covers them. Verify
            // that, and report a genuine mismatch as corruption.
            Err(StoreError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                verify_leftover(oid, &target, &frame)?;
                PutOutcome::AdoptedOrphan
            }
            Err(other) => return Err(other),
        }
    };

    record(database, oid, role, relative, frame.len())?;
    Ok(outcome)
}

/// Writes the frame to `tmp/`, makes it durable, then renames it into place.
fn stage_and_publish(layout: &Layout, frame: &[u8], target: &Path) -> StoreResult<()> {
    let staged = layout.temporaries().join(temporary_name());
    {
        let mut file = File::create(&staged)?;
        file.write_all(frame)?;
        // The bytes must be on stable storage before any name points at them.
        sync_file(&file)?;
    }
    rename_replace(&staged, target)?;
    // Best effort on platforms without a documented directory flush; see the
    // durability module for exactly what this does and does not promise.
    sync_directory(target.parent().unwrap_or(layout.objects().as_path()))?;
    Ok(())
}

/// Commits the index row. This call is the publication point.
fn record(
    database: &Database,
    oid: ObjectId,
    role: Role,
    relative: String,
    length: usize,
) -> StoreResult<()> {
    let inserted = database.connection.execute(
        "INSERT INTO object_location (oid, role, length, location) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(oid) DO NOTHING",
        (oid.to_string(), role.tag(), length as i64, relative),
    )?;
    // `inserted == 0` means a concurrent writer published the same object
    // first; the outcome is identical, so it is still success (ADR-002 §8).
    let _ = inserted;
    Ok(())
}

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
    pub(crate) relative: String,
}

pub(crate) fn locate(database: &Database, oid: ObjectId) -> StoreResult<Option<Located>> {
    use rusqlite::OptionalExtension;
    let row: Option<(i64, i64, String)> = database
        .connection
        .query_row(
            "SELECT role, length, location FROM object_location WHERE oid = ?1",
            [oid.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((role_tag, length, relative)) = row else {
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
    Ok(Some(Located {
        oid,
        role,
        length,
        relative,
    }))
}

/// Reads, decodes and verifies one published object (ADR-002 §6).
pub(crate) fn get(database: &Database, layout: &Layout, oid: ObjectId) -> StoreResult<Vec<u8>> {
    let located = locate(database, oid)?.ok_or(StoreError::NotFound { oid })?;
    let path = layout.root().join(&located.relative);
    let bytes = std::fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::Corrupt {
            oid,
            cause: CorruptCause::LengthMismatch {
                expected: located.length,
                actual: 0,
            },
        },
        _ => StoreError::Io(error),
    })?;
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

/// Checks a leftover object file against what we are about to publish.
///
/// A crash between `rename` and the index commit leaves a complete file behind.
/// Reusing it is safe only if it is byte-identical to the frame we would write,
/// which also means it hashes to `oid`. Any difference is a corrupt leftover and
/// is reported rather than overwritten, so a disk fault is never masked.
///
/// The read is retried a bounded number of times when a concurrent writer is
/// replacing the same path (Windows reports `NotFound`/`PermissionDenied` for a
/// moment during another process's rename). This is a read of an immutable file,
/// not a retry of a write: no operation with an effect is replayed.
fn verify_leftover(oid: ObjectId, path: &Path, expected_frame: &[u8]) -> StoreResult<()> {
    const ATTEMPTS: u32 = 50;
    let mut bytes = None;
    for attempt in 0..ATTEMPTS {
        match std::fs::read(path) {
            Ok(read) => {
                bytes = Some(read);
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) && attempt + 1 < ATTEMPTS =>
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    let Some(bytes) = bytes else {
        return Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("object {} vanished while being read", path.display()),
        )));
    };
    if bytes != expected_frame {
        let cause = if bytes.len() != expected_frame.len() {
            CorruptCause::LengthMismatch {
                expected: expected_frame.len() as u64,
                actual: bytes.len() as u64,
            }
        } else {
            match cas_core::verify_id(oid, &bytes) {
                Err(cas_core::EnvelopeError::IdentityMismatch { expected, actual }) => {
                    CorruptCause::IdentityMismatch { expected, actual }
                }
                _ => CorruptCause::Envelope(cas_core::EnvelopeError::LengthMismatch {
                    declared: expected_frame.len() as u64,
                    actual: bytes.len() as u64,
                }),
            }
        };
        return Err(StoreError::Corrupt { oid, cause });
    }
    Ok(())
}

/// One failed object reported by `verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationFailure {
    /// The object that failed.
    pub oid: ObjectId,
    /// Why it failed.
    pub reason: String,
}

/// The result of checking every published object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// How many published objects were checked.
    pub checked: usize,
    /// The objects that failed, in id order.
    pub failures: Vec<VerificationFailure>,
}

impl VerificationReport {
    /// Whether every published object was readable and intact.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Read-only check of every published object (ADR-002 §6, §10).
///
/// Repairs and deletes nothing: it reports. Orphans (files with no index row)
/// are out of scope because they are not published.
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
    Ok(VerificationReport { checked, failures })
}
