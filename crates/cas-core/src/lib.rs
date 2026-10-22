//! Object identity and the canonical object envelope for the Workspace CAS
//! filesystem.
//!
//! This crate is the whole of Phase 1 task T1: an [`ObjectId`] whose value is
//! derived from a domain-separated BLAKE3 digest over canonical object bytes,
//! the [`Object`] envelope those bytes are framed in, and the [`Role`] tag that
//! keeps objects of different kinds from sharing an identity even when their
//! payloads coincide.
//!
//! Everything here is pure computation over bytes owned by the caller. The
//! crate opens no file, talks to no database and knows nothing about the
//! Store, a Workspace, a namespace or a platform: the persisted layout, chunk
//! and extent encoding, compaction and garbage collection are later tasks and
//! are deliberately absent (see `docs/adr/ADR-001-object-encoding.md`).
//!
//! # Objects and bytes
//!
//! An object is framed as
//!
//! ```text
//! "WCAS" | version u8 | role u8 | payload_len BE32 | value_len BE32 | value
//! ```
//!
//! and its [`ObjectId`] is the BLAKE3 digest of the entire frame prefixed by a
//! domain string. Two objects are the same object exactly when their frames
//! are the same bytes, so the role, version and both length fields participate
//! in identity: identical values stored under different roles are different
//! objects.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

mod envelope;
mod error;
mod id;
mod role;

pub use envelope::{
    verify_id, Envelope, Object, ENVELOPE_HEADER_LEN, MAGIC, MAX_OBJECT_LEN, MAX_VALUE_LEN, VERSION,
};
pub use error::{EnvelopeError, IdParseError};
pub use id::{ObjectId, ObjectIdHasher, ID_BYTES, ID_HEX_LEN};
pub use role::Role;
