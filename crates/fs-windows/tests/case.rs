//! `case` tests (P3-1 §1): exact spelling wins, folded match returns the stored spelling, ambiguity is refused.

use fs_windows::case::{collision, fold, lookup, Lookup};

#[test]
fn exact_spelling_wins_over_a_folded_match() {
    let entries = ["README", "readme"];
    assert_eq!(
        lookup(&entries, "readme"),
        Lookup::Exact("readme"),
        "cell wanted=readme"
    );
    assert_eq!(
        lookup(&entries, "README"),
        Lookup::Exact("README"),
        "cell wanted=README"
    );
}

#[test]
fn a_single_folded_match_returns_the_stored_spelling() {
    let entries = ["README.md"];
    assert_eq!(
        lookup(&entries, "readme.MD"),
        Lookup::Folded("README.md"),
        "cell wanted=readme.MD: case-insensitive match, stored spelling preserved"
    );
}

#[test]
fn two_entries_that_fold_together_are_ambiguous_not_merged() {
    let entries = ["Readme", "readme"];
    assert_eq!(lookup(&entries, "readme"), Lookup::Exact("readme"));
    assert_eq!(
        lookup(&entries, "READ"),
        Lookup::Missing,
        "cell wanted=READ: no entry folds to READ"
    );
    let no_exact = ["Readme", "READMEX"];
    assert_eq!(lookup(&no_exact, "READMEX"), Lookup::Exact("READMEX"));
    let ambiguous = ["Readme", "READMe"];
    assert_eq!(
        lookup(&ambiguous, "readme"),
        Lookup::Ambiguous,
        "cell wanted=readme alongside Readme/READMe"
    );
}

/// The Unicode policy, pinned: ASCII folds, non-ASCII does not. Written as an assertion (not a comment), so
/// the documented deviation cannot drift silently — when a real upcase table lands, this is the test that has
/// to change.
#[test]
fn folding_is_ascii_only_and_that_is_the_documented_policy() {
    assert_eq!(fold("Readme"), fold("readme"), "cell ASCII: case folds");
    assert_ne!(
        fold("\u{00C4}"),
        fold("\u{00E4}"),
        "cell Latin-1 supplement: not folded (documented gap)"
    );
    assert_ne!(
        fold("\u{0410}"),
        fold("\u{0430}"),
        "cell Cyrillic: not folded (documented gap)"
    );
    // The deviation can only make this model *stricter*: two names Windows would merge stay distinct here,
    // never the other way round (which would let a collision through).
    assert_eq!(
        lookup(&["\u{00C4}"], "\u{00E4}"),
        Lookup::Missing,
        "cell wanted=lower-a-umlaut alongside upper: distinct in this model"
    );
}

#[test]
fn unicode_is_not_normalized() {
    // U+00E9 vs "e" + U+0301: Windows treats these as different names, and so must this model.
    let composed = "caf\u{00E9}";
    let decomposed = "cafe\u{0301}";
    assert_ne!(
        fold(composed),
        fold(decomposed),
        "folding must not normalize Unicode"
    );
    assert_eq!(lookup(&[composed], decomposed), Lookup::Missing);
}

#[test]
fn a_collision_names_the_existing_spelling() {
    let entries = ["Readme"];
    assert_eq!(
        collision(&entries, "readme"),
        Some("Readme"),
        "cell candidate=readme: the collision must report the stored name"
    );
    assert_eq!(collision(&entries, "readme2"), None);
}
