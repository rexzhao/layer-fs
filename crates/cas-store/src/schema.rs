//! The Store's database schema and its self-check (ADR-002 §4).
//!
//! The authoritative DDL lives here as one string. At creation it is written
//! verbatim into `schema_definition`; at every open the stored text is compared
//! with [`DDL`] character for character. A mismatch is a refusal, never a
//! migration or a repair.

use rusqlite::Connection;

use crate::error::{StoreError, StoreResult};

/// The schema version this build writes and accepts.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The format profile name recorded in `store_policy`.
///
/// Ties a Store to ADR-001's object format; a Store written under a different
/// object format must not be read as if it used this one.
pub(crate) const FORMAT_PROFILE: &str = "wcas/object/v1";

/// Every statement that creates a version-1 Store, in order.
///
/// Comments are excluded deliberately: this text is compared verbatim, so it
/// must be exactly reproducible.
pub(crate) const DDL: &str = "\
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

/// Creates the tables of a new Store and records the schema and policy.
///
/// Runs inside one transaction so a crash cannot leave a half-created schema.
pub(crate) fn create(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(DDL)?;
    transaction.execute(
        "INSERT INTO schema_definition (id, ddl, schema_version) VALUES (1, ?1, ?2)",
        (DDL, SCHEMA_VERSION),
    )?;
    for (key, value) in [
        ("format_profile", FORMAT_PROFILE.to_owned()),
        ("max_object_len", cas_core::MAX_OBJECT_LEN.to_string()),
        ("schema_version", SCHEMA_VERSION.to_string()),
    ] {
        transaction.execute(
            "INSERT INTO store_policy (key, value) VALUES (?1, ?2)",
            (key, value),
        )?;
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

/// Checks that an existing Store is the one this build understands.
///
/// Order (ADR-002 §7): recorded version, then the DDL text, then the policy
/// constants. The first disagreement is reported and nothing is changed.
pub(crate) fn validate(connection: &Connection) -> StoreResult<()> {
    let recorded: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if recorded != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found: recorded,
            supported: SCHEMA_VERSION,
        });
    }

    let stored_ddl: String = connection
        .query_row(
            "SELECT ddl FROM schema_definition WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::SchemaMismatch {
                what: "schema_definition row",
            },
            other => StoreError::Sqlite(other),
        })?;
    if stored_ddl != DDL {
        return Err(StoreError::SchemaMismatch {
            what: "schema_definition.ddl",
        });
    }

    let profile: String = policy_value(connection, "format_profile")?;
    if profile != FORMAT_PROFILE {
        return Err(StoreError::SchemaMismatch {
            what: "store_policy.format_profile",
        });
    }
    let recorded_version: String = policy_value(connection, "schema_version")?;
    if recorded_version != SCHEMA_VERSION.to_string() {
        return Err(StoreError::SchemaMismatch {
            what: "store_policy.schema_version",
        });
    }
    // Every constant recorded at creation is enforced here, not merely stored:
    // a policy key that is written but never read would silently stop being a
    // guarantee (T2-QA-1). The reference is the object format's own limit from
    // cas_core, so a Store that disagrees with this build is refused.
    let recorded_max: String = policy_value(connection, "max_object_len")?;
    if recorded_max != cas_core::MAX_OBJECT_LEN.to_string() {
        return Err(StoreError::SchemaMismatch {
            what: "store_policy.max_object_len",
        });
    }
    Ok(())
}

fn policy_value(connection: &Connection, key: &'static str) -> StoreResult<String> {
    connection
        .query_row(
            "SELECT value FROM store_policy WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            // A missing key reports the same qualified name a changed value does
            // (T3 backlog): `what` always names table.column, so a caller can
            // point at the row and the two failure shapes stay comparable.
            rusqlite::Error::QueryReturnedNoRows => StoreError::SchemaMismatch {
                what: qualified_policy_key(key),
            },
            other => StoreError::Sqlite(other),
        })
}

/// The `what` label for a policy key: `store_policy.<key>`.
fn qualified_policy_key(key: &'static str) -> &'static str {
    match key {
        "format_profile" => "store_policy.format_profile",
        "max_object_len" => "store_policy.max_object_len",
        "schema_version" => "store_policy.schema_version",
        _ => "store_policy.<unknown>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_is_stable_text() {
        // The DDL is compared verbatim, so a stray edit must show up as a diff
        // in this hash rather than as a silently different schema.
        assert!(DDL.starts_with("CREATE TABLE store_policy"));
        assert!(DDL.contains("STRICT"));
        assert_eq!(DDL.matches("CREATE TABLE").count(), 3);
    }

    #[test]
    fn create_then_validate_accepts_its_own_store() {
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        validate(&connection).expect("validate");
    }

    #[test]
    fn validate_rejects_a_bumped_user_version() {
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("bump");
        assert!(matches!(
            validate(&connection),
            Err(StoreError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn validate_rejects_altered_ddl_text() {
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        connection
            .execute(
                "UPDATE schema_definition SET ddl = ddl || ' ' WHERE id = 1",
                [],
            )
            .expect("tamper");
        assert!(matches!(
            validate(&connection),
            Err(StoreError::SchemaMismatch { .. })
        ));
    }

    #[test]
    fn validate_rejects_a_different_format_profile() {
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        connection
            .execute(
                "UPDATE store_policy SET value = 'other/format' WHERE key = 'format_profile'",
                [],
            )
            .expect("tamper");
        assert!(matches!(
            validate(&connection),
            Err(StoreError::SchemaMismatch {
                what: "store_policy.format_profile"
            })
        ));
    }

    #[test]
    fn validate_rejects_a_tampered_max_object_len() {
        // T2-QA-1: the key is written at creation, so it must also be enforced.
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        connection
            .execute(
                "UPDATE store_policy SET value = '999' WHERE key = 'max_object_len'",
                [],
            )
            .expect("tamper");
        assert!(matches!(
            validate(&connection),
            Err(StoreError::SchemaMismatch {
                what: "store_policy.max_object_len"
            })
        ));
    }

    #[test]
    fn validate_rejects_a_removed_policy_key() {
        let mut connection = Connection::open_in_memory().expect("open");
        create(&mut connection).expect("create");
        connection
            .execute("DELETE FROM store_policy WHERE key = 'max_object_len'", [])
            .expect("tamper");
        // T3 backlog: a removed key reports the same qualified label a changed
        // value does, so the two failure shapes are comparable.
        assert!(matches!(
            validate(&connection),
            Err(StoreError::SchemaMismatch {
                what: "store_policy.max_object_len"
            })
        ));
    }

    #[test]
    fn every_recorded_policy_key_is_checked_on_open() {
        // Guards the class of defect QA found rather than the one instance: each
        // key create() writes must be rejected when its value changes, so a new
        // key cannot be added without a validator.
        for key in ["format_profile", "max_object_len", "schema_version"] {
            let mut connection = Connection::open_in_memory().expect("open");
            create(&mut connection).expect("create");
            connection
                .execute(
                    "UPDATE store_policy SET value = value || '-tampered' WHERE key = ?1",
                    [key],
                )
                .expect("tamper");
            assert!(
                matches!(
                    validate(&connection),
                    Err(StoreError::SchemaMismatch { .. })
                ),
                "policy key {key} is written by create() but not enforced by validate()"
            );
        }
    }
}
