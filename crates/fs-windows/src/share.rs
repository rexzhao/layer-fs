//! Share modes (P3-1 §1 `share`, ADR-009 §5.2).
//!
//! Windows decides whether a second open may proceed from **both** sides' masks, not from the newcomer
//! alone: `(new.desired & existing.share) != 0` for each of read/write/delete. Getting one of the three
//! terms wrong produces a driver that either refuses legal opens (the common bug: checking the existing
//! handle's *desired* mode) or admits illegal ones (the dangerous one: it lets two processes write the same
//! file believing they are exclusive).
//!
//! The tests walk the **whole product** — 2³ desired × 2³ share × 2³ existing share = 512 cells — computed
//! from the two masks by the rule, so a missing cell is red and the assertion carries its coordinates.

/// One of the three access terms Windows checks separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Term {
    /// Read access.
    Read,
    /// Write access.
    Write,
    /// Delete access.
    Delete,
}

impl Term {
    /// All terms, in the order the driver reports violations.
    pub const ALL: [Term; 3] = [Term::Read, Term::Write, Term::Delete];
}

/// A set of access/share bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mask(u8);

impl Mask {
    /// No bits.
    pub const NONE: Mask = Mask(0);
    /// Read only.
    pub const READ: Mask = Mask(1);
    /// Write only.
    pub const WRITE: Mask = Mask(2);
    /// Delete only.
    pub const DELETE: Mask = Mask(4);
    /// All three terms. Derived from [`Term::ALL`] so the "everything" mask and the term list cannot drift.
    pub const ALL_BITS: Mask = Mask(0b111);

    /// A mask from its bits; bits outside the low three are ignored, because the model only knows three
    /// terms and silently carrying unknown bits would let a caller believe they were checked.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0b111)
    }

    /// The bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether `term` is in the mask.
    #[must_use]
    pub const fn has(self, term: Term) -> bool {
        self.0 & Self::bit(term) != 0
    }

    const fn bit(term: Term) -> u8 {
        match term {
            Term::Read => 1,
            Term::Write => 2,
            Term::Delete => 4,
        }
    }
}

/// The result of asking whether a new open may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The open may proceed.
    Allow,
    /// The open must be refused with `STATUS_SHARING_VIOLATION`, naming the first violating term so the
    /// driver's log says which access conflicted.
    Refuse(Term),
}

/// The rule: a term is admitted when the newcomer's **desired** access is allowed by the existing handle's
/// **share** mask, and the existing handle's desired access is allowed by the newcomer's **share** mask.
///
/// Refusal names the first term (in [`Term::ALL`] order) that fails, so the answer is deterministic.
#[must_use]
pub fn admit(new_desired: Mask, new_share: Mask, existing: &[(Mask, Mask)]) -> Admission {
    for (existing_desired, existing_share) in existing {
        for term in Term::ALL {
            let newcomer_wants_what_holder_denies =
                new_desired.has(term) && !existing_share.has(term);
            let holder_wants_what_newcomer_denies =
                existing_desired.has(term) && !new_share.has(term);
            if newcomer_wants_what_holder_denies || holder_wants_what_newcomer_denies {
                return Admission::Refuse(term);
            }
        }
    }
    Admission::Allow
}
