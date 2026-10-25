//! Driver-independent Windows filesystem semantics (P3-1, L1).
//!
//! This crate is a **model**, not a driver: no FFI, no WinFsp, no `cfg(windows)`-only dependency, so its
//! tables can be tested on any platform and in CI without an SDK (card §1, §2.3).
//!
//! The card's hard requirement is that every module's knowledge lives in a **table with a
//! missing-cell-is-red mechanism** (§2.1). Two shapes are used, and each module says which it uses:
//!
//! - **exhaustive `match`** — adding a variant stops compiling until it is classified (used where the
//!   domain is an enum: [`error`]);
//! - **完备性断言** — the test walks the *whole* enumerated domain (all inputs of a small domain, every
//!   row of the reference table) and fails if a cell is missing or unclassified (used where the domain is
//!   a product of small enumerations: [`disposition`], [`share`]).
//!
//! Both are stronger than "a few representative cases": a variant or cell that nobody thought about is
//! exactly what these checks refuse to let through.

pub mod case;
pub mod delete;
pub mod dirent;
pub mod disposition;
pub mod error;
pub mod path;
pub mod share;
