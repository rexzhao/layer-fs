//! **G5** — nothing outside `fs-workspace` can reach the overlay, and nothing inside hands it out.
//!
//! The publication gate only means something if **every** data path goes through it. Two doors existed before S2 (`Workspace::overlay()` used by `fs-api` for
//! `set_times`/`set_attributes`, and the public accessor itself); both were closed in step 2. A criterion is what keeps them closed, because the next person to
//! need "just one field" will find the escape hatch attractive.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives under crates/")
        .to_path_buf()
}

/// Every `.rs` file under `crates/*/src`, with its text.
fn source_files() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let crates = workspace_root().join("crates");
    for entry in std::fs::read_dir(&crates).expect("crates/").flatten() {
        let src = entry.path().join("src");
        if !src.is_dir() {
            continue;
        }
        collect(&src, &mut out);
    }
    out
}

fn collect(directory: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(directory)
        .expect("a source directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            out.push((path, text));
        }
    }
}

/// **G5**: `.overlay()` is not called outside `fs-workspace`.
///
/// Inside, `Workspace`'s own gate-protected methods call it — that is the point of the crate boundary: the overlay is reachable, but only from code that has
/// taken the gate first.
#[test]
fn nothing_outside_the_workspace_crate_reaches_the_overlay() {
    let mut offenders = Vec::new();
    for (path, text) in source_files() {
        if path
            .components()
            .any(|part| part.as_os_str() == "fs-workspace")
        {
            continue;
        }
        for (number, line) in text.lines().enumerate() {
            // A call, not a mention: the criteria and this file talk about `.overlay()` by name, and so do comments.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains(".overlay()") {
                offenders.push(format!("{}:{}", path.display(), number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "cell G5: the overlay must not be reachable outside fs-workspace, found {offenders:?}"
    );
}

/// **G5 (the other half)**: the accessor inside the crate is **not** `pub`.
#[test]
fn the_accessor_inside_the_crate_is_crate_private() {
    let path = workspace_root().join("crates/fs-workspace/src/workspace.rs");
    let text = std::fs::read_to_string(&path).expect("workspace.rs");
    let mut public = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.contains("fn overlay(") && trimmed.starts_with("pub fn") {
            public.push(format!("{}:{}", path.display(), number + 1));
        }
    }
    assert!(
        public.is_empty(),
        "cell G5: `overlay()` must stay crate-private, found {public:?}"
    );
}
