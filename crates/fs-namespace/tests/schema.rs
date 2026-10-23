//! Schema governance for the namespace tables (ADR-003 §4.2, ADR-004 §9).
//!
//! These cover the T4 documentation corrections that live in code: the policy
//! key labels and the version/parameter checks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::{OpenMode, Store};
use fs_namespace::{Namespace, NamespaceError};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-ns-schema-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A namespace created by this build records version 2 and the block size.
#[test]
fn a_new_namespace_records_version_2_and_the_block_size() {
    let root = TempRoot::new("recorded");
    let store = Store::open(&root.path, OpenMode::Create).expect("store");
    Namespace::open(&store, "default", OpenMode::Create).expect("namespace");

    let connection =
        rusqlite::Connection::open(store.metadata_path()).expect("open metadata directly");
    let version: String = connection
        .query_row(
            "SELECT value FROM namespace_policy WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("version");
    assert_eq!(version, "2");

    let block: String = connection
        .query_row(
            "SELECT value FROM namespace_policy WHERE key = 'block_bytes'",
            [],
            |row| row.get(0),
        )
        .expect("block size");
    assert_eq!(block, "65536");
}

/// ADR-004 §9: a v1 Store is refused, not silently migrated.
#[test]
fn a_version_1_store_is_refused() {
    let root = TempRoot::new("v1");
    {
        let store = Store::open(&root.path, OpenMode::Create).expect("store");
        Namespace::open(&store, "default", OpenMode::Create).expect("namespace");
    }
    let path = root.path.join("store.sqlite");
    {
        let connection = rusqlite::Connection::open(&path).expect("open");
        connection
            .execute(
                "UPDATE namespace_policy SET value = '1' WHERE key = 'schema_version'",
                [],
            )
            .expect("downgrade");
    }

    let store = Store::open(&root.path, OpenMode::ReadWrite).expect("store");
    match Namespace::open(&store, "default", OpenMode::ReadWrite) {
        Err(NamespaceError::SchemaMismatch { what }) => {
            assert_eq!(what, "namespace_policy.schema_version");
        }
        other => panic!("expected a schema mismatch, got {other:?}"),
    }
}

/// QA note 3: a removed policy key reports the qualified name, with the key.
#[test]
fn a_removed_policy_key_reports_the_qualified_key_name() {
    for (key, expected) in [
        ("schema_version", "namespace_policy.schema_version"),
        ("block_bytes", "namespace_policy.block_bytes"),
    ] {
        let root = TempRoot::new("removed");
        {
            let store = Store::open(&root.path, OpenMode::Create).expect("store");
            Namespace::open(&store, "default", OpenMode::Create).expect("namespace");
        }
        let path = root.path.join("store.sqlite");
        {
            let connection = rusqlite::Connection::open(&path).expect("open");
            connection
                .execute("DELETE FROM namespace_policy WHERE key = ?1", [key])
                .expect("delete key");
        }

        let store = Store::open(&root.path, OpenMode::ReadWrite).expect("store");
        match Namespace::open(&store, "default", OpenMode::ReadWrite) {
            Err(NamespaceError::SchemaMismatch { what }) => {
                assert_eq!(what, expected, "key {key} reported the wrong label");
                assert!(
                    what.contains('.') && what.contains(key),
                    "the label must name the table and the key: {what}"
                );
            }
            other => panic!("expected a schema mismatch for {key}, got {other:?}"),
        }
    }
}

/// The recorded DDL is compared verbatim, so a changed table is caught.
#[test]
fn a_changed_ddl_record_is_refused() {
    let root = TempRoot::new("ddl");
    {
        let store = Store::open(&root.path, OpenMode::Create).expect("store");
        Namespace::open(&store, "default", OpenMode::Create).expect("namespace");
    }
    let path = root.path.join("store.sqlite");
    {
        let connection = rusqlite::Connection::open(&path).expect("open");
        connection
            .execute(
                "UPDATE namespace_policy SET value = value || ' ' WHERE key = 'ddl'",
                [],
            )
            .expect("tamper");
    }

    let store = Store::open(&root.path, OpenMode::ReadWrite).expect("store");
    match Namespace::open(&store, "default", OpenMode::ReadWrite) {
        Err(NamespaceError::SchemaMismatch { what }) => {
            assert_eq!(what, "namespace_policy.ddl");
        }
        other => panic!("expected a schema mismatch, got {other:?}"),
    }
}

/// The recorded DDL must match the tables that actually exist.
#[test]
fn every_recorded_ddl_table_exists() {
    let root = TempRoot::new("tables");
    let store = Store::open(&root.path, OpenMode::Create).expect("store");
    Namespace::open(&store, "default", OpenMode::Create).expect("namespace");

    let connection =
        rusqlite::Connection::open(store.metadata_path()).expect("open metadata directly");
    let ddl: String = connection
        .query_row(
            "SELECT value FROM namespace_policy WHERE key = 'ddl'",
            [],
            |row| row.get(0),
        )
        .expect("ddl");

    for table in [
        "namespace_policy",
        "namespace",
        "file_id_allocator",
        "file",
        "directory_entry",
        "root",
        "file_extent",
    ] {
        assert!(
            ddl.contains(&format!("CREATE TABLE {table} (")),
            "the recorded DDL omits {table}"
        );
        let exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(exists, 1, "table {table} was not created");
    }

    // ADR-004 §5.2: the extent table's columns are the documented ones.
    for column in ["logical_start", "length", "object_id", "source_offset"] {
        assert!(
            ddl.contains(column),
            "the extent table omits the {column} column"
        );
    }
}
