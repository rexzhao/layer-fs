//! Commit identities, serial scopes and tiny allocation helpers (ADR-006 §10.1, §6).
//!
//! Two different kinds of naming live here: a commit id is a **content** identity
//! (BLAKE3 over the tree and the parent), while a serial scope is an **allocation**
//! identity — a 128-bit value shared by every workspace in one lineage so their serials
//! cannot collide.

use std::sync::atomic::{AtomicU64, Ordering};

use cas_core::ObjectId;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::main_db::{hex, now_ns, MainDb};
use crate::schema;

/// Domain separation for commit ids.
const COMMIT_DOMAIN: &str = "wcas/commit/v1\x00";

/// Domain separation for generated identifiers.
const ENTROPY_DOMAIN: &str = "wcas/entropy/v1\x00";

/// Distinguishes identifiers generated within the same nanosecond.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Computes a commit id (ADR-006 §10.1).
///
/// Git-like and CLI-friendly: the tree, the parent, the timestamp and the message all
/// enter the hash, so two commits of the same tree under the same parent still differ
/// (they were made at different times).
#[must_use]
pub fn commit_id(
    root: ObjectId,
    parent: Option<ObjectId>,
    created_ns: i64,
    message: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(COMMIT_DOMAIN.as_bytes());
    hasher.update(root.as_bytes());
    match parent {
        Some(parent) => hasher.update(parent.as_bytes()),
        None => hasher.update(&[0u8; 32]),
    };
    hasher.update(&created_ns.to_be_bytes());
    let len = u32::try_from(message.len()).unwrap_or(u32::MAX);
    hasher.update(&len.to_be_bytes());
    hasher.update(message.as_bytes());
    hex(hasher.finalize().as_bytes())
}

/// Bytes with no two calls alike, as far as this process can tell.
///
/// This is a hash of everything the process can cheaply observe about *this* call — the
/// clock, the process id, a counter and a stack address — expanded through BLAKE3's
/// extendable output. It is deliberately not a cryptographic random number generator and
/// is not claimed to be one: what the identifiers here need is **uniqueness**, not
/// unpredictability, and there is no adversary in the model (ADR-006 §6). Adding a
/// `getrandom` dependency for a stronger guarantee is a decision for a later phase, not
/// something to smuggle in here.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ENTROPY_DOMAIN.as_bytes());
    hasher.update(&entropy());
    let mut reader = hasher.finalize_xof();
    let mut out = [0u8; N];
    reader.fill(&mut out);
    out
}

/// The observable state this call is happening in.
fn entropy() -> [u8; 32] {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = now_ns();
    let pid = u64::from(std::process::id());
    let probe = 0u8;
    let address = std::ptr::addr_of!(probe) as usize as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(ENTROPY_DOMAIN.as_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&pid.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    hasher.update(&address.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// A fresh scope id (ADR-006 §6: 32 random bytes).
#[must_use]
pub fn new_scope_id() -> [u8; 32] {
    random_bytes::<32>()
}

/// A fresh workspace directory id (ADR-006 §4: 128 random bits).
#[must_use]
pub fn new_workspace_id() -> String {
    hex(&random_bytes::<16>())
}

/// A reserved run of serials for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialSegment {
    /// First serial the workspace may use.
    pub first: u64,
    /// One past the last.
    pub end: u64,
}

/// Reserves the next `count` serials of a scope.
///
/// A short main-database transaction, which is exactly the contention §11.1's bounded
/// wait exists for.
///
/// # Errors
///
/// [`WorkspaceError::NotFound`] when the scope is unknown.
pub fn reserve_serials(db: &MainDb, scope_id: &str, count: u64) -> WorkspaceResult<SerialSegment> {
    let connection = db.lock();
    let transaction = connection.unchecked_transaction()?;
    let next: Option<i64> = transaction
        .query_row(
            "SELECT next_serial FROM scope_allocator WHERE scope_id = ?1",
            [scope_id],
            |row| row.get(0),
        )
        .ok();
    let Some(next) = next else {
        return Err(WorkspaceError::NotFound {
            what: format!("scope {scope_id}"),
        });
    };
    let first = next as u64;
    let end = first + count;
    transaction.execute(
        "UPDATE scope_allocator SET next_serial = ?1 WHERE scope_id = ?2",
        (end as i64, scope_id),
    )?;
    transaction.commit()?;
    Ok(SerialSegment { first, end })
}

/// Reserves one segment for a workspace, the size ADR-006 §6 specifies.
///
/// # Errors
///
/// As [`reserve_serials`].
pub fn reserve_segment(db: &MainDb, scope_id: &str) -> WorkspaceResult<SerialSegment> {
    reserve_serials(db, scope_id, schema::SERIAL_SEGMENT)
}

/// Parses a 64-character hex id back into an object id.
///
/// # Errors
///
/// [`WorkspaceError::Inconsistent`] when the text is not a stored id.
pub fn parse_oid(text: &str) -> WorkspaceResult<ObjectId> {
    text.parse::<ObjectId>()
        .map_err(|error| WorkspaceError::Inconsistent {
            detail: format!("object id {text:?}: {error}"),
        })
}
