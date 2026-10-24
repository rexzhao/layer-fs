//! Store behaviour: publication, idempotence, verification, recovery, refusal.
//!
//! Each test owns a fresh directory under the system temp area and removes it
//! on success.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cas_core::{Object, ObjectId, Role};
use cas_store::{CorruptCause, OpenMode, PutOutcome, Store, StoreError};

/// A directory that removes itself when dropped.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cas-store-test-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp root");
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

fn oid_of(role: Role, value: &[u8]) -> ObjectId {
    Object::new(role, value)
        .expect("framed")
        .id()
        .expect("hashed")
}

/// The pack file that holds `oid`, read straight out of the index (ADR-008 §4).
///
/// The tests need to damage a *specific* object's bytes, and a pack id is not derivable from
/// an object id (it carries no content identity), so the index is the only way to ask.
fn pack_path_of(root: &Path, oid: ObjectId) -> PathBuf {
    let connection = rusqlite::Connection::open(root.join("store.sqlite")).expect("db");
    let pack_id: String = connection
        .query_row(
            "SELECT location FROM object_location WHERE oid = ?1",
            [oid.to_string()],
            |row| row.get(0),
        )
        .expect("index row");
    root.join("packs").join(format!("{pack_id}.pack"))
}

/// Every file under `directory`, as absolute paths.
fn files_under(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn open_creates_the_store_shape() {
    let root = TempRoot::new("create");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    assert!(store.opened().created);
    assert_eq!(store.opened().temporaries_removed, 0);
    assert!(root.path().join("store.sqlite").is_file());
    assert!(root.path().join("objects").is_dir());
    assert!(root.path().join("tmp").is_dir());
    assert_eq!(store.object_count().expect("count"), 0);
}

#[test]
fn put_then_get_roundtrips() {
    let root = TempRoot::new("roundtrip");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"hello, store").expect("put");
    assert_eq!(published.outcome, PutOutcome::Inserted);
    assert!(published.outcome.wrote_file());

    let frame = store.get(published.oid).expect("get");
    assert_eq!(frame, published.frame);
    let object = Object::decode(&frame).expect("decode");
    assert_eq!(object.value(), b"hello, store");
    assert_eq!(object.role(), Role::Bytes);
}

#[test]
fn repeated_put_writes_nothing() {
    let root = TempRoot::new("idempotent");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let first = store.put(Role::Bytes, b"same").expect("first put");
    assert_eq!(first.outcome, PutOutcome::Inserted);

    let objects = root.path().join("objects");
    let before = files_under(&objects);
    let second = store.put(Role::Bytes, b"same").expect("second put");
    assert_eq!(second.outcome, PutOutcome::AlreadyPresent);
    assert!(!second.outcome.wrote_file());
    assert_eq!(second.oid, first.oid);
    assert_eq!(files_under(&objects).len(), before.len());
    assert_eq!(store.object_count().expect("count"), 1);
}

#[test]
fn identical_values_under_different_roles_are_separate_objects() {
    let root = TempRoot::new("roles");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let bytes = store.put(Role::Bytes, b"shared").expect("bytes");
    let leaf = store.put(Role::DirectoryLeaf, b"shared").expect("leaf");
    assert_ne!(bytes.oid, leaf.oid);
    assert_eq!(store.object_count().expect("count"), 2);
    assert_eq!(
        Object::decode(&store.get(leaf.oid).expect("get"))
            .expect("decode")
            .role(),
        Role::DirectoryLeaf
    );
}

#[test]
fn exists_reflects_publication() {
    let root = TempRoot::new("exists");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let oid = oid_of(Role::Bytes, b"absent");
    assert!(!store.exists(oid).expect("exists"));
    store.put(Role::Bytes, b"absent").expect("put");
    assert!(store.exists(oid).expect("exists"));
}

#[test]
fn get_reports_a_missing_object() {
    let root = TempRoot::new("missing");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let oid = oid_of(Role::Bytes, b"never-written");
    assert!(matches!(store.get(oid), Err(StoreError::NotFound { .. })));
}

