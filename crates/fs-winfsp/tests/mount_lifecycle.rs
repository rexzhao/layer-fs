//! create → start → stop → delete, **without mounting** (P3-4 step ③).
//!
//! A failure here is about the library or our structures, never about a callback: that is the point of doing it before any
//! mount point exists.

use fs_winfsp::loader::{default_library, resolve_library, LoadError};
use fs_winfsp::mount::lifecycle;

#[test]
fn the_library_lifecycle_completes_without_a_mount_point() {
    let path = default_library();
    let path = path.as_path();
    if let Err(LoadError::NotFound { path }) = resolve_library(path) {
        eprintln!(
            "SKIP: winfsp library not installed at {} — lifecycle not exercised (skip, not pass)",
            path.display()
        );
        return;
    }
    let report = lifecycle(path).expect("the four lifecycle calls are available");
    assert_eq!(
        report.create, 0,
        "cell create: FspFileSystemCreate returned STATUS_SUCCESS"
    );
    assert_eq!(
        report.start, 0,
        "cell start: the dispatcher started with WinFsp's own thread count"
    );
    // The stop is not observable at this level, and the report says so rather than implying it was verified.
    assert!(
        !report.stop_is_observable,
        "cell stop: WinFsp 2.x exposes no query for the dispatcher threads ending — recorded as not observable, not as verified"
    );
}

/// After the whole cycle the library must still be usable: the module is freed, and reading the version again proves we did
/// not leave a broken state behind (the same health check the handshake uses).
#[test]
fn the_library_still_works_after_the_cycle() {
    let path = default_library();
    let path = path.as_path();
    if let Err(LoadError::NotFound { .. }) = resolve_library(path) {
        eprintln!("SKIP: winfsp library not installed — post-cycle health check not exercised (skip, not pass)");
        return;
    }
    lifecycle(path).expect("lifecycle");
    let version =
        fs_winfsp::handshake::read_version(path).expect("the library still reports its version");
    assert!(
        version >> 16 == 2,
        "cell health: after create/start/stop/delete the same WinFsp 2.x answers — no stuck state"
    );
}
