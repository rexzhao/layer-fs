//! The main database's schema: workspaces, branches, commits (ADR-006 §4).
//!
//! fs-workspace owns these tables. They live in the store's `store.sqlite` beside
//! cas-store's object index, but they are a different concern: the index says *what
//! objects exist*, these say *which tree a workspace starts from and where its branch
//! points*.
//!
//! The DDL is declared once and validated by reading it back on open — the per-crate
//! governance ADR-003 §4.2 established, since two crates write the same database and
//! each needs a checkable claim about what it created.

/// The schema version this build writes and accepts (ADR-006 §14).
pub const SCHEMA_VERSION: i64 = 3;

/// The DDL, statement by statement, so validation can compare text exactly.
///
/// `commit`, `branch` and `snapshot` are quoted because they are SQL keywords or would
/// be ambiguous; the names match ADR-006 §4 verbatim.
pub const DDL: &[&str] = &[
    "CREATE TABLE workspace_policy (
         key TEXT NOT NULL PRIMARY KEY,
         value TEXT NOT NULL
     ) STRICT",
    "CREATE TABLE scope_allocator (
         scope_id TEXT NOT NULL PRIMARY KEY,
         next_serial INTEGER NOT NULL
     ) STRICT",
    "CREATE TABLE \"commit\" (
         commit_id TEXT NOT NULL PRIMARY KEY,
         root_oid TEXT NOT NULL,
         parent_commit TEXT,
         created_ns INTEGER NOT NULL,
         message TEXT NOT NULL
     ) STRICT",
    "CREATE TABLE branch (
         name TEXT NOT NULL PRIMARY KEY,
         scope_id TEXT NOT NULL,
         head_commit TEXT NOT NULL
     ) STRICT",
    "CREATE TABLE branch_log (
         seq INTEGER PRIMARY KEY,
         branch TEXT NOT NULL,
         old_commit TEXT,
         new_commit TEXT NOT NULL,
         op TEXT NOT NULL,
         created_ns INTEGER NOT NULL
     ) STRICT",
    "CREATE TABLE workspace (
         name TEXT NOT NULL PRIMARY KEY,
         id TEXT NOT NULL UNIQUE,
         branch TEXT NOT NULL,
         base_commit TEXT NOT NULL,
         base_root TEXT NOT NULL,
         state TEXT NOT NULL,
         created_ns INTEGER NOT NULL
     ) STRICT",
    "CREATE TABLE snapshot (
         root_oid TEXT NOT NULL,
         workspace TEXT NOT NULL,
         created_ns INTEGER NOT NULL,
         PRIMARY KEY (root_oid, workspace, created_ns)
     ) STRICT",
];

/// How many serials a workspace reserves at a time (ADR-006 §6).
pub const SERIAL_SEGMENT: u64 = 1024;

/// Reserve more once fewer than this many serials are left.
///
/// Topping up at a low-water mark rather than on exhaustion means a single operation can
/// always allocate: the only cost of a bigger margin is wasted serials, which are never
/// reused anyway (ADR-003 §4.3).
pub const SERIAL_LOW_WATER: u64 = 256;

/// The page size the overlay files use, recorded in the policy (ADR-006 §4).
pub const OVERLAY_PAGE_BYTES: u64 = 4096;

/// How long a main-database statement waits for a competing writer (ADR-006 §11.1).
///
/// This is the documented exception to ADR-001 D10's "no busy handler": after workspaces
/// became exclusive the only remaining contention is short cross-workspace publication
/// and serial reservation, and failing those at random would break correct concurrent
/// use. The overlay connection keeps `busy_timeout = 0` — it is exclusive, so a lock
/// error there is a bug, not contention.
pub const MAIN_BUSY_TIMEOUT_MS: u64 = 5_000;

/// `workspace.state` while its overlay file is being created.
pub const STATE_CREATING: &str = "creating";

/// `workspace.state` once it is usable.
pub const STATE_READY: &str = "ready";

/// The branch a fresh store's first commit lands on.
pub const DEFAULT_BRANCH: &str = "main";

/// The message of the commit `init` creates.
pub const INIT_MESSAGE: &str = "init";

/// The DDL text stored in `workspace_policy`, so a schema change is detected instead of
/// discovered halfway through a read.
#[must_use]
pub fn ddl_text() -> String {
    DDL.join(";\n")
}
