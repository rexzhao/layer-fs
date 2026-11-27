//! Lightweight SQLite metadata database (`meta.sqlite`) and `store.json` config.
//!
//! Stores ONLY directory trees, inode metadata, inline compact chunk manifests
//! (`[hash32 | raw_len_u32] * N`), chunk index (`hash -> pack_id, offset, refcount`),
//! and pack accounting (`live_bytes`, `dead_bytes`). File payloads NEVER live in SQLite.

use crate::chunker::{Codec, Hash32};
use crate::error::{Error, Result};
use crate::pack::{ChunkLoc, DEFAULT_PACK_TARGET_BYTES};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STORE_FORMAT_VERSION: u32 = 1;
pub const ROOT_INODE: u64 = 0;

pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;

pub const STORAGE_CHUNKS: u8 = 0;
pub const STORAGE_OVERLAY: u8 = 1;

/// Persistent store configuration written to `<store>/store.json`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoreConfig {
    pub version: u32,
    pub codec: Codec,
    pub pack_target_bytes: u64,
    pub volume_size_bytes: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            version: STORE_FORMAT_VERSION,
            codec: Codec::Zstd1,
            pack_target_bytes: DEFAULT_PACK_TARGET_BYTES,
            volume_size_bytes: 2 * 1024 * 1024 * 1024 * 1024, // 2 TiB reported virtual volume
        }
    }
}

/// One workspace record.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WorkspaceInfo {
    pub ws_id: u64,
    pub name: String,
    pub root_inode: u64,
    pub created_ns: i64,
    pub files: u64,
    pub dirs: u64,
    pub logical_bytes: u64,
    pub overlay_files: u64,
    pub overlay_bytes: u64,
}

/// Overall store storage & deduplication statistics.
#[derive(Clone, Debug, serde::Serialize)]
pub struct StoreStats {
    pub workspaces: u64,
    pub total_files: u64,
    pub total_logical_bytes: u64,
    pub overlay_files: u64,
    pub overlay_bytes: u64,
    pub unique_chunks: u64,
    pub unique_raw_bytes: u64,
    pub stored_chunk_bytes: u64,
    pub pack_count: u64,
    pub pack_disk_bytes: u64,
    pub pack_dead_bytes: u64,
    pub meta_db_bytes: u64,
}

/// One inode row loaded from `meta.sqlite`.
#[derive(Clone, Debug)]
pub struct InodeRow {
    pub inode_id: u64,
    pub kind: u8,
    pub size: u64,
    pub attributes: u32,
    pub created_ns: i64,
    pub accessed_ns: i64,
    pub written_ns: i64,
    pub changed_ns: i64,
    pub storage_mode: u8,
    pub manifest: Vec<u8>,
    pub dirty_since_ns: i64,
    pub pinned_overlay: bool,
}

/// Entry inside a decoded chunk manifest: `(chunk_hash, raw_len)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    pub hash: Hash32,
    pub raw_len: u32,
}

pub const MANIFEST_ENTRY_BYTES: usize = 36;

/// Encodes chunk manifest entries as tightly packed 36-byte records (`hash[32] || raw_len_le[4]`).
pub fn encode_manifest(entries: &[ManifestEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * MANIFEST_ENTRY_BYTES);
    for e in entries {
        out.extend_from_slice(&e.hash);
        out.extend_from_slice(&e.raw_len.to_le_bytes());
    }
    out
}

/// Decodes a packed manifest blob.
pub fn decode_manifest(blob: &[u8]) -> Result<Vec<ManifestEntry>> {
    if blob.len() % MANIFEST_ENTRY_BYTES != 0 {
        return Err(Error::Corrupt(format!(
            "invalid manifest blob length {}",
            blob.len()
        )));
    }
    let mut out = Vec::with_capacity(blob.len() / MANIFEST_ENTRY_BYTES);
    for rec in blob.chunks_exact(MANIFEST_ENTRY_BYTES) {
        let hash: Hash32 = rec[0..32].try_into().unwrap();
        let raw_len = u32::from_le_bytes(rec[32..36].try_into().unwrap());
        out.push(ManifestEntry { hash, raw_len });
    }
    Ok(out)
}

/// Returns `true` for hot Unity / SVN / Git database or lock files that should stay pinned in
/// the local NTFS sparse overlay so LMDB `mmap` and SQLite locking never contend with background dedup.
pub fn is_hot_db_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "artifactdb"
            | "sourceassetdb"
            | "artifactdb-lock"
            | "sourceassetdb-lock"
            | "wc.db"
            | "wc.db-journal"
            | "wc.db-wal"
            | "wc.db-shm"
            | "index.lock"
    ) || lower.ends_with(".db")
        || lower.ends_with(".db-wal")
        || lower.ends_with(".db-shm")
        || lower.ends_with(".db-journal")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite-wal")
        || lower.ends_with(".sqlite-shm")
        || lower.ends_with(".mdb")
        || lower.ends_with(".lmdb")
        || lower.ends_with(".lock")
}

