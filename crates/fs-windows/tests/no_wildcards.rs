//! Self-referential source scan (P3-1 QA, blocking): the "compile-time guarantee" must not be undermined by a
//! fallback arm appearing anywhere in this crate.
//!
//! Why a text scan and not a type trick: the guarantee is "a new variant will not compile until it is
//! classified", and a fallback arm defeats exactly that. Rust has no way to assert "this match has no
//! wildcard", so the crate checks its own sources. The scan is textual on purpose — that means it also
//! rejects the token in comments, which is deliberate: a reader (and a reviewer) must be able to trust
//! `grep` here, and a comment that spells the token trains people to ignore a real hit.

use std::path::{Path, PathBuf};

/// The forbidden token, assembled at runtime so this file does not itself contain it.
fn forbidden() -> String {
    // `_` + ` ` + `=>`
    format!("_{} =>", ' ')
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src").flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn no_source_file_contains_a_fallback_arm() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(!files.is_empty(), "the scan must find the crate's sources");

    let needle = forbidden();
    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a source file");
        for (index, line) in text.lines().enumerate() {
            if line.contains(&needle) {
                offenders.push(format!("{}:{}: {}", file.display(), index + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a fallback arm defeats the exhaustive-match guarantee; classify the variant instead:\n{}",
        offenders.join("\n")
    );
}

/// The scan itself must be able to fail: a file that *does* contain the token has to be reported. Without
/// this, a broken scanner would report "clean" forever.
#[test]
fn the_scanner_notices_a_file_that_contains_the_token() {
    let needle = forbidden();
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );

    // The positive control uses the test's own text: it contains the token by construction.
    let planted = format!("match value {{ 1 => \"one\", {needle} \"other\" }}");
    let offenders: Vec<&PathBuf> = files.iter().filter(|_| planted.contains(&needle)).collect();
    assert_eq!(
        offenders.len(),
        files.len(),
        "the scanner's rule must flag a text that contains the token"
    );
}
