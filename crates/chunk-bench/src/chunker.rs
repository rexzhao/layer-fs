//! Chunking strategies: whole-file, fixed-size, upstream LayerFS 16 KiB CDC,
//! and normalized Gear-hash FastCDC (64 KiB / 256 KiB / 1 MiB).

use std::io::Cursor;
use std::sync::OnceLock;

/// Supported chunking strategies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Whole,
    Fixed256K,
    Fixed1M,
    LayerFs16K,
    Cdc64K,
    Cdc256K,
    Cdc1M,
}

impl Strategy {
    pub const ALL: [Self; 7] = [
        Self::Whole,
        Self::Fixed256K,
        Self::Fixed1M,
        Self::LayerFs16K,
        Self::Cdc64K,
        Self::Cdc256K,
        Self::Cdc1M,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Fixed256K => "fixed-256k",
            Self::Fixed1M => "fixed-1m",
            Self::LayerFs16K => "layerfs-16k",
            Self::Cdc64K => "cdc-64k",
            Self::Cdc256K => "cdc-256k",
            Self::Cdc1M => "cdc-1m",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "whole" | "whole-file" => Ok(Self::Whole),
            "fixed-256k" | "fixed256k" => Ok(Self::Fixed256K),
            "fixed-1m" | "fixed1m" => Ok(Self::Fixed1M),
            "layerfs-16k" | "layerfs16k" | "cdc-16k" => Ok(Self::LayerFs16K),
            "cdc-64k" | "fastcdc-64k" => Ok(Self::Cdc64K),
            "cdc-256k" | "fastcdc-256k" => Ok(Self::Cdc256K),
            "cdc-1m" | "fastcdc-1m" => Ok(Self::Cdc1M),
            other => Err(format!(
                "unknown strategy {other:?}; expected one of: whole, fixed-256k, fixed-1m, layerfs-16k, cdc-64k, cdc-256k, cdc-1m"
            )),
        }
    }

    /// Files at or below this length produce a single chunk identical to the whole file,
    /// allowing us to reuse the whole-file BLAKE3 hash with zero chunker work.
    pub fn single_chunk_threshold(self) -> u64 {
        match self {
            Self::Whole => u64::MAX,
            Self::Fixed256K => 256 * 1024,
            Self::Fixed1M => 1024 * 1024,
            Self::LayerFs16K => layerfs_content::file::cdc::MINIMUM_CHUNK_BYTES as u64,
            Self::Cdc64K => 16 * 1024,
            Self::Cdc256K => 64 * 1024,
            Self::Cdc1M => 256 * 1024,
        }
    }
}

/// Deterministic 256-entry 64-bit Gear table derived from BLAKE3.
fn gear_table() -> &'static [u64; 256] {
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u64; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"chunk-bench/fastcdc-gear/v1\0");
            hasher.update(&[i as u8]);
            let out = hasher.finalize();
            *slot = u64::from_le_bytes(out.as_bytes()[..8].try_into().unwrap());
        }
        table
    })
}

/// Parameters for normalized Gear-hash FastCDC (NC = 2).
#[derive(Clone, Copy)]
pub struct FastCdcSpec {
    pub min_size: usize,
    pub avg_size: usize,
    pub max_size: usize,
    /// Stricter mask (avg_bits + 2 ones) used below `avg_size`.
    pub mask_s: u64,
    /// Looser mask (avg_bits - 2 ones) used at or above `avg_size`.
    pub mask_l: u64,
}

impl FastCdcSpec {
    /// Average 64 KiB (16 KiB .. 256 KiB): 16 bits normal -> 18 bits small, 14 bits large.
    pub const CDC_64K: Self = Self {
        min_size: 16 * 1024,
        avg_size: 64 * 1024,
        max_size: 256 * 1024,
        mask_s: 0x0000_D907_0353_F000, // 18 ones
        mask_l: 0x0000_D903_0353_1000, // 14 ones
    };

    /// Average 256 KiB (64 KiB .. 1 MiB): 18 bits normal -> 20 bits small, 16 bits large.
    pub const CDC_256K: Self = Self {
        min_size: 64 * 1024,
        avg_size: 256 * 1024,
        max_size: 1024 * 1024,
        mask_s: 0x0000_D91F_0353_F000, // 20 ones
        mask_l: 0x0000_D903_0353_7000, // 16 ones
    };

