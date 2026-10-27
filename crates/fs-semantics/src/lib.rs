//! Semantics shared by the core and the Windows frontend (P3-2 §12).
//!
//! Zero dependencies, on purpose: the frontend needs these tables, and the core needs them too, but neither should
//! inherit the other's dependency closure. Anything added here must be pure logic over `core`.

pub mod attributes;
pub mod disposition;
pub mod name;
