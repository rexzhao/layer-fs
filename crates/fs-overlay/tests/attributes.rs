//! `set_attributes` / `attributes` on the overlay (P3-2 G3): the materialise path and its refusals.
//!
//! Three claims, each with a control, because all three have an implementation that "looks right":
//!
//! - a **base** inode gets an override row on first write, and the row keeps every base field except the one being set
//!   (an INSERT that forgot `size`/`created_ns` would silently rewrite the entry);
//! - a second write **does not** INSERT again (a row count, not a guess: "it still reads correctly" is compatible with
//!   duplicate rows);
//! - a **derived** bit is refused **and** creates no row — a refusal that materialised first would leave a half-created
//!   override behind, which nothing else in the system would ever clean up.

use fs_namespace::{FileId, TreeRead, TreeStore};
use fs_overlay::{OverlayOptions, OverlayStore};

fn base_store(dir: &std::path::Path) -> (cas_store::Store, cas_core::ObjectId) {
    let store =
        cas_store::Store::open(dir.join("store"), cas_store::OpenMode::Create).expect("store");
    struct Sink<'a>(&'a cas_store::Store);
    impl fs_snapshot::PageSink for Sink<'_> {
        fn put(
            &mut self,
            role: cas_core::Role,
            value: &[u8],
        ) -> fs_snapshot::SnapshotResult<cas_core::ObjectId> {
            Ok(self.0.put(role, value).expect("put").oid)
        }
    }
    let profile = fs_snapshot::Profile::V2;
    let mut sink = Sink(&store);
    // One file inode in the base, so `set_attributes` has a base-layer entry to materialise.
    let table = fs_snapshot::encode_inode_table(
        &profile,
        std::iter::once(fs_snapshot::InodeRecord {
            serial: 1,
            kind: fs_snapshot::Kind::File,
            size: 4096,
            created_ns: 1_000,
            modified_ns: 2_000,
            accessed_ns: 0,
            changed_ns: 0,
            attributes: 0,
            security: Vec::new(),
            content: fs_snapshot::Content::Empty,
        }),
        &mut sink,
    )
    .expect("table");
    let root = fs_snapshot::encode_root(
        &profile,
        &fs_snapshot::RootValue {
            scope_id: [3u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("root");
    (store, root)
}

fn overlay(label: &str) -> (std::path::PathBuf, OverlayStore) {
    let dir = std::env::temp_dir().join(format!(
        "fs-overlay-attrs-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let (store, base_root) = base_store(&dir);
    let overlay = OverlayStore::open(
        dir.join("overlay.sqlite"),
        store,
        &OverlayOptions {
            base_root,
            profile: fs_snapshot::Profile::V2,
            scope_id: [3u8; 32],
            serial_first: 1,
            serial_end: 1024,
        },
    )
    .expect("overlay");
    (dir, overlay)
}

fn id() -> FileId {
    FileId::from_serial(1)
}

/// A base inode acquires an override row that keeps every base field but the one being set.
#[test]
fn setting_attributes_materialises_a_base_inode_without_rewriting_it() {
    let (dir, overlay) = overlay("materialise");
    let before = overlay
        .read(|tree: &fs_overlay::OverlayRead<'_>| TreeRead::inode(tree, id()))
        .expect("read")
        .expect("base inode");
    assert_eq!(
        before.attributes, 0,
        "cell setup: the base row has no stored attributes"
    );
    assert_eq!(
        overlay.inode_row_count().expect("count"),
        0,
        "cell setup: no overlay row yet"
    );

    overlay.set_attributes(id(), 0x01 | 0x20).expect("set");

    assert_eq!(
        overlay.inode_row_count().expect("count"),
        1,
        "cell materialise: exactly one override row was created"
    );
    assert_eq!(
        overlay.attributes(id()).expect("read back"),
        0x01 | 0x20,
        "cell value"
    );
    let after = overlay
        .read(|tree: &fs_overlay::OverlayRead<'_>| TreeRead::inode(tree, id()))
        .expect("read")
        .expect("inode");
    assert_eq!(
        after.size, before.size,
        "cell size: the INSERT kept the base value"
    );
    assert_eq!(
        after.created_ns, before.created_ns,
        "cell created: untouched"
    );
    assert_eq!(
        after.modified_ns, before.modified_ns,
        "cell modified: untouched — only attributes change"
    );
    assert_ne!(
        after.changed_ns, before.changed_ns,
        "cell changed: the metadata change time moved"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second write updates the existing row instead of inserting another one.
#[test]
fn a_second_write_updates_the_row_instead_of_inserting() {
    let (dir, overlay) = overlay("second");
    overlay.set_attributes(id(), 0x01).expect("first");
    let rows = overlay.inode_row_count().expect("count");
    overlay.set_attributes(id(), 0x02).expect("second");
    assert_eq!(
        overlay.inode_row_count().expect("count"),
        rows,
        "cell rows: the second write must not INSERT (a duplicate row still reads correctly, which is why this counts)"
    );
    assert_eq!(
        overlay.attributes(id()).expect("read back"),
        0x02,
        "cell value: the later write wins"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A derived bit is refused **and** leaves no row behind.
#[test]
fn a_derived_bit_is_refused_and_creates_no_row() {
    let (dir, overlay) = overlay("refused");
    for derived in [0x10u32, 0x80, 0x10 | 0x01] {
        let error = overlay
            .set_attributes(id(), derived)
            .expect_err("cell derived: must be refused");
        assert!(
            format!("{error:?}").contains("InvalidRecord"),
            "cell {derived:#x}: the refusal is a value-domain error, not a generic failure: {error:?}"
        );
    }
    assert_eq!(
        overlay.inode_row_count().expect("count"),
        0,
        "cell no row: a refusal must not materialise half an override"
    );
    // Control: the same call with a writable bit succeeds, so the refusals above are about the value.
    overlay
        .set_attributes(id(), 0x20)
        .expect("cell control: writable bits are accepted");
    assert_eq!(
        overlay.inode_row_count().expect("count"),
        1,
        "cell control: and it did create the row"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
