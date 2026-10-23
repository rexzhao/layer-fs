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
}

impl Profile {
    /// The P2a profile, exactly as ADR-005 §5 writes it.
    pub const V1: Profile = Profile {
        fmt: 1,
        mask_bits: 6,
        min_entries: 4,
        max_page_bytes: 16_384,
        block_bytes: 65_536,
        max_extent_len: 65_536,
        max_object_value: 16_777_216,
        name_max: 255,
    };

    /// The canonical description string. Any character change is a new profile.
    #[must_use]
    pub fn description(&self) -> String {
        format!(
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
        )
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
