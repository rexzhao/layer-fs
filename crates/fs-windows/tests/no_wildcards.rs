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
///
/// **One space.** The first version wrote `format!("_{} =>", ' ')`, which produced two spaces (`_  =>`) and
/// therefore matched nothing, anywhere, ever — a scan that cannot fail. That is the same failure mode as a
/// probe that was never wired, and it is why the positive control below scans a *planted* string with the very
/// same rule instead of asserting that a string contains what it was built from.
fn forbidden() -> String {
    format!("_{}", " =>")
}

/// The scan rule itself, over one text. Used for real sources **and** for the planted controls, so the control
/// exercises the production rule rather than a paraphrase of it.
fn hits(text: &str, needle: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(index, line)| format!("{}: {}", index + 1, line.trim()))
        .collect()
}

/// Every `.rs` file under `dir`, recursively.
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
    // The needle must be the token the compilers sees, character for character.
    assert_eq!(needle.chars().count(), 4, "the needle is four characters");
    assert_eq!(
        needle,
        concat!("_", " ", "=", ">"),
        "exactly one space between the hole and the arrow"
    );

    let mut offenders = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a source file");
        for hit in hits(&text, &needle) {
            offenders.push(format!("{}:{}", file.display(), hit));
        }
    }
    assert!(
        offenders.is_empty(),
        "a fallback arm defeats the exhaustive-match guarantee; classify the variant instead:\n{}",
        offenders.join("\n")
    );
}

/// The scan must be able to **fail**: a planted text containing the token is reported by the same rule, and a
/// clean text is not. Without both halves, a broken scanner reports "clean" forever — which is exactly what the
/// two-space needle did.
#[test]
fn the_scanner_reports_a_planted_token_and_stays_quiet_otherwise() {
    let needle = forbidden();

    // Built with `concat!` so this file still does not contain the token contiguously.
    let planted = concat!(
        "match value { 1 => \"one\", ",
        "_",
        " ",
        "=",
        "> \"other\" }"
    );
    let reported = hits(planted, &needle);
    assert_eq!(
        reported.len(),
        1,
        "the planted token must be reported: {reported:?}"
    );

    let clean = "match value { 1 => \"one\", 2 => \"two\" }";
    assert!(
        hits(clean, &needle).is_empty(),
        "a text without the token must produce no hits"
    );

    // And the real sources are scanned with the same rule, which is what makes the control above meaningful.
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(
        files.len() >= 8,
        "the crate has at least eight source files"
    );
}
