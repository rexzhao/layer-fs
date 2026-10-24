//! Offline garbage collection (ADR-008 §6, P2b-6).
//!
//! The collector is **offline** (P5): `Repo::open_exclusive` takes the main database and every
//! overlay, and refuses rather than waiting. Inside that window nothing else can publish, which is
//! what makes "an object with no root is garbage" safe to act on.
//!
//! This module currently holds the piece the rest of the design hangs from: the root set and the
//! replaceable source it comes from ([`roots`]). Mark, sweep, repacking and the CLI add
//! themselves on top of it, and each one consumes [`roots::RootSet`] rather than re-querying the
//! database — a collector with two views of "what is live" cannot be tested for G2.

pub mod roots;

pub use roots::{RepoRoots, RootKind, RootRef, RootSet, RootSource};
