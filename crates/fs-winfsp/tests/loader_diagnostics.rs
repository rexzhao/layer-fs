//! Load failures must name **which** problem happened (P3-4, process 277).
//!
//! The interesting property is not "we return an error" but "the error distinguishes absent / wrong-version / broken
//! load", because those have different fixes. The `LoadFailed` arm cannot be exercised without the OS, and is therefore
//! constructed directly here rather than faked through a real loader call.

use std::path::Path;

use fs_winfsp::loader::{resolve_library, LoadError};

#[test]
fn a_missing_library_names_the_path_it_tried() {
    let path = Path::new("F:/definitely/not/here/winfsp-x64.dll");
    match resolve_library(path) {
        Err(LoadError::NotFound { path: tried }) => assert_eq!(
            tried, path,
            "cell path: the error carries the path that was tried, not just \"not found\""
        ),
        other => panic!("cell missing: expected NotFound, got {other:?}"),
    }
}

#[test]
fn an_existing_file_passes_the_pre_check() {
    // The test's own source file exists and is definitely not a DLL: this asserts the *check* is about presence, and that
    // a later, deeper failure (wrong format) belongs to the loader arm rather than to this one.
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert_eq!(
        resolve_library(&here),
        Ok(()),
        "cell present: the pre-check only asks `is_file`"
    );
}

/// Each arm describes itself with the **kind** of problem first, so a log line points at the fix.
#[test]
fn every_arm_says_which_kind_of_problem_it_is() {
    let absent = LoadError::NotFound {
        path: "X:/sdk/winfsp-x64.dll".into(),
    };
    assert!(
        absent.describe().contains("not found") && absent.describe().contains("SDK"),
        "cell absent: reads as an installation problem: {}",
        absent.describe()
    );

    let old = LoadError::MissingExport {
        name: "FspFileSystemSetOperationGuardStrategy".into(),
    };
    assert!(
        old.describe().contains("does not export") && old.describe().contains("version"),
        "cell version: reads as a version problem: {}",
        old.describe()
    );

    let broken = LoadError::LoadFailed {
        path: "X:/sdk/winfsp-x64.dll".into(),
        last_error: 193,
    };
    assert!(
        broken.describe().contains("GetLastError = 193"),
        "cell load: carries the OS error (193 = wrong architecture): {}",
        broken.describe()
    );
}