#[test]
fn get_refuses_corrupted_bytes_instead_of_returning_them() {
    let root = TempRoot::new("corrupt");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let published = store.put(Role::Bytes, b"original bytes").expect("put");

    // Flip one payload byte inside the frame (a one-object pack starts its frame at 0):
    // the frame still decodes, but its identity changes.
    let path = pack_path_of(root.path(), published.oid);
    let mut bytes = std::fs::read(&path).expect("read pack");
    // A one-object pack starts its frame at offset 0, so the frame's last byte is inside
    // the body — not the tail, whose corruption `verify` (not `get`) is meant to catch.
    let last = published.frame.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write tampered");

    match store.get(published.oid) {
        Err(StoreError::Corrupt {
            cause: CorruptCause::IdentityMismatch { expected, actual },
            ..
        }) => {
            assert_eq!(expected, published.oid);
            assert_ne!(actual, published.oid);
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
}

#[test]
fn get_refuses_a_truncated_pack_instead_of_zero_filling() {
    let root = TempRoot::new("truncated");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"truncate me").expect("put");
    let path = pack_path_of(root.path(), published.oid);
    let bytes = std::fs::read(&path).expect("read");

    // (a) Losing the tail leaves the frame readable — a read never touches the tail — but
    // `verify` must name the pack: without the tail the file is not a valid pack.
    std::fs::write(&path, &bytes[..bytes.len() - 3]).expect("truncate the tail");
    assert_eq!(
        store.get(published.oid).expect("frame still intact"),
        published.frame
    );
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.pack_failures.len(), 1, "the tail is gone");

    // (b) Truncating into the frame itself is a read failure, never zero-filled bytes.
    std::fs::write(&path, &bytes[..published.frame.len() - 3]).expect("truncate the body");
    assert!(matches!(
        store.get(published.oid),
        Err(StoreError::Io(error))
            if error.kind() == std::io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn get_refuses_an_object_whose_role_row_disagrees() {
    let root = TempRoot::new("role-mismatch");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"role").expect("put");
    // Rewrite the index row as if the object had been stored under another
    // role; the envelope still says Bytes, so the two disagree.
    drop(store);
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    let connection = rusqlite::Connection::open(store.layout().database()).expect("db");
    connection
        .execute(
            "UPDATE object_location SET role = ?1 WHERE oid = ?2",
            (Role::Chunk.tag(), published.oid.to_string()),
        )
        .expect("tamper");
    drop(connection);
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen again");
    assert!(matches!(
        store.get(published.oid),
        Err(StoreError::Corrupt {
            cause: CorruptCause::RoleMismatch { .. },
            ..
        })
    ));
}

#[test]
fn verify_reports_a_clean_store_and_names_a_broken_object() {
    let root = TempRoot::new("verify");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    store.put(Role::Bytes, b"one").expect("put one");
    let two = store.put(Role::Bytes, b"two").expect("put two");

    let clean = store.verify().expect("verify");
    assert!(clean.is_clean());
    assert_eq!(clean.checked, 2);

    let path = pack_path_of(root.path(), two.oid);
    std::fs::write(&path, b"not an object at all").expect("break");
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].oid, two.oid);
}

#[test]
fn verify_does_not_repair_anything() {
    let root = TempRoot::new("verify-readonly");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"leave me").expect("put");
    let path = pack_path_of(root.path(), published.oid);
    let broken = b"garbage".to_vec();
    std::fs::write(&path, &broken).expect("break");

    store.verify().expect("verify");
    assert_eq!(std::fs::read(&path).expect("read"), broken);
    assert!(store.exists(published.oid).expect("exists"));
}

