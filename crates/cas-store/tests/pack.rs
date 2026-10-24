//! Pack files: round trip, one-frame reads, tail verification, splitting (ADR-008 §3, §7).
//!
//! The assertions here are the ones a pack can get wrong silently: a read that happens to
//! work because the whole file was slurped, a tail nobody validates, or a size limit that
//! does not actually split. The sync *counts* per pack live in `sync_probe.rs`, because that
//! needs the probe feature; the geometry and read-cost claims live here.

use std::path::{Path, PathBuf};

use cas_core::{Object, Role};
use cas_store::{OpenMode, Store};

/// The pack tail: magic 4 + count 4 + frames_len 8 + BLAKE3 32 (ADR-008 §3).
///
/// Not 44: the `count` field is part of the format, and `verify` uses it.
const TAIL_BYTES: usize = 48;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cas-store-pack-{label}-{}-{}",
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

/// The canonical frame length of a bytes object carrying `value` (header + payload).
fn frame_len(value: &[u8]) -> usize {
    Object::new(Role::Bytes, value)
        .expect("framed")
        .to_vec()
        .len()
}

/// The value a published object decodes to.
fn value_of(store: &Store, oid: cas_core::ObjectId) -> Vec<u8> {
    let frame = store.get(oid).expect("get");
    Object::decode(&frame).expect("decode").value().to_vec()
}

/// A payload of exactly `len` bytes, filled from `index`.
fn payload(index: u8, len: usize) -> Vec<u8> {
    vec![index; len]
}

/// `(pack_id, bytes, objects)` rows, in id order.
fn pack_rows(store: &Path) -> Vec<(String, u64, u64)> {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    let mut statement = connection
        .prepare("SELECT pack_id, bytes, objects FROM pack ORDER BY pack_id")
        .expect("prepare");
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        })
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

/// Rewrites `store_policy.pack_max_bytes`, so a test can force splits without 64 MiB of data.
fn set_pack_max_bytes(store: &Path, bytes: u64) {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    connection
        .execute(
            "UPDATE store_policy SET value = ?1 WHERE key = 'pack_max_bytes'",
            (bytes.to_string(),),
        )
        .expect("update policy");
}

#[test]
fn a_batch_lands_in_one_pack_and_each_object_reads_back_byte_for_byte() {
    let root = TempRoot::new("roundtrip");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let first = payload(1, 1000);
    let second = payload(2, 1000);

    let mut batch = store.batch();
    let first_oid = batch.put(Role::Bytes, &first).expect("put");
    let second_oid = batch.put(Role::Bytes, &second).expect("put");
    let report = batch.commit().expect("commit");
    assert_eq!(report.new_objects, 2);
    assert_eq!(report.packs, 1, "two small objects share one pack");

    assert_eq!(value_of(&store, first_oid), first);
    assert_eq!(value_of(&store, second_oid), second);

    let rows = pack_rows(&root.store());
    assert_eq!(rows.len(), 1);
    let (_, bytes, objects) = &rows[0];
    assert_eq!(*objects, 2, "pack.objects counts the frames it carries");
    // `bytes` is the file size: the frames plus this pack's one tail.
    assert_eq!(
        *bytes as usize,
        frame_len(&first) + frame_len(&second) + TAIL_BYTES,
        "pack.bytes is the whole file"
    );

    // Loose files are the retired shape: nothing may be written there.
    let objects_dir = root.store().join("objects");
    let loose = std::fs::read_dir(&objects_dir).map_or(0, |entries| entries.count());
    assert_eq!(loose, 0, "the write path is packs only");
    assert!(store.verify().expect("verify").is_clean());
}

#[test]
fn reading_one_object_reads_exactly_one_frame() {
    let root = TempRoot::new("one-frame");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");

    let mut batch = store.batch();
    let small = batch.put(Role::Bytes, &payload(7, 100)).expect("put");
    let large = batch.put(Role::Bytes, &payload(8, 4096)).expect("put");
    batch.commit().expect("commit");

    // The claim is "a single-object read does not touch the tail or the other frames"
    // (ADR-008 §3). The counter is incremented by the only code that reads a pack, so a
    // regression that slurps the file shows up as a number, not as a feeling.
    cas_store::testing::reset_pack_read_bytes();
    let frame = store.get(small).expect("get");
    assert_eq!(
        cas_store::testing::pack_read_bytes(),
        frame.len() as u64,
        "one seek, one frame, nothing else"
    );

    cas_store::testing::reset_pack_read_bytes();
    let frame = store.get(large).expect("get");
    assert_eq!(cas_store::testing::pack_read_bytes(), frame.len() as u64);
}

