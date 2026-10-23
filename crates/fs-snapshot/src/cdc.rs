//! FastCDC content-defined chunking (ADR-007 §3.1).
//!
//! This is a **pure function of the bytes**: given the same input it produces the same
//! boundaries on every platform and in every implementation, because the specification
//! pins the gear table, the masks, the window and the reset rule bit for bit. That property
//! is what lets an old chunk be reused verbatim after a small edit (§3.4), and what makes
//! the snapshot of a file independent of how it was written.
//!
//! # The four details that must not drift
//!
//! 1. **The gear table is derived, not transcribed.** `gear[i]` is
//!    `u64::from_be_bytes(BLAKE3("wcas/gear/v1\0" || u8(i))[0..8])`. A hand-copied table is
//!    one transcription error away from two builds disagreeing.
//! 2. **The hash rolls even while the minimum is skipped.** Skipping the *decision* is a
//!    speed trick; skipping the *rolling* would make the boundaries depend on the
//!    implementation's shortcuts.
//! 3. **The masks are the high bits.** `fp << 1` makes the top bit depend on the last 64
//!    bytes; a low-bitmask would only look at a dozen bytes and cut poorly.
//! 4. **Every cut resets the whole state** (`fp = 0`, new chunk start). §4's incremental
//!    pager depends on this: a chunk's boundary may not depend on bytes before it.

/// The domain-separated prefix for the gear table.
const GEAR_DOMAIN: &str = "wcas/gear/v1\x00";

/// Normalisation level 2: the small mask is this many bits *above* `log2(avg)`.
const NORMALISATION: u32 = 2;

/// The gear table: 256 pseudo-random u64s derived from BLAKE3.
///
/// Computed once per process. Being derived rather than written out is deliberate
/// (ADR-007 §3.1): the derivation is the specification.
static GEAR: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();

/// The gear table, built on first use.
#[must_use]
pub fn gear_table() -> &'static [u64; 256] {
    GEAR.get_or_init(|| {
        let mut table = [0u64; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut hasher = blake3::Hasher::new();
            hasher.update(GEAR_DOMAIN.as_bytes());
            hasher.update(&[u8::try_from(index).expect("0..=255")]);
            let digest = hasher.finalize();
            let mut eight = [0u8; 8];
            eight.copy_from_slice(&digest.as_bytes()[0..8]);
            *slot = u64::from_be_bytes(eight);
        }
        table
    })
}

/// The chunking parameters of a profile (ADR-007 §3.1, §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CdcParams {
    /// No cut before this many bytes into a chunk.
    pub min: usize,
    /// The target average; the masks are derived from it.
    pub avg: usize,
    /// A cut is forced at this size.
    pub max: usize,
    /// Normalisation level; 2 is the FastCDC default and what ADR-007 fixes.
    pub normalisation: u32,
}

impl CdcParams {
    /// The parameters ADR-007 §3.5 writes into profile v2: 8 KiB / 16 KiB / 32 KiB.
    pub const V2: CdcParams = CdcParams {
        min: 8_192,
        avg: 16_384,
        max: 32_768,
        normalisation: NORMALISATION,
    };

    /// The mask that must be all-zero for a cut *before* the average size, and the one for
    /// after it.
    ///
    /// ADR-007 §3.1: `MASK_S` has the top `log2(avg)+normalisation` bits set, `MASK_L` the
    /// top `log2(avg)-normalisation` bits. Both are read from the top of the 64-bit hash.
    #[must_use]
    pub fn masks(&self) -> (u64, u64) {
        let log2_avg = self.avg.trailing_zeros();
        let small_bits = (log2_avg + self.normalisation).min(63);
        let large_bits = log2_avg.saturating_sub(self.normalisation).clamp(1, 63);
        let mask = |bits: u32| {
            if bits >= 64 {
                u64::MAX
            } else {
                !(u64::MAX >> bits)
            }
        };
        (mask(small_bits), mask(large_bits))
    }
}

/// One chunk: where it starts and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpan {
    /// Offset of the chunk's first byte.
    pub offset: usize,
    /// The chunk's length. Shorter than `min` only for the final chunk.
    pub length: usize,
}

/// Chunks `bytes` into spans, in order (ADR-007 §3.1).
///
/// Empty input produces no spans.
#[must_use]
pub fn chunk(bytes: &[u8], params: &CdcParams) -> Vec<ChunkSpan> {
    let gear = gear_table();
    let (mask_small, mask_large) = params.masks();
    let min = params.min.max(1);
    let max = params.max.max(min);
    let avg = params.avg.clamp(min, max);

    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut fingerprint = 0u64;
        let mut position = start;
        let mut cut = bytes.len();
        while position < bytes.len() {
            let byte = usize::from(bytes[position]);
            // The hash rolls on every byte, including those inside the minimum —
            // ADR-007 §3.1 makes skipping the roll a specification violation.
            fingerprint = (fingerprint << 1).wrapping_add(gear[byte]);
            let length = position - start + 1;
            let cut_here = if length < min {
                false
            } else if length < avg {
                fingerprint & mask_small == 0
            } else if length >= max {
                true
            } else {
                fingerprint & mask_large == 0
            };
            position += 1;
            if cut_here {
                cut = position;
                break;
            }
        }
        spans.push(ChunkSpan {
            offset: start,
            length: cut - start,
        });
        // Every cut resets the whole state: the next chunk's boundary may not depend on
        // the bytes before it (§4's incremental reuse depends on this).
        start = cut;
    }
    spans
}

/// Whether every byte of a chunk is zero.
///
/// ADR-007 §3.2: an all-zero chunk is omitted from the extent list, leaving a gap.
#[must_use]
pub fn is_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}