#[test]
fn reopening_keeps_published_objects() {
    let root = TempRoot::new("reopen");
    let oid = {
        let store = Store::open(root.path(), OpenMode::Create).expect("open");
        store.put(Role::Bytes, b"durable").expect("put").oid
    };
    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    assert!(!store.opened().created);
    assert_eq!(
        Object::decode(&store.get(oid).expect("get"))
            .expect("decode")
            .value(),
        b"durable"
    );
}

#[test]
fn opening_clears_stale_temporaries() {
    let root = TempRoot::new("tmp");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let tmp = root.path().join("tmp");
    std::fs::write(tmp.join("dead-1.tmp"), b"half written").expect("stage 1");
    std::fs::write(tmp.join("dead-2.tmp"), b"half written").expect("stage 2");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    assert_eq!(store.opened().temporaries_removed, 2);
    assert_eq!(std::fs::read_dir(&tmp).expect("list").count(), 0);
}

#[test]
fn a_loose_orphan_file_is_invisible_and_is_never_adopted() {
    let root = TempRoot::new("orphan");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let value = b"orphaned";
    let oid = oid_of(Role::Bytes, value);
    let frame = Object::new(Role::Bytes, value).expect("framed").to_vec();

    // Place the file where a crash between rename and index commit leaves it.
    let path = store.layout().object_path(oid);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(&path, &frame).expect("write orphan");
    assert!(!store.exists(oid).expect("exists"));
    assert!(matches!(store.get(oid), Err(StoreError::NotFound { .. })));

    // A later put publishes the object into a pack: loose writes are retired (ADR-008 §3),
    // so the leftover file is no longer adopted — it stays until GC removes it.
    let published = store.put(Role::Bytes, value).expect("put");
    assert_eq!(published.outcome, PutOutcome::Inserted);
    assert_eq!(store.get(oid).expect("get"), frame);
    assert_eq!(store.object_count().expect("count"), 1);
    assert!(path.is_file(), "the leftover loose file is left alone");
}

#[test]
fn opening_refuses_an_unsupported_schema_version() {
    let root = TempRoot::new("version");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let database = root.path().join("store.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("db");
    connection
        .pragma_update(None, "user_version", 99)
        .expect("bump");
    drop(connection);

    match Store::open(root.path(), OpenMode::ReadWrite) {
        Err(StoreError::UnsupportedSchemaVersion { found, supported }) => {
            assert_eq!(found, 99);
            assert_eq!(supported, 3);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
}

#[test]
fn opening_refuses_a_store_whose_recorded_ddl_was_changed() {
    let root = TempRoot::new("ddl");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let database = root.path().join("store.sqlite");
    let connection = rusqlite::Connection::open(&database).expect("db");
    connection
        .execute(
            "UPDATE schema_definition SET ddl = ddl || '-- tampered' WHERE id = 1",
            [],
        )
        .expect("tamper");
    drop(connection);

    assert!(matches!(
        Store::open(root.path(), OpenMode::ReadWrite),
        Err(StoreError::SchemaMismatch { .. })
    ));
}

#[test]
fn opening_refuses_a_store_whose_recorded_policy_was_changed() {
    // T2-QA-1 reproduction: tamper `store_policy.max_object_len` from an
    // external SQLite connection, then reopen read-write. The value is written
    // at creation, so it must be enforced on open like every other recorded
    // constant.
    let root = TempRoot::new("policy");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let connection = rusqlite::Connection::open(root.path().join("store.sqlite")).expect("db");
    connection
        .execute(
            "UPDATE store_policy SET value = '999' WHERE key = 'max_object_len'",
            [],
        )
        .expect("tamper");
    drop(connection);

    match Store::open(root.path(), OpenMode::ReadWrite) {
        Err(StoreError::SchemaMismatch { what }) => {
            assert_eq!(what, "store_policy.max_object_len");
        }
        Err(other) => panic!("expected SchemaMismatch, got {other}"),
        Ok(_) => panic!("opened a store whose recorded max_object_len was changed"),
    }
}

#[test]
fn identify_matches_the_object_identity_without_writing() {
    let root = TempRoot::new("identify");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let oid = Store::identify(Role::Bytes, b"hello").expect("identify");
    assert_eq!(
        oid,
        Object::new(Role::Bytes, b"hello")
            .expect("framed")
            .id()
            .expect("hashed")
    );
    // Identifying is pure: nothing was published.
    assert!(!store.exists(oid).expect("exists"));
    assert_eq!(store.object_count().expect("count"), 0);
}

#[test]
fn identify_preserves_the_put_error_classification() {
    // `identify` runs the same framing checks `put` does, so the error variants
    // must match; a caller may rely on classifying before writing.
    let oversized = vec![0u8; cas_core::MAX_VALUE_LEN + 1];
    assert!(matches!(
        Store::identify(Role::Bytes, &oversized),
        Err(StoreError::TooLarge { .. })
    ));
    let root = TempRoot::new("identify-err");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    assert!(matches!(
        store.put(Role::Bytes, &oversized),
        Err(StoreError::TooLarge { .. })
    ));
}

#[test]
fn get_reports_an_object_whose_pack_disappeared() {
    // The index has rows but the pack is gone: a published object that can no longer be
    // read is a store failure, not a missing object.
    let root = TempRoot::new("vanished");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"soon deleted").expect("put");
    std::fs::remove_file(pack_path_of(root.path(), published.oid)).expect("delete pack");

    // A deleted pack is storage damage with a name (P2b-2d), not an errno.
    match store.get(published.oid) {
        Err(StoreError::MissingPack { pack_id }) => assert!(!pack_id.is_empty()),
        other => panic!("expected MissingPack, got {other:?}"),
    }
    // `verify` reports it twice: the object cannot be read, and its pack cannot be opened.
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].oid, published.oid);
    assert_eq!(report.pack_failures.len(), 1);
    // `exists` still answers from the index: publication happened.
    assert!(store.exists(published.oid).expect("exists"));
}

