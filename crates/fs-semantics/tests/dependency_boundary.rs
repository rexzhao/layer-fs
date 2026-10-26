//! The boundary of `fs-semantics` (P3-2 §12): shared semantics, **zero dependencies**, and one table repo-wide.
//!
//! Why this is a test and not a paragraph: the reason the table moved here is that the Windows frontend must not
//! inherit a C toolchain through `fs-namespace`'s SQLite. If somebody later adds a dependency "just for one helper",
//! the crate still compiles — the boundary would only be discovered by whoever next builds the frontend on a machine
//! without a compiler. So the manifest is read and checked.

use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn the_crate_has_no_dependencies() {
    let manifest =
        std::fs::read_to_string(crate_root().join("Cargo.toml")).expect("read Cargo.toml");
    let mut in_dependencies = false;
    let mut offenders = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]" || trimmed.starts_with("[dependencies.");
            continue;
        }
        if in_dependencies && !trimmed.is_empty() && !trimmed.starts_with('#') {
            offenders.push(trimmed.to_owned());
        }
    }
    assert!(
        offenders.is_empty(),
        "cell dependencies: `fs-semantics` must stay dependency-free (P3-2 §12), found {offenders:?}"
    );
}

/// `FileId` (and anything else from the tree/overlay model) must **not** live here: it would drag the model in and
/// break the zero-dependency boundary that makes this crate usable from the frontend.
#[test]
fn no_file_id_or_model_types_are_defined_here() {
    let mut offenders = Vec::new();
    let mut stack = vec![crate_root().join("src")];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read src").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            for (index, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // Declaring the type here is what would create the dependency; *mentioning* it in prose is fine.
                if line.contains("enum FileId")
                    || line.contains("struct FileId")
                    || line.contains("pub struct InodeRecord")
                {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "cell model-types: the tree model stays in fs-namespace, only the semantics table belongs here: {offenders:?}"
    );
}

/// The rule used by the table scan, extracted so the controls exercise **the same** code as the sweep.
fn defines_disposition_table(line: &str) -> bool {
    !line.trim_start().starts_with("//")
        && line.contains("pub const TABLE")
        && line.contains("Disposition")
}

/// One table, repo-wide: a second `pub const TABLE` describing dispositions is the drift this move exists to end.
#[test]
fn the_disposition_table_is_defined_once_in_the_repository() {
    let crates = crate_root().parent().expect("crates/").to_path_buf();
    let mut definitions = Vec::new();
    let mut stack = vec![crates];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read crates/").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                // This file names the pattern it looks for, so it would always match itself: excluded, with the
                // reason stated here rather than being a silent special case.
                && path.file_name().is_none_or(|name| name != "dependency_boundary.rs")
            {
                let text = std::fs::read_to_string(&path).expect("read a source file");
                for (index, line) in text.lines().enumerate() {
                    if defines_disposition_table(line) {
                        definitions.push(format!("{}:{}", path.display(), index + 1));
                    }
                }
            }
        }
    }
    assert_eq!(
        definitions.len(),
        1,
        "cell single-table: the 18-cell disposition table must be defined exactly once, found {definitions:?}"
    );
}

/// **Positive and negative controls** for the single-table rule.
///
/// Without them the sweep proves only that *some* loop ran: an over-eager rule (matching every `pub const TABLE`,
/// including the ones for other domains) and a dead one would both report a plausible list. The two cases below pin
/// the rule itself, and they call the same `defines_disposition_table` the sweep uses.
#[test]
fn the_single_table_rule_has_both_controls() {
    // Positive: the exact shape this repository defines.
    assert!(
        defines_disposition_table("pub const TABLE: [(Disposition, Exists, Outcome); 18] = ["),
        "cell control=positive: a disposition table definition must be detected"
    );
    // Negative: another table, and a comment mentioning the pattern.
    assert!(
        !defines_disposition_table("pub const TABLE: [(Kind, Mode); 4] = ["),
        "cell control=negative-other-domain: a table for another domain is not this rule's business"
    );
    assert!(
        !defines_disposition_table("// pub const TABLE: [(Disposition, Exists, Outcome); 18] = ["),
        "cell control=negative-comment: a commented-out definition is not a definition"
    );
}
