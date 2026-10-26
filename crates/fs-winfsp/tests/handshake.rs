//! The first handshake: does the DLL load, and can one symbol be read from it?
//!
//! The test **skips with a printed reason** when the SDK is not installed, rather than passing silently: a skipped check
//! that looks green is worse than no check (it is the same rule as "an unverified claim is not a verified one").

use std::path::Path;

use fs_winfsp::handshake::read_version;
use fs_winfsp::loader::{resolve_library, LoadError};

#[test]
fn the_library_loads_and_reports_its_version() {
    let resolved = fs_winfsp::loader::default_library();
    let path = resolved.as_path();
    if let Err(LoadError::NotFound { path }) = resolve_library(path) {
        eprintln!(
            "SKIP: winfsp library not installed at {} — the handshake was not exercised (this is a skip, not a pass)",
            path.display()
        );
        return;
    }
    // Two assertions, because each alone can be satisfied by a wrong call: a version read from the wrong slot is positive
    // garbage, and an NTSTATUS-only check would pass for a zero version. `read_version` fails on a non-zero status, so
    // reaching this line already means `STATUS_SUCCESS`; the version still has to look like a WinFsp version.
    let version = read_version(path).expect("the library loads and exports FspVersion");
    eprintln!(
        "winfsp version = {version:#010x} ({}.{})",
        version >> 16,
        version & 0xffff
    );
    assert!(
        version > 0,
        "cell version: a loaded WinFsp reports a non-zero version; zero means the symbol resolved to the wrong place"
    );
    assert_eq!(
        version >> 16,
        2,
        "cell major: WinFsp 2.x is what this frontend targets — a different major means the layout assertions were made          against the wrong header"
    );
}

/// The three failure modes stay distinguishable at this level too: an absent library is `NotFound`, not a load failure.
#[test]
fn an_absent_library_is_not_a_load_failure() {
    let missing = Path::new("F:/definitely/not/here/winfsp-x64.dll");
    match read_version(missing) {
        Err(LoadError::NotFound { .. }) => {}
        other => panic!("cell absent: expected NotFound before any loader call, got {other:?}"),
    }
}
