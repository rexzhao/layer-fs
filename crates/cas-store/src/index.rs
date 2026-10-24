//! Read-only views of the store's index tables, for offline maintenance (ADR-008 §6.3).
//!
//! The collector works on this index, so the *shape* of the rows belongs to cas-store: a caller that
//! wrote its own `SELECT ... FROM object_location` would be a second copy of the schema, and the copy
//! that drifts is always the one nobody compiles against. The columns come back as the text and
//! integers they are stored as; turning them into domain types is the caller's job, because the
//! caller is the one that has to report a typed error naming the bad row.

use crate::error::StoreResult;

/// One `object_location` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    /// The object's id, as stored (lowercase hex).
    pub oid: String,
    /// The object's length in bytes.
    pub length: u64,
    /// `KIND_LOOSE` (0) or `KIND_PACK` (1).
    pub kind: i64,
    /// The file the bytes live in, relative to the store root.
    pub location: String,
    /// Byte offset of the frame (0 for a loose object).
    pub offset: u64,
}

/// One `pack` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRow {
    /// The pack's file name (without directory), primary key of the table.
    pub pack_id: String,
    /// Total file size when the pack was published.
    pub bytes: u64,
    /// How many objects the pack declared.
    pub objects: u64,
}

/// Every index row, ordered by location then offset (a pack's frames come out in file order).
///
/// # Errors
///
/// A SQLite failure.
pub fn index_rows(connection: &rusqlite::Connection) -> StoreResult<Vec<IndexRow>> {
    let mut statement = connection.prepare(
        "SELECT oid, length, kind, location, offset FROM object_location
         ORDER BY location, offset, oid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(IndexRow {
            oid: row.get(0)?,
            length: to_u64(row.get::<_, i64>(1)?),
            kind: row.get(2)?,
            location: row.get(3)?,
            offset: to_u64(row.get::<_, i64>(4)?),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every `pack` row, ordered by id.
///
/// # Errors
///
/// A SQLite failure.
pub fn pack_rows(connection: &rusqlite::Connection) -> StoreResult<Vec<PackRow>> {
    let mut statement =
        connection.prepare("SELECT pack_id, bytes, objects FROM pack ORDER BY pack_id")?;
    let rows = statement.query_map([], |row| {
        Ok(PackRow {
            pack_id: row.get(0)?,
            bytes: to_u64(row.get::<_, i64>(1)?),
            objects: to_u64(row.get::<_, i64>(2)?),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// SQLite integers are signed; these columns are counts and lengths, so a negative value means the
/// row was written by something that does not respect the schema. Saturating is deliberate: a
/// maintenance read must not turn a weird row into a licence to ignore it.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The kinds an index row can have, as the schema's constants.
#[must_use]
pub fn kind_loose() -> i64 {
    crate::schema::KIND_LOOSE
}

/// The `kind` value for an object stored inside a pack.
#[must_use]
pub fn kind_pack() -> i64 {
    crate::schema::KIND_PACK
}