#[test]
fn create_on_an_existing_store_reuses_it() {
    let root = TempRoot::new("create-twice");
    let oid = {
        let store = Store::open(root.path(), OpenMode::Create).expect("first create");
        assert!(store.opened().created);
        store.put(Role::Bytes, b"kept").expect("put").oid
    };
    let store = Store::open(root.path(), OpenMode::Create).expect("second create");
    assert!(!store.opened().created, "the existing store is reused");
    assert_eq!(
        Object::decode(&store.get(oid).expect("get"))
            .expect("decode")
            .value(),
        b"kept"
    );
    assert_eq!(store.object_count().expect("count"), 1);
}

#[test]
fn verify_reports_a_file_whose_name_is_not_an_object_id() {
    // ADR-002 §3: objects/ holds object files only. A stray name is damaged
    // state; a correctly named orphan is not.
    let root = TempRoot::new("bad-name");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    store.put(Role::Bytes, b"legit").expect("put");

    let objects = root.path().join("objects");
    std::fs::write(objects.join("README.txt"), b"not an object").expect("stray file");
    let nested = objects.join("ab").join("cd");
    std::fs::create_dir_all(&nested).expect("dirs");
    std::fs::write(nested.join("SHORT"), b"x").expect("bad name");

    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert!(report.failures.is_empty(), "published objects are intact");
    let paths: Vec<&str> = report
        .bad_names
        .iter()
        .map(|bad| bad.path.as_str())
        .collect();
    assert_eq!(paths.len(), 2, "both stray names are reported: {paths:?}");
    assert!(paths.contains(&"objects/README.txt"));
    assert!(paths.iter().any(|path| path.ends_with("/SHORT")));
}

#[test]
fn verify_does_not_report_correctly_named_orphans() {
    let root = TempRoot::new("orphan-ok");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let value = b"orphan but well named";
    let oid = oid_of(Role::Bytes, value);
    let path = store.layout().object_path(oid);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(
        &path,
        Object::new(Role::Bytes, value).expect("framed").to_vec(),
    )
    .expect("write orphan");

    let report = store.verify().expect("verify");
    assert!(report.bad_names.is_empty(), "an orphan is not a bad name");
    assert!(report.is_clean(), "and it is not a failure either");
}

