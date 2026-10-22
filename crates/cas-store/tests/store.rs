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

    // Flip one payload byte: the frame still decodes, but its identity changes.
    let path = store.layout().object_path(published.oid);
    let mut bytes = std::fs::read(&path).expect("read object");
    let last = bytes.len() - 1;
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
fn get_refuses_a_truncated_object_file() {
    let root = TempRoot::new("truncated");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, b"truncate me").expect("put");
    let path = store.layout().object_path(published.oid);
    let bytes = std::fs::read(&path).expect("read");
    std::fs::write(&path, &bytes[..bytes.len() - 3]).expect("truncate");

    assert!(matches!(
        store.get(published.oid),
        Err(StoreError::Corrupt {
            cause: CorruptCause::LengthMismatch { .. },
            ..
        })
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

    let path = store.layout().object_path(two.oid);
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
    let path = store.layout().object_path(published.oid);
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
fn an_orphan_object_file_is_invisible_until_it_is_published() {
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

    // A later put adopts it: the file is reused, not rewritten.
    let published = store.put(Role::Bytes, value).expect("put");
    assert_eq!(published.outcome, PutOutcome::AdoptedOrphan);
    assert_eq!(store.get(oid).expect("get"), frame);
    assert_eq!(store.object_count().expect("count"), 1);
}

#[test]
fn a_put_refuses_a_corrupt_orphan_instead_of_overwriting_it() {
    let root = TempRoot::new("corrupt-orphan");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    let value = b"expected bytes";
    let oid = oid_of(Role::Bytes, value);
    let path = store.layout().object_path(oid);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(&path, b"a different length entirely").expect("write bad orphan");

    assert!(matches!(
        store.put(Role::Bytes, value),
        Err(StoreError::Corrupt { .. })
    ));
    // The bad file is still there; nothing was silently replaced.
    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"a different length entirely"
    );
    assert!(!store.exists(oid).expect("exists"));
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
            assert_eq!(supported, 1);
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
    assert_eq!(files_under(&root.path().join("objects")).len(), 1);
    let oid = oid_of(Role::Bytes, value);
    Object::decode(&store.get(oid).expect("get")).expect("decode");
}

#[test]
fn different_objects_keep_separate_files_under_distinct_directories() {
    let root = TempRoot::new("many");
    let store = Store::open(root.path(), OpenMode::Create).expect("open");
    for index in 0..32u32 {
        store.put(Role::Bytes, &index.to_be_bytes()).expect("put");
    }
    assert_eq!(store.object_count().expect("count"), 32);
    assert_eq!(files_under(&root.path().join("objects")).len(), 32);
    assert!(store.verify().expect("verify").is_clean());
}
