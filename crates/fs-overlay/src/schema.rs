//! The overlay file's schema and pragmas (ADR-006 §5.1, §5.2).
//!
//! fs-overlay owns this file outright: nothing else opens it, and it is not the
//! store's metadata database. The DDL is validated by reading it back on open, the
//! same per-crate governance ADR-003 §4.2 established.

/// The overlay schema version this build writes and accepts.
///
/// Changing any DDL statement below means bumping this: an overlay written by another
/// build is refused rather than misinterpreted.
/// The overlay schema version.
///
/// **2** added the four v3 metadata columns to `ov_inode` (`accessed_ns`, `changed_ns`, `attributes`, `security`). They are
/// `NOT NULL DEFAULT`: an overlay written by version 1 is still readable, and reading it reports 0 / empty, which is the
/// same "absent, not zero" convention the snapshot record uses. The column is where the value *can* live; whether it does
/// is decided by the writer, not by the schema.
pub const SCHEMA_VERSION: i64 = 2;

/// The DDL, statement by statement.
///
/// Kept as a list rather than one blob so the open-time check can compare statement
/// counts and text exactly, which is what makes "the file was written by this build"
/// a fact rather than a hope.
pub const DDL: &[&str] = &[
    "CREATE TABLE ov_meta (
         key TEXT NOT NULL PRIMARY KEY,
         value TEXT NOT NULL
     ) STRICT",
    "CREATE TABLE ov_inode (
         serial INTEGER NOT NULL PRIMARY KEY,
         origin INTEGER NOT NULL,
         live INTEGER NOT NULL,
         kind INTEGER NOT NULL,
         size INTEGER NOT NULL,
         created_ns INTEGER NOT NULL,
         modified_ns INTEGER NOT NULL,
         base_limit INTEGER NOT NULL,
         entries INTEGER NOT NULL,
         accessed_ns INTEGER NOT NULL DEFAULT 0,
         changed_ns INTEGER NOT NULL DEFAULT 0,
         attributes INTEGER NOT NULL DEFAULT 0,
         security BLOB NOT NULL DEFAULT x''
     ) STRICT",
    "CREATE TABLE ov_entry (
         parent INTEGER NOT NULL,
         name BLOB NOT NULL,
         child INTEGER,
         PRIMARY KEY (parent, name)
     ) STRICT",
    "CREATE TABLE ov_page (
         serial INTEGER NOT NULL,
         page_index INTEGER NOT NULL,
         data BLOB NOT NULL,
         PRIMARY KEY (serial, page_index)
     ) STRICT",
];

/// `origin = 0`: the inode exists in the base and this row overrides it.
pub const ORIGIN_BASE: i64 = 0;

/// `origin = 1`: the inode was created in this workspace.
pub const ORIGIN_LOCAL: i64 = 1;

/// `live = 0` marks a base inode deleted in this workspace (a tombstone).
pub const TOMBSTONE: i64 = 0;

/// `live = 1` marks a visible inode.
pub const LIVE: i64 = 1;

/// The page size overlay content is stored in.
///
/// 4 KiB rather than the 64 KiB block of the P1 extent format: a page is the unit a
/// partial write rewrites, and ADR-006 §7.2/§7.3 describe the content paths in terms of
/// 4 KiB pages (only the first and last page of a write may need to touch the base).
pub const PAGE_BYTES: u64 = 4096;

/// The overlay's pragmas (ADR-006 §5.1).
///
/// `WAL` + `NORMAL` is the decision that a change is committed but not fsynced;
/// `flush` raises the synchronous level for one commit. `EXCLUSIVE` locking is what
/// makes a second opener fail instead of racing (W2), and `busy_timeout = 0` follows
/// from that: there is no other writer to wait for, so a lock error is a bug.
pub const PRAGMAS: &[&str] = &[
    "PRAGMA journal_mode = WAL",
    "PRAGMA synchronous = NORMAL",
    "PRAGMA locking_mode = EXCLUSIVE",
    "PRAGMA busy_timeout = 0",
    "PRAGMA foreign_keys = OFF",
];

/// The pragma values the file must report back after opening.
///
/// Read back rather than trusted: a pragma silently not applied would turn the
/// durability contract into a guess.
pub const EXPECTED_JOURNAL_MODE: &str = "wal";
