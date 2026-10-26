//! The snapshot profile: every format parameter, frozen (ADR-005 §5).
//!
//! The description string is canonical and byte-exact. Any change to any character
//! is a new profile, and a root written under one profile cannot be read under
//! another — that is what makes the format parameters a decision rather than a
//! tuning knob.

use cas_core::ObjectId;

/// Domain separation for the profile id hash.
const PROFILE_DOMAIN: &str = "wcas/snapshot-profile/v1\x00";

/// Domain separation for page splitting within a tree (ADR-005 §4).
const PAGE_DOMAIN_PREFIX: &str = "wcas/snapshot-page/v1\x00";

/// The tree a page belongs to.
///
/// Each tree gets its own domain string, so the same key in two trees splits at
/// unrelated places — otherwise a directory and an inode table would correlate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeDomain {
    /// The inode table.
    Inode,
    /// A directory's entry map.
    Dir,
    /// A large file's extent map.
    Extent,
}

impl TreeDomain {
    /// The name used in the profile description and the page-split hash.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            TreeDomain::Inode => "inode",
            TreeDomain::Dir => "dir",
            TreeDomain::Extent => "extent",
        }
    }
}

/// The format parameters of ADR-005 §5.
///
/// There is exactly one value in P2a; the type exists so a future profile is a new
/// constant rather than an edit to scattered literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Value format number (`fmt`), currently 1.
    pub fmt: u8,
    /// Page-split hash mask bits: a page boundary is taken when the low bits of the
    /// key hash are zero.
    pub mask_bits: u32,
    /// A page may not be split before it holds this many entries, so the hash cannot
    /// produce pathologically small pages.
    pub min_entries: u32,
    /// Hard upper bound on an encoded page, in bytes.
    pub max_page_bytes: usize,
    /// Fixed block size for large files.
    pub block_bytes: u64,
    /// Largest legal extent length.
    pub max_extent_len: u64,
    /// Largest legal object value (mirrors ADR-001's 16 MiB limit).
    pub max_object_value: u64,
    /// Largest legal name length.
    pub name_max: usize,
    /// How a file's bytes are cut into objects (ADR-007 §3).
    pub chunker: Chunker,
    /// The `fmt` byte of **inode table** pages under this profile (ADR-005-v3 draft §2.1).
    ///
    /// A page's wire format is chosen by the **profile**, not by the page: v3 changes only the inode record, so
    /// directory/extent/root pages stay `fmt = 1` and only inode pages become `fmt = 2`. Keeping it as a profile field
    /// (rather than as "if the profile is v3 then 2") means the decoder can compare one number and the description
    /// string below cannot disagree with the encoder.
    pub inode_fmt: u8,
}

/// How a file's content is cut into objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunker {
    /// One object per fixed `block_bytes` block (ADR-005 §3.2).
    FixedBlocks,
    /// Content-defined chunks whose boundaries follow the bytes (ADR-007 §3.1).
    FastCdc(crate::cdc::CdcParams),
}

impl Chunker {
    /// The role a file-content object carries (ADR-007 §3.3).
    #[must_use]
    pub const fn content_role(self) -> cas_core::Role {
        match self {
            // v1 wrote content as `Bytes`; v2 writes every content object as `Chunk`, so a
            // single-chunk file and a chunk inside a large one are the same object.
            Chunker::FixedBlocks => cas_core::Role::Bytes,
            Chunker::FastCdc(_) => cas_core::Role::Chunk,
        }
    }
}

impl Profile {
    /// The P2a profile, exactly as ADR-005 §5 writes it.
    ///
    /// Still readable, never written: ADR-007 §3.5 makes v2 the write profile, and a v2
    /// build refuses a v1 root (§7). Keeping the constant is what lets the refusal be a
    /// *decision* — "this is the older format" — rather than an opaque parse failure.
    pub const V1: Profile = Profile {
        fmt: 1,
        mask_bits: 6,
        min_entries: 4,
        max_page_bytes: 16_384,
        block_bytes: 65_536,
        max_extent_len: 65_536,
        max_object_value: 16_777_216,
        name_max: 255,
        chunker: Chunker::FixedBlocks,
        inode_fmt: 1,
    };

    /// The P2b profile (ADR-007 §3.5): content-defined chunking replaces fixed blocks.
    pub const V2: Profile = Profile {
        fmt: 1,
        mask_bits: 6,
        min_entries: 4,
        max_page_bytes: 16_384,
        // `block_bytes` is gone from the description; the field keeps the largest legal
        // extent length, which is what the decoder checks (ADR-007 §3.2).
        block_bytes: 32_768,
        max_extent_len: 32_768,
        max_object_value: 16_777_216,
        name_max: 255,
        chunker: Chunker::FastCdc(crate::cdc::CdcParams::V2),
        inode_fmt: 1,
    };

