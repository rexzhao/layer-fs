//! **Lookup by folded key** (478): `readme.txt` finds an entry stored as `ReadMe.txt`, and the name still reads back as it was written.
//!
//! The base's pages are ordered by the bytes that were written, so a folded key cannot be found by searching for it — the view has to compare folded
//! keys, and that is what these two tests pin: one for an entry that lives in the base (the data being searched) and one for an entry the overlay
//! wrote itself.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_core::{ObjectId, Role};
use cas_store::{OpenMode, Store};
use fs_namespace::{Path, Rules};
use fs_overlay::{OverlayOptions, OverlayStore};
use fs_snapshot::{
    encode_inode_table, encode_root, Content, DirEntry, InodeRecord, Kind as SnapKind, PageSink,
    Profile, RootValue, SnapshotResult,
};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-overlay-fold-{label}-{}-{sequence}",
            std::process::id()
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

struct StoreSink<'a> {
    store: &'a Store,
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        Ok(self.store.put(role, value).expect("put").oid)
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

/// A base holding one file named `name` at the root, with `payload` as its content.
fn base_with(root: &std::path::Path, name: &[u8], payload: &[u8]) -> (Store, ObjectId) {
    let store = Store::open(root.join("base"), OpenMode::Create).expect("open store");
    let profile = Profile::V1;
    let blob = store.put(Role::Bytes, payload).expect("put").oid;
    let base_root = {
        let mut sink = StoreSink { store: &store };
        let records = vec![
            InodeRecord {
                serial: 0,
                kind: SnapKind::Directory,
                size: 0,
                created_ns: 0,
                modified_ns: 0,
                accessed_ns: 0,
                changed_ns: 0,
                attributes: 0,
                security: Vec::new(),
                content: Content::Dir(
                    fs_snapshot::encode_directory(
                        &profile,
                        std::iter::once(DirEntry {
                            name: name.to_vec(),
                            child_serial: 1,
                        }),
                        &mut sink,
                    )
                    .expect("dir")
                    .expect("non-empty"),
                ),
            },
            InodeRecord {
                serial: 1,
                kind: SnapKind::File,
                size: payload.len() as u64,
                created_ns: 0,
                modified_ns: 0,
                accessed_ns: 0,
                changed_ns: 0,
                attributes: 0,
                security: Vec::new(),
                content: Content::Blob(blob),
            },
        ];
        let table = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("table");
        encode_root(
            &profile,
            &RootValue {
                scope_id: [1u8; 32],
                inode_table: table,
            },
            &mut sink,
        )
        .expect("root")
    };
    (store, base_root)
}

fn rules_with_base(root: &TempRoot, store: Store, base_root: ObjectId) -> Rules<OverlayStore> {
    let overlay = OverlayStore::open(
        root.path.join("overlay.sqlite"),
        store,
        &OverlayOptions {
            base_root,
            profile: Profile::V1,
            scope_id: [1u8; 32],
            serial_first: 1,
            serial_end: 1 << 20,
        },
    )
    .expect("open overlay");
    Rules::new(overlay)
}

#[test]
fn a_base_entry_is_found_by_another_case() {
    let root = TempRoot::new("base");
    let (store, base_root) = base_with(&root.path, b"ReadMe.txt", b"hello");
    let rules = rules_with_base(&root, store, base_root);

    let found = rules
        .stat(&p("/readme.txt"))
        .expect("folded lookup hits the base");
    let listing = rules.read_dir(&p("/")).expect("list");
    let entry = listing
        .iter()
        .find(|entry| entry.id == found.id)
        .expect("the entry is listed");
    // **The display name is the stored one**, not the one that was asked for: this is the half that a folded-key store would lose.
    assert_eq!(entry.name, "ReadMe.txt");
    // **Negative criterion**: a name that differs by more than case still misses. Without this, "the scan finds everything" would also pass.
    assert!(
        rules.stat(&p("/readme.md")).is_err(),
        "the folded key must not match a different name"
    );
    // And the exact spelling still works, so folding did not replace exact matching.
    assert_eq!(rules.stat(&p("/ReadMe.txt")).expect("exact").id, found.id);
}

#[test]
fn an_overlay_entry_is_found_by_another_case() {
    let root = TempRoot::new("overlay");
    let (store, base_root) = base_with(&root.path, b"unrelated", b"x");
    let rules = rules_with_base(&root, store, base_root);
    rules
        .create_or_open(&p("/Local"), fs_semantics::disposition::Disposition::Create)
        .expect("create");

    let found = rules
        .stat(&p("/local"))
        .expect("folded lookup hits the overlay row");
    let listing = rules.read_dir(&p("/")).expect("list");
    let entry = listing
        .iter()
        .find(|entry| entry.id == found.id)
        .expect("listed");
    assert_eq!(entry.name, "Local");
}