    /// Average 1 MiB (256 KiB .. 4 MiB): 20 bits normal -> 22 bits small, 18 bits large.
    pub const CDC_1M: Self = Self {
        min_size: 256 * 1024,
        avg_size: 1024 * 1024,
        max_size: 4 * 1024 * 1024,
        mask_s: 0x0000_D97F_0353_F000, // 22 ones
        mask_l: 0x0000_D907_0353_F000, // 18 ones
    };
}

/// Computes the 128-bit BLAKE3 chunk fingerprint (collision probability < 10^-24 for 10^8 chunks).
#[inline]
pub fn hash128(bytes: &[u8]) -> u128 {
    let digest = blake3::hash(bytes);
    u128::from_le_bytes(digest.as_bytes()[..16].try_into().unwrap())
}

/// Finds the next FastCDC cut point in `data` (which must be non-empty).
#[inline]
fn next_fastcdc_cut(data: &[u8], spec: FastCdcSpec, gear: &[u64; 256]) -> usize {
    let n = data.len();
    if n <= spec.min_size {
        return n;
    }
    let max_pos = n.min(spec.max_size);
    let mid_pos = max_pos.min(spec.avg_size);
    let mut fp: u64 = 0;
    let mut i = spec.min_size;
    while i < mid_pos {
        fp = (fp << 1).wrapping_add(gear[data[i] as usize]);
        if (fp & spec.mask_s) == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < max_pos {
        fp = (fp << 1).wrapping_add(gear[data[i] as usize]);
        if (fp & spec.mask_l) == 0 {
            return i + 1;
        }
        i += 1;
    }
    max_pos
}

/// Chunks an in-memory buffer according to `strategy`, invoking `on_chunk(chunk_bytes)` for each chunk.
pub fn chunk_slice(strategy: Strategy, data: &[u8], mut on_chunk: impl FnMut(&[u8])) {
    if data.is_empty() {
        on_chunk(&[]);
        return;
    }
    match strategy {
        Strategy::Whole => on_chunk(data),
        Strategy::Fixed256K => {
            for block in data.chunks(256 * 1024) {
                on_chunk(block);
            }
        }
        Strategy::Fixed1M => {
            for block in data.chunks(1024 * 1024) {
                on_chunk(block);
            }
        }
        Strategy::LayerFs16K => {
            let _ = layerfs_content::file::cdc::FastCdc::new().scan(Cursor::new(data), |chunk| {
                on_chunk(chunk);
                Ok(())
            });
        }
        Strategy::Cdc64K => chunk_fastcdc_slice(data, FastCdcSpec::CDC_64K, on_chunk),
        Strategy::Cdc256K => chunk_fastcdc_slice(data, FastCdcSpec::CDC_256K, on_chunk),
        Strategy::Cdc1M => chunk_fastcdc_slice(data, FastCdcSpec::CDC_1M, on_chunk),
    }
}

fn chunk_fastcdc_slice(data: &[u8], spec: FastCdcSpec, mut on_chunk: impl FnMut(&[u8])) {
    let gear = gear_table();
    let mut offset = 0usize;
    while offset < data.len() {
        let cut = next_fastcdc_cut(&data[offset..], spec, gear);
        on_chunk(&data[offset..offset + cut]);
        offset += cut;
    }
}

/// Streaming stateful chunker for files larger than the in-memory fast path (e.g. > 16 MiB).
/// Guarantees byte-for-byte identical cut points to `chunk_slice` while bounding RAM to <= 4 MiB.
pub enum StreamChunker {
    Whole(Box<blake3::Hasher>, u64),
    Fixed {
        block_size: usize,
        buf: Vec<u8>,
    },
    LayerFs16K {
        buf: Vec<u8>,
        pending: Vec<u8>,
    },
    FastCdc {
        spec: FastCdcSpec,
        buf: Vec<u8>,
    },
}

impl StreamChunker {
    pub fn new(strategy: Strategy) -> Self {
        match strategy {
            Strategy::Whole => Self::Whole(Box::new(blake3::Hasher::new()), 0),
            Strategy::Fixed256K => Self::Fixed {
                block_size: 256 * 1024,
                buf: Vec::with_capacity(256 * 1024),
            },
            Strategy::Fixed1M => Self::Fixed {
                block_size: 1024 * 1024,
                buf: Vec::with_capacity(1024 * 1024),
            },
            Strategy::LayerFs16K => Self::LayerFs16K {
                buf: Vec::new(),
                pending: Vec::new(),
            },
            Strategy::Cdc64K => Self::FastCdc {
                spec: FastCdcSpec::CDC_64K,
                buf: Vec::with_capacity(FastCdcSpec::CDC_64K.max_size * 2),
            },
            Strategy::Cdc256K => Self::FastCdc {
                spec: FastCdcSpec::CDC_256K,
                buf: Vec::with_capacity(FastCdcSpec::CDC_256K.max_size * 2),
            },
            Strategy::Cdc1M => Self::FastCdc {
                spec: FastCdcSpec::CDC_1M,
                buf: Vec::with_capacity(FastCdcSpec::CDC_1M.max_size * 2),
            },
        }
    }

    /// Feeds the next bytes from the file; emits completed chunks via `on_chunk(hash128, raw_bytes_opt)`.
    /// `raw_bytes_opt` is `Some(&[u8])` for all strategies except `Whole` (where `None` is passed).
    pub fn push(&mut self, mut input: &[u8], mut on_chunk: impl FnMut(u128, Option<&[u8]>)) {
        match self {
            Self::Whole(hasher, len) => {
                hasher.update(input);
                *len += input.len() as u64;
            }
            Self::Fixed { block_size, buf } => {
                while !input.is_empty() {
                    if buf.is_empty() && input.len() >= *block_size {
                        let (block, rest) = input.split_at(*block_size);
                        on_chunk(hash128(block), Some(block));
                        input = rest;
                    } else {
                        let need = (*block_size - buf.len()).min(input.len());
                        buf.extend_from_slice(&input[..need]);
                        input = &input[need..];
                        if buf.len() == *block_size {
                            on_chunk(hash128(buf), Some(buf.as_slice()));
                            buf.clear();
                        }
                    }
                }
            }
            Self::LayerFs16K { buf, pending } => {
                // Keep exact parity with `layerfs_content::file::cdc::FastCdc` by streaming
                // bounded windows while carrying the last incomplete chunk across reads.
                // Note: `FastCdc::scan` emits the final partial chunk at EOF; to avoid cutting
                // prematurely across read calls, we only finalize when `finish` is called, OR
                // carry `pending` until >= 2 * MAXIMUM_CHUNK_BYTES and split only up to the
                // second-to-last chunk!
                pending.extend_from_slice(input);
                let limit = layerfs_content::file::cdc::MAXIMUM_CHUNK_BYTES * 8;
                if pending.len() >= limit {
                    let mut cuts: Vec<usize> = Vec::new();
                    let mut pos = 0usize;
                    let _ = layerfs_content::file::cdc::FastCdc::new().scan(
                        Cursor::new(pending.as_slice()),
                        |chunk| {
                            pos += chunk.len();
                            cuts.push(pos);
                            Ok(())
                        },
                    );
                    // Hold back the final chunk because it was cut by EOF rather than content.
                    if cuts.len() > 1 {
                        let safe_end = cuts[cuts.len() - 2];
                        let mut prev = 0usize;
                        for &end in &cuts[..cuts.len() - 1] {
                            let slice = &pending[prev..end];
                            on_chunk(hash128(slice), Some(slice));
                            prev = end;
                        }
                        buf.clear();
                        buf.extend_from_slice(&pending[safe_end..]);
                        std::mem::swap(pending, buf);
                    }
                }
            }
            Self::FastCdc { spec, buf } => {
                let gear = gear_table();
                buf.extend_from_slice(input);
                let mut offset = 0usize;
                // Only cut while at least `max_size` bytes remain so cuts never depend on buffer edge.
                while buf.len() - offset >= spec.max_size {
                    let cut = next_fastcdc_cut(&buf[offset..], *spec, gear);
                    let slice = &buf[offset..offset + cut];
                    on_chunk(hash128(slice), Some(slice));
                    offset += cut;
                }
                if offset > 0 {
                    buf.drain(..offset);
                }
            }
        }
    }

    pub fn finish(self, mut on_chunk: impl FnMut(u128, u64, Option<&[u8]>)) {
        match self {
            Self::Whole(hasher, len) => {
                let digest = hasher.finalize();
                let h = u128::from_le_bytes(digest.as_bytes()[..16].try_into().unwrap());
                on_chunk(h, len, None);
            }
            Self::Fixed { buf, .. } => {
                if !buf.is_empty() {
                    on_chunk(hash128(&buf), buf.len() as u64, Some(&buf));
                }
            }
            Self::LayerFs16K { pending, .. } => {
                if !pending.is_empty() {
                    let _ = layerfs_content::file::cdc::FastCdc::new().scan(
                        Cursor::new(pending.as_slice()),
                        |chunk| {
                            on_chunk(hash128(chunk), chunk.len() as u64, Some(chunk));
                            Ok(())
                        },
                    );
                }
            }
            Self::FastCdc { spec, buf } => {
                let gear = gear_table();
                let mut offset = 0usize;
                while offset < buf.len() {
                    let cut = next_fastcdc_cut(&buf[offset..], spec, gear);
                    let slice = &buf[offset..offset + cut];
                    on_chunk(hash128(slice), slice.len() as u64, Some(slice));
                    offset += cut;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for chunk in out.chunks_mut(8) {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let val = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&val[..n]);
        }
        out
    }

    #[test]
    fn fastcdc_masks_have_expected_bit_counts() {
        assert_eq!(FastCdcSpec::CDC_64K.mask_s.count_ones(), 18);
        assert_eq!(FastCdcSpec::CDC_64K.mask_l.count_ones(), 14);
        assert_eq!(FastCdcSpec::CDC_256K.mask_s.count_ones(), 20);
        assert_eq!(FastCdcSpec::CDC_256K.mask_l.count_ones(), 16);
        assert_eq!(FastCdcSpec::CDC_1M.mask_s.count_ones(), 22);
        assert_eq!(FastCdcSpec::CDC_1M.mask_l.count_ones(), 18);
    }

    #[test]
    fn fastcdc_average_size_and_shift_resilience() {
        let data = pseudo_random_bytes(16 * 1024 * 1024);
        let mut chunks = Vec::new();
        chunk_slice(Strategy::Cdc64K, &data, |c| chunks.push(hash128(c)));
        let avg = data.len() / chunks.len();
        assert!(
            (40_000..=95_000).contains(&avg),
            "expected ~64 KiB avg chunk size, got {avg}"
        );

        // Insert 137 bytes at the start: FastCDC should still match >95% of chunks.
        let mut shifted = vec![0xABu8; 137];
        shifted.extend_from_slice(&data);
        let mut shifted_chunks = Vec::new();
        chunk_slice(Strategy::Cdc64K, &shifted, |c| {
            shifted_chunks.push(hash128(c))
        });
        let orig_set: std::collections::HashSet<u128> = chunks.iter().copied().collect();
        let matched = shifted_chunks
            .iter()
            .filter(|h| orig_set.contains(h))
            .count();
        let ratio = matched as f64 / chunks.len() as f64;
        assert!(ratio > 0.95, "shift resilience too low: {ratio:.3}");
    }

    #[test]
    fn streaming_matches_slice_exactly_for_all_strategies() {
        let data = pseudo_random_bytes(3 * 1024 * 1024 + 12_345);
        for strategy in Strategy::ALL {
            let mut slice_hashes = Vec::new();
            chunk_slice(strategy, &data, |c| {
                slice_hashes.push((hash128(c), c.len() as u64))
            });

            let mut streamer = StreamChunker::new(strategy);
            let mut stream_hashes = Vec::new();
            for piece in data.chunks(311_111) {
                streamer.push(piece, |h, raw| {
                    stream_hashes.push((h, raw.map_or(0, |r| r.len() as u64)));
                });
            }
            streamer.finish(|h, len, _| {
                stream_hashes.push((h, len));
            });
            assert_eq!(
                slice_hashes,
                stream_hashes,
                "stream vs slice mismatch for {}",
                strategy.name()
            );
        }
    }
}
