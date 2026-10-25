//! `dirent` tests (P3-1 §1): pages concatenate to the directory, without repeats or omissions.

use fs_windows::dirent::{all_pages, page};
use fs_windows::error::Failure;

fn names(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("entry{index:04}")).collect()
}

/// Pages of every size over a 1000-entry listing must concatenate to exactly the listing.
#[test]
fn every_page_size_reconstructs_the_whole_directory() {
    let owned = names(1000);
    let listing: Vec<&str> = owned.iter().map(String::as_str).collect();
    for limit in [1usize, 2, 3, 7, 64, 999, 1000, 1001] {
        let seen = all_pages(&listing, limit).expect("enumeration succeeds");
        assert_eq!(
            seen.len(),
            listing.len(),
            "cell limit={limit}: page size must not lose or duplicate entries"
        );
        assert_eq!(
            seen, owned,
            "cell limit={limit}: the concatenation must equal the listing"
        );
    }
}

/// The marker is exclusive: the next page starts strictly after it, never repeating it.
#[test]
fn a_marker_is_never_returned_twice() {
    let listing = ["a", "b", "c"];
    let first = page(&listing, None, 2).expect("page");
    assert_eq!(
        first.entries,
        vec!["a".to_owned(), "b".to_owned()],
        "cell limit=2"
    );
    let second = page(&listing, first.next.as_deref(), 2).expect("page");
    assert_eq!(second.entries, vec!["c".to_owned()], "cell after=b");
    assert_eq!(second.next, None, "the last page carries no marker");
}

/// `.` and `..` are synthesized by Windows; a store that contains them must not leak them.
#[test]
fn dot_entries_are_filtered() {
    let listing = [".", "..", "real"];
    assert_eq!(
        all_pages(&listing, 10).expect("pages"),
        vec!["real".to_owned()],
        "cell listing with . and .."
    );
}

/// A marker that vanished between pages must not make enumeration fatal, and must not restart it.
#[test]
fn a_vanished_marker_resumes_after_the_name() {
    let listing = ["a", "b", "c", "d"];
    let resumed = page(&listing, Some("b"), 2).expect("page");
    assert_eq!(
        resumed.entries,
        vec!["c".to_owned(), "d".to_owned()],
        "cell after=b"
    );
}

/// A zero-sized page is refused: honouring it would trap the caller in an endless loop.
#[test]
fn a_zero_limit_is_refused() {
    assert_eq!(
        page(&["a"], None, 0),
        Err(Failure::InvalidParameter),
        "cell limit=0"
    );
}
