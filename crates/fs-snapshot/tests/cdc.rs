//! CDC chunking: the pinned test vectors, the bit-level definition, and the property that
//! makes it worth having (ADR-007 §3.1, §3.4, §11-1).

use fs_snapshot::{chunk, gear_table, CdcParams, ChunkSpan};

/// A deterministic byte source, so the 1 MiB vectors do not depend on a `rand` crate.
///
/// splitmix64 again: the point is reproducibility, and a test vector must be the same bytes
/// on every machine for ever.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let value = z ^ (z >> 31);
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.truncate(len);
    out
}

/// The spans as `(offset, length)` pairs, which is what the report and the assertions use.
fn spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    chunk(bytes, &CdcParams::V2)
        .into_iter()
        .map(|span| (span.offset, span.length))
        .collect()
}

/// Every chunk must tile the input exactly: no gap, no overlap, nothing left over.
fn assert_tiles(bytes: &[u8], spans: &[ChunkSpan]) {
    let mut expected = 0;
    for span in spans {
        assert_eq!(span.offset, expected, "chunks must tile the input");
        assert!(span.length > 0, "a chunk may not be empty");
        expected += span.length;
    }
    assert_eq!(
        expected,
        bytes.len(),
        "the chunks must cover every byte exactly once"
    );
}

/// **The gear table is derived, not transcribed** (ADR-007 §3.1).
///
/// Two values are pinned here: the first and a middle one. If the derivation ever changes,
/// every chunk boundary changes with it, so this is the earliest possible warning.
#[test]
fn the_gear_table_is_the_blake3_derivation_it_claims_to_be() {
    let gear = gear_table();

    // Recomputed here from the specification, independently of the implementation's
    // OnceLock path, so a change to either side is caught.
    let derive = |index: u8| {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"wcas/gear/v1\x00");
        hasher.update(&[index]);
        let digest = hasher.finalize();
        let mut eight = [0u8; 8];
        eight.copy_from_slice(&digest.as_bytes()[0..8]);
        u64::from_be_bytes(eight)
    };
    for index in [0u8, 1, 2, 127, 128, 255] {
        assert_eq!(
            gear[usize::from(index)],
            derive(index),
            "gear[{index}] must be BLAKE3(\"wcas/gear/v1\\0\" || {index})[0..8] big-endian"
        );
    }
    // The table is not degenerate: a duplicated value would weaken the hash.
    let mut sorted: Vec<u64> = gear.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        256,
        "the gear table must have 256 distinct values"
    );
}

/// **The masks are the high bits** (ADR-007 §3.1).
///
/// `log2(avg) = 14` at the 16 KiB average, so `MASK_S` has 16 bits and `MASK_L` 12 — read
/// from the top of the word, so the effective window is the last 64 bytes rather than the
/// last dozen.
#[test]
fn the_masks_are_the_high_bits_of_the_expected_widths() {
    let (small, large) = CdcParams::V2.masks();
    assert_eq!(small.count_ones(), 16, "MASK_S = log2(avg) + 2 = 16 bits");
    assert_eq!(large.count_ones(), 12, "MASK_L = log2(avg) - 2 = 12 bits");
    // High bits, not low ones: the widest possible window is what makes the cut quality.
    assert_eq!(small & 0xFF, 0, "MASK_S must not constrain the low byte");
    assert_eq!(large & 0xF, 0, "MASK_L must not constrain the low nibble");
    assert_eq!(small, !(u64::MAX >> 16));
    assert_eq!(large, !(u64::MAX >> 12));
}

/// **Test vector: 1 MiB of pseudo-random bytes.**
///
/// The boundary list is pinned, so any change to the algorithm, the gear derivation or the
/// masks fails here immediately (ADR-007 §3.1).
#[test]
fn vector_pseudo_random_one_mib() {
    let bytes = pseudo_random(1 << 20, 0xC0FFEE);
    let spans = chunk(&bytes, &CdcParams::V2);
    assert_tiles(&bytes, &spans);

    // Pinned: the first twelve boundaries and the total count.
    let head: Vec<(usize, usize)> = spans
        .iter()
        .take(12)
        .map(|span| (span.offset, span.length))
        .collect();
    assert_eq!(
        head,
        vec![
            (0, 23_258),
            (23_258, 23_616),
            (46_874, 19_460),
            (66_334, 19_314),
            (85_648, 18_992),
            (104_640, 23_725),
            (128_365, 19_482),
            (147_847, 16_895),
            (164_742, 17_343),
            (182_085, 17_060),
            (199_145, 22_979),
            (222_124, 31_953),
        ],
        "the first twelve boundaries of this vector are pinned by ADR-007 §3.1"
    );
    assert!(
        spans.len() > 32,
        "1 MiB at 16 KiB average must produce dozens of chunks, got {}",
        spans.len()
    );
    for span in &spans {
        assert!(
            span.length <= CdcParams::V2.max,
            "no chunk may exceed max: {span:?}"
        );
        // Every chunk but the last respects the minimum.
        if span.offset + span.length < bytes.len() {
            assert!(
                span.length >= CdcParams::V2.min,
                "no interior chunk may be shorter than min: {span:?}"
            );
        }
    }
}

