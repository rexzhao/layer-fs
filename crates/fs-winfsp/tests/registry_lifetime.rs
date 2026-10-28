//! The mount registry's lifetime rule, as assertions (P3-4 batch (i)).
//!
//! The rule exists because a callback can arrive on a dispatcher thread at any time: `FSP_FILE_SYSTEM*` is the only key it
//! carries, and reading a freed key is the dangling-registry failure. So the order is fixed — register after `create`,
//! unregister after `stop` **and before** `delete` — and the last line of the sequence is asserted here.

use std::sync::Arc;

use fs_winfsp::ops::Path;
use fs_winfsp::registry::Registry;
use fs_winfsp::{Adapter, MockBackend};

fn adapter() -> Arc<Adapter<MockBackend>> {
    Arc::new(Adapter::new(MockBackend::new()))
}

/// The sequence a mount performs, with the assertion that closes the lifetime rule.
#[test]
fn a_mount_is_unregistered_before_it_is_deleted() {
    let registry: Registry<MockBackend> = Registry::new();
    let key = 0x1000usize;

    // create → register.
    assert!(
        registry.insert(key, adapter()).is_none(),
        "cell create: the first registration finds the key free"
    );
    assert!(
        registry.contains(key),
        "cell live: a mounted volume is findable by its pointer"
    );
    assert_eq!(registry.len(), 1);

    // stop → unregister (a callback after this must find nothing).
    assert!(
        registry.remove(key),
        "cell stop: the mount was there to remove"
    );
    assert!(
        !registry.contains(key),
        "cell stopped: **no key after `stop`** — a callback arriving now must not find a mount"
    );

    // delete → the key must still be gone; this is the assertion the teardown path makes.
    assert!(
        !registry.contains(key),
        "cell deleted: after `delete` the registry must not answer for that pointer"
    );
    assert!(registry.is_empty(), "cell empty: nothing left registered");
}

/// Two mounts coexist and are looked up independently — the property that removes the old process-wide
/// configuration and with it the tests' sequential dependency.
#[test]
fn two_mounts_are_independent() {
    let registry: Registry<MockBackend> = Registry::new();
    let first = adapter();
    let second = adapter();
    registry.insert(0x1000, Arc::clone(&first));
    registry.insert(0x2000, Arc::clone(&second));

    assert_eq!(
        registry.len(),
        2,
        "cell coexist: both mounts are registered"
    );
    assert!(
        Arc::ptr_eq(&registry.get(0x1000).expect("first"), &first),
        "cell lookup: each pointer answers its own adapter"
    );
    assert!(
        Arc::ptr_eq(&registry.get(0x2000).expect("second"), &second),
        "cell lookup: and the second one too"
    );

    registry.remove(0x1000);
    assert!(
        registry.contains(0x2000),
        "cell independent: removing one leaves the other mounted"
    );
    assert_eq!(
        registry.get(0x1000).map(|_| ()),
        None,
        "cell gone: the removed key answers nothing"
    );
}

/// Registering the same key twice returns the old adapter instead of overwriting silently: WinFsp reusing a pointer while a
/// mount is live is worth spotting.
#[test]
fn registering_a_taken_key_reports_the_previous_adapter() {
    let registry: Registry<MockBackend> = Registry::new();
    let first = adapter();
    registry.insert(0x1000, Arc::clone(&first));
    let previous = registry.insert(0x1000, adapter());

    let previous = previous.expect("cell collision: the previous adapter is handed back");
    assert!(
        Arc::ptr_eq(&previous, &first),
        "cell collision: and it is the one that was registered before"
    );
    assert_eq!(
        registry.len(),
        1,
        "cell collision: the map still holds exactly one mount per key"
    );
}

/// A pointer that was never registered answers `None` — never "a default configuration", which is what a process-wide
/// `OnceLock` would have answered.
#[test]
fn an_unregistered_pointer_answers_nothing() {
    let registry: Registry<MockBackend> = Registry::new();
    assert_eq!(
        registry.get(0xDEAD).map(|_| ()),
        None,
        "cell unknown: no adapter, no fallback"
    );
}

