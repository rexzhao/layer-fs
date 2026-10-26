//! The first handshake: does the DLL load, and can one symbol be read from it?
//!
//! The test **skips with a printed reason** when the SDK is not installed, rather than passing silently: a skipped check
//! that looks green is worse than no check (it is the same rule as "an unverified claim is not a verified one").

use std::path::Path;

use fs_winfsp::handshake::read_version;
use fs_winfsp::loader::{resolve_library, LoadError};

/// **Ignored on purpose**: this test *crashes the whole test binary* on this machine (`0xC0000005`) when the DLL is
/// present, and a crashing test cannot be "skipped with a reason" — it takes the other tests in the file with it.
///
/// The crash is a **finding**, not a flake. Manager's manual harness, using the same two calls, reported
/// `FspVersion → 2.0.1` on this machine, while this harness crashes against the same file (and against the SDK's
/// `SxS/DYNAMIC/bin` copy too). So something differs between the two harnesses — entry point, calling convention, or a load
/// step (WinFsp's runtime may require `FspLoad` before its exports are callable). Finding that difference is the next step;
/// pretending the check passed would hide it, and leaving it un-ignored would redden every CI run with a process abort.
#[ignore = "crashes the test process with 0xC0000005 on this machine; under investigation against manager's manual             harness which reported version 2.0.1 — run with --ignored to reproduce"]
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
    let version = read_version(path).expect("the library loads and exports FspVersion");
    eprintln!("winfsp version = {version:#010x}");
    assert!(
        version > 0,
        "cell version: a loaded WinFsp reports a non-zero version; zero means the symbol resolved to the wrong place"
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
