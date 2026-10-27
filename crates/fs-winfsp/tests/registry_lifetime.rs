//! The mount registry's lifetime rule, as assertions (P3-4 batch (i)).
//!
//! The rule exists because a callback can arrive on a dispatcher thread at any time: `FSP_FILE_SYSTEM*` is the only key it
//! carries, and reading a freed key is the dangling-registry failure. So the order is fixed — register after `create`,
//! unregister after `stop` **and before** `delete` — and the last line of the sequence is asserted here.

use std::sync::Arc;

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
