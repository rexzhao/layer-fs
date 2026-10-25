//! `share` tests (P3-1 §1, §5): the whole 8×8 product against a hand-computed expectation.

use fs_windows::share::{admit, Admission, Mask, Term};

/// The expected answer computed **independently** of `admit`: two-holder rules written out from the
/// Windows definition, not from the implementation.
fn expected(
    new_desired: Mask,
    new_share: Mask,
    existing_desired: Mask,
    existing_share: Mask,
) -> Admission {
    for term in Term::ALL {
        if new_desired.has(term) && !existing_share.has(term) {
            return Admission::Refuse(term);
        }
        if existing_desired.has(term) && !new_share.has(term) {
            return Admission::Refuse(term);
        }
    }
    Admission::Allow
}

#[test]
fn every_cell_of_the_two_holder_product_matches_the_rule() {
    let mut cells = 0usize;
    for new_desired in 0..8u8 {
        for new_share in 0..8u8 {
            for existing_desired in 0..8u8 {
                for existing_share in 0..8u8 {
                    cells += 1;
                    let (nd, ns) = (Mask::from_bits(new_desired), Mask::from_bits(new_share));
                    let (ed, es) = (
                        Mask::from_bits(existing_desired),
                        Mask::from_bits(existing_share),
                    );
                    assert_eq!(
                        admit(nd, ns, &[(ed, es)]),
                        expected(nd, ns, ed, es),
                        "cell new_desired={new_desired} new_share={new_share} \
                         existing_desired={existing_desired} existing_share={existing_share}"
                    );
                }
            }
        }
    }
    // Rows = the four masks (8 values each), one column pair per combination: 8^4 cells, no wildcard.
    assert_eq!(
        cells,
        8 * 8 * 8 * 8,
        "missing cell: the product must be walked in full"
    );
}

/// The bug this module exists to prevent: checking the holder's **desired** mask against the newcomer's
/// desired mask (`exclusive open admits only itself`), instead of the holder's **share** mask.
#[test]
fn a_holder_sharing_nothing_refuses_a_second_reader_and_one_sharing_read_admits_it() {
    assert_eq!(
        admit(Mask::READ, Mask::NONE, &[(Mask::READ, Mask::NONE)]),
        Admission::Refuse(Term::Read),
        "holder shared nothing, so even a reader must be refused"
    );
    // The rule is **bidirectional**: the holder shares read, but the newcomer shared nothing, so the
    // holder's own read access would be violated. This is the case a one-sided implementation gets wrong,
    // and it is why the expectation is written from the definition rather than from `admit`.
    assert_eq!(
        admit(Mask::READ, Mask::NONE, &[(Mask::READ, Mask::READ)]),
        Admission::Refuse(Term::Read),
        "the holder's read access is unprotected by the newcomer's share mask"
    );
    assert_eq!(
        admit(Mask::READ, Mask::READ, &[(Mask::READ, Mask::READ)]),
        Admission::Allow,
        "both parties share read, so a second reader is admitted"
    );
}

/// An empty existing set is the first open: always admitted, which is what makes the rule total.
#[test]
fn the_first_open_is_always_admitted() {
    for desired in 0..8u8 {
        for share in 0..8u8 {
            assert_eq!(
                admit(Mask::from_bits(desired), Mask::from_bits(share), &[]),
                Admission::Allow,
                "cell desired={desired} share={share}: the first open cannot conflict"
            );
        }
    }
}

/// `ALL_BITS` is the mask that admits every term — asserted against the terms, not against a literal, so a
/// fourth term cannot leave it behind.
#[test]
fn the_all_bits_mask_covers_every_term() {
    for term in Term::ALL {
        assert!(
            Mask::ALL_BITS.has(term),
            "cell term={term:?}: ALL_BITS must contain every term"
        );
    }
    assert_eq!(
        Mask::ALL_BITS,
        admit_all_bits(),
        "cell ALL_BITS: the mask must equal the union of the terms"
    );
}

fn admit_all_bits() -> Mask {
    let mut bits = 0u8;
    for term in Term::ALL {
        bits |= match term {
            Term::Read => 1,
            Term::Write => 2,
            Term::Delete => 4,
        };
    }
    Mask::from_bits(bits)
}

/// Bits outside the three known terms are dropped rather than carried, so a caller cannot believe an
/// unknown access bit was checked.
#[test]
fn unknown_bits_are_dropped() {
    assert_eq!(Mask::from_bits(0xFF).bits(), 0b111);
    assert_eq!(Mask::from_bits(0b1000).bits(), 0);
}
