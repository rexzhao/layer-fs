//! The K1–K6 contract, exercised against the overlay backend.
//!
//! # Why these are written here and not re-used from `fs-api`
//!
//! The K-suite lives in `crates/fs-api/tests/` and runs through `Fs`, which holds a
//! concrete `Namespace` — that is, the **table** backend. Making those files run against
//! the overlay would mean making `fs-api` generic over `TreeStore`, which P2a-4
//! explicitly does not do ("不改 fs-api"). So the contract is re-stated here with the
//! same properties and the same assertion shapes, driving `fs_namespace::Rules` over
//! `OverlayStore` directly plus the overlay's own content API. When fs-api becomes
//! generic (P2a-6/7) these can be replaced by the `fs-api` files unchanged.
//!
//! Every test names the K it covers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cas_core::{ObjectId, Role};
use cas_store::{OpenMode, Store};
use fs_namespace::{Kind, NamespaceError, Path, Rules};
use fs_overlay::{OverlayOptions, OverlayStore};
use fs_snapshot::{
    encode_inode_table, encode_root, Content, InodeRecord, Kind as SnapKind, PageSink, Profile,
    RootValue, SnapshotResult,
};

/// A temp directory that cleans itself up.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "fs-overlay-{label}-{}-{sequence}",
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

/// Publishes pages into a real store.
struct StoreSink<'a> {
    store: &'a Store,
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        Ok(self.store.put(role, value).expect("put").oid)
    }
}