    /// The P3 profile (ADR-005-v3 draft): the inode record gains `accessed_ns`, `changed_ns`, `attributes`,
    /// `security_len` + security bytes.
    ///
    /// `fmt` stays 1 and `inode_fmt` becomes 2, which is the whole of the write-side change: directory, extent and root
    /// pages keep their layout, so a v2 reader can still parse everything except the inode table. That is exactly why
    /// the refusal must be `UnsupportedFormat` (a version difference it can see) rather than `Malformed` (a corrupt
    /// file it cannot).
    pub const V3: Profile = Profile {
        fmt: 1,
        mask_bits: 6,
        min_entries: 4,
        max_page_bytes: 16_384,
        block_bytes: 32_768,
        max_extent_len: 32_768,
        max_object_value: 16_777_216,
        name_max: 255,
        chunker: Chunker::FastCdc(crate::cdc::CdcParams::V2),
        inode_fmt: 2,
    };

    /// The profile this build writes.
    pub const CURRENT: Profile = Profile::V3;

    /// Whether this profile is the one this build writes.
    #[must_use]
    pub fn is_current(&self) -> bool {
        *self == Profile::CURRENT
    }

    /// The canonical description string. Any character change is a new profile.
    #[must_use]
    pub fn description(&self) -> String {
        // The v3 description is **literally** the string in ADR-005-v3 draft §2.3: the profile id is its BLAKE3, so a
        // reformatted line here silently creates a different profile. It is written as one `concat!`-style literal with
        // the numeric fields interpolated only where the struct owns the value.
        if self.inode_fmt == 2 {
            return format!(
                "wcas/snapshot/v3;fmt={};inode_fmt={};mask_bits={};min_entries={};max_page_bytes={};cdc=fastcdc-nc{};cdc_min={};cdc_avg={};cdc_max={};gear=blake3:wcas/gear/v1;content_role=chunk;max_object_value={};name_max={};hash=blake3;domains=inode,dir,extent",
                self.fmt,
                self.inode_fmt,
                self.mask_bits,
                self.min_entries,
                self.max_page_bytes,
                match self.chunker {
                    Chunker::FastCdc(params) => params.normalisation,
                    Chunker::FixedBlocks => 0,
                },
                match self.chunker {
                    Chunker::FastCdc(params) => params.min,
                    Chunker::FixedBlocks => 0,
                },
                match self.chunker {
                    Chunker::FastCdc(params) => params.avg,
                    Chunker::FixedBlocks => 0,
                },
                match self.chunker {
                    Chunker::FastCdc(params) => params.max,
                    Chunker::FixedBlocks => 0,
                },
                self.max_object_value,
                self.name_max
            );
        }
        match self.chunker {
            Chunker::FixedBlocks => format!(
                "wcas/snapshot/v1;fmt={};mask_bits={};min_entries={};max_page_bytes={};\
                 block_bytes={};max_extent_len={};max_object_value={};\
                 name_max={};hash=blake3;domains=inode,dir,extent",
                self.fmt,
                self.mask_bits,
                self.min_entries,
                self.max_page_bytes,
                self.block_bytes,
                self.max_extent_len,
                self.max_object_value,
                self.name_max
            ),
            Chunker::FastCdc(params) => format!(
                "wcas/snapshot/v2;fmt={};mask_bits={};min_entries={};max_page_bytes={};\
                 cdc=fastcdc-nc{};cdc_min={};cdc_avg={};cdc_max={};gear=blake3:wcas/gear/v1;\
                 content_role=chunk;max_object_value={};name_max={};hash=blake3;\
                 domains=inode,dir,extent",
                self.fmt,
                self.mask_bits,
                self.min_entries,
                self.max_page_bytes,
                params.normalisation,
                params.min,
                params.avg,
                params.max,
                self.max_object_value,
                self.name_max
            ),
        }
    }

    /// `profile_id = BLAKE3("wcas/snapshot-profile/v1\0" || description)`.
    #[must_use]
    pub fn id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(PROFILE_DOMAIN.as_bytes());
        hasher.update(self.description().as_bytes());
        *hasher.finalize().as_bytes()
    }

    /// The domain-separated hash of a key at a level, whose low bits decide a page
    /// boundary (ADR-005 §4 condition 1).
    #[must_use]
    pub fn split_hash(&self, domain: TreeDomain, level: u8, key: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(PAGE_DOMAIN_PREFIX.as_bytes());
        hasher.update(domain.name().as_bytes());
        hasher.update(&[level]);
        hasher.update(key);
        *hasher.finalize().as_bytes()
    }

    /// Whether a page must end *after* the entry with this key.
    #[must_use]
    pub fn is_split_point(&self, domain: TreeDomain, level: u8, key: &[u8]) -> bool {
        let hash = self.split_hash(domain, level, key);
        // The low `mask_bits` bits of the big-endian digest, read as a u64.
        let mut low = [0u8; 8];
        low.copy_from_slice(&hash[24..32]);
        let value = u64::from_be_bytes(low);
        let mask = if self.mask_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << self.mask_bits) - 1
        };
        value & mask == 0
    }
}

/// A profile id as stored in a root object.
#[must_use]
pub fn profile_bytes(id: [u8; 32]) -> ObjectId {
    ObjectId::from_bytes(id)
}
