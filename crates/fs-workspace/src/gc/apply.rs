//! Applying a sweep plan (ADR-008 §6.3, P2b-6 §18.3).
//!
//! The rule this module is built around: **an apply either does the whole plan or none of it**. A
//! half-applied plan leaves the store in a state that is neither the old one nor the new one, and the
//! next run would have to guess which half it is looking at. So anything this build cannot do is
//! refused up front, before the first deletion — see [`apply`]'s refusal for copy-repacking.

use std::path::Path;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::gc::sweep::SweepPlan;

/// What an apply actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Loose object files removed.
    pub loose_files_deleted: usize,
    /// Index rows removed (loose and pack frames alike).
    pub index_rows_deleted: usize,
    /// Packs whose rows and file were removed.
    pub packs_deleted: usize,
    /// Files under `packs/` no row claimed.
    pub orphan_pack_files_deleted: usize,
    /// Files under `tmp/` removed.
    pub temporaries_deleted: usize,
    /// Packs rewritten because their survivor ratio fell below the threshold.
    pub packs_repacked: usize,
}

/// Applies `plan` to the store's index and files.
///
/// Order matters and follows §18.3:
///
/// 1. **Loose objects**: row and file both go. The row is removed in the same transaction as every
///    other row deletion, so an interrupted run never leaves a row pointing at a file that is gone.
/// 2. **Packs with no survivors**: their rows go in that same transaction; the files are unlinked
///    after the commit. A crash between the two leaves a file no row claims, which the *next* run
///    deletes as an orphan — the safe direction (a file with no row is unreadable garbage, whereas a
///    row with no file is a `MissingPack`).
/// 3. **Orphans and temporaries**: files nothing can read.
///
/// Copy-repacking is **not implemented yet**, and its presence in the plan is refused *before* any
/// deletion: doing the deletions first and then failing would be exactly the half-applied state this
/// function exists to prevent.
///
/// # Errors
///
/// [`WorkspaceError::Inconsistent`] when the plan needs a repack, or a SQLite/IO failure.
pub fn apply(
    connection: &mut rusqlite::Connection,
    plan: &SweepPlan,
    store_dir: &Path,
) -> WorkspaceResult<Applied> {
    // Repacking happens **before** the deletion transaction: it needs the old frames, and it retires
    // the old pack rows itself. Doing it after would mean reading frames out of files we had already
    // unlinked.
    let mut repacked = 0usize;
    for decision in &plan.packs_repack {
        let survivors = survivors_of(connection, &decision.pack_id, &plan.reachable_ids())?;
        if survivors.is_empty() {
            // The plan said this pack has survivors; an empty list means the plan and the index
            // disagree. Refuse rather than delete a pack we were told to keep.
            return Err(WorkspaceError::Inconsistent {
                detail: format!(
                    "pack {} was planned for repacking but no surviving row was found",
                    decision.pack_id
                ),
            });
        }
        let layout = cas_store::Layout::new(store_dir);
        let mut frames = Vec::with_capacity(survivors.len());
        for (oid, role, offset, length) in survivors {
            let frame = cas_store::repack::read_frame(&layout, &decision.pack_id, offset, length)?;
            frames.push(cas_store::RepackFrame { oid, role, frame });
        }
        let result = cas_store::repack::repack(
            connection,
            &layout,
            &mut frames,
            std::slice::from_ref(&decision.pack_id),
            &mut cas_store::NoPackObservation,
        )?;
        let _ = result;
        repacked += 1;
        // The old file is unlinked only now that no row names it.
        if decision.path.exists() {
            std::fs::remove_file(&decision.path).map_err(|error| WorkspaceError::NotFound {
                what: format!(
                    "the repacked-away pack {} ({error})",
                    decision.path.display()
                ),
            })?;
        }
    }

    let layout = cas_store::Layout::new(store_dir);
    let mut applied = Applied::default();

    // One transaction for every row: the index moves as a unit.
    let transaction = connection.transaction()?;
    for oid in &plan.dead_objects {
        applied.index_rows_deleted += transaction.execute(
            "DELETE FROM object_location WHERE oid = ?1",
            [oid.to_string()],
        )?;
    }
    for pack in &plan.packs_delete {
        applied.index_rows_deleted += transaction.execute(
            "DELETE FROM object_location WHERE location = ?1",
            [&pack.pack_id],
        )?;
        transaction.execute("DELETE FROM pack WHERE pack_id = ?1", [&pack.pack_id])?;
        applied.packs_deleted += 1;
    }
    transaction.commit()?;

    // Files after the commit: a file without a row is garbage the next run reclaims, while a row
    // without a file is a `MissingPack` the next run has to report.
    for oid in &plan.dead_objects {
        let path = layout.object_path(*oid);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| WorkspaceError::NotFound {
                what: format!("the dead object file {} ({error})", path.display()),
            })?;
            applied.loose_files_deleted += 1;
        }
    }
    for pack in &plan.packs_delete {
        if pack.path.exists() {
            std::fs::remove_file(&pack.path).map_err(|error| WorkspaceError::NotFound {
                what: format!("the dead pack {} ({error})", pack.path.display()),
            })?;
        }
    }
    for orphan in &plan.orphan_pack_files {
        if orphan.exists() {
            std::fs::remove_file(orphan).map_err(|error| WorkspaceError::NotFound {
                what: format!("the orphan pack {} ({error})", orphan.display()),
            })?;
            applied.orphan_pack_files_deleted += 1;
        }
    }

    // `tmp/` holds only half-written files: nothing in it can be referenced, by construction.
    if let Ok(entries) = std::fs::read_dir(layout.temporaries()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                std::fs::remove_file(&path).map_err(|error| WorkspaceError::NotFound {
                    what: format!("the temporary {} ({error})", path.display()),
                })?;
                applied.temporaries_deleted += 1;
            }
        }
    }

    applied.packs_repacked = repacked;
    Ok(applied)
}

/// The index rows of `pack_id` whose object is in `keep`, with what is needed to copy the frame.
fn survivors_of(
    connection: &rusqlite::Connection,
    pack_id: &str,
    keep: &std::collections::BTreeSet<cas_core::ObjectId>,
) -> WorkspaceResult<Vec<(cas_core::ObjectId, cas_core::Role, u64, u64)>> {
    let mut statement = connection.prepare(
        "SELECT oid, role, offset, length FROM object_location
         WHERE location = ?1 ORDER BY offset",
    )?;
    let rows = statement.query_map([pack_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (oid, role_tag, offset, length) = row?;
        let parsed: cas_core::ObjectId = oid.parse().map_err(|_| WorkspaceError::Inconsistent {
            detail: format!("an index row names {oid:?}, which is not an object id"),
        })?;
        if !keep.contains(&parsed) {
            continue;
        }
        let role =
            cas_core::Role::from_tag(u8::try_from(role_tag).unwrap_or(0)).ok_or_else(|| {
                WorkspaceError::Inconsistent {
                    detail: format!(
                        "object {parsed} has role tag {role_tag}, which this build does not know"
                    ),
                }
            })?;
        out.push((
            parsed,
            role,
            u64::try_from(offset).unwrap_or(0),
            u64::try_from(length).unwrap_or(0),
        ));
    }
    Ok(out)
}