#[test]
fn verify_reports_an_uppercase_object_name() {
    // Parsing an id accepts uppercase, so the on-disk rule needs its own check:
    // on a case-insensitive filesystem two spellings would be one file with two
    // identities.
    let root = TempRoot::new("uppercase");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let nested = root.path().join("objects").join("ab").join("cd");
    std::fs::create_dir_all(&nested).expect("dirs");
    let name = "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef";
    std::fs::write(nested.join(name), b"x").expect("write");

    let report = store.verify().expect("verify");
    assert_eq!(report.bad_names.len(), 1);
    assert!(report.bad_names[0].reason.contains("lowercase"));
}

#[test]
fn verify_reports_a_valid_name_in_the_wrong_place() {
    // cov-F1: a correctly named file outside its own prefix directories is as
    // invisible to a lookup as a badly named one, and a collector would never
    // find it. Both the flat and the wrong-prefix cases must be reported.
    let root = TempRoot::new("misplaced");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let objects = root.path().join("objects");
    let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let other = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    // Flat: a valid name directly under objects/.
    std::fs::write(objects.join(oid), b"x").expect("flat");
    // Wrong prefix: valid name under prefix directories its id does not imply.
    let wrong = objects.join("32").join("75").join("deep");
    std::fs::create_dir_all(&wrong).expect("dirs");
    std::fs::write(wrong.join(other), b"x").expect("misplaced");

    let report = store.verify().expect("verify");
    let paths: Vec<&str> = report
        .bad_names
        .iter()
        .map(|bad| bad.path.as_str())
        .collect();
    // Three entries are reported: the flat file, the unexpected deep directory,
    // and the misplaced file inside it. Reporting the directory as well as its
    // contents is deliberate — the directory is not a place this Store writes.
    assert_eq!(paths.len(), 3, "all three are reported: {paths:?}");
    assert!(
        paths.contains(&format!("objects/{oid}").as_str()),
        "{paths:?}"
    );
    assert!(paths.contains(&"objects/32/75/deep"), "{paths:?}");
    assert!(
        paths.iter().any(|path| path.ends_with(other)),
        "the misplaced file is reported: {paths:?}"
    );
    assert!(
        report
            .bad_names
            .iter()
            .any(|bad| bad.reason.contains("prefix directories")),
        "reason names the placement rule"
    );
}

#[test]
fn verify_accepts_a_correctly_placed_object() {
    let root = TempRoot::new("misplaced-ok");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    store.put(Role::Bytes, b"placed").expect("put");
    let report = store.verify().expect("verify");
    assert!(report.is_clean(), "{:?}", report.bad_names);
}

#[test]
fn verify_reports_a_directory_whose_name_is_not_a_prefix() {
    // A directory outside the two-hex-digit rule is not somewhere this Store
    // ever writes, so anything under it is unreachable — report it whether or
    // not it is empty.
    let root = TempRoot::new("bad-dir");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let objects = root.path().join("objects");
    let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    std::fs::create_dir_all(objects.join("junk")).expect("junk dir");
    std::fs::create_dir_all(objects.join("ZZ")).expect("uppercase dir");
    std::fs::write(objects.join("ZZ").join(oid), b"x").expect("nested file");
    std::fs::create_dir_all(objects.join("01").join("QW")).expect("bad depth-2 dir");

    let report = store.verify().expect("verify");
    let paths: Vec<&str> = report
        .bad_names
        .iter()
        .map(|bad| bad.path.as_str())
        .collect();
    assert!(paths.contains(&"objects/junk"), "empty junk dir: {paths:?}");
    assert!(paths.contains(&"objects/ZZ"), "uppercase prefix: {paths:?}");
    assert!(
        paths.contains(&"objects/01/QW"),
        "bad depth-2 dir: {paths:?}"
    );
    assert!(
        report
            .bad_names
            .iter()
            .any(|bad| bad.reason.contains("two lowercase hex digits")),
        "reason names the prefix rule"
    );
}

