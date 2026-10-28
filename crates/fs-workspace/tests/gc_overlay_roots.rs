//! Overlay roots under maintenance (P2b-6 §19).
//!
//! The ruling is "whoever holds the lock is who reads". This file pins both halves: the maintenance
//! source sees the overlay's `pending_root` **while** maintenance holds every overlay exclusively, and
//! the premise is live — a second connection to that same overlay really is refused, so the test would
//! not pass on a filesystem where the lock was not exclusive.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use fs_namespace::Path as FsPath;
use fs_workspace::gc::{MaintenanceRoots, RepoRoots, RootKind, RootSource};
use fs_workspace::{MaintenanceRepo, Repo};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gcoverlayroots-{label}-{}-{}",
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

fn put_file(workspace: &fs_workspace::Workspace, path: &str, bytes: &[u8]) {
    let store = workspace.store();
    let path = FsPath::parse(path).expect("path");
    if store.content_state(&path).is_err() {
        store.create_file(&path).expect("create");
    }
    store.write_at(&path, 0, bytes).expect("write");
    store
        .truncate(&path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

/// A repository whose one workspace has an **uncommitted** file, i.e. a real `pending_root`.
///
/// The workspace handle is dropped before the caller touches the store: an open workspace holds its
/// overlay, and maintenance would (correctly) refuse to take it. The overlay's own path is returned
/// because workspace directories are named by id, not by name.
fn dirty_workspace(label: &str) -> (TempRoot, Arc<Repo>, PathBuf) {
    let root = TempRoot::new(label);
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"uncommitted bytes");
    let overlay = workspace.overlay_path().to_path_buf();
    drop(workspace);

    // `pending_root` is a **publication intent**, not "the overlay is dirty": publish sets it, then
    // clears it on success, so a settled workspace never has one. The state GC must handle is the one
    // a crash leaves behind — an intent with no commit — so the fixture writes that intent directly,
    // using a real object id and the same key the publisher uses.
    let head = repo.branch_head("main").expect("head");
    let tree = repo.commit_info(&head).expect("info").root;
    let connection = rusqlite::Connection::open(&overlay).expect("open overlay");
    connection
        .execute(
            "INSERT INTO ov_meta (key, value) VALUES ('pending_root', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [tree.to_string()],
        )
        .expect("record the intent");
    drop(connection);
    (root, repo, overlay)
}

#[test]
fn maintenance_reads_the_overlay_pending_root_through_its_own_connection() {
    common::strict_gate();
    let (root, repo, _overlay) = dirty_workspace("pending");
    drop(repo);

    let maintenance = MaintenanceRepo::open(root.store()).expect("maintenance");
    let set = MaintenanceRoots {
        maintenance: &maintenance,
    }
    .roots()
    .expect("maintenance roots");

    let pending: Vec<_> = set
        .entries()
        .iter()
        .filter(|entry| entry.kind == RootKind::OverlayPending)
        .collect();
    assert_eq!(
        pending.len(),
        1,
        "the dirty workspace must contribute exactly one pending root, got {:?}",
        set.entries()
            .iter()
            .map(|entry| (entry.kind, entry.label.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        pending[0].label.starts_with("overlay-pending:"),
        "the label names the workspace row: {}",
        pending[0].label
    );
    assert!(
        set.entries()
            .iter()
            .any(|entry| entry.root == pending[0].root),
        "the pending root is a real object id taken from the overlay"
    );
    assert!(
        set.entries()
            .iter()
            .any(|entry| entry.kind == RootKind::Commit),
        "the commit roots must still be enumerated"
    );
}

/// The premise of §19, measured rather than assumed: while maintenance holds the overlay, a **second**
/// connection to that file cannot read `ov_meta`.
///
/// If this ever starts succeeding, the ruling's reason has changed and the shape of `Overlays` should
/// be revisited — which is why the assertion is here and not in a comment.
#[test]
fn a_second_connection_to_a_maintenance_held_overlay_is_refused() {
    common::strict_gate();
    let (root, repo, overlay) = dirty_workspace("premise");
    drop(repo);

    let maintenance = MaintenanceRepo::open(root.store()).expect("maintenance");
    assert!(maintenance.overlays_held() >= 1, "one overlay was taken");

    assert!(
        overlay.is_file(),
        "the overlay exists at {}",
        overlay.display()
    );

    let second =
        rusqlite::Connection::open_with_flags(&overlay, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("opening the file itself succeeds");
    // Fail fast rather than after the default timeout: the point is that the lock is *exclusive*,
    // not that this connection eventually gave up.
    second
        .pragma_update(None, "busy_timeout", 0)
        .expect("busy timeout");
    let read = second.query_row(
        "SELECT value FROM ov_meta WHERE key = 'pending_root'",
        [],
        |row| row.get::<_, String>(0),
    );
    assert!(
        read.is_err(),
        "a second connection read a maintenance-held overlay; the lock is not exclusive and §19's \
         premise no longer holds"
    );
}

/// The everyday path still works, and the empty-`pending_root` bug stays fixed: after a commit the
/// workspace is clean, so there is no `OverlayPending` row at all — not an id parsed from `""`.
#[test]
fn a_committed_workspace_contributes_no_pending_root() {
    common::strict_gate();
    let root = TempRoot::new("clean");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"committed bytes");
    workspace.commit("one", false).expect("commit");
    drop(workspace);

    let set = RepoRoots { repo: &repo }
        .roots()
        .expect("roots of a clean workspace");
    assert!(
        !set.entries()
            .iter()
            .any(|entry| entry.kind == RootKind::OverlayPending),
        "a clean workspace has no pending root; an empty string is not an object id: {:?}",
        set.entries()
            .iter()
            .map(|entry| (entry.kind, entry.label.clone()))
            .collect::<Vec<_>>()
    );
    assert!(set
        .entries()
        .iter()
        .any(|entry| entry.kind == RootKind::Commit));
}

// ------------------------------------------------- two enumerations, deliberately different

/// `Repo::roots()` (what `verify` reads) **never touches an overlay**: it is a UNION of three
/// main-database columns, so it answers in microseconds even while a workspace holds its overlay.
///
/// Pinned because the asymmetry is easy to forget: the bare list is *not* the GC root set, and using
/// it for GC would silently protect nothing from a pending publication.
#[test]
fn the_bare_root_list_works_while_a_workspace_is_open_and_omits_overlay_roots() {
    common::strict_gate();
    let root = TempRoot::new("bare");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"uncommitted bytes");

    let bare = repo.roots().expect("the bare list must not touch overlays");
    assert!(!bare.is_empty(), "the commit and base roots are there");
    let labelled = RepoRoots { repo: &repo }.roots();
    assert!(
        labelled.is_err(),
        "the labelled enumeration needs the overlay, which this workspace is holding; got {:?}",
        labelled.map(|set| set.entries().len())
    );

    drop(workspace);
    let after = RepoRoots { repo: &repo }
        .roots()
        .expect("roots after the workspace closes");
    assert!(
        after.entries().len() > bare.len(),
        "the labelled enumeration carries one row per fact ({} bare vs {} labelled)",
        bare.len(),
        after.entries().len()
    );
}

/// The failure while a workspace is open must be **loud**: a silently omitted overlay root is a
/// subtree the sweep would delete. This is the property, not the error class.
#[test]
fn a_live_overlay_is_never_silently_omitted() {
    common::strict_gate();
    let root = TempRoot::new("loud");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    put_file(&workspace, "/f", b"uncommitted bytes");

    let result = RepoRoots { repo: &repo }.roots();
    match result {
        Err(error) => assert!(
            matches!(error, fs_workspace::WorkspaceError::Busy { .. }),
            "expected Busy (the everyday path's vocabulary), got {error:?}"
        ),
        Ok(set) => {
            panic!(
            "a live overlay was enumerated anyway; if this is now supported, prove the read is \
             still consistent: {:?}",
            set.entries().iter().map(|e| e.label.clone()).collect::<Vec<_>>()
        )
        }
    }
}
