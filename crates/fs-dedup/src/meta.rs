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
    /// Directory/file patterns skipped during `import` and `import --update`
    /// (e.g. `"Temp"`, `"Logs"`, `"Library/ShaderCache"`).
    #[serde(default)]
    pub import_exclude: Vec<String>,
    /// Directory/file patterns kept in local NTFS sparse overlay (never moved to pack CAS
    /// by background `dedup`), so Unity/IDE ephemeral folders stay native-speed without
    /// creating dead pack churn.
    #[serde(default)]
    pub no_dedup: Vec<String>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            version: STORE_FORMAT_VERSION,
            codec: Codec::Zstd1,
            pack_target_bytes: DEFAULT_PACK_TARGET_BYTES,
            volume_size_bytes: 2 * 1024 * 1024 * 1024 * 1024, // 2 TiB reported virtual volume
            import_exclude: Vec::new(),
            no_dedup: Vec::new(),
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

CREATE INDEX IF NOT EXISTS idx_dentries_child
    ON dentries(ws_id, child_id);

CREATE TABLE IF NOT EXISTS pending_chunk_releases (
    hash        BLOB PRIMARY KEY,
    count       INTEGER NOT NULL
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
         PRAGMA cache_size = -262144;\
         PRAGMA mmap_size = 4294967296;\
         PRAGMA wal_autocheckpoint = 16384;\
         PRAGMA temp_store = MEMORY;\
         PRAGMA foreign_keys = OFF;",
    )?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

/// Opens a fast read-only WAL connection for concurrent metadata queries (`read`, `get_info`,
/// `read_dir`, `open_dir`) without contending with other readers or writer locks.
pub fn open_readonly_meta_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    // 4 GiB OS mmap already caches hot SQLite pages in shared OS memory; keep per-conn
    // private page cache at 16 MiB so 16 RO connections use ~256 MiB max instead of 2+ GiB.
    conn.execute_batch(
        "PRAGMA query_only = ON;\
         PRAGMA cache_size = -16384;\
         PRAGMA mmap_size = 4294967296;\
         PRAGMA temp_store = MEMORY;",
    )?;
    conn.set_prepared_statement_cache_capacity(64);
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
    let mut stmt = conn.prepare_cached(
        "SELECT pack_id, offset, raw_len, stored_len, codec FROM chunks WHERE hash = ?1",
    )?;
    let row: Option<(i64, i64, i64, i64, i64)> = stmt
        .query_row(params![hash.as_slice()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
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

/// Records deferred chunk refcount decrements durably inside the same SQLite transaction
/// as manifest updates/deletes so a crash or mid-import failure never loses refcount releases.
pub fn enqueue_pending_chunk_release(
    tx: &rusqlite::Transaction<'_>,
    hash: &Hash32,
    count: u64,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO pending_chunk_releases (hash, count) VALUES (?1, ?2)
         ON CONFLICT(hash) DO UPDATE SET count = count + excluded.count",
    )?;
    stmt.execute(params![hash.as_slice(), count as i64])?;
    Ok(())
}

/// Cancels up to `add_refs` against any durably queued release for `hash` inside `tx`,
/// returning the number of refs cancelled (which therefore need neither decrement nor increment).
pub fn cancel_pending_chunk_release(
    tx: &rusqlite::Transaction<'_>,
    hash: &Hash32,
    add_refs: u64,
) -> Result<u64> {
    if add_refs == 0 {
        return Ok(0);
    }
    let mut sel =
        tx.prepare_cached("SELECT count FROM pending_chunk_releases WHERE hash = ?1")?;
    let existing: Option<i64> = sel
        .query_row(params![hash.as_slice()], |r| r.get(0))
        .optional()?;
    let Some(cur) = existing else {
        return Ok(0);
    };
    let cur_u64 = cur.max(0) as u64;
    let cancel = add_refs.min(cur_u64);
    let rem = cur_u64 - cancel;
    if rem == 0 {
        let mut del = tx.prepare_cached("DELETE FROM pending_chunk_releases WHERE hash = ?1")?;
        del.execute(params![hash.as_slice()])?;
    } else {
        let mut upd = tx.prepare_cached(
            "UPDATE pending_chunk_releases SET count = ?1 WHERE hash = ?2",
        )?;
        upd.execute(params![rem as i64, hash.as_slice()])?;
    }
    Ok(cancel)
}

/// Applies and clears all durably queued `pending_chunk_releases` inside `tx`, returning the
/// list of chunk hashes whose refcount dropped to 0 and were removed from `chunks`.
pub fn replay_pending_chunk_releases(tx: &rusqlite::Transaction<'_>) -> Result<Vec<Hash32>> {
    let mut rows_to_apply: Vec<(Hash32, u64)> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT hash, count FROM pending_chunk_releases")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (h_bytes, count) = row?;
            if h_bytes.len() == 32 && count > 0 {
                let hash: Hash32 = h_bytes.try_into().unwrap();
                rows_to_apply.push((hash, count as u64));
            }
        }
    }
    if rows_to_apply.is_empty() {
        return Ok(Vec::new());
    }
    let mut deleted = Vec::new();
    for (hash, count) in rows_to_apply {
        if decrement_chunk_refs(tx, &hash, count)? {
            deleted.push(hash);
        }
    }
    tx.execute("DELETE FROM pending_chunk_releases", [])?;
    Ok(deleted)
}