/// Current Unix timestamp in nanoseconds.
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS packs (
    pack_id     INTEGER PRIMARY KEY,
    size_bytes  INTEGER NOT NULL,
    live_bytes  INTEGER NOT NULL,
    dead_bytes  INTEGER NOT NULL,
    sealed      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    hash        BLOB PRIMARY KEY,
    pack_id     INTEGER NOT NULL,
    offset      INTEGER NOT NULL,
    raw_len     INTEGER NOT NULL,
    stored_len  INTEGER NOT NULL,
    codec       INTEGER NOT NULL,
    refcount    INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_chunks_pack ON chunks(pack_id);

CREATE TABLE IF NOT EXISTS workspaces (
    ws_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    root_inode  INTEGER NOT NULL,
    next_inode  INTEGER NOT NULL,
    created_ns  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS inodes (
    ws_id          INTEGER NOT NULL,
    inode_id       INTEGER NOT NULL,
    kind           INTEGER NOT NULL,
    size           INTEGER NOT NULL,
    attributes     INTEGER NOT NULL,
    created_ns     INTEGER NOT NULL,
    accessed_ns    INTEGER NOT NULL,
    written_ns     INTEGER NOT NULL,
    changed_ns     INTEGER NOT NULL,
    storage_mode   INTEGER NOT NULL,
    manifest       BLOB NOT NULL,
    dirty_since_ns INTEGER NOT NULL,
    pinned_overlay INTEGER NOT NULL,
    PRIMARY KEY (ws_id, inode_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_inodes_overlay
    ON inodes(ws_id, storage_mode, pinned_overlay, dirty_since_ns)
    WHERE storage_mode = 1;

CREATE TABLE IF NOT EXISTS dentries (
    ws_id        INTEGER NOT NULL,
    parent_id    INTEGER NOT NULL,
    folded_name  BLOB NOT NULL,
    display_name BLOB NOT NULL,
    child_id     INTEGER NOT NULL,
    kind         INTEGER NOT NULL,
    PRIMARY KEY (ws_id, parent_id, folded_name)
) WITHOUT ROWID;
"#;

/// Opens and configures `meta.sqlite` with fast WAL settings tuned for metadata-only workloads.
pub fn open_meta_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\
         PRAGMA synchronous = NORMAL;\
         PRAGMA page_size = 4096;\
         PRAGMA cache_size = -131072;\
         PRAGMA wal_autocheckpoint = 16384;\
         PRAGMA temp_store = MEMORY;\
         PRAGMA foreign_keys = OFF;",
    )?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

/// Queries the next unused `pack_id` (and optionally the latest unsealed pack).
pub fn next_pack_id(conn: &Connection) -> Result<u64> {
    let max_id: Option<i64> = conn
        .query_row("SELECT MAX(pack_id) FROM packs", [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(max_id.map_or(1, |v| (v as u64) + 1))
}

/// Looks up one chunk's pack location by BLAKE3-256 hash.
pub fn get_chunk_loc(conn: &Connection, hash: &Hash32) -> Result<Option<ChunkLoc>> {
    let row: Option<(i64, i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT pack_id, offset, raw_len, stored_len, codec FROM chunks WHERE hash = ?1",
            params![hash.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    match row {
        Some((pack_id, offset, raw_len, stored_len, codec)) => Ok(Some(ChunkLoc {
            pack_id: pack_id as u64,
            offset: offset as u64,
            raw_len: raw_len as u32,
            stored_len: stored_len as u32,
            codec: Codec::from_u8(codec as u8)?,
        })),
        None => Ok(None),
    }
}

/// Decrements chunk refcounts for every chunk in `manifest_blob`; chunks whose refcount hits 0
/// are removed from `chunks` and accounted as `dead_bytes` in `packs`.
pub fn release_manifest_chunks(tx: &rusqlite::Transaction<'_>, manifest_blob: &[u8]) -> Result<()> {
    if manifest_blob.is_empty() {
        return Ok(());
    }
    let entries = decode_manifest(manifest_blob)?;
    let mut dec_stmt = tx.prepare_cached(
        "UPDATE chunks SET refcount = refcount - 1 WHERE hash = ?1 RETURNING refcount, pack_id, stored_len",
    )?;
    let mut del_stmt = tx.prepare_cached("DELETE FROM chunks WHERE hash = ?1")?;
    let mut pack_dead_stmt = tx.prepare_cached(
        "UPDATE packs SET live_bytes = MAX(0, live_bytes - ?1), dead_bytes = dead_bytes + ?1 WHERE pack_id = ?2",
    )?;

    for entry in entries {
        let info: Option<(i64, i64, i64)> = dec_stmt
            .query_row(params![entry.hash.as_slice()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        if let Some((refcount, pack_id, stored_len)) = info {
            if refcount <= 0 {
                del_stmt.execute(params![entry.hash.as_slice()])?;
                let record_bytes = crate::pack::RECORD_HEADER_BYTES as i64 + stored_len;
                pack_dead_stmt.execute(params![record_bytes, pack_id])?;
            }
        }
    }
    Ok(())
}

/// Persists pack header/size/sealed updates into `packs`.
pub fn upsert_pack_updates(
    tx: &rusqlite::Transaction<'_>,
    updates: &[(u64, u64, bool)],
) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO packs (pack_id, size_bytes, live_bytes, dead_bytes, sealed)
         VALUES (?1, ?2, 0, 0, ?3)
         ON CONFLICT(pack_id) DO UPDATE SET
           size_bytes = excluded.size_bytes,
           sealed = MAX(packs.sealed, excluded.sealed)",
    )?;
    for &(pack_id, size_bytes, sealed) in updates {
        stmt.execute(params![pack_id as i64, size_bytes as i64, i64::from(sealed)])?;
    }
    Ok(())
}

/// Inserts a newly written chunk (refcount = `add_refs`) or increments its refcount if it already exists.
pub fn upsert_chunk_ref(
    tx: &rusqlite::Transaction<'_>,
    hash: &Hash32,
    loc: Option<ChunkLoc>,
    add_refs: u64,
) -> Result<()> {
    if add_refs == 0 {
        return Ok(());
    }
    if let Some(loc) = loc {
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO chunks (hash, pack_id, offset, raw_len, stored_len, codec, refcount)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                hash.as_slice(),
                loc.pack_id as i64,
                loc.offset as i64,
                i64::from(loc.raw_len),
                i64::from(loc.stored_len),
                i64::from(loc.codec as u8),
                add_refs as i64,
            ],
        )?;
        if inserted > 0 {
            let record_bytes = crate::pack::RECORD_HEADER_BYTES as i64 + i64::from(loc.stored_len);
            tx.execute(
                "UPDATE packs SET live_bytes = live_bytes + ?1 WHERE pack_id = ?2",
                params![record_bytes, loc.pack_id as i64],
            )?;
            return Ok(());
        }
    }
    tx.execute(
        "UPDATE chunks SET refcount = refcount + ?1 WHERE hash = ?2",
        params![add_refs as i64, hash.as_slice()],
    )?;
    Ok(())
}

/// Creates a brand-new empty workspace with root inode 1 and returns its `ws_id`.
pub fn create_empty_workspace(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<u64> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidInput(format!("invalid workspace name {name:?}")));
    }
    let exists: Option<i64> = tx
        .query_row(
            "SELECT ws_id FROM workspaces WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Err(Error::AlreadyExists(format!("workspace {name:?} already exists")));
    }
    let ts = now_ns();
    tx.execute(
        "INSERT INTO workspaces (name, root_inode, next_inode, created_ns) VALUES (?1, ?2, ?3, ?4)",
        params![name, ROOT_INODE as i64, (ROOT_INODE + 1) as i64, ts],
    )?;
    let ws_id = tx.last_insert_rowid() as u64;
    tx.execute(
        "INSERT INTO inodes (ws_id, inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay)
         VALUES (?1, ?2, ?3, 0, 0, ?4, ?4, ?4, ?4, ?5, X'', 0, 0)",
        params![ws_id as i64, ROOT_INODE as i64, i64::from(KIND_DIR), ts, i64::from(STORAGE_CHUNKS)],
    )?;
    Ok(ws_id)
}

/// Resolves workspace name to `ws_id`.
pub fn find_workspace_id(conn: &Connection, name: &str) -> Result<u64> {
    conn.query_row(
        "SELECT ws_id FROM workspaces WHERE name = ?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    .map(|v| v as u64)
    .ok_or_else(|| Error::NotFound(format!("workspace {name:?}")))
}

/// Reads an inode row by `(ws_id, inode_id)`.
pub fn load_inode(conn: &Connection, ws_id: u64, inode_id: u64) -> Result<Option<InodeRow>> {
    conn.query_row(
        "SELECT inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay
         FROM inodes WHERE ws_id = ?1 AND inode_id = ?2",
        params![ws_id as i64, inode_id as i64],
        |r| {
            Ok(InodeRow {
                inode_id: r.get::<_, i64>(0)? as u64,
                kind: r.get::<_, i64>(1)? as u8,
                size: r.get::<_, i64>(2)? as u64,
                attributes: r.get::<_, i64>(3)? as u32,
                created_ns: r.get(4)?,
                accessed_ns: r.get(5)?,
                written_ns: r.get(6)?,
                changed_ns: r.get(7)?,
                storage_mode: r.get::<_, i64>(8)? as u8,
                manifest: r.get(9)?,
                dirty_since_ns: r.get(10)?,
                pinned_overlay: r.get::<_, i64>(11)? != 0,
            })
        },
    )
    .optional()
    .map_err(Error::from)
}
