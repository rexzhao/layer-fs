//! The public file-system semantics over the content-addressed Store.
//!
//! `fs-api` is the only crate a consumer talks to. It is path-addressed and
//! stateless (ADR-004 §3), resolves every path inside the same transaction that
//! uses it, and stores file content in the CAS **before** the namespace
//! references it, so a namespace can never point at a missing object.
//!
//! ```no_run
//! use fs_api::Fs;
//!
//! let fs = Fs::open("my-store", "default", cas_store::OpenMode::Create)?;
//! fs.create("/hello.txt")?;
//! fs.write_at("/hello.txt", 0, b"hello, world")?;
//!
//! let mut buffer = [0u8; 5];
//! assert_eq!(fs.read_at("/hello.txt", 0, &mut buffer)?, 5);
//! assert_eq!(&buffer, b"hello");
//! assert!(fs.verify()?.is_clean());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Storage layout
//!
//! A file under [`BLOCK_BYTES`] is one object; a larger file is a run of
//! fixed-size blocks recorded as extent rows. A range read fetches only the
//! blocks its range intersects — never the whole file ([`ReadStats`] counts the
//! fetches, and T5 reuses that counter).
//!
//! # What this crate does not do
//!
//! No open handles (P3), no real `flush` ([`Fs::flush`] is a no-op here), no
//! hard or symbolic links, no recursive delete, no CDC or delta encoding, no
//! garbage collection, no file locks, no memory mapping, and no Windows
//! semantics.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod error;
mod file_system;
mod stats;
mod verify;

pub use error::{FsError, FsResult};
pub use file_system::{Fs, BLOCK_BYTES};
pub use stats::{ReadStats, Stats};
pub use verify::{DanglingReference, Inconsistency, VerifyReport};

// Re-exported so entry points (the CLI) can name the open mode without taking a
// dependency on `cas-store`, which is an implementation detail of this layer
// (`docs/architecture.md` §1: entry points depend on the API, not on what it is
// built from). The metadata types are re-exported for the same reason.
pub use cas_store::OpenMode;
pub use fs_namespace::{Entry, Kind, RootRecord, Stat};
