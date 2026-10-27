//! `Path` — the one place that decides "what to match by" and "what to hand back".

use fs_winfsp::ops::Path;

#[test]
fn the_key_folds_and_the_display_does_not() {
    let path = Path::new(b"/ReadMe.txt");
    assert_eq!(path.key(), b"/README.TXT");
    assert_eq!(path.display(), b"/ReadMe.txt");
}

#[test]
fn a_rename_that_only_changes_case_keeps_the_same_key() {
    // This is the property 08 depends on at the semantic layer: two spellings, one entry.
    let from = Path::new(b"/dir/readme.txt");
    let to = Path::new(b"/dir/README.TXT");
    assert_eq!(from.key(), to.key());
    assert_ne!(from.display(), to.display());
    assert_eq!(to.display_name(), Some(&b"README.TXT"[..]));
}

#[test]
fn the_name_is_the_display_last_component() {
    assert_eq!(Path::new(b"/a/b/c.txt").display_name(), Some(&b"c.txt"[..]));
    // The root and a trailing separator have no name — answering `b""` would store an empty name.
    assert_eq!(Path::new(b"/").display_name(), None);
    assert_eq!(Path::new(b"/a/").display_name(), None);
}

#[test]
fn bytes_that_are_not_utf8_still_work() {
    // Names are whatever the caller wrote; refusing to fold one would make an entry we created unreachable.
    let path = Path::new(b"/\xff\xfeAb");
    assert_eq!(path.key(), b"/\xff\xfeAB");
    assert_eq!(path.display_name(), Some(&b"\xff\xfeAb"[..]));
}