/// Decrements a chunk's refcount by `dec_refs`; if refcount reaches `<= 0`, deletes the row
/// from `chunks`, updates `packs.live_bytes` / `dead_bytes`, and returns `Ok(true)`.
pub fn decrement_chunk_refs(
    tx: &rusqlite::Transaction<'_>,
    hash: &Hash32,
    dec_refs: u64,
) -> Result<bool> {
    if dec_refs == 0 {
        return Ok(false);
    }
    let mut dec_stmt = tx.prepare_cached(
        "UPDATE chunks SET refcount = refcount - ?1 WHERE hash = ?2 RETURNING refcount, pack_id, stored_len",
    )?;
    let info: Option<(i64, i64, i64)> = dec_stmt
        .query_row(params![dec_refs as i64, hash.as_slice()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    if let Some((refcount, pack_id, stored_len)) = info {
        if refcount <= 0 {
            let mut del_stmt = tx.prepare_cached("DELETE FROM chunks WHERE hash = ?1")?;
            del_stmt.execute(params![hash.as_slice()])?;
            let record_bytes = crate::pack::RECORD_HEADER_BYTES as i64 + stored_len;
            let mut pack_dead_stmt = tx.prepare_cached(
                "UPDATE packs SET live_bytes = MAX(0, live_bytes - ?1), dead_bytes = dead_bytes + ?1 WHERE pack_id = ?2",
            )?;
            pack_dead_stmt.execute(params![record_bytes, pack_id])?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Decrements chunk refcounts for every chunk in `manifest_blob`; chunks whose refcount hits 0
/// are removed from `chunks` and accounted as `dead_bytes` in `packs`.
/// Returns the list of chunk hashes deleted from `chunks` so callers can evict `chunk_loc_cache`.
pub fn release_manifest_chunks(
    tx: &rusqlite::Transaction<'_>,
    manifest_blob: &[u8],
) -> Result<Vec<Hash32>> {
    if manifest_blob.is_empty() {
        return Ok(Vec::new());
    }
    let entries = decode_manifest(manifest_blob)?;
    let mut deleted = Vec::new();
    for entry in entries {
        if decrement_chunk_refs(tx, &entry.hash, 1)? {
            deleted.push(entry.hash);
        }
    }
    Ok(deleted)
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
/// Returns `Error::Corrupt` if `loc` is `None` and the chunk row does not exist (never silently drops a ref).
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
        let mut ins = tx.prepare_cached(
            "INSERT OR IGNORE INTO chunks (hash, pack_id, offset, raw_len, stored_len, codec, refcount)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let inserted = ins.execute(params![
            hash.as_slice(),
            loc.pack_id as i64,
            loc.offset as i64,
            i64::from(loc.raw_len),
            i64::from(loc.stored_len),
            i64::from(loc.codec as u8),
            add_refs as i64,
        ])?;
        if inserted > 0 {
            let record_bytes = crate::pack::RECORD_HEADER_BYTES as i64 + i64::from(loc.stored_len);
            let mut add_live = tx.prepare_cached(
                "UPDATE packs SET live_bytes = live_bytes + ?1 WHERE pack_id = ?2",
            )?;
            add_live.execute(params![record_bytes, loc.pack_id as i64])?;
            return Ok(());
        }
    }
    let mut upd =
        tx.prepare_cached("UPDATE chunks SET refcount = refcount + ?1 WHERE hash = ?2")?;
    let updated = upd.execute(params![add_refs as i64, hash.as_slice()])?;
    if updated == 0 {
        return Err(Error::Corrupt(format!(
            "upsert_chunk_ref: missing chunk row for hash {:02x?}",
            &hash[..4]
        )));
    }
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
    let mut stmt = conn.prepare_cached(
        "SELECT inode_id, kind, size, attributes, created_ns, accessed_ns, written_ns, changed_ns, storage_mode, manifest, dirty_since_ns, pinned_overlay
         FROM inodes WHERE ws_id = ?1 AND inode_id = ?2",
    )?;
    stmt.query_row(params![ws_id as i64, inode_id as i64], |r| {
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
    })
    .optional()
    .map_err(Error::from)
}
