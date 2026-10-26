//! Open dispositions — re-exported from the core (P3-2 §12).
//!
//! The eighteen-cell table is defined in `fs_namespace::disposition`: the core is what performs the action, and the
//! frontend has to classify the request the same way. Keeping a copy here would let the two drift, and P3-1's tests
//! would only ever prove the copy right.

pub use fs_namespace::disposition::{decide, Action, Disposition, Exists, Outcome, TABLE};
