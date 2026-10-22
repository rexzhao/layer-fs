//! Persistent content-addressed store for the Workspace CAS filesystem.
//!
//! **Nothing here is implemented yet.** This crate exists in Phase 1 task T1
//! only to fix the workspace shape and the dependency direction
//! (`fs-cli` -> `cas-store` -> `cas-core`), so that T2 can fill it in without
//! moving anything.
//!
//! T2 adds the object file layout, `put`/`get`/`exists`/`verify`, the atomic
//! publication order, restart recovery, temporary-file cleanup and the crash
//! injection tests. See `docs/adr/ADR-001-object-encoding.md` and the Phase 1
//! task breakdown in the specification.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