#[test]
fn verify_reports_a_directory_below_the_prefix_levels() {
    let root = TempRoot::new("deep-dir");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let objects = root.path().join("objects");
    std::fs::create_dir_all(objects.join("ab").join("cd").join("deeper")).expect("dirs");
    let report = store.verify().expect("verify");
    assert_eq!(report.bad_names.len(), 1, "{:?}", report.bad_names);
    assert!(report.bad_names[0]
        .reason
        .contains("unexpected directory below the two prefix levels"));
}

#[test]
fn verify_accepts_the_normal_two_level_layout() {
    let root = TempRoot::new("layout-ok");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    store.put(Role::Bytes, b"one").expect("put");
    store.put(Role::Bytes, b"two").expect("put");
    let report = store.verify().expect("verify");
    assert!(report.is_clean(), "{:?}", report.bad_names);
}

#[test]
fn read_only_open_refuses_writes_but_allows_reads() {
    let root = TempRoot::new("readonly");
    let oid = {
        let store = Store::open(root.path(), OpenMode::Create).expect("open");
        store.put(Role::Bytes, b"read me").expect("put").oid
    };
    let store = Store::open(root.path(), OpenMode::ReadOnly).expect("readonly open");
    assert!(store.is_read_only());
    assert!(store.exists(oid).expect("exists"));
    store.get(oid).expect("read");
    assert!(matches!(
        store.put(Role::Bytes, b"write me"),
        Err(StoreError::ReadOnly)
    ));
}

#[test]
fn read_only_open_does_not_clear_temporaries() {
    let root = TempRoot::new("readonly-tmp");
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    let staged = root.path().join("tmp").join("still-here.tmp");
    std::fs::write(&staged, b"untouched").expect("stage");
    let store = Store::open(root.path(), OpenMode::ReadOnly).expect("readonly");
    assert_eq!(store.opened().temporaries_removed, 0);
    assert!(staged.is_file());
}

#[test]
fn read_only_open_of_a_missing_store_fails() {
    let root = TempRoot::new("absent");
    let missing = root.path().join("no-such-store");
    assert!(matches!(
        Store::open(&missing, OpenMode::ReadOnly),
        Err(StoreError::Io(_))
    ));
    assert!(!missing.exists());
}

#[test]
fn an_object_beyond_the_format_limit_is_refused() {
    let root = TempRoot::new("too-large");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let value = vec![0u8; cas_core::MAX_VALUE_LEN + 1];
    assert!(matches!(
        store.put(Role::Bytes, &value),
        Err(StoreError::TooLarge { .. })
    ));
    assert_eq!(store.object_count().expect("count"), 0);
}

