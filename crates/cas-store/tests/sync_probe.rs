//! The ADR-008 §5 acceptance: what a batch does on disk, in a checkable event sequence.
//!
//! Three properties, all of them about *absence* and therefore all of them easy to believe
//! without measuring:
//!
//! 1. **no `fsync` inside the index transaction** (KI-7) — the transaction must be able to end
//!    as soon as the rows are in, so the write lock's duration does not depend on object count;
//! 2. **one pack = one file sync + one directory sync + one transaction**, repeated per pack
//!    in order;
//! 3. a batch of already-published objects writes nothing at all.
//!
//! The transaction boundaries come from SQLite's own `trace` callback, not from markers the
//! write path emits itself — a self-reported marker would only prove that the code agrees
//! with the code. The positive control at the bottom arms an injection that makes one
//! `sync_file` happen *inside* the transaction: the same checker must then fail, which is what
//! rules out "the checker is simply not wired up".
//!
//! `COMMIT` itself fsyncs the WAL (`synchronous = FULL`): that is SQLite's own durability
//! work and is not one of our `sync_file`/`sync_directory` events, so it is not a violation.
#![cfg(feature = "sync-probe")]

use std::path::{Path, PathBuf};

use cas_core::{Object, Role};
use cas_store::probe::{self, Event};
use cas_store::{OpenMode, Store};

/// The probe's log is process-global, so tests that reset it and then read it must not run
/// beside each other: libtest runs the tests in a binary in parallel by default. One lock per
/// test keeps "reset → do the batch → read events" an atomic observation.
fn serialised() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cas-store-probe-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn set_pack_max_bytes(store: &Path, bytes: u64) {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    connection
        .execute(
            "UPDATE store_policy SET value = ?1 WHERE key = 'pack_max_bytes'",
            (bytes.to_string(),),
        )
        .expect("update policy");
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    pack_write: usize,
    sync_file: usize,
    sync_dir: usize,
    rename: usize,
    txn_begin: usize,
    txn_commit: usize,
    inserts: usize,
}

impl Counts {
    fn of(events: &[Event]) -> Self {
        let mut counts = Self::default();
        for event in events {
            match event {
                Event::Attempt(_) => {}
                Event::PackWrite => counts.pack_write += 1,
                Event::SyncFile => counts.sync_file += 1,
                Event::SyncDir => counts.sync_dir += 1,
                Event::Rename => counts.rename += 1,
                Event::Sql(sql) => {
                    if event.is_begin() {
                        counts.txn_begin += 1;
                    } else if event.is_commit() {
                        counts.txn_commit += 1;
                    } else if sql.trim_start().to_ascii_uppercase().starts_with("INSERT") {
                        counts.inserts += 1;
                    }
                }
            }
        }
        counts
    }

    fn syncs(&self) -> usize {
        self.sync_file + self.sync_dir
    }
}