/// **The configuration belongs to the mount** (352): two adapters injected with different hooks answer differently, and an
/// adapter with none answers "nothing" rather than sporting somebody else's configuration.
#[test]
fn configuration_belongs_to_the_mount() {
    use fs_winfsp::adapter::AdapterConfig;

    let first = Adapter::with_config(
        MockBackend::new(),
        AdapterConfig {
            resolve: Some(|path| (path == "/first").then_some(1)),
            ..AdapterConfig::default()
        },
    );
    let second = Adapter::with_config(
        MockBackend::new(),
        AdapterConfig {
            resolve: Some(|path| (path == "/second").then_some(2)),
            ..AdapterConfig::default()
        },
    );

    let first_resolve = first
        .config()
        .resolve
        .expect("first: a resolver was injected");
    let second_resolve = second.config().resolve.expect("second: its own resolver");
    assert_eq!(
        first_resolve("/first"),
        Some(1),
        "cell first: answers its own path"
    );
    assert_eq!(
        first_resolve("/second"),
        None,
        "cell first: and not the other mount's path — the configuration is not shared"
    );
    assert_eq!(
        second_resolve("/second"),
        Some(2),
        "cell second: answers its own path"
    );

    // An adapter built without configuration answers nothing at all — the answer a process-wide static could not give.
    let bare: Adapter<MockBackend> = Adapter::new(MockBackend::new());
    assert!(
        bare.config().resolve.is_none() && bare.config().lookup.is_none(),
        "cell bare: no configuration means `None`, never a leftover from another mount"
    );
}

/// A miss is **observable** (defence and observation are different jobs): the lookup still answers `None`, and the counter
/// records that it happened, so a dangling callback cannot hide behind "some refusal".
#[test]
fn a_missing_lookup_is_counted() {
    use fs_winfsp::registry::miss_count;

    let registry: Registry<MockBackend> = Registry::new();
    let before = miss_count();
    assert_eq!(registry.get(0xBAD).map(|_| ()), None);
    assert_eq!(
        miss_count() - before,
        1,
        "cell miss: an unregistered pointer is counted, not just refused"
    );
    // A hit does not count.
    registry.insert(0x1000, adapter());
    let before = miss_count();
    assert!(registry.get(0x1000).is_some());
    assert_eq!(
        miss_count() - before,
        0,
        "cell hit: a successful lookup is not a miss"
    );
}

/// 300 migrates here: context counts are **per mount**, so the question "did this mount balance" is answerable without
/// reading a process-wide counter that other mounts also touch.
#[test]
fn context_counts_are_per_mount() {
    let first = adapter();
    let second = adapter();

    first.note_context_allocated();
    first.note_context_freed();
    second.note_context_allocated();

    assert_eq!(
        first.context_counts(),
        (1, 1),
        "cell first: this mount allocated one and freed one — balanced"
    );
    assert_eq!(
        second.context_counts(),
        (1, 0),
        "cell second: and this one is still holding a context, which the first mount's numbers cannot hide"
    );
}

/// The volume answer is per-mount configuration too: two adapters can describe different volumes, and a bare adapter describes
/// none — the property a process-wide static could not provide (352).
#[test]
fn the_volume_answer_belongs_to_the_mount() {
    use fs_winfsp::abi::VolumeInfo;
    use fs_winfsp::adapter::AdapterConfig;

    let first = Adapter::with_config(
        MockBackend::new(),
        AdapterConfig {
            volume: Some(VolumeInfo {
                total_size: 1 << 20,
                free_size: 1 << 19,
                volume_label_length: 0,
                volume_label: [0; 32],
            }),
            ..AdapterConfig::default()
        },
    );
    let second: Adapter<MockBackend> = Adapter::new(MockBackend::new());

    assert_eq!(
        first.config().volume.map(|volume| volume.total_size),
        Some(1 << 20),
        "cell first: its own volume size"
    );
    assert!(
        second.config().volume.is_none(),
        "cell second: a bare mount describes no volume — no borrowed answer"
    );
}

