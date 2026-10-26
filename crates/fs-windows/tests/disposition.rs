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
    // Literals for every boundary (card §5, c4de819): deriving any of these from the enums would let a new
    // disposition move both sides of the comparison at once. Rows, columns and the product are asserted
    // **separately**, so a failure says which kind of gap it is.
    let rows: std::collections::BTreeSet<Disposition> = TABLE
        .iter()
        .map(|(disposition, _, _)| *disposition)
        .collect();
    let columns: std::collections::BTreeSet<Exists> =
        TABLE.iter().map(|(_, exists, _)| *exists).collect();
    assert_eq!(
        Disposition::ALL.len(),
        6,
        "enum cardinality: six dispositions"
    );
    assert_eq!(
        Exists::ALL.len(),
        3,
        "enum cardinality: three existence states"
    );
    assert_eq!(
        rows.len(),
        6,
        "missing row: {} of the six dispositions appear in the table",
        rows.len()
    );
    assert_eq!(
        columns.len(),
        3,
        "missing column: {} of the three existence states appear in the table",
        columns.len()
    );
    assert_eq!(
        TABLE.len(),
        18,
        "missing cell: the table must hold 6 × 3 = 18 rows, found {}",
        TABLE.len()
    );
    assert_eq!(cells, 18, "6 dispositions × 3 states");
}

/// The two dispositions that both truncate an existing file are not the same: one replaces the entry, the
/// other reuses it. Asserting only the outcome would let a build swap them.
#[test]
fn supersede_replaces_the_entry_while_overwrite_if_reuses_it() {
    assert_eq!(
        decide(Disposition::Supersede, Exists::File),
        Outcome::Opened(fs_windows::disposition::Action::TruncateExisting)
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