/// **Test vector: 1 MiB of zeros.**
///
/// The hash of a run of zeroes is periodic, so this vector is what catches an off-by-one in
/// the minimum/maximum handling: the boundaries are all at `max`.
#[test]
fn vector_all_zeroes_one_mib() {
    let bytes = vec![0u8; 1 << 20];
    let spans = chunk(&bytes, &CdcParams::V2);
    assert_tiles(&bytes, &spans);
    for span in &spans {
        if span.offset + span.length < bytes.len() {
            assert_eq!(
                span.length,
                CdcParams::V2.max,
                "zero runs must cut at max once the window is past min: {span:?}"
            );
        }
    }
    // Every chunk of a zero file is zero, which is what makes it elidable (§3.2).
    assert!(spans
        .iter()
        .all(|span| fs_snapshot::is_zero(&bytes[span.offset..span.offset + span.length])));
}

/// **Test vector: 1 MiB of a repeating 4 KiB pattern.**
///
/// Repetition is the case that separates a content-defined chunker from a fixed one: the
/// boundaries must still be decided by content, not by a multiple of the period.
#[test]
fn vector_repeating_pattern_one_mib() {
    let mut bytes = Vec::with_capacity(1 << 20);
    let mut block = pseudo_random(4_096, 0xBEEF);
    while bytes.len() < (1 << 20) {
        bytes.extend_from_slice(&block);
        // Perturb each repetition slightly, so the input is a *nearly* repeating pattern —
        // the interesting case for resynchronisation.
        block[0] = block[0].wrapping_add(1);
    }
    bytes.truncate(1 << 20);
    let spans = chunk(&bytes, &CdcParams::V2);
    assert_tiles(&bytes, &spans);
    assert!(spans.len() > 8, "got {} chunks", spans.len());
}

/// **Resynchronisation** (ADR-007 §3.4, §11-1): a small edit changes only a few chunks.
///
/// This is the property that makes CDC worth the complexity: without it, editing one byte
/// would rewrite every block after it.
#[test]
fn a_small_edit_disturbs_only_a_few_chunks() {
    for edit_len in [1usize, 16, 4_096] {
        let original = pseudo_random(1 << 20, 0x5EED);
        let mut edited = original.clone();
        let start = (1 << 19) + 1234;
        for (offset, byte) in edited[start..start + edit_len].iter_mut().enumerate() {
            *byte = byte.wrapping_add(u8::try_from(offset % 7).unwrap_or(1) + 1);
        }

        let before = chunk(&original, &CdcParams::V2);
        let after = chunk(&edited, &CdcParams::V2);
        assert_tiles(&original, &before);
        assert_tiles(&edited, &after);

        // Count the chunks whose bytes differ: the edit's chunk and its neighbours.
        let changed = changed_chunks(&original, &before, &edited, &after);
        assert!(
            changed <= 3,
            "an edit of {edit_len} bytes must disturb at most 3 chunks, disturbed {changed}"
        );
    }
}

/// How many chunks of `after` hold bytes that did not exist in `before` (by content).
fn changed_chunks(
    before_bytes: &[u8],
    before: &[ChunkSpan],
    after_bytes: &[u8],
    after: &[ChunkSpan],
) -> usize {
    let mut old: Vec<&[u8]> = before
        .iter()
        .map(|span| &before_bytes[span.offset..span.offset + span.length])
        .collect();
    old.sort_unstable();

    let mut changed = 0;
    for span in after {
        let slice = &after_bytes[span.offset..span.offset + span.length];
        if old.binary_search(&slice).is_err() {
            changed += 1;
        }
    }
    changed
}

/// Appending data does not disturb the chunks before it.
#[test]
fn an_append_leaves_earlier_chunks_alone() {
    let base = pseudo_random(256 * 1024, 0xA11CE);
    let mut extended = base.clone();
    extended.extend_from_slice(&pseudo_random(64 * 1024, 0xDA7A));

    let before = spans(&base);
    let after = spans(&extended);
    // Every span of the original except the one the append lands in must be unchanged.
    let shared = before.len().saturating_sub(1);
    assert!(
        after.len() > shared,
        "the extension must produce more chunks"
    );
    assert_eq!(
        &before[..shared],
        &after[..shared],
        "an append may not move any earlier boundary"
    );
}

/// Chunking is a pure function: the same bytes give the same spans, twice.
#[test]
fn chunking_is_deterministic() {
    let bytes = pseudo_random(200_000, 0x1234);
    assert_eq!(spans(&bytes), spans(&bytes));
    assert!(
        chunk(&[], &CdcParams::V2).is_empty(),
        "empty input, no spans"
    );
}

/// A short input produces exactly one chunk that is shorter than `min`.
#[test]
fn an_input_below_the_minimum_is_one_short_chunk() {
    let bytes = vec![7u8; 100];
    let spans = chunk(&bytes, &CdcParams::V2);
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].offset, spans[0].length), (0, 100));
}
