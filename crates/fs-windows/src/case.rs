//! Name case handling (P3-1 §1 `case`, ADR-009 §4.1).
//!
//! Windows names are case-**insensitive** but case-**preserving**: `README.md` must come back with its
//! stored spelling, and two names that fold together must be refused rather than silently merged. And
//! Windows does *not* normalize Unicode, so `é` (U+00E9) and `e` + U+0301 are two different names that
//! happen to look alike — this model keeps that, because a frontend that normalized would report collisions
//! the store never had.
//!
//! The fold used here is `to_ascii_uppercase` on ASCII and "leave everything else alone": that is a model of
//! `CompareStringOrdinal(TRUE)`, not a copy of the NT upcase table, and the module says so instead of
//! pretending otherwise.

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

/// Folds one name for comparison: ASCII case only, **no** Unicode normalization, no trimming.
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
    match matches.len() {
        0 => Lookup::Missing,
        1 => Lookup::Folded(matches[0]),
        _ => Lookup::Ambiguous,
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
