//! The **name matching key**, defined once (443/470).
//!
//! Windows names are case-**insensitive** but case-**preserving**: `README.md` must come back with the spelling it was created with, while
//! `readme.md` must resolve to it. That shape needs two things per name — a key to match on and the bytes to hand back — and this module owns the
//! first one.
//!
//! # Why this lives in `fs-semantics` and not in a frontend
//!
//! Folding is a **semantic rule about how a key is built**, not a Windows detail. The core stores names and must answer lookups with the same rule
//! the frontend uses; if the two disagreed, a file created through the mount could be invisible to a core-side operation and vice versa. The core is
//! platform-independent, so the rule has to live in the crate both sides already depend on and which has **zero dependencies of its own**.
//!
//! # The Unicode policy, stated once (moved, not rewritten)
//!
//! [`fold`] upper-cases **ASCII only**; every non-ASCII character is left exactly as it is. `Ä`/`ä` are therefore two different names here, while
//! `A`/`a` are one.
//!
//! This is a **documented deviation** from Windows, which folds non-ASCII with its invariant upcase table (so `Ä`/`ä` *are* one name there). The
//! reason is not laziness: reproducing that table means shipping a data table per Unicode version, and a wrong copy is worse than an explicit gap —
//! it would make the frontend reject names Windows accepts, with no way for a user to tell why. The deviation is **narrow and visible**: one
//! function, one test, and it can only ever make us *stricter* about collisions (it never merges two names Windows considers distinct).
//!
//! # Bytes, not strings
//!
//! [`fold_bytes`] is the form the core uses: a name in the store is `&[u8]` (the exact bytes the caller gave us), and a name that is not valid UTF-8
//! must still be matchable. It folds ASCII bytes and copies everything else unchanged, which makes it agree with [`fold`] on every input `fold` can
//! accept — the property the two tests below pin.

/// Folds one name for comparison: ASCII case only (see the module note on the Unicode policy), **no** Unicode normalization, and no trimming.
#[must_use]
pub fn fold(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// [`fold`] over bytes: the matching key for a stored name.
///
/// Byte-for-byte identical to [`fold`] on valid UTF-8, and defined on bytes that are not UTF-8 at all — a name is whatever the caller wrote, and
/// refusing to match one would make an entry we created unreachable.
#[must_use]
pub fn fold_bytes(name: &[u8]) -> Vec<u8> {
    name.iter().map(u8::to_ascii_uppercase).collect()
}

#[cfg(test)]
mod tests {
    use super::{fold, fold_bytes};

    #[test]
    fn ascii_folds_and_non_ascii_does_not() {
        assert_eq!(fold("ReadMe.MD"), "README.MD");
        // The documented deviation: `ä` is left alone, so it does **not** collide with `Ä`.
        assert_eq!(fold("ä"), "ä");
        assert_ne!(fold("ä"), fold("Ä"));
    }

    #[test]
    fn bytes_agree_with_the_string_form() {
        for name in ["", "a", "A", "l2-File", "ä", "文件的.txt"] {
            assert_eq!(
                fold_bytes(name.as_bytes()),
                fold(name).into_bytes(),
                "{name}"
            );
        }
    }

    #[test]
    fn the_key_is_not_the_display() {
        // The whole point of keeping two fields: folding must not be used to *store* the name, or `README` would come back as `README` even when it
        // was created as `ReadMe`. This test pins the direction of the conversion, not just its output.
        let display = "ReadMe.txt";
        assert_ne!(&fold_bytes(display.as_bytes())[..], display.as_bytes());
        assert_eq!(display.len(), fold_bytes(display.as_bytes()).len());
    }
}