/// Checks the event sequence against the per-pack template, returning every violation.
///
/// The events carry no pack id on purpose (the durability primitives do not know one), so the
/// per-pack claims are checked structurally: the five per-pack event kinds must occur equally
/// often, and each pack's block must appear in the documented order. V1–V7 from the acceptance
/// criteria map onto the checks below, each one its own assertion message.
fn violations(events: &[Event]) -> Vec<String> {
    let mut found = Vec::new();
    let events = match published_events(events) {
        Ok(events) => events,
        Err(problems) => return problems,
    };
    let events = events.as_slice();
    let counts = Counts::of(events);

    // V3: transactions must not nest or dangle.
    if counts.txn_begin != counts.txn_commit {
        found.push(format!(
            "V3: {} transaction(s) began but {} committed",
            counts.txn_begin, counts.txn_commit
        ));
    }

    // Walk the log, remembering whether a transaction is open.
    let mut in_transaction = false;
    let mut block: Vec<&Event> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if event.is_begin() {
            // ADR-008 §5 step 3 is `BEGIN IMMEDIATE`: a `DEFERRED` transaction would take a
            // read snapshot first and could later fail with `BUSY_SNAPSHOT` instead of `Busy`.
            if !event.is_begin_immediate() {
                found.push(format!(
                    "V8: transaction at {index} is not BEGIN IMMEDIATE: {:?}",
                    event.sql()
                ));
            }
            if in_transaction {
                found.push(format!("V3: transaction at {index} began inside another"));
            }
            in_transaction = true;
            // V2: the pack's durable steps must all precede the transaction.
            let order: Vec<&str> = block.iter().map(|event| label(event)).collect();
            let expected = ["PackWrite", "SyncFile", "Rename", "SyncDir"];
            if order != expected {
                found.push(format!(
                    "V2: block before transaction at {index} was {order:?}, expected {expected:?}"
                ));
            }
            block.clear();
        } else if event.is_commit() {
            if !in_transaction {
                found.push(format!("V3: commit at {index} with no open transaction"));
            }
            in_transaction = false;
        } else if in_transaction {
            // V1: the main criterion — nothing durable may happen inside the transaction.
            if event.is_durability() {
                found.push(format!(
                    "V1: {} at {index} happened inside a transaction",
                    label(event)
                ));
            }
            // V4: the transaction holds INSERTs only.
            match event.sql() {
                Some(sql) => {
                    let sql = sql.trim_start().to_ascii_uppercase();
                    if !(sql.starts_with("INSERT") || sql.starts_with("BEGIN")) {
                        found.push(format!(
                            "V4: non-INSERT statement inside a transaction at {index}: {sql}"
                        ));
                    }
                }
                None => found.push(format!(
                    "V4: non-SQL event {} inside a transaction at {index}",
                    label(event)
                )),
            }
        } else if event.is_durability() {
            block.push(event);
        }
    }

    // V5/V7: one file sync, one directory sync, one pack write, one transaction per pack.
    let packs = counts.pack_write;
    for (what, seen) in [
        ("SyncFile", counts.sync_file),
        ("SyncDir", counts.sync_dir),
        ("Rename", counts.rename),
        ("TxnBegin", counts.txn_begin),
        ("TxnCommit", counts.txn_commit),
    ] {
        if seen != packs {
            found.push(format!(
                "V5/V7: {seen} {what} event(s) for {packs} pack write(s): \
                 each pack costs exactly one"
            ));
        }
    }
    found
}

fn label(event: &Event) -> &'static str {
    match event {
        Event::Attempt(_) => "Attempt",
        Event::PackWrite => "PackWrite",
        Event::SyncFile => "SyncFile",
        Event::SyncDir => "SyncDir",
        Event::Rename => "Rename",
        Event::Sql(_) => "SQL",
    }
}

/// Splits the log into staging attempts (ADR-002 §7.0).
///
/// A retry writes a fresh staging file, so its events repeat `PackWrite`/`SyncFile` and must
/// not be counted as another pack. An attempt that never reached `Rename` published nothing:
/// its events are dropped from the counts but still checked for the one thing a failed attempt
/// must never do — open a transaction.
fn attempts(events: &[Event]) -> Vec<Vec<Event>> {
    let mut groups: Vec<Vec<Event>> = Vec::new();
    for event in events {
        if matches!(event, Event::Attempt(_)) {
            groups.push(Vec::new());
        } else if let Some(last) = groups.last_mut() {
            last.push(event.clone());
        } else {
            groups.push(vec![event.clone()]);
        }
    }
    groups
}

/// The events of the attempts that published a pack, plus the retry-scope check.
fn published_events(events: &[Event]) -> Result<Vec<Event>, Vec<String>> {
    let mut kept = Vec::new();
    let mut problems = Vec::new();
    for group in attempts(events) {
        let published = group.iter().any(|event| matches!(event, Event::Rename));
        if published {
            kept.extend(group);
        } else if group
            .iter()
            .any(|event| event.is_begin() || event.is_commit())
        {
            problems.push(
                "retry: a failed staging attempt opened a transaction; retries must wrap                  staging only (ADR-002 §7.0)"
                    .to_owned(),
            );
        }
    }
    if problems.is_empty() {
        Ok(kept)
    } else {
        Err(problems)
    }
}

/// Publishes `objects` objects of 1000-byte payloads through one batch.
fn publish(store: &Store, objects: usize) -> cas_store::BatchReport {
    let mut batch = store.batch();
    for index in 0..objects {
        let value = vec![u8::try_from(index % 251).expect("fits"); 1000];
        batch.put(Role::Bytes, &value).expect("put");
    }
    batch.commit().expect("commit")
}

