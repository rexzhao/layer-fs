//! Pack files: round trip, one-frame reads, tail verification, splitting (ADR-008 §3, §7).
//!
//! The assertions here are the ones a pack can get wrong silently: a read that happens to
//! work because the whole file was slurped, a tail nobody validates, or a size limit that
//! does not actually split. The sync *counts* per pack live in `sync_probe.rs`, because that
//! needs the probe feature; the geometry and read-cost claims live here.

use std::path::{Path, PathBuf};

use cas_core::{Object, Role};
use cas_store::{OpenMode, Store};

// The pack tail (magic 4 + count 4 + frames_len 8 + BLAKE3 32, ADR-008 §3) comes from the crate's exported
// constant; this file deliberately has no local one (card §5), and "not 44" is asserted once, in
// `the_pack_tail_is_forty_eight_bytes`.

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

/// The pack file that holds `oid`.
fn pack_path_of_pack(store: &Path, oid: &cas_core::ObjectId) -> PathBuf {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    let pack_id: String = connection
        .query_row(
            "SELECT location FROM object_location WHERE oid = ?1",
            [oid.to_string()],
            |row| row.get(0),
        )
        .expect("index row");
    store.join("packs").join(format!("{pack_id}.pack"))
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
        frame_len(&first)
            + frame_len(&second)
            + usize::try_from(cas_store::PACK_TAIL_BYTES).expect("fits"),
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
    // The read-byte counter is process-global, so this observation must not race another test
    // that also reads packs (libtest runs tests in parallel).
    let _guard = cas_store::testing::serialised();
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
    // Frames are 1014 bytes (1000 + the 14-byte envelope). `pack_max_bytes` bounds the *frame
    // body*; the 48-byte tail is not counted (ADR-008 §3), so the file may be 48 bytes larger.
    let cases = [
        (2usize, 1usize, 2400u64), // 2 028 <= 2 400 -> one pack
        (2, 1, 2028),              // exactly the limit -> still one pack ("at most")
        (3, 2, 2400),              // 3 042 > 2 400 -> two packs
        (5, 3, 2400),              // 5 070 > 2 400 -> three packs
    ];
    for (objects, expected_packs, limit) in cases {
        let root = TempRoot::new(&format!("split-{objects}"));
        drop(Store::open(root.store(), OpenMode::Create).expect("open"));
        set_pack_max_bytes(&root.store(), limit);
        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
        assert_eq!(store.pack_max_bytes(), limit);

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
            "{objects} objects of 1000 bytes with a {limit}-byte limit"
        );

        let rows = pack_rows(&root.store());
        assert_eq!(rows.len(), expected_packs);
        let objects_written: u64 = rows.iter().map(|(_, _, objects)| objects).sum();
        assert_eq!(
            objects_written, objects as u64,
            "every object has a pack row"
        );
        let tail = cas_store::PACK_TAIL_BYTES;
        let bytes: u64 = rows.iter().map(|(_, bytes, _)| bytes).sum();
        assert_eq!(
            bytes as u64,
            u64::try_from(frame_bytes + usize::try_from(tail).expect("fits") * expected_packs)
                .expect("fits"),
            "each pack pays its own tail (outside pack_max_bytes)"
        );
        // The limit bounds the frames, not the file: every pack's body is at most `limit`.
        for (_, bytes, _) in &rows {
            assert!(
                *bytes <= limit + tail,
                "pack of {bytes} bytes exceeds {limit} + the tail"
            );
        }

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

// ----------------------------------------------------------------- KI-10: verify's shape rules
//
// `verify` must accept exactly what ADR-008 §5/§7 allow — dead frames and therefore holes —
// and reject only genuine damage. Asserting that here is what keeps a future tightening of the
// checker from turning healthy stores into "corrupt" reports (KI-10).

/// `(oid, length, offset)` for one pack, in offset order.
fn rows_of(store: &Path, pack_id: &str) -> Vec<(String, i64, i64)> {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    let mut statement = connection
        .prepare(
            "SELECT oid, length, offset FROM object_location
             WHERE kind = 1 AND location = ?1 ORDER BY offset",
        )
        .expect("prepare");
    statement
        .query_map([pack_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

fn update_row(store: &Path, oid: &str, offset: i64, length: i64) {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    connection
        .execute(
            "UPDATE object_location SET offset = ?2, length = ?3 WHERE oid = ?1",
            (oid, offset, length),
        )
        .expect("update row");
}

/// The pack with one 2-frame body, plus the ids of its two objects.
fn two_frame_pack(label: &str) -> (TempRoot, Store, String, Vec<(String, i64, i64)>) {
    let root = TempRoot::new(label);
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let mut batch = store.batch();
    batch.put(Role::Bytes, &payload(1, 1000)).expect("put");
    batch.put(Role::Bytes, &payload(2, 1000)).expect("put");
    batch.commit().expect("commit");
    let pack_id = pack_rows(&root.store())[0].0.clone();
    let rows = rows_of(&root.store(), &pack_id);
    assert_eq!(rows.len(), 2);
    (root, store, pack_id, rows)
}

#[test]
fn verify_accepts_a_hole_left_by_a_dead_frame() {
    let (root, store, pack_id, rows) = two_frame_pack("hole");
    // Drop the second row: this is exactly what `INSERT OR IGNORE` leaves behind when another
    // workspace published that object first — a dead frame, hence a hole the index does not
    // cover. ADR-008 §5 says it is legal; GC's repack is what reclaims it.
    let connection = rusqlite::Connection::open(root.store().join("store.sqlite")).expect("db");
    connection
        .execute("DELETE FROM object_location WHERE oid = ?1", (&rows[1].0,))
        .expect("drop the losing row");
    let report = store.verify().expect("verify");
    assert!(
        report.is_clean(),
        "a hole is legal (KI-10), got {report:?} for pack {pack_id}"
    );
    assert_eq!(report.checked, 1, "one row survived");
}

#[test]
fn verify_rejects_overlapping_rows() {
    let (root, store, _pack_id, rows) = two_frame_pack("overlap");
    update_row(&root.store(), &rows[1].0, 500, 100);
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.pack_failures.len(), 1);
    assert!(
        report.pack_failures[0].reason.contains("inside the frame"),
        "{}",
        report.pack_failures[0].reason
    );
}

#[test]
fn verify_rejects_a_row_that_runs_past_the_frames() {
    let (root, store, _pack_id, rows) = two_frame_pack("past");
    let frames_len = 2 * 1014i64;
    update_row(&root.store(), &rows[1].0, frames_len - 10, 100);
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert!(
        report.pack_failures[0].reason.contains("past the pack's"),
        "{}",
        report.pack_failures[0].reason
    );
}

#[test]
fn verify_rejects_a_zero_length_row() {
    let (root, store, _pack_id, rows) = two_frame_pack("zero");
    update_row(&root.store(), &rows[1].0, rows[1].2, 0);
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert!(
        report.pack_failures[0].reason.contains("non-positive"),
        "{}",
        report.pack_failures[0].reason
    );
}

#[test]
fn verify_uses_the_tail_count_as_a_necessary_condition() {
    // The tail's `count` records physical frames, so it may exceed the row count (dead
    // frames) but never fall below it. The tail's BLAKE3 covers the frames only, so the count
    // can be edited without invalidating the checksum — which makes this a cheap, honest test.
    let (root, store, pack_id, _rows) = two_frame_pack("count");

    let path = root.store().join("packs").join(format!("{pack_id}.pack"));
    let mut bytes = std::fs::read(&path).expect("read");
    let count_at = bytes.len() - usize::try_from(cas_store::PACK_TAIL_BYTES).expect("fits") + 4;
    bytes[count_at..count_at + 4].copy_from_slice(&3u32.to_le_bytes());
    std::fs::write(&path, &bytes).expect("widen count");
    assert!(
        store.verify().expect("verify").is_clean(),
        "more physical frames than rows is legal (dead frames)"
    );

    bytes[count_at..count_at + 4].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &bytes).expect("shrink count");
    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert!(
        report.pack_failures[0]
            .reason
            .contains("tail declares 1 frames, the index has 2 rows"),
        "{}",
        report.pack_failures[0].reason
    );
}

// ------------------------------------------------------- ADR-008 §7: a missing pack is damage
//
// Deleting a pack file is the one *legitimate* way to simulate damage: the index keeps its
// rows, so the store claims bytes it does not have. This must surface as a typed
// `MissingPack { pack_id }` — never as `Io(os error 2)` and never as "object not found",
// because those send a caller looking for the wrong thing.

/// Deletes the pack that holds `oid`, leaving the index alone.
fn delete_pack_of(store: &Path, oid: &cas_core::ObjectId) -> String {
    let connection = rusqlite::Connection::open(store.join("store.sqlite")).expect("db");
    let pack_id: String = connection
        .query_row(
            "SELECT location FROM object_location WHERE oid = ?1",
            [oid.to_string()],
            |row| row.get(0),
        )
        .expect("index row");
    std::fs::remove_file(store.join("packs").join(format!("{pack_id}.pack"))).expect("delete pack");
    pack_id
}

#[test]
fn a_missing_pack_is_reported_as_damage_with_its_id() {
    let root = TempRoot::new("missing-pack");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, &payload(4, 400)).expect("put");
    let pack_id = delete_pack_of(&root.store(), &published.oid);

    match store.get(published.oid) {
        Err(cas_store::StoreError::MissingPack { pack_id: named }) => {
            assert_eq!(
                named, pack_id,
                "the error names the pack, so it can be found"
            )
        }
        other => panic!("a deleted pack is storage damage, got {other:?}"),
    }

    let report = store.verify().expect("verify");
    assert!(!report.is_clean());
    assert_eq!(report.missing_packs, 1, "the summary counts missing packs");
    assert_eq!(report.pack_failures.len(), 1);
    assert!(
        report.pack_failures[0]
            .reason
            .contains("storage damage, not a missing object"),
        "the message must say what kind of failure this is: {}",
        report.pack_failures[0].reason
    );
    // The object-level failure is reported too, and it is the same damage — not `NotFound`.
    assert_eq!(report.failures.len(), 1);
    assert!(
        report.failures[0].reason.contains(&pack_id),
        "the object failure points at the pack: {}",
        report.failures[0].reason
    );
}

#[test]
fn a_damaged_pack_is_not_counted_as_missing() {
    let root = TempRoot::new("damaged-not-missing");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    store.put(Role::Bytes, &payload(5, 400)).expect("put");
    let path = {
        let rows = pack_rows(&root.store());
        root.store()
            .join("packs")
            .join(format!("{}.pack", rows[0].0))
    };
    let mut bytes = std::fs::read(&path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("corrupt tail");

    let report = store.verify().expect("verify");
    assert_eq!(report.pack_failures.len(), 1);
    assert_eq!(
        report.missing_packs, 0,
        "a corrupt pack is present; only an absent one is missing"
    );
}

// ------------------------------------------------- ADR-008 §5.2: the publication pin
//
// A pack is renamed into `packs/` before its index rows exist. An external deleter can remove
// it inside that window (that is exactly what KI-11 observed), so the write path holds the file
// open without `FILE_SHARE_DELETE` until the commit returns.

/// While a pack is pinned, another attempt to delete it fails; readers still work.
#[cfg(windows)]
#[test]
fn a_pinned_pack_cannot_be_deleted_but_can_be_read() {
    let root = TempRoot::new("pin");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");
    let published = store.put(Role::Bytes, &payload(6, 300)).expect("put");
    let path = pack_path_of_pack(&root.store(), &published.oid);

    let pin = cas_store::durability_pin_for_test(&path).expect("pin");
    let delete = std::fs::remove_file(&path);
    assert_eq!(
        delete
            .expect_err("a pinned file must not be deletable")
            .raw_os_error(),
        Some(32),
        "ERROR_SHARING_VIOLATION (32) is the expected refusal"
    );
    // The pin denies deletion only; reading keeps working.
    assert_eq!(
        store.get(published.oid).expect("get while pinned"),
        published.frame
    );
    drop(pin);
    std::fs::remove_file(&path).expect("after the pin is dropped the file can be deleted");
}

/// ADR-008 §5.2: if the pack is gone between its rename and the pin, the publication fails as
/// `MissingPack` and commits **nothing** — the file is left as an unreferenced pack for GC.
///
/// Driven by `fail_next_pin()` (staging-audit only), which is the deterministic form of "the
/// external deleter won the race". The important half is the second: no index row may exist,
/// because a row pointing at a pack that is not there is exactly the damage P2b-2d names.
#[cfg(feature = "staging-audit")]
#[test]
fn a_failed_pin_fails_the_publication_and_commits_no_rows() {
    let root = TempRoot::new("pin-fail");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");

    cas_store::fail_next_pin();
    let mut batch = store.batch();
    let value = payload(11, 200);
    batch.put(Role::Bytes, &value).expect("put");
    match batch.commit() {
        Err(cas_store::StoreError::MissingPack { pack_id }) => assert!(!pack_id.is_empty()),
        other => panic!("a lost pack must surface as MissingPack, got {other:?}"),
    }

    assert_eq!(
        store.object_count().expect("count"),
        0,
        "nothing was committed"
    );
    assert_eq!(pack_rows(&root.store()).len(), 0, "no pack row either");
    // The bytes are on disk but nameless: an orphan, which is GC's business (ADR-008 §5.1).
    let packs = root.store().join("packs");
    let present = std::fs::read_dir(&packs)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".pack"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        present, 1,
        "the renamed file stays behind as an unreferenced pack"
    );
    assert!(
        store.verify().expect("verify").is_clean(),
        "an unreferenced pack is not a failure — it is not published"
    );
}

/// The pack tail is **48 bytes**, defined once (card §5).
///
/// This is the crate's single tail-size assertion: every other use site names the exported constant, so a change
/// to the format shows up here as one failing test instead of as a scatter of magic numbers that might each be
/// "the other 48" (a frame length, an object header, …).
#[test]
// The subject *is* a constant: clippy's "assertion has a constant value" is exactly the point here, so the lint
// is allowed locally rather than worked around (a helper variable would hide the number from the grep the card
// uses for verification).
#[allow(clippy::assertions_on_constants)]
fn the_pack_tail_is_forty_eight_bytes() {
    // One line, one assertion (criterion 3 greps the source and requires exactly one hit).
    assert!(cas_store::PACK_TAIL_BYTES == 48, "cell constant: tail size");
    assert_eq!(
        4 + 4 + 8 + 32,
        48,
        "cell composition: magic + count + frames_len + BLAKE3 (ADR-008 §3)"
    );
}