/// `DirOutcome` — an identity and a binding, answered **once**.
///
/// The pairing is the criterion: the id the door answers and the id the binding resolves to are the same question asked twice, and a value that
/// carries both makes a disagreement observable instead of silent. The negative half (a path that names nothing has no binding at all) is what keeps
/// `None` meaning "cannot tell" rather than becoming a binding that fails later, at `remove_at`.
#[test]
fn dir_outcome_pairs_the_identity_with_the_binding() {
    use fs_winfsp::mock::MockBackend;
    use fs_winfsp::ops::WorkspaceOps;

    let backend = MockBackend::new();
    let dir = backend.mkdir(&Path::new(b"/d")).expect("mkdir");
    let outcome = backend
        .dir_binding(&Path::new(b"/d"))
        .expect("a directory that exists has a binding");
    assert_eq!(
        outcome.id, dir,
        "the binding resolves to the entry the door answered"
    );
    assert_eq!(outcome.binding.0, 0, "the parent is the root");
    assert_eq!(outcome.binding.1, b"d".to_vec(), "the raw name bytes");

    assert!(
        backend.dir_binding(&Path::new(b"/nope")).is_none(),
        "a path that names nothing has no binding — not a name that would fail later"
    );
}

/// `mount_serving` hands the closure a [`Mounted`] **while the volume is up**, and `open_handles` reports what the handle table holds.
///
/// The number matters twice over: `status.handles` reports it, and a **remote** `umount` refuses on it (ADR-010 §7.2) — the caller asking over the pipe cannot
/// see who has a file open, so the answer has to come from the mount that can.
#[test]
fn mount_serving_sees_its_own_open_handles() {
    use std::path::{Path, PathBuf};

    use fs_winfsp::abi::VolumeInfo;
    use fs_winfsp::loader::{default_library, resolve_library, LoadError};
    use fs_winfsp::mock::MockBackend;
    use fs_winfsp::mounted::mount_serving;
    use fs_winfsp::ops::DynOps;

    let library = default_library();
    if let Err(LoadError::NotFound { path }) = resolve_library(&library) {
        eprintln!("SKIP: no winfsp at {}", path.display());
        return;
    }
    let mount_point = std::env::var_os("LAYER_FS_MNT_SERVING")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("O:"));
    let volume = VolumeInfo {
        total_size: 1 << 20,
        free_size: 1 << 19,
        volume_label_length: 4,
        volume_label: {
            let mut label = [0u16; 32];
            label[..4].copy_from_slice(&[b'w' as u16, b'r' as u16, b'i' as u16, b't' as u16]);
            label
        },
    };
    let backend: DynOps = Box::new(MockBackend::new());
    let report = mount_serving(&library, &mount_point, backend, volume, |mounted: &_| {
        let root = if mounted.path().to_string_lossy().ends_with(':') {
            PathBuf::from(format!("{}\\", mounted.path().display()))
        } else {
            PathBuf::from(mounted.path())
        };
        // Nothing open yet: the closure runs before anyone has touched the volume.
        assert_eq!(mounted.open_handles(), 0, "cell start: no handles");

        std::fs::write(root.join("open.txt"), b"x").expect("write");
        let held = std::fs::File::open(root.join("open.txt")).expect("open");
        let handles = mounted.open_handles();
        assert!(
            handles >= 1,
            "cell open: a file opened through the mount must be visible to the mount (got {handles})"
        );
        drop(held);
        Ok(())
    })
    .expect("the mount path is available");
    assert_eq!(report.mount, 0, "cell mounted");
    let _ = Path::new(&mount_point);
}