#[test]
fn concurrent_puts_of_one_object_store_it_once() {
    let root = TempRoot::new("concurrent");
    let value = b"contended value";
    // Create the Store first; the racing writers below open it read-write.
    drop(Store::open(root.path(), OpenMode::Create).expect("create"));
    // One Store handle per process is the supported model (a `Store` owns one
    // SQLite connection, which is neither `Send` nor `Sync`), so contention is
    // exercised with one handle per thread, each opening the same files.
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = root.path();
                scope.spawn(move || match Store::open(root, OpenMode::ReadWrite) {
                    Ok(store) => store.put(Role::Bytes, value),
                    Err(error) => Err(error),
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let mut published_any = false;
    for outcome in outcomes {
        match outcome {
            Ok(published) => {
                // Every writer that succeeded names the same object.
                assert_eq!(published.oid, oid_of(Role::Bytes, value));
                published_any = true;
            }
            // A concurrent writer holding the write lane is a typed refusal,
            // never a retry (ADR-002 §8).
            Err(StoreError::Busy) => {}
            Err(other) => panic!("unexpected failure: {other:?}"),
        }
    }
    assert!(published_any, "at least one writer publishes");

    let store = Store::open(root.path(), OpenMode::ReadWrite).expect("reopen");
    assert_eq!(store.object_count().expect("count"), 1);
    // One index row wins; a loser's frame may sit in its own unreferenced pack for GC.
    assert!(!files_under(&root.path().join("packs")).is_empty());
    let oid = oid_of(Role::Bytes, value);
    Object::decode(&store.get(oid).expect("get")).expect("decode");
}

#[test]
fn every_put_lands_in_a_pack_and_leaves_objects_empty() {
    // ADR-008 §3: the write path is packs only. Each `put` is a batch of one, so 32 puts
    // make 32 one-frame packs and `objects/` stays untouched (loose is read-only shape).
    let root = TempRoot::new("many");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    for index in 0..32u32 {
        store.put(Role::Bytes, &index.to_be_bytes()).expect("put");
    }
    assert_eq!(store.object_count().expect("count"), 32);
    assert_eq!(files_under(&root.path().join("objects")).len(), 0);
    assert_eq!(files_under(&root.path().join("packs")).len(), 32);
    let report = store.verify().expect("verify");
    assert!(report.is_clean());
    assert_eq!(report.packs_checked, 32);
}

/// **ADR-006 §11.1 (F2)** — `open_with` waits a bounded time, then reports `Busy`.
///
/// `Store::open` keeps ADR-002 §8's "no busy handler"; this is the opt-in that ADR-006
/// §11.1 added for the one caller that shares the main database between workspaces. The
/// wait must be SQLite waiting inside the statement: the object is not published and the
/// error type is the same `Busy` a zero-timeout store reports.
#[test]
fn open_with_a_bounded_wait_reports_busy_after_roughly_that_long() {
    use std::time::{Duration, Instant};

    use cas_store::StoreOptions;

    let root = TempRoot::new("bounded-wait");
    let first = Store::open(root.path(), OpenMode::Create).expect("create");
    first.put(Role::Bytes, b"before").expect("put");
    drop(first);

    // A second connection takes the write lane and holds it longer than the wait.
    let blocker =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lane");

    let waiter = Store::open_with(
        root.path(),
        OpenMode::ReadWrite,
        StoreOptions {
            busy_timeout: Duration::from_millis(200),
        },
    )
    .expect("open with a wait");

    let started = Instant::now();
    let outcome = waiter.put(Role::Bytes, b"during");
    let waited = started.elapsed();
    let _ = blocker.execute_batch("ROLLBACK");

    match outcome {
        Err(StoreError::Busy) => {}
        other => panic!("expected Busy, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_millis(150),
        "the wait must be the configured ~200 ms, measured {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "the wait must be bounded, measured {waited:?}"
    );
    // Nothing was published: a waited-for lock still ends in a typed failure, not a
    // half-applied write.
    let during = oid_of(Role::Bytes, b"during");
    assert!(matches!(
        waiter.get(during),
        Err(StoreError::NotFound { .. })
    ));
}

/// **ADR-002 §8 unchanged** — the default has no busy handler at all.
#[test]
fn the_default_open_does_not_wait() {
    use std::time::{Duration, Instant};

    let root = TempRoot::new("no-wait");
    let first = Store::open(root.path(), OpenMode::Create).expect("create");
    first.put(Role::Bytes, b"before").expect("put");
    drop(first);

    let blocker =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lane");
    let waiter = Store::open(root.path(), OpenMode::ReadWrite).expect("open");

    let started = Instant::now();
    let outcome = waiter.put(Role::Bytes, b"during");
    let waited = started.elapsed();
    let _ = blocker.execute_batch("ROLLBACK");

    assert!(matches!(outcome, Err(StoreError::Busy)), "got {outcome:?}");
    assert!(
        waited < Duration::from_millis(150),
        "the default must not wait, measured {waited:?}"
    );
}
