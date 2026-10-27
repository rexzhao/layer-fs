//! §3 criterion 6: directory cursors with first-read snapshot semantics (P3-3a).
//!
//! The criterion has two halves that fail in opposite ways, so both are asserted for every scenario:
//!
//! - a **new** entry created after the first read must **not** appear — that is the promise, not a caching bug;
//! - the entries that existed must **all** still appear, in the same order — a snapshot that dropped or reordered
//!   entries would satisfy "no new entries" trivially.
//!
//! `.`/`..` and de-duplication are asserted **absent** by construction: the cursor has no way to produce them, and the
//! tests below show `A` and `a` both survive.

use fs_semantics::disposition::Disposition;
use fs_winfsp::{DirentCursor, MockBackend, WorkspaceOps};

fn create(backend: &MockBackend, path: &[u8]) {
    backend
        .create_or_open(path, Disposition::Create)
        .expect("create");
}

/// The snapshot is taken on the first read and never refreshed: entries created afterwards do not appear.
#[test]
fn a_new_entry_created_after_the_first_read_does_not_appear() {
    let backend = MockBackend::new();
    let dir = backend.mkdir(b"/d").expect("mkdir");
    create(&backend, b"/d/one");
    create(&backend, b"/d/two");

    let mut cursor = DirentCursor::new();
    let first = cursor
        .fill(backend.read_dir(dir).expect("read_dir"), |name| {
            name.to_ascii_lowercase()
        })
        .len();
    assert_eq!(
        first, 2,
        "cell first read: both entries are in the snapshot"
    );

    // The world changes after the snapshot.
    create(&backend, b"/d/three");
    assert_eq!(
        backend.read_dir(dir).expect("read_dir").len(),
        3,
        "cell control: the backend now has three entries — so a cursor that re-read would show three"
    );

    let page = cursor.page(None, 32);
    assert_eq!(
        page.len(),
        2,
        "cell snapshot: the handle keeps showing the two entries it started with (snapshot semantics, not a cache bug)"
    );
    assert!(
        page.iter().all(|entry| entry.name != b"three"),
        "cell snapshot: the new entry is not in the listing"
    );
}

/// Pages are cut by marker **from the same snapshot**, so a second page cannot see anything the first page did not.
#[test]
fn later_pages_are_cut_from_the_same_snapshot() {
    let backend = MockBackend::new();
    let dir = backend.mkdir(b"/d").expect("mkdir");
    for name in [&b"/d/a"[..], b"/d/b", b"/d/c"] {
        create(&backend, name);
    }
    let mut cursor = DirentCursor::new();
    cursor.fill(backend.read_dir(dir).expect("read_dir"), |name| {
        name.to_ascii_lowercase()
    });

    let first = cursor.page(None, 2).to_vec();
    assert_eq!(
        first
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>(),
        vec![b"a".to_vec(), b"b".to_vec()],
        "cell page 1: the first two entries in folded order"
    );

    // Change the directory between the two pages: the second page must not notice.
    create(&backend, b"/d/bb");
    let marker = first.last().expect("a marker exists").name.clone();
    let second: Vec<Vec<u8>> = cursor
        .page(Some(&marker), 2)
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    assert_eq!(
        second,
        vec![b"c".to_vec()],
        "cell page 2: the remaining snapshot entry, without the entry created in between"
    );
}

/// A marker that is no longer in the snapshot yields an **empty** page, not a restart — a restart would duplicate the
/// whole directory for a caller that is iterating.
#[test]
fn an_unknown_marker_yields_an_empty_page_rather_than_restarting() {
    let backend = MockBackend::new();
    let dir = backend.mkdir(b"/d").expect("mkdir");
    create(&backend, b"/d/a");
    create(&backend, b"/d/b");
    let mut cursor = DirentCursor::new();
    cursor.fill(backend.read_dir(dir).expect("read_dir"), |name| {
        name.to_ascii_lowercase()
    });

    assert!(
        cursor.page(Some(b"gone"), 32).is_empty(),
        "cell marker: a stale marker ends the enumeration instead of duplicating it"
    );
    assert_eq!(
        cursor.page(None, 32).len(),
        2,
        "cell control: without a marker the same snapshot still lists both entries"
    );
}

/// Raw-byte names: `A` and `a` are two entries, and the folded key only decides their **order**, never their presence.
#[test]
fn folding_orders_entries_without_merging_them() {
    let backend = MockBackend::new();
    let dir = backend.mkdir(b"/d").expect("mkdir");
    create(&backend, b"/d/A");
    create(&backend, b"/d/a");
    let mut cursor = DirentCursor::new();
    let list = cursor
        .fill(backend.read_dir(dir).expect("read_dir"), |name| {
            name.to_ascii_lowercase()
        })
        .to_vec();

    assert_eq!(
        list.len(),
        2,
        "cell count: two entries, not one merged entry"
    );
    assert_eq!(
        list.iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>(),
        vec![b"A".to_vec(), b"a".to_vec()],
        "cell order: equal folded keys fall back to raw bytes, so the order is deterministic"
    );
    assert_eq!(
        list[0].folded, list[1].folded,
        "cell fold: both fold to the same key — that is why the raw name must be the secondary key"
    );
}

