//! The v3 inode record: encode/decode on its own, before any page encoder touches it (ADR-005-v3 draft §2.2).
//!
//! Keeping the record codec separate is what lets the acceptance matrix (§3.1) be checked row by row without a tree, a
//! store, or a root: every row there is a statement about **these bytes**.
//!
//! Layout, all big-endian and fixed-width except the security bytes:
//!
//! ```text
//! serial u64 | kind u8 | size u64 | created_ns i64 | modified_ns i64 | accessed_ns i64 | changed_ns i64 |
//! attributes u32 | security_len u32 | security[security_len] | content_tag u8 | content_oid[32] (tag ∈ {1,2,3})
//! ```
//!
//! Two rules from the draft that are easy to get subtly wrong, so each has its own error and its own test:
//!
//! - `security_len ≤ 65536` **inclusive** (0 = no explicit SD). Above that is `Malformed`, not a truncation;
//! - `attributes` stores only the four writable bits (`READONLY 0x01`, `HIDDEN 0x02`, `SYSTEM 0x04`, `ARCHIVE 0x20`).
//!   `DIRECTORY 0x10` and `NORMAL 0x80` are **derived** from `kind`/content, so finding them on disk is a `Malformed`
//!   record: writing them would make the same fact storable in two places, which is how they drift.

use cas_core::ObjectId;

use crate::error::SnapshotError;
use crate::record::{Content, Kind};
use crate::SnapshotResult;

/// The attribute bits that may be **stored** (the rest are derived).
pub const WRITABLE_ATTRIBUTES: u32 = 0x01 | 0x02 | 0x04 | 0x20;

/// `FILE_ATTRIBUTE_DIRECTORY`, derived from `kind` and therefore never legal on disk.
pub const DIRECTORY_ATTRIBUTE: u32 = 0x10;

/// `FILE_ATTRIBUTE_NORMAL`, derived from "no other attribute set" and therefore never legal on disk.
pub const NORMAL_ATTRIBUTE: u32 = 0x80;

/// The largest legal `security_len` (ADR-005-v3 draft §2.2).
pub const MAX_SECURITY_LEN: u32 = 65_536;

/// An inode record as v3 stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeRecordV3 {
    /// The key: strictly increasing within a page.
    pub serial: u64,
    /// File or directory.
    pub kind: Kind,
    /// Logical size; must be 0 for a directory.
    pub size: u64,
    /// Creation time (Unix epoch nanoseconds).
    pub created_ns: i64,
    /// Last content write.
    pub modified_ns: i64,
    /// Last access; written only when explicitly set (reads do not touch it).
    pub accessed_ns: i64,
    /// Metadata change time.
    pub changed_ns: i64,
    /// Stored attribute bits (see [`WRITABLE_ATTRIBUTES`]).
    pub attributes: u32,
    /// The self-relative security descriptor, ≤ [`MAX_SECURITY_LEN`] bytes. Empty means "inherit the default".
    pub security: Vec<u8>,
    /// What the byte content points at.
    pub content: Content,
}

fn malformed(detail: &str) -> SnapshotError {
    SnapshotError::Malformed {
        detail: detail.to_owned(),
    }
}

/// Checks the two invariants that are about the record rather than about its bytes.
///
/// # Errors
///
/// [`SnapshotError::Malformed`] for a security descriptor over the limit, a stored derived attribute bit, an unknown
/// attribute bit, or a directory with a non-zero size.
pub fn validate(record: &InodeRecordV3) -> SnapshotResult<()> {
    let security_len = u32::try_from(record.security.len())
        .map_err(|_| malformed("security_len does not fit in u32"))?;
    if security_len > MAX_SECURITY_LEN {
        return Err(malformed("security_len exceeds 65536"));
    }
    let derived = record.attributes & (DIRECTORY_ATTRIBUTE | NORMAL_ATTRIBUTE);
    if derived != 0 {
        return Err(malformed(
            "attributes contains a derived bit (0x10 or 0x80)",
        ));
    }
    let unknown = record.attributes & !WRITABLE_ATTRIBUTES;
    if unknown != 0 {
        return Err(malformed("attributes contains an unknown bit"));
    }
    if record.kind == Kind::Directory && record.size != 0 {
        return Err(malformed("a directory must have size 0"));
    }
    Ok(())
}

/// Encodes one record. The caller has already validated it (the page encoder validates a whole page's worth).
///
/// # Errors
///
/// [`SnapshotError::Malformed`] via [`validate`].
pub fn encode(record: &InodeRecordV3) -> SnapshotResult<Vec<u8>> {
    let mut out = Vec::with_capacity(58 + record.security.len());
    encode_into(record, &mut out)?;
    Ok(out)
}