#[test]
fn verify_names_a_pack_whose_tail_no_longer_hashes() {
    let root = TempRoot::new("tail");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, &payload(3, 500)).expect("put");

    let pack_path = {
        let rows = pack_rows(&root.store());
        root.store()
            .join("packs")
            .join(format!("{}.pack", rows[0].0))
    };
    let mut bytes = std::fs::read(&pack_path).expect("read pack");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&pack_path, &bytes).expect("corrupt tail");

    // The frame is intact, so a read still works: only `verify` looks at the tail.
    assert_eq!(store.get(published.oid).expect("get"), published.frame);

    let report = store.verify().expect("verify");
    assert!(!report.is_clean(), "a broken tail must be reported");
    assert_eq!(report.pack_failures.len(), 1);
    assert!(
        report.pack_failures[0].reason.contains("BLAKE3"),
        "the reason names what failed: {}",
        report.pack_failures[0].reason
    );
}

#[test]
fn pack_max_bytes_splits_the_batch_where_the_arithmetic_says() {
    // Frames are 1014 bytes (1000 + the 14-byte envelope), so a 2400-byte limit fits two
    // frames per pack: 2 objects -> 1 pack, 3 -> 2 packs, 5 -> 3 packs.
    let cases = [(2usize, 1usize), (3, 2), (5, 3)];
    for (objects, expected_packs) in cases {
        let root = TempRoot::new(&format!("split-{objects}"));
        drop(Store::open(root.store(), OpenMode::Create).expect("open"));
        set_pack_max_bytes(&root.store(), 2400);
        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
        assert_eq!(store.pack_max_bytes(), 2400);

        let payloads: Vec<Vec<u8>> = (0..objects)
            .map(|index| payload(u8::try_from(index).expect("fits"), 1000))
            .collect();
        let mut frame_bytes = 0usize;
        let mut batch = store.batch();
        for value in &payloads {
            batch.put(Role::Bytes, value).expect("put");
            frame_bytes += frame_len(value);
        }
        let report = batch.commit().expect("commit");
        assert_eq!(
            report.packs, expected_packs,
            "{objects} objects of 1000 bytes with a 2400-byte limit"
        );

        let rows = pack_rows(&root.store());
        assert_eq!(rows.len(), expected_packs);
        let objects_written: u64 = rows.iter().map(|(_, _, objects)| objects).sum();
        assert_eq!(
            objects_written, objects as u64,
            "every object has a pack row"
        );
        let bytes: u64 = rows.iter().map(|(_, bytes, _)| bytes).sum();
        assert_eq!(
            bytes as usize,
            frame_bytes + TAIL_BYTES * expected_packs,
            "each pack pays its own 48-byte tail"
        );

        // Splitting must not lose or duplicate an object.
        assert_eq!(store.object_count().expect("count"), objects as u64);
        for value in &payloads {
            let oid = Store::identify(Role::Bytes, value).expect("identify");
            assert_eq!(&value_of(&store, oid), value);
        }
        assert!(store.verify().expect("verify").is_clean());
    }
}

#[test]
fn a_repeated_object_is_written_once_and_a_second_batch_is_a_no_op() {
    let root = TempRoot::new("dedupe");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let value = payload(9, 700);

    let mut batch = store.batch();
    let first = batch.put(Role::Bytes, &value).expect("put");
    let again = batch.put(Role::Bytes, &value).expect("put");
    assert_eq!(first, again);
    assert_eq!(batch.len(), 1, "in-batch dedupe keeps one frame");
    assert_eq!(batch.commit().expect("commit").new_objects, 1);

    // Same object through a fresh batch: the index already has it, so nothing is written.
    let mut batch = store.batch();
    batch.put(Role::Bytes, &value).expect("put");
    let report = batch.commit().expect("commit");
    assert_eq!(report.new_objects, 0);
    assert_eq!(report.already_present, 1);
    assert_eq!(
        report.packs, 0,
        "an AlreadyPresent-only batch writes no pack"
    );
    assert_eq!(pack_rows(&root.store()).len(), 1);
    assert_eq!(store.object_count().expect("count"), 1);
}