/// The cursor never invents `.`/`..`: they are the adapter's to synthesise, and a backend that also returned them would
/// make the adapter show them twice.
#[test]
fn the_cursor_does_not_synthesise_dot_entries() {
    let backend = MockBackend::new();
    let dir = backend.mkdir(b"/d").expect("mkdir");
    create(&backend, b"/d/only");
    let mut cursor = DirentCursor::new();
    let list = cursor.fill(backend.read_dir(dir).expect("read_dir"), |name| {
        name.to_ascii_lowercase()
    });

    assert!(
        list.iter()
            .all(|entry| entry.name != b"." && entry.name != b".."),
        "cell dots: absent — the adapter synthesises them"
    );
}

/// **N2 variant** — the adapter must **not** filter `.`/`..` out of a backend's output.
///
/// The rule the card sets is "the backend never sends them; the adapter synthesises them". This test is the **negative**
/// of that rule: if a backend *did* send a `.`, the adapter would pass it through and the caller would see two of them.
/// Asserting the pass-through looks like asserting a bug — and it is exactly the point: it proves the "backend sends no
/// dots" half is load-bearing, because nothing downstream hides a violation.
///
/// Without this, "each entry appears exactly once" would be satisfied by an adapter that silently de-duplicated instead
/// of by a backend that behaved.
#[test]
fn a_dot_from_the_backend_would_reach_the_caller_twice() {
    use fs_winfsp::{DirEntry, DirentCursor, Kind};

    let mut cursor = DirentCursor::new();
    // A backend misbehaving in the way the rule forbids: it returns its own `.`.
    let entries = vec![
        DirEntry {
            name: b".".to_vec(),
            kind: Kind::Directory,
            file_id: 1,
        },
        DirEntry {
            name: b"real".to_vec(),
            kind: Kind::File,
            file_id: 2,
        },
    ];
    cursor.fill(entries, |name| name.to_ascii_lowercase());
    let listed: Vec<Vec<u8>> = cursor
        .page(None, 32)
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    assert_eq!(
        listed,
        vec![b".".to_vec(), b"real".to_vec()],
        "cell pass-through: the adapter does not filter — so a backend that sent `.` would surface it, and the caller \
         (which synthesises its own `.`) would show it twice. This is why the rule says the backend must not send it"
    );
    assert_eq!(
        listed.len(),
        2,
        "cell count: two entries survive — no de-duplication is hiding a misbehaving backend"
    );
}

/// The listing's `file_id` is the id `get_info` answers with (367's criterion): one translation, not two.
///
/// Without this, a listing could carry ids that agree with themselves but not with the backend — every record would still look
/// right in isolation, which is exactly the kind of wrong that only a cross-check catches.
#[test]
fn a_listing_carries_the_ids_the_backend_uses() {
    use fs_semantics::disposition::Disposition;
    use fs_winfsp::mock::MockBackend;
    use fs_winfsp::ops::WorkspaceOps;
    use fs_winfsp::Kind as EntryKind;

    let backend = MockBackend::new();
    let file = backend
        .create_or_open(b"/alpha.txt", Disposition::Create)
        .expect("create the file");
    let dir = backend.mkdir(b"/sub").expect("create the directory");

    let entries = backend.read_dir(0).expect("list the root");
    assert_eq!(entries.len(), 2, "cell count: the two entries just made");

    for entry in &entries {
        // Each listed id must answer metadata — that is what a dirent record needs.
        let info = backend
            .get_info(entry.file_id)
            .expect("cell agreement: every listed id must be resolvable");
        assert_eq!(
            info.kind,
            entry.kind,
            "cell agreement: the id and the listing describe the same entry (name {:?})",
            String::from_utf8_lossy(&entry.name)
        );
        // The by-name route must agree too: resolving the same name again gives the same id the listing reported.
        // A file resolves by name to the same id the listing reported. A **directory** is checked against the id `mkdir`
        // returned instead: `create_or_open` is the file door (`NotADirectory` on a directory is the correct refusal, and it is
        // how this test found out that the two doors are not interchangeable).
        if entry.kind == EntryKind::Directory {
            assert_eq!(
                entry.file_id, dir,
                "cell agreement: the listed directory carries the id `mkdir` returned"
            );
        } else {
            let by_name = backend
                .create_or_open(b"/alpha.txt", Disposition::Open)
                .expect("resolve the file by name");
            assert_eq!(
                by_name.file_id,
                entry.file_id,
                "cell agreement: the listing's id is the id the name resolves to (name {:?})",
                String::from_utf8_lossy(&entry.name)
            );
        }
    }

    let ids: Vec<u64> = entries.iter().map(|entry| entry.file_id).collect();
    assert!(
        ids.contains(&file.file_id) && ids.contains(&dir),
        "cell ids: the listing names the file and the directory by the ids `create_or_open`/`mkdir` returned"
    );
}
