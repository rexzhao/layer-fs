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
