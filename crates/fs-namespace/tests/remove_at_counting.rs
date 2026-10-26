//! `remove_at` tombstones **exactly once** (P3-2 G2).
//!
//! Observed through the backend's own counter, not through the outcome: tombstoning an already-dead row is a no-op at
//! the SQL level, so "did it happen twice?" is invisible from outside — which is exactly why the counter exists.
//! (The adapter's M12 counts how often it *calls* `remove_at`; this counts how often the core *tombstones*.)

use fs_namespace::{FileId, Path, Rules};
use fs_overlay::{OverlayOptions, OverlayStore};

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

fn rules(label: &str) -> (TempDir, Rules<OverlayStore>) {
    let dir = std::env::temp_dir().join(format!(
        "fs-namespace-count-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let store =
        cas_store::Store::open(dir.join("store"), cas_store::OpenMode::Create).expect("store");
    // A **real** empty base: the overlay verifies that the base root exists in the store, so a fabricated id is refused
    // before any test logic runs.
    let base_root = {
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
        let profile = fs_snapshot::Profile::V1;
        let mut sink = Sink(&store);
        // The root inode is required (an inode table may not be empty); the root directory is `Empty`, which is legal.
        let table = fs_snapshot::encode_inode_table(
            &profile,
            std::iter::once(fs_snapshot::InodeRecord {
                serial: 0,
                kind: fs_snapshot::Kind::Directory,
                size: 0,
                created_ns: 0,
                modified_ns: 0,

                accessed_ns: 0,
                changed_ns: 0,
                attributes: 0,
                security: Vec::new(),
                content: fs_snapshot::Content::Empty,
            }),
            &mut sink,
        )
        .expect("table");
        fs_snapshot::encode_root(
            &profile,
            &fs_snapshot::RootValue {
                scope_id: [7u8; 32],
                inode_table: table,
            },
            &mut sink,
        )
        .expect("root")
    };
    let overlay = OverlayStore::open(
        dir.join("overlay.sqlite"),
        store,
        &OverlayOptions {
            base_root,
            profile: fs_snapshot::Profile::V1,
            scope_id: [7u8; 32],
            serial_first: 1,
            serial_end: 1024,
        },
    )
    .expect("overlay");
    (TempDir(dir), Rules::new(overlay))
}

#[test]
fn remove_at_tombstones_exactly_once_including_the_idempotent_path() {
    let (_dir, rules) = rules("once");
    rules
        .create_or_open(&p("/f"), fs_semantics::disposition::Disposition::Create)
        .expect("create");
    let id = rules.resolve(&p("/f")).expect("id");

    fs_overlay::counting::reset_tombstones();
    rules.remove_at(FileId::ROOT, b"f", id).expect("remove");
    assert_eq!(
        fs_overlay::counting::tombstones(),
        1,
        "cell count: exactly one tombstone for one removal"
    );

    // Idempotent second call: nothing is tombstoned again — a build that "retried the removal" would show 2 here while
    // every observable assertion still passed.
    rules
        .remove_at(FileId::ROOT, b"f", id)
        .expect("a repeated remove is a no-op");
    assert_eq!(
        fs_overlay::counting::tombstones(),
        1,
        "cell count: the idempotent path must not tombstone a second time"
    );
}

/// A **refused** `remove_at` must not tombstone anything: a guard that cleaned up "just in case" would delete data on a
/// stale handle, which is the failure the guard exists to prevent.
#[test]
fn a_refused_remove_at_tombstones_nothing() {
    let (_dir, rules) = rules("refused");
    rules
        .create_or_open(&p("/f"), fs_semantics::disposition::Disposition::Create)
        .expect("create");
    let id = rules.resolve(&p("/f")).expect("id");
    let stranger = rules
        .create_or_open(&p("/g"), fs_semantics::disposition::Disposition::Create)
        .expect("create")
        .file_id;
    assert_ne!(stranger, id);

    fs_overlay::counting::reset_tombstones();
    assert!(
        rules.remove_at(FileId::ROOT, b"f", stranger).is_err(),
        "cell control: the guard must refuse this call"
    );
    assert_eq!(
        fs_overlay::counting::tombstones(),
        0,
        "cell refusal: a refused removal must not tombstone anything"
    );
}
