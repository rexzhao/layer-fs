//! Shared test support.
//!
//! `strict_gate` is **G4b**: every integration test binary turns the publication gate's nesting assertion on, so a public data method that takes
//! the read lock twice on one thread — the shape that deadlocks a reader against a queued writer — fails here instead of in production. An atomic seam
//! rather than an environment variable, because `fs-workspace` forbids `unsafe`. Calling it more than once is harmless.

pub fn strict_gate() {
    fs_workspace::gate::set_nesting_assertion(true);
}
