//! Name case handling (P3-1 §1 `case`, ADR-009 §4.1).
//!
//! Windows names are case-**insensitive** but case-**preserving**: `README.md` must come back with its
//! stored spelling, and two names that fold together must be refused rather than silently merged. And
//! Windows does *not* normalize Unicode, so `é` (U+00E9) and `e` + U+0301 are two different names that
//! happen to look alike — this model keeps that, because a frontend that normalized would report collisions
//! the store never had.
//!
//! ## The Unicode policy, stated once
//!
//! [`fold`] upper-cases **ASCII only**; every non-ASCII character is left exactly as it is. That means
//! `Ä`/`ä` are two different names in this model, while `A`/`a` are one.
//!
//! This is a **documented deviation** from Windows, which folds non-ASCII with its invariant upcase table
//! (so `Ä`/`ä` *are* one name there). The reason is not laziness: reproducing that table means shipping a
//! data table per Unicode version, and a wrong copy is worse than an explicit gap — it would make the
//! frontend reject names Windows accepts, with no way for a user to tell why. The deviation is therefore
//! **narrow and visible**: one function, one test, and it can only ever make the frontend *stricter* about
//! collisions (it never merges two names Windows considers distinct).
//!
//! When the table is available (P3-4, after the FFI is in), the change is local to `fold` and the test that
//! pins the current policy is the one that has to be updated.

/// How a lookup found its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// An exact byte match.
    Exact(&'a str),
    /// A single case-insensitive match, whose stored spelling is returned.
    Folded(&'a str),
    /// Two or more entries fold to the same name: Windows refuses this situation outright.
    Ambiguous,
    /// Nothing matched.
    Missing,
}

/// Folds one name for comparison: ASCII case only (see the module note on the Unicode policy), **no** Unicode
/// normalization, and no trimming.
#[must_use]
pub fn fold(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// Looks `wanted` up in `entries`.
///
/// Exact spelling wins over a folded match — that is what makes `README` and `readme` resolve predictably
/// on a store that (through a bug elsewhere) ended up with both.
#[must_use]
pub fn lookup<'a>(entries: &[&'a str], wanted: &str) -> Lookup<'a> {
    if let Some(exact) = entries.iter().find(|entry| **entry == wanted) {
        return Lookup::Exact(exact);
    }
    let folded_wanted = fold(wanted);
    let matches: Vec<&'a str> = entries
        .iter()
        .copied()
        .filter(|entry| fold(entry) == folded_wanted)
        .collect();
    // Spelled out rather than a wildcard on the count: a fallback arm would silently absorb "impossible"
    // counts too, and the reviewer would have to reason about which arm it is. (P3-1 QA: no fallback in
    // code that classifies — even when the domain is `usize`.)
    if matches.is_empty() {
        Lookup::Missing
    } else if matches.len() == 1 {
        Lookup::Folded(matches[0])
    } else {
        Lookup::Ambiguous
    }
}

/// Whether creating `candidate` next to `entries` would be a case collision.
///
/// Returns the existing spelling that collides, so the driver's message can name it.
#[must_use]
pub fn collision<'a>(entries: &[&'a str], candidate: &str) -> Option<&'a str> {
    let folded_candidate = fold(candidate);
    entries
        .iter()
        .copied()
        .find(|entry| fold(entry) == folded_candidate)
}