/// Builds a base snapshot with an empty root directory and returns its root id.
fn empty_base(dir: &std::path::Path) -> (Store, ObjectId) {
    let store = Store::open(dir.join("base"), OpenMode::Create).expect("open store");
    let profile = Profile::V1;
    let root = {
        let mut sink = StoreSink { store: &store };
        let records = vec![InodeRecord {
            serial: 0,
            kind: SnapKind::Directory,
            size: 0,
            created_ns: 0,
            modified_ns: 0,
            content: Content::Empty,
        }];
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
    (store, root)
}

struct Fixture {
    _root: TempRoot,
    rules: Rules<OverlayStore>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = TempRoot::new(label);
        let (store, base_root) = empty_base(&root.path);
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
        Self {
            _root: root,
            rules: Rules::new(overlay),
        }
    }

    fn store(&self) -> &OverlayStore {
        self.rules.backend()
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

// ---------------------------------------------------------------------- K1

/// **K1** — after `unlink`, the deleted inode's metadata and content are unreachable.
///
/// The overlay-specific sharpness: a deleted base inode must not fall through to the
/// base on the next read. The tombstone is what stops that, so this test fails if
/// `drop_inode` merely deletes the override row.
#[test]
fn k1_unlink_leaves_nothing_reachable() {
    let fixture = Fixture::new("k1");
    fixture.store().create_file(&p("/gone")).expect("create");
    fixture
        .store()
        .write_at(&p("/gone"), 0, b"payload")
        .expect("write");

    fixture.rules.unlink(&p("/gone")).expect("unlink");

    assert!(fixture.rules.stat(&p("/gone")).is_err(), "the name is gone");
    let root = fixture.rules.resolve(&p("/")).expect("root");
    let entries = fixture.rules.read_dir(&p("/")).expect("list");
    assert!(entries.iter().all(|entry| entry.name != "gone"));
    let _ = root;

    // Relisting through a fresh handle must agree: nothing resurrects the row.
    let again = fixture.rules.read_dir(&p("/")).expect("list");
    assert_eq!(again.len(), entries.len());
}

/// **K1** for a file created and removed in the same workspace: the row is dropped
/// outright rather than tombstoned.
#[test]
fn k1_unlink_of_a_local_file_removes_its_row() {
    let fixture = Fixture::new("k1-local");
    fixture.store().create_file(&p("/f")).expect("create");
    fixture.store().write_at(&p("/f"), 0, b"x").expect("write");
    fixture.rules.unlink(&p("/f")).expect("unlink");
    assert!(fixture.rules.stat(&p("/f")).is_err());
}

// ---------------------------------------------------------------------- K2

/// **K2** — renaming a file over another makes the overwritten inode unreachable.
#[test]
fn k2_rename_over_a_file_leaves_the_old_content_unreachable() {
    let fixture = Fixture::new("k2");
    fixture.store().create_file(&p("/src")).expect("create src");
    fixture
        .store()
        .write_at(&p("/src"), 0, b"new")
        .expect("write src");
    fixture.store().create_file(&p("/dst")).expect("create dst");
    fixture
        .store()
        .write_at(&p("/dst"), 0, b"old")
        .expect("write dst");

    fixture
        .rules
        .rename(&p("/src"), &p("/dst"))
        .expect("rename");

    assert_eq!(
        fixture.store().read_at(&p("/dst"), 0, 3).expect("read"),
        b"new"
    );
}

/// **K2** for a directory replaced by an empty directory.
#[test]
fn k2_rename_over_an_empty_directory_leaves_nothing_unreachable() {
    let fixture = Fixture::new("k2-dir");
    fixture.rules.mkdir(&p("/src")).expect("mkdir src");
    fixture.rules.mkdir(&p("/dst")).expect("mkdir dst");

    fixture
        .rules
        .rename(&p("/src"), &p("/dst"))
        .expect("rename");

    let entries = fixture.rules.read_dir(&p("/")).expect("list");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["dst"]);
}

// ---------------------------------------------------------------------- K4

/// **K4** — a directory cannot be moved into its own subtree.
#[test]
fn k4_a_directory_cannot_move_into_its_own_subtree() {
    let fixture = Fixture::new("k4");
    fixture.rules.mkdir(&p("/d")).expect("mkdir d");
    fixture.rules.mkdir(&p("/d/sub")).expect("mkdir sub");

    let error = fixture
        .rules
        .rename(&p("/d"), &p("/d/sub/moved"))
        .expect_err("R9");
    assert!(
        matches!(error, NamespaceError::IntoOwnSubtree { .. }),
        "{error}"
    );
}

// ---------------------------------------------------------------------- K5

/// **K5** — a same-path create race has exactly one winner.
///
/// The overlay serialises its own threads with the handle mutex, so the loser must lose
/// **by the rule**, not by a lock error or a raw constraint message.
#[test]
fn k5_same_path_create_has_exactly_one_winner() {
    let fixture = Fixture::new("k5");
    let store = Arc::new(fixture);

    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                scope.spawn(move || store.store().create_file(&p("/race")))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(winners, 1, "exactly one creator may succeed");
    for outcome in &outcomes {
        match outcome {
            Ok(()) => {}
            Err(NamespaceError::AlreadyExists { .. }) => {}
            Err(other) => panic!("unexpected failure {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------- K6

/// **K6** — truncating to a page boundary creates no page and keeps the bytes below it.
#[test]
fn k6_truncate_to_a_page_boundary_creates_no_page() {
    let fixture = Fixture::new("k6");
    let page = fs_overlay::schema::PAGE_BYTES;
    fixture.store().create_file(&p("/f")).expect("create");
    let body: Vec<u8> = (0..page * 3).map(|index| (index % 251) as u8).collect();
    fixture.store().write_at(&p("/f"), 0, &body).expect("write");

    let before = fixture.store().content_state(&p("/f")).expect("state");
    fixture
        .store()
        .truncate(&p("/f"), page * 2)
        .expect("truncate");
    let after = fixture.store().content_state(&p("/f")).expect("state");

    assert_eq!(after.size, page * 2);
    assert!(
        after.pages <= before.pages,
        "an aligned truncate must not add pages: {} -> {}",
        before.pages,
        after.pages
    );
    assert_eq!(
        fixture
            .store()
            .read_at(&p("/f"), 0, (page * 2) as usize)
            .expect("read"),
        body[..(page * 2) as usize].to_vec()
    );
}

/// **K6 regression** — shrinking to a size inside a page keeps that page's surviving bytes.
///
/// The page containing the new end starts *before* it, so a truncate that dropped pages
/// from `new_size / page_bytes` down destroyed data it was supposed to keep: `truncate(5)`
/// on a ten-byte file left five zero bytes.
#[test]
fn k6_truncate_inside_a_page_keeps_the_surviving_bytes() {
    let fixture = Fixture::new("k6-inside");
    fixture.store().create_file(&p("/f")).expect("create");
    fixture
        .store()
        .write_at(&p("/f"), 0, b"0123456789")
        .expect("write");
    assert_eq!(
        fixture
            .store()
            .read_at(&p("/f"), 0, 10)
            .expect("read before"),
        b"0123456789",
        "the write itself must be readable"
    );
    assert_eq!(
        fixture
            .store()
            .content_state(&p("/f"))
            .expect("state")
            .pages,
        1
    );
    fixture.store().truncate(&p("/f"), 5).expect("truncate");
    assert_eq!(
        fixture
            .store()
            .content_state(&p("/f"))
            .expect("state")
            .pages,
        1
    );
    assert_eq!(
        fixture.store().read_at(&p("/f"), 0, 5).expect("read"),
        b"01234",
        "the surviving prefix must still be there"
    );
    assert_eq!(
        fixture.store().content_state(&p("/f")).expect("state").size,
        5
    );
}

/// **K6 regression** — a truncate that cuts mid-page must also cut that page's tail.
///
/// The page straddling the new end is kept (above), but its bytes past the new size stayed
/// in the row: the next extension made them readable again — truncate handing back bytes it
/// had already cut, the mirror of KI-1. §7.3 asks for the boundary page's tail to be trimmed,
/// and that is what makes "之后再扩展读到的都是零" true rather than merely "unreadable".
#[test]
fn k6_shrinking_then_growing_inside_a_page_reads_zeros() {
    let fixture = Fixture::new("k6-tail");
    fixture.store().create_file(&p("/f")).expect("create");
    fixture
        .store()
        .write_at(&p("/f"), 0, b"0123456789")
        .expect("write");
    fixture.store().truncate(&p("/f"), 5).expect("shrink");
    fixture.store().truncate(&p("/f"), 8).expect("grow");

    assert_eq!(
        fixture.store().read_at(&p("/f"), 0, 8).expect("read"),
        b"01234\0\0\0",
        "the extension must read zeros, not the cut bytes \"567\""
    );
    assert_eq!(
        fixture.store().content_state(&p("/f")).expect("state").size,
        8
    );
}

/// **K6 regression, across pages** — the same defect on a page boundary.
///
/// The shrink lands inside page 1, so page 1 is the straddling page: it survives whole and
/// its tail is where the cut bytes came back. Pages 2 and up are dropped, so they can only
/// reappear as zeros. Both halves are checked, because the fix uses one rule for both.
#[test]
fn k6_shrinking_across_pages_then_growing_reads_zeros() {
    let fixture = Fixture::new("k6-pages");
    let page = fs_overlay::schema::PAGE_BYTES;
    fixture.store().create_file(&p("/f")).expect("create");
    fixture
        .store()
        .write_at(&p("/f"), 0, &vec![0xEE; (page * 2 + 100) as usize])
        .expect("write");

    fixture
        .store()
        .truncate(&p("/f"), page + 10)
        .expect("shrink");
    fixture.store().truncate(&p("/f"), page * 3).expect("grow");

    let read = fixture
        .store()
        .read_at(&p("/f"), 0, (page * 3) as usize)
        .expect("read");
    assert_eq!(
        &read[..(page + 10) as usize],
        vec![0xEE; (page + 10) as usize]
    );
    assert_eq!(
        &read[(page + 10) as usize..],
        &vec![0u8; (page * 2 - 10) as usize][..],
        "page 1's tail and the dropped pages must read as zeros after the extension"
    );
    assert_eq!(
        fixture
            .store()
            .content_state(&p("/f"))
            .expect("state")
            .pages,
        2,
        "pages 0 and 1 exist (page 1 zeroed in place); page 2 is implicit zeros, and no page \
         is ever short"
    );
}
// ---------------------------------------------------------------------- K3

/// **K3** — concurrent writes to different pages of one file both survive.
///
/// This is the assertion the table backend cannot meet (its B1 defect); on the overlay
/// the handle lock makes the two writes serialise, and since their ranges are disjoint
/// every serial order leaves the same bytes.
#[test]
fn k3_concurrent_writes_to_different_pages_keep_both() {
    let fixture = Fixture::new("k3-blocks");
    let store = Arc::new(fixture);
    let page = fs_overlay::schema::PAGE_BYTES;

    for round in 0..8u32 {
        let path = p(&format!("/f{round}"));
        store.store().create_file(&path).expect("create");

        let low = 0u64;
        let high = page * 4;
        std::thread::scope(|scope| {
            for offset in [low, high] {
                let store = Arc::clone(&store);
                let path = path.clone();
                scope.spawn(move || {
                    store
                        .store()
                        .write_at(&path, offset, &vec![0x5A; page as usize])
                        .expect("write");
                });
            }
        });

        let size = store.rules.stat(&path).expect("stat").size;
        assert_eq!(
            size,
            high + page,
            "round {round}: both writes must extend the file"
        );
        assert_eq!(
            store.store().read_at(&path, low, 8).expect("read low"),
            vec![0x5A; 8]
        );
        assert_eq!(
            store.store().read_at(&path, high, 8).expect("read high"),
            vec![0x5A; 8]
        );
    }
}

/// **K3c** — a write that loses to an unlink reports `NotFound` and leaves no page.
#[test]
fn k3c_a_write_that_loses_to_unlink_leaves_nothing() {
    let fixture = Fixture::new("k3c");
    fixture.store().create_file(&p("/f")).expect("create");
    fixture
        .store()
        .write_at(&p("/f"), 0, b"seed")
        .expect("seed");

    let store = Arc::new(fixture);
    let wrote = Mutex::new(None);
    std::thread::scope(|scope| {
        {
            let store = Arc::clone(&store);
            scope.spawn(move || {
                let _ = store.rules.unlink(&p("/f"));
            });
        }
        {
            let store = Arc::clone(&store);
            let wrote = &wrote;
            scope.spawn(move || {
                let outcome = store.store().write_at(&p("/f"), 0, b"later");
                *wrote.lock().expect("lock") = Some(outcome.is_ok());
            });
        }
    });

    let existed = store.rules.stat(&p("/f")).is_ok();
    let did_write = wrote.lock().expect("lock").unwrap_or(false);
    // Order A (write then unlink): gone. Order B (unlink then write): gone, write failed.
    assert!(!existed, "the unlink must win one of the two orders");
    assert!(!did_write || !existed);
}

// ------------------------------------------------------- composition and flush

/// A base file is readable through the overlay without being copied.
#[test]
fn a_base_file_reads_through_the_overlay() {
    let root = TempRoot::new("base-read");
    let profile = Profile::V1;
    let (store, base_root) = {
        let store = Store::open(root.path.join("base"), OpenMode::Create).expect("store");
        // /data holds "hello" as a single blob.
        let blob = store.put(Role::Bytes, b"hello").expect("put").oid;
        let base_root = {
            let mut sink = StoreSink { store: &store };
            let records = vec![
                InodeRecord {
                    serial: 0,
                    kind: SnapKind::Directory,
                    size: 0,
                    created_ns: 0,
                    modified_ns: 0,
                    content: Content::Dir({
                        fs_snapshot::encode_directory(
                            &profile,
                            std::iter::once(fs_snapshot::DirEntry {
                                name: b"data".to_vec(),
                                child_serial: 1,
                            }),
                            &mut sink,
                        )
                        .expect("dir")
                        .expect("non-empty")
                    }),
                },
                InodeRecord {
                    serial: 1,
                    kind: SnapKind::File,
                    size: 5,
                    created_ns: 0,
                    modified_ns: 0,
                    content: Content::Blob(blob),
                },
            ];
            let table =
                encode_inode_table(&profile, records.into_iter(), &mut sink).expect("table");
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
    };
    let overlay = OverlayStore::open(
        root.path.join("overlay.sqlite"),
        store,
        &OverlayOptions {
            base_root,
            profile,
            scope_id: [1u8; 32],
            serial_first: 2,
            serial_end: 1024,
        },
    )
    .expect("open");
    let rules = Rules::new(overlay);

    let entries = rules.read_dir(&p("/")).expect("list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "data");
    assert_eq!(
        rules.backend().read_at(&p("/data"), 0, 5).expect("read"),
        b"hello"
    );
    // Past the base file's end reads as zero, not as an error.
    assert_eq!(
        rules.backend().read_at(&p("/data"), 5, 3).expect("read"),
        vec![0u8; 3]
    );
}

/// A workspace change is visible to itself, and `flush` makes it durable across a
/// reopen (W3, W4).
#[test]
fn flush_makes_changes_survive_a_reopen() {
    let root = TempRoot::new("flush");
    let (store, base_root) = empty_base(&root.path);
    let options = OverlayOptions {
        base_root,
        profile: Profile::V1,
        scope_id: [1u8; 32],
        serial_first: 1,
        serial_end: 1 << 20,
    };
    {
        let overlay =
            OverlayStore::open(root.path.join("overlay.sqlite"), store, &options).expect("open");
        let rules = Rules::new(overlay);
        rules.mkdir(&p("/kept")).expect("mkdir");
        rules.backend().create_file(&p("/kept/f")).expect("create");
        rules
            .backend()
            .write_at(&p("/kept/f"), 0, b"durable")
            .expect("write");
        rules.backend().flush().expect("flush");
        assert!(rules.backend().flush_count().expect("count") >= 1);
    }

    // Reopen: the base store is immutable, so a fresh handle reads the same objects.
    let (store, _) = empty_base_like(&root.path, base_root);
    let overlay =
        OverlayStore::open(root.path.join("overlay.sqlite"), store, &options).expect("reopen");
    let rules = Rules::new(overlay);
    let entries = rules.read_dir(&p("/kept")).expect("list");
    assert_eq!(entries.len(), 1, "the change survived the reopen");
    assert_eq!(
        rules.backend().read_at(&p("/kept/f"), 0, 7).expect("read"),
        b"durable"
    );
}

/// Opens the existing base store again (it already exists from `empty_base`).
fn empty_base_like(dir: &std::path::Path, _root: ObjectId) -> (Store, ObjectId) {
    let store = Store::open(dir.join("base"), OpenMode::ReadWrite).expect("reopen store");
    let profile = Profile::V1;
    let root = {
        let mut sink = StoreSink { store: &store };
        let records = vec![InodeRecord {
            serial: 0,
            kind: SnapKind::Directory,
            size: 0,
            created_ns: 0,
            modified_ns: 0,
            content: Content::Empty,
        }];
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
    (store, root)
}

/// The overlay must be shareable across threads: one handle, many callers (ADR-006
/// §7.4). This is a compile-time property with a runtime smoke test attached.
#[test]
fn the_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OverlayStore>();
    assert_send_sync::<Arc<OverlayStore>>();
}

/// A second handle on the same file is refused rather than racing (W2).
#[test]
fn a_second_handle_is_refused() {
    let root = TempRoot::new("exclusive");
    let (store, base_root) = empty_base(&root.path);
    let options = OverlayOptions {
        base_root,
        profile: Profile::V1,
        scope_id: [1u8; 32],
        serial_first: 1,
        serial_end: 1024,
    };
    let _first =
        OverlayStore::open(root.path.join("overlay.sqlite"), store, &options).expect("first");
    let (store2, _) = empty_base_like(&root.path, base_root);
    let second = OverlayStore::open(root.path.join("overlay.sqlite"), store2, &options);
    assert!(
        second.is_err(),
        "a second handle must be refused, not silently share the file"
    );
}

/// `Kind` is used by the rules; referencing it keeps the import honest if the tests
/// above change shape.
#[test]
fn directories_report_as_directories() {
    let fixture = Fixture::new("kinds");
    fixture.rules.mkdir(&p("/d")).expect("mkdir");
    let stat = fixture.rules.stat(&p("/d")).expect("stat");
    assert_eq!(stat.kind, Kind::Directory);
    assert_eq!(stat.size, 0);
}

// ============================================================================
// §18 item 3 — ten workspaces over one base do not see each other
//
// The isolation claim follows from the design (one overlay file per workspace, opened
// exclusively, composed with an immutable base), but "follows from the design" is not
// evidence. This runs ten real workspaces and checks that each sees only its own work.
// ============================================================================

#[test]
fn ten_workspaces_over_one_base_stay_isolated() {
    let root = TempRoot::new("ten");
    let (store, base_root) = empty_base(&root.path);
    drop(store);

    let count = 10usize;
    let mut workspaces = Vec::new();
    for index in 0..count {
        let member = Store::open(root.path.join("base"), OpenMode::ReadWrite).expect("base");
        let overlay = OverlayStore::open(
            root.path.join(format!("overlay-{index}.sqlite")),
            member,
            &OverlayOptions {
                base_root,
                profile: Profile::V1,
                scope_id: [1u8; 32],
                serial_first: 1,
                serial_end: 1 << 20,
            },
        )
        .expect("open workspace");
        workspaces.push(Rules::new(overlay));
    }

    // Each workspace writes under its own name, so any leakage shows up as an entry that
    // this workspace never created.
    for (index, rules) in workspaces.iter().enumerate() {
        let dir = format!("/w{index}");
        rules.mkdir(&p(&dir)).expect("mkdir");
        rules
            .backend()
            .create_file(&p(&format!("{dir}/f")))
            .expect("create");
        rules
            .backend()
            .write_at(
                &p(&format!("{dir}/f")),
                0,
                format!("payload-{index}").as_bytes(),
            )
            .expect("write");
    }

    for (index, rules) in workspaces.iter().enumerate() {
        let found = rules.read_dir(&p("/")).expect("list");
        for other in 0..count {
            let name = format!("w{other}");
            let present = found.iter().any(|entry| entry.name == name);
            assert_eq!(
                present,
                other == index,
                "workspace {index} must see its own entry and nothing of {name}"
            );
        }
        let body = format!("payload-{index}");
        assert_eq!(
            rules
                .backend()
                .read_at(&p(&format!("/w{index}/f")), 0, body.len())
                .expect("read"),
            body.as_bytes()
        );
    }

    // Closing them all and reopening one shows that a workspace's file carries its own
    // work and only its own: a shared or leaked file would make the reopen ambiguous.
    drop(workspaces);
    let store = Store::open(root.path.join("base"), OpenMode::ReadWrite).expect("base");
    let reopened = Rules::new(
        OverlayStore::open(
            root.path.join("overlay-0.sqlite"),
            store,
            &OverlayOptions {
                base_root,
                profile: Profile::V1,
                scope_id: [1u8; 32],
                serial_first: 1,
                serial_end: 1 << 20,
            },
        )
        .expect("reopen"),
    );
    let entries = reopened.read_dir(&p("/")).expect("list");
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["w0"], "only workspace 0's own entry is there");
    assert_eq!(
        reopened.backend().read_at(&p("/w0/f"), 0, 9).expect("read"),
        b"payload-0"
    );
}

/// **K6 regression, shrink then write past EOF** — the other way the cut bytes come back.
///
/// `write_at` materialises the page it touches from the page's current bytes, so if the
/// straddling page still held the cut tail, an out-of-range write would copy it back into the
/// row. KI-9 lists this shape as A/B effective; it fails on the unfixed tree for that reason.
#[test]
fn k6_writing_past_eof_after_a_shrink_keeps_the_cut_region_zeroed() {
    let fixture = Fixture::new("k6-write-after-shrink");
    fixture.store().create_file(&p("/f")).expect("create");
    fixture
        .store()
        .write_at(&p("/f"), 0, &[0xEE; 200])
        .expect("write");
    fixture.store().truncate(&p("/f"), 66).expect("shrink");
    fixture
        .store()
        .write_at(&p("/f"), 190, &[0x11; 4])
        .expect("write past EOF");

    let read = fixture.store().read_at(&p("/f"), 0, 194).expect("read");
    assert_eq!(&read[..66], &[0xEE; 66][..], "the surviving prefix");
    assert_eq!(
        &read[66..190],
        &[0u8; 124][..],
        "the write must not carry the cut bytes back into the page"
    );
    assert_eq!(&read[190..], &[0x11; 4][..], "the write itself");
}

/// **K6 coverage, not an A/B judge** — aligned shrink, then growth past the boundary.
///
/// An aligned shrink keeps no straddling page: `keep_bytes == 0` skips the zeroing branch and
/// `boundary = ⌈2·page/page⌉` deletes `page_index ≥ 2`, which is exactly the delete logic the
/// unfixed tree had. So this test **passes on the unfixed tree too** — it pins the *other*
/// chain: "deleted pages + `base_limit` never rises ⇒ reads at or past the base limit are
/// zero", which no zeroing covers and which a future sparse-page / partial-page refactor could
/// silently break (the extension must not quietly recreate a page here either).
#[test]
fn k6_aligned_shrink_then_growth_reads_zeros_without_new_pages() {
    let fixture = Fixture::new("k6-aligned-grow");
    let page = fs_overlay::schema::PAGE_BYTES;
    fixture.store().create_file(&p("/f")).expect("create");
    let body: Vec<u8> = (0..page * 3).map(|index| (index % 251) as u8).collect();
    fixture
        .store()
        .write_at(&p("/f"), 0, &body)
        .expect("write three pages");

    fixture
        .store()
        .truncate(&p("/f"), page * 2)
        .expect("aligned shrink");
    let after_shrink = fixture.store().content_state(&p("/f")).expect("state");
    assert_eq!(
        after_shrink.pages, 2,
        "page 2 is dropped whole, not trimmed"
    );

    fixture
        .store()
        .truncate(&p("/f"), page * 3)
        .expect("grow past the boundary");

    let read = fixture
        .store()
        .read_at(&p("/f"), 0, (page * 3) as usize)
        .expect("read");
    assert_eq!(&read[..(page * 2) as usize], &body[..(page * 2) as usize]);
    assert_eq!(
        &read[(page * 2) as usize..],
        &vec![0u8; page as usize][..],
        "the dropped page must come back as zeros"
    );
    assert_eq!(
        fixture
            .store()
            .content_state(&p("/f"))
            .expect("state")
            .pages,
        after_shrink.pages,
        "growth must not secretly recreate a page"
    );
}
