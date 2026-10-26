//! The eighteen-cell disposition table now lives in the core (P3-2 §12), so its "missing cell is red" check
//! lives here too — with the rows/columns/product assertions split, and literals on the right-hand side.

use fs_semantics::disposition::{decide, Disposition, Exists, Outcome, TABLE};

#[test]
fn every_cell_of_the_product_has_a_row_and_matches_decide() {
    let mut visited = 0usize;
    for disposition in Disposition::ALL {
        for exists in Exists::ALL {
            visited += 1;
            let row = TABLE
                .iter()
                .find(|(d, e, _)| *d == disposition && *e == exists)
                .unwrap_or_else(|| {
                    panic!("{disposition:?} × {exists:?} has no row: the cell is unclassified")
                });
            assert_eq!(
                decide(disposition, exists),
                row.2,
                "cell disposition={disposition:?} exists={exists:?}: code and table disagree"
            );
        }
    }

    // Literals, not enum lengths: deriving either side from the enums moves both at once.
    assert_eq!(
        Disposition::ALL.len(),
        6,
        "enum cardinality: six NT dispositions"
    );
    assert_eq!(
        Exists::ALL.len(),
        3,
        "enum cardinality: absent, file, directory"
    );

    let rows: std::collections::BTreeSet<Disposition> = TABLE
        .iter()
        .map(|(disposition, _, _)| *disposition)
        .collect();
    let columns: std::collections::BTreeSet<Exists> =
        TABLE.iter().map(|(_, exists, _)| *exists).collect();
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
    assert_eq!(visited, 18, "the walk must visit every cell exactly once");
}

/// SUPERSEDE and OVERWRITE_IF take the **same action** on an existing file (ADR-009 §4.2, owner ruling (a)).
///
/// They used to differ (replace-the-entry vs reuse-it); on a real Windows system both arrive as an overwrite of the same
/// file object, so the entry keeps its identity. The difference that remains is the **attribute replacement policy**,
/// which belongs to the caller performing the truncation, not to this table — that is why this test no longer has an
/// "new FileId" cell to assert.
#[test]
fn supersede_and_overwrite_if_take_the_same_action_on_a_file() {
    assert_eq!(
        decide(Disposition::Supersede, Exists::File),
        Outcome::Opened(fs_semantics::disposition::Action::TruncateExisting),
        "cell disposition=Supersede exists=File: truncated in place (same FileId)"
    );
    assert_eq!(
        decide(Disposition::OverwriteIf, Exists::File),
        Outcome::Opened(fs_semantics::disposition::Action::TruncateExisting),
        "cell disposition=OverwriteIf exists=File: also in place — the two cells are now equal"
    );
    assert_eq!(
        decide(Disposition::Supersede, Exists::File),
        decide(Disposition::OverwriteIf, Exists::File),
        "cell equality: the table may not re-introduce a difference that the OS does not have"
    );
}
