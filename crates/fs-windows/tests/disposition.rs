//! `disposition` table tests (P3-1 §1): the whole 6×3 product, with no cell left to a wildcard.

use fs_windows::disposition::{decide, Disposition, Exists, Outcome, TABLE};

/// **Missing cell is red**: every (disposition, exists) pair has a row, and `decide` agrees with it.
///
/// This is the mechanism the card asks for (§2.1): the table is the specification, `decide` is the code, and
/// the test refuses to pass unless the two cover the *entire* product.
#[test]
fn every_cell_of_the_product_has_a_row_and_matches_decide() {
    let mut cells = 0usize;
    for disposition in Disposition::ALL {
        for exists in Exists::ALL {
            cells += 1;
            let row = TABLE
                .iter()
                .find(|(d, e, _)| *d == disposition && *e == exists)
                .unwrap_or_else(|| {
                    panic!("{disposition:?} × {exists:?} has no row: the cell is unclassified")
                });
            assert_eq!(
                decide(disposition, exists),
                row.2,
                "{disposition:?} × {exists:?}: code and table disagree"
            );
        }
    }
    assert_eq!(cells, 18, "6 dispositions × 3 states");
    assert_eq!(
        TABLE.len(),
        cells,
        "the table must have exactly one row per cell"
    );
}

/// The two dispositions that both truncate an existing file are not the same: one replaces the entry, the
/// other reuses it. Asserting only the outcome would let a build swap them.
#[test]
fn supersede_replaces_the_entry_while_overwrite_if_reuses_it() {
    assert_eq!(
        decide(Disposition::Supersede, Exists::File),
        Outcome::Opened(fs_windows::disposition::Action::ReplaceExisting)
    );
    assert_eq!(
        decide(Disposition::OverwriteIf, Exists::File),
        Outcome::Opened(fs_windows::disposition::Action::TruncateExisting)
    );
}

/// A directory is never replaced by these dispositions; each refuses, and the refusals differ on purpose
/// (a collision for CREATE, "not a directory" for the file-opening ones, denied for SUPERSEDE).
#[test]
fn a_directory_target_is_refused_with_the_disposition_specific_error() {
    for disposition in Disposition::ALL {
        match decide(disposition, Exists::Directory) {
            Outcome::Opened(action) => {
                panic!("{disposition:?} must not open a directory, got {action:?}")
            }
            Outcome::Failed(failure) => {
                let expected = match disposition {
                    Disposition::Create => fs_windows::error::Failure::NameCollision,
                    Disposition::Open
                    | Disposition::OpenIf
                    | Disposition::OverwriteIf
                    | Disposition::Overwrite => fs_windows::error::Failure::NotADirectory,
                    Disposition::Supersede => fs_windows::error::Failure::AccessDenied,
                };
                assert_eq!(failure, expected, "{disposition:?}");
            }
        }
    }
}
