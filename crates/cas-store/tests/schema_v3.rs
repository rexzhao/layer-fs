//! Schema v3 refusal matrix (ADR-008 §4, acceptance 2.1).
//!
//! A Store at any other version is **refused, not migrated**, and the refusal must leave the
//! database byte-identical: no `-wal`, no half-migration, and `tmp/` untouched (the recovery
//! sweep runs after validation, so a rejected Store must not have been "cleaned").
//!
//! Version 1 is a **real P2a Store**: the verbatim v1 DDL from `docs/adr/ADR-002-store-layout.md`
//! §4 (`object_location(oid, role, length, location)`, no `kind`/`offset`, no `pack` table).
//! A synthetic database that merely says `user_version = 1` while carrying the v3 tables would
//! exercise the version check and nothing else, which is exactly the fixture ADR-008 §4 warns
//! against. Versions 0, 2 and 4 are synthetic by nature — 0 and 2 never shipped as Store
//! versions here (`SCHEMA_VERSION` was 1, then 3), and 4 is this build's DDL with a bumped
//! `user_version`.

use std::path::{Path, PathBuf};

/// The verbatim `DDL` of `cas-store` at P2a (`SCHEMA_VERSION = 1`).
const V1_DDL: &str = "\
CREATE TABLE store_policy (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE schema_definition (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    ddl TEXT NOT NULL,
    schema_version INTEGER NOT NULL
) STRICT;
CREATE TABLE object_location (
    oid TEXT NOT NULL PRIMARY KEY,
    role INTEGER NOT NULL,
    length INTEGER NOT NULL,
    location TEXT NOT NULL
) STRICT;
";

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cas-store-v3-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Builds a Store at `version` using the DDL that version actually had.
fn store_at(root: &Path, version: u32) {
    let store = root.join("store");
    std::fs::create_dir_all(store.join("objects")).expect("objects");
    std::fs::create_dir_all(store.join("tmp")).expect("tmp");
    std::fs::create_dir_all(store.join("packs")).expect("packs");
    let database = store.join("store.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("db");

    if version == 0 {
        // Never initialised: an empty SQLite file, `user_version = 0`.
        connection
            .pragma_update(None, "user_version", 0)
            .expect("version");
        return;
    }

    let ddl = if version == 1 { V1_DDL } else { V3_DDL };
    connection.execute_batch(ddl).expect("ddl");
    connection
        .execute(
            "INSERT INTO schema_definition (id, ddl, schema_version) VALUES (1, ?1, ?2)",
            (ddl, version),
        )
        .expect("schema row");
    for (key, value) in [
        ("format_profile", "wcas/object/v1".to_owned()),
        ("max_object_len", cas_core::MAX_OBJECT_LEN.to_string()),
        ("schema_version", version.to_string()),
    ] {
        connection
            .execute(
                "INSERT INTO store_policy (key, value) VALUES (?1, ?2)",
                (key, value),
            )
            .expect("policy");
    }
    if version >= 2 {
        connection
            .execute(
                "INSERT INTO store_policy (key, value) VALUES ('pack_max_bytes', ?1)",
                (67108864u64.to_string(),),
            )
            .expect("pack policy");
    }
    connection
        .pragma_update(None, "user_version", version)
        .expect("version");
}

/// The v3 DDL, as this build creates it (a bumped copy is the "version 4" fixture).
const V3_DDL: &str = "\
CREATE TABLE store_policy (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE schema_definition (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    ddl TEXT NOT NULL,
    schema_version INTEGER NOT NULL
) STRICT;
CREATE TABLE object_location (
    oid TEXT NOT NULL PRIMARY KEY,
    role INTEGER NOT NULL,
    length INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    location TEXT NOT NULL,
    offset INTEGER NOT NULL
) STRICT;
CREATE TABLE pack (
    pack_id TEXT NOT NULL PRIMARY KEY,
    bytes INTEGER NOT NULL,
    objects INTEGER NOT NULL,
    created_ns INTEGER NOT NULL
) STRICT;
";

fn hash_of(path: &Path) -> blake3::Hash {
    blake3::hash(&std::fs::read(path).expect("read database"))
}

/// The columns of `object_location`, so a fixture's shape is checked rather than assumed.
fn location_columns(database: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(database).expect("db");
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info('object_location') ORDER BY cid")
        .expect("prepare");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("names")
}

#[test]
fn the_p2a_fixture_really_is_a_version_1_store() {
    // Guard for the fixture itself: if someone "upgrades" this to the v3 DDL with a bumped
    // user_version, the refusal tests would stop covering the real thing (ADR-008 §4).
    let root = TempRoot::new("fixture");
    store_at(root.path(), 1);
    let database = root.path().join("store").join("store.sqlite");
    assert_eq!(
        location_columns(&database),
        vec!["oid", "role", "length", "location"],
        "P2a's object_location has no kind/offset and no pack table"
    );
    let connection = rusqlite::Connection::open(&database).expect("db");
    let has_pack: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pack'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(has_pack, 0, "P2a has no pack table");
}

#[test]
fn every_other_version_is_refused_without_touching_the_store() {
    for version in [0u32, 1, 2, 4] {
        let root = TempRoot::new(&format!("v{version}"));
        store_at(root.path(), version);
        let store = root.path().join("store");
        let database = store.join("store.sqlite");
        let sentinel = store.join("tmp").join("left-by-a-crash.tmp");
        std::fs::write(&sentinel, b"not published").expect("sentinel");
        let before = hash_of(&database);

        // Opening twice must fail the same way: a first attempt that half-migrated would
        // make the second attempt behave differently.
        for attempt in ["first", "second"] {
            match cas_store::Store::open(&store, cas_store::OpenMode::ReadWrite) {
                Err(cas_store::StoreError::UnsupportedSchemaVersion { found, supported }) => {
                    assert_eq!(
                        found, version,
                        "{attempt} open of a version-{version} store"
                    );
                    assert_eq!(supported, 3, "this build writes version 3");
                }
                other => panic!("version {version}: expected a refusal, got {other:?}"),
            }
            assert_eq!(
                hash_of(&database),
                before,
                "version {version}: the {attempt} refusal changed the database"
            );
            for sidecar in ["store.sqlite-wal", "store.sqlite-shm"] {
                assert!(
                    !store.join(sidecar).exists(),
                    "version {version}: the {attempt} refusal left {sidecar} behind"
                );
            }
            // `clear_temporaries` runs after validation: a refused Store must not be cleaned.
            assert!(
                sentinel.is_file(),
                "version {version}: the {attempt} refusal removed a tmp file"
            );
        }
    }
}