#[test]
fn a_pack_costs_one_sync_and_one_short_transaction() {
    for (objects, expected_packs) in [(2usize, 1usize), (3, 2), (5, 3)] {
        let _guard = serialised();
        let root = TempRoot::new(&format!("counts-{objects}"));
        drop(Store::open(root.store(), OpenMode::Create).expect("open"));
        set_pack_max_bytes(&root.store(), 2400);
        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");

        probe::reset();
        probe::reset_timings();
        let report = publish(&store, objects);
        let events = probe::events();

        assert_eq!(report.packs, expected_packs);
        let counts = Counts::of(&published_events(&events).expect("no retry-scope violation"));
        // V6: a batch that publishes something must have written a pack.
        assert!(counts.pack_write >= 1, "the probe saw no pack write at all");
        assert_eq!(counts.pack_write, expected_packs, "PackWrite");
        assert_eq!(counts.sync_file, expected_packs, "one file fsync per pack");
        assert_eq!(
            counts.sync_dir, expected_packs,
            "one directory fsync per pack"
        );
        assert_eq!(counts.rename, expected_packs, "one rename per pack");
        assert_eq!(counts.txn_begin, expected_packs, "one transaction per pack");
        assert_eq!(
            counts.txn_commit, expected_packs,
            "each transaction commits"
        );
        assert_eq!(
            counts.syncs(),
            2 * expected_packs,
            "syncs per batch = 2 per pack (upper bound too: > 2k is a failure)"
        );
        // The point of packing: fewer fsyncs than objects.
        assert!(
            counts.sync_file < objects,
            "{} file fsyncs for {objects} objects: no batching happened",
            counts.sync_file
        );
        assert!(counts.inserts >= objects, "every new object gets an INSERT");
        assert_eq!(
            violations(&events),
            Vec::<String>::new(),
            "event sequence for {objects} objects"
        );
        // The index rows agree with the arithmetic: k packs, each paying its own tail.
        let mut frame_bytes = 0u64;
        for index in 0..objects {
            let value = vec![u8::try_from(index % 251).expect("fits"); 1000];
            frame_bytes += cas_store::Store::identify(Role::Bytes, &value)
                .map(|_| 1014u64)
                .expect("identify");
        }
        let rows: u64 = {
            let connection =
                rusqlite::Connection::open(root.store().join("store.sqlite")).expect("db");
            connection
                .query_row("SELECT SUM(bytes) FROM pack", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("sum") as u64
        };
        assert_eq!(
            rows,
            frame_bytes + cas_store::PACK_TAIL_BYTES * expected_packs as u64
        );
    }
}

#[test]
fn a_batch_of_published_objects_writes_nothing() {
    let _guard = serialised();
    let root = TempRoot::new("already");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let value = vec![7u8; 500];
    store.put(Role::Bytes, &value).expect("first put");

    probe::reset();
    let mut batch = store.batch();
    batch.put(Role::Bytes, &value).expect("put");
    let report = batch.commit().expect("commit");

    assert_eq!(report.packs, 0);
    assert_eq!(report.already_present, 1);
    let counts = Counts::of(&probe::events());
    assert_eq!(
        counts.pack_write, 0,
        "no pack for an already-present object"
    );
    assert_eq!(counts.syncs(), 0, "no fsync either");
    assert_eq!(counts.txn_begin, 0, "and no transaction at all");
}

#[test]
fn the_positive_control_is_caught_by_the_same_checker() {
    // Without this, "no sync inside the transaction" could simply mean the probe is dead.
    let _guard = serialised();
    let root = TempRoot::new("control");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let scratch = root.store().join("scratch.bin");
    std::fs::write(&scratch, b"scratch").expect("scratch");

    probe::reset();
    probe::inject_sync_inside_next_transaction(&scratch);
    publish(&store, 1);
    let events = probe::events();

    let found = violations(&events);
    assert!(
        found.iter().any(|violation| violation.starts_with("V1:")),
        "the injected in-transaction sync must be reported as V1, got {found:?}"
    );
}