/// Encodes into an existing buffer.
///
/// The page builder writes tens of thousands of records into **one** page buffer; returning a `Vec` per record (the first
/// version of `encode` did) costs an allocation and a copy per record — measured at 456 ms against 187 ms for v2 on the
/// 99,999-record baseline, i.e. the whole "v3 is slower" reading was this allocation, not the extra fields.
///
/// # Errors
///
/// [`SnapshotError::Malformed`] via [`validate`].
pub fn encode_into(record: &InodeRecordV3, out: &mut Vec<u8>) -> SnapshotResult<()> {
    validate(record)?;
    out.extend_from_slice(&record.serial.to_be_bytes());
    out.push(match record.kind {
        Kind::Directory => 1,
        Kind::File => 2,
    });
    out.extend_from_slice(&record.size.to_be_bytes());
    out.extend_from_slice(&record.created_ns.to_be_bytes());
    out.extend_from_slice(&record.modified_ns.to_be_bytes());
    out.extend_from_slice(&record.accessed_ns.to_be_bytes());
    out.extend_from_slice(&record.changed_ns.to_be_bytes());
    out.extend_from_slice(&record.attributes.to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(record.security.len())
            .expect("validated above")
            .to_be_bytes(),
    );
    out.extend_from_slice(&record.security);
    match record.content {
        Content::Empty => out.push(0),
        Content::Blob(oid) => {
            out.push(1);
            out.extend_from_slice(oid.as_bytes());
        }
        Content::Extents(oid) => {
            out.push(2);
            out.extend_from_slice(oid.as_bytes());
        }
        Content::Dir(oid) => {
            out.push(3);
            out.extend_from_slice(oid.as_bytes());
        }
    }
    Ok(())
}

/// Decodes one record, returning it and how many bytes were consumed.
///
/// # Errors
///
/// [`SnapshotError::Malformed`] for a truncated record, an unknown `kind`/`content_tag`, a security descriptor over the
/// limit, or an attribute bit that must not be stored.
pub fn decode(bytes: &[u8]) -> SnapshotResult<(InodeRecordV3, usize)> {
    let mut at = 0usize;

    fn take<'a>(
        bytes: &'a [u8],
        at: &mut usize,
        count: usize,
        what: &str,
    ) -> SnapshotResult<&'a [u8]> {
        let end = at
            .checked_add(count)
            .ok_or_else(|| malformed(&format!("{what} length overflows")))?;
        let slice = bytes
            .get(*at..end)
            .ok_or_else(|| malformed(&format!("record ends inside {what}")))?;
        *at = end;
        Ok(slice)
    }

    let serial = u64::from_be_bytes(
        take(bytes, &mut at, 8, "serial")?
            .try_into()
            .expect("8 bytes"),
    );
    let kind = match take(bytes, &mut at, 1, "kind")?[0] {
        1 => Kind::Directory,
        2 => Kind::File,
        other => return Err(malformed(&format!("unknown kind {other}"))),
    };
    let size = u64::from_be_bytes(
        take(bytes, &mut at, 8, "size")?
            .try_into()
            .expect("8 bytes"),
    );
    let created_ns = i64::from_be_bytes(
        take(bytes, &mut at, 8, "created_ns")?
            .try_into()
            .expect("8 bytes"),
    );
    let modified_ns = i64::from_be_bytes(
        take(bytes, &mut at, 8, "modified_ns")?
            .try_into()
            .expect("8 bytes"),
    );
    let accessed_ns = i64::from_be_bytes(
        take(bytes, &mut at, 8, "accessed_ns")?
            .try_into()
            .expect("8 bytes"),
    );
    let changed_ns = i64::from_be_bytes(
        take(bytes, &mut at, 8, "changed_ns")?
            .try_into()
            .expect("8 bytes"),
    );
    let attributes = u32::from_be_bytes(
        take(bytes, &mut at, 4, "attributes")?
            .try_into()
            .expect("4 bytes"),
    );
    let security_len = u32::from_be_bytes(
        take(bytes, &mut at, 4, "security_len")?
            .try_into()
            .expect("4 bytes"),
    );
    if security_len > MAX_SECURITY_LEN {
        return Err(malformed("security_len exceeds 65536"));
    }
    let security = take(
        bytes,
        &mut at,
        usize::try_from(security_len).expect("≤ 65536"),
        "security",
    )?
    .to_vec();
    let content_tag = take(bytes, &mut at, 1, "content_tag")?[0];
    let content = match content_tag {
        0 => Content::Empty,
        tag @ 1..=3 => {
            let oid = ObjectId::from_bytes(
                take(bytes, &mut at, 32, "content_oid")?
                    .try_into()
                    .expect("32 bytes"),
            );
            match tag {
                1 => Content::Blob(oid),
                2 => Content::Extents(oid),
                _ => Content::Dir(oid),
            }
        }
        other => return Err(malformed(&format!("unknown content_tag {other}"))),
    };
    let record = InodeRecordV3 {
        serial,
        kind,
        size,
        created_ns,
        modified_ns,
        accessed_ns,
        changed_ns,
        attributes,
        security,
        content,
    };
    // The same invariants on the way in: a record that could not have been written by this build must not be read as if
    // it had been.
    validate(&record)?;
    Ok((record, at))
}
