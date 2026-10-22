//! The namespace tables inside the Store database (ADR-003 §4).
//!
//! `fs-namespace` opens its own connection to the Store's metadata file and owns
//! these tables, their DDL and their version. Nothing here knows the object
//! layout; nothing in `cas-store` knows these tables exist.

use rusqlite::Connection;

use crate::error::NamespaceError;

/// The namespace schema version this build writes.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The `namespace_policy` key holding the version.
const VERSION_KEY: &str = "schema_version";

/// The `namespace_policy` key holding this crate's DDL text.
const DDL_KEY: &str = "ddl";

/// Every statement that creates the namespace tables, in order.
///
/// Compared verbatim against the recorded copy, so it must be exactly
/// reproducible (the same self-check pattern `cas-store` uses).
pub(crate) const DDL: &str = "\
CREATE TABLE namespace_policy (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE namespace (
    name TEXT NOT NULL PRIMARY KEY,
    head_root INTEGER NOT NULL
) STRICT;
CREATE TABLE file_id_allocator (
    namespace TEXT NOT NULL PRIMARY KEY,
    next_serial INTEGER NOT NULL
) STRICT;
CREATE TABLE file (
    namespace TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    content_root TEXT,
    created_ns INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL,
    PRIMARY KEY (namespace, file_id)
) STRICT;
CREATE TABLE directory_entry (
    namespace TEXT NOT NULL,
    parent_id INTEGER NOT NULL,
    name BLOB NOT NULL,
    child_id INTEGER NOT NULL,
    PRIMARY KEY (namespace, parent_id, name)
) STRICT;
CREATE TABLE root (
    root_id INTEGER PRIMARY KEY,
    namespace TEXT NOT NULL,
    parent_root_id INTEGER,
    op TEXT NOT NULL,
    created_ns INTEGER NOT NULL
) STRICT;
CREATE INDEX root_by_namespace ON root (namespace, root_id);
";

/// Creates the namespace tables and records their version and DDL.
pub(crate) fn create(connection: &mut Connection) -> Result<(), NamespaceError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(DDL)?;
    transaction.execute(
        "INSERT INTO namespace_policy (key, value) VALUES (?1, ?2)",
        (VERSION_KEY, SCHEMA_VERSION.to_string()),
    )?;
    transaction.execute(
        "INSERT INTO namespace_policy (key, value) VALUES (?1, ?2)",
        (DDL_KEY, DDL),
    )?;
    transaction.commit()?;
    Ok(())
}

/// Checks that an existing database carries this crate's tables unchanged.
pub(crate) fn validate(connection: &Connection) -> Result<(), NamespaceError> {
    let version = policy_value(connection, VERSION_KEY)?;
    if version != SCHEMA_VERSION.to_string() {
        return Err(NamespaceError::SchemaMismatch {
            what: "namespace_policy.schema_version",
        });
    }
    let ddl = policy_value(connection, DDL_KEY)?;
    if ddl != DDL {
        return Err(NamespaceError::SchemaMismatch {
            what: "namespace_policy.ddl",
        });
    }
    Ok(())
}

/// Whether this database already has the namespace tables.
pub(crate) fn is_present(connection: &Connection) -> Result<bool, NamespaceError> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'namespace_policy'",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 1)
}

fn policy_value(connection: &Connection, key: &str) -> Result<String, NamespaceError> {
    connection
        .query_row(
            "SELECT value FROM namespace_policy WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map_err(|_| NamespaceError::SchemaMismatch {
            what: "namespace_policy",
        })
}
