//! FastCDC-256K content-defined chunking, BLAKE3-256 hashing, and LZ4/Zstd-1 chunk
//! compression with an 8 KiB incompressible high-entropy probe.

use crate::error::{Error, Result};
use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Read;
use std::sync::OnceLock;

/// 32-byte BLAKE3-256 chunk digest.
pub type Hash32 = [u8; 32];

/// Fast identity hasher for `Hash32` keys: BLAKE3 hashes are already uniformly distributed,
/// so XOR-folding `bytes[8..32]` avoids SipHash overhead while staying completely independent
/// of `hash[0]` used for shard routing (`hash[0] & (SHARDS - 1)`), preventing hashbrown bucket
/// clustering inside sharded maps/sets (N5).
#[derive(Default)]
pub struct Hash32Hasher(u64);

impl std::hash::Hasher for Hash32Hasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() >= 32 {
            let a = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
            let b = u64::from_ne_bytes(bytes[16..24].try_into().unwrap());
            let c = u64::from_ne_bytes(bytes[24..32].try_into().unwrap());
            self.0 = a ^ b.rotate_left(17) ^ c.rotate_left(35);
        } else if bytes.len() >= 8 {
            self.0 = u64::from_ne_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        } else {
            let mut acc = 0u64;
            for &b in bytes {
                acc = (acc << 8) | u64::from(b);
            }
            self.0 = acc;
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// `BuildHasher` for `HashMap<Hash32, V, Hash32BuildHasher>` and `HashSet<Hash32, Hash32BuildHasher>`.
pub type Hash32BuildHasher = std::hash::BuildHasherDefault<Hash32Hasher>;

/// FastCDC-256K parameters (validated on 484.5 GB Unity dataset).
pub const MIN_CHUNK_BYTES: usize = 64 * 1024;
pub const AVG_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// Stricter mask (20 ones) below `AVG_CHUNK_BYTES`.
const MASK_S: u64 = 0x0000_D91F_0353_F000;
/// Looser mask (16 ones) at or above `AVG_CHUNK_BYTES`.
const MASK_L: u64 = 0x0000_D903_0353_7000;

/// Probe width for skipping incompressible data (PNG/MP3/MP4/compressed AssetBundles).
const PROBE_BYTES: usize = 8 * 1024;
const PROBE_MIN_CHUNK: usize = 16 * 1024;

/// Compression codec stored per chunk record in `.pack` files.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Codec {
    None = 0,
    Lz4 = 1,
    #[default]
    Zstd1 = 2,
}

impl Codec {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Lz4),
            2 => Ok(Self::Zstd1),
            other => Err(Error::Corrupt(format!("unknown chunk codec id {other}"))),
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "raw" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "zstd" | "zstd1" | "zstd-1" => Ok(Self::Zstd1),
            other => Err(Error::InvalidInput(format!(
                "unknown codec {other:?}; expected one of: zstd1, lz4, none"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd1 => "zstd1",
        }
    }
}

fn gear_table() -> &'static [u64; 256] {
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u64; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"fs-dedup/fastcdc-256k-gear/v1\0");
            hasher.update(&[i as u8]);
            let out = hasher.finalize();
            *slot = u64::from_le_bytes(out.as_bytes()[..8].try_into().unwrap());
        }
        table
    })
}

/// Computes full BLAKE3-256 digest of `data`.
#[inline]
pub fn blake3_hash(data: &[u8]) -> Hash32 {
    *blake3::hash(data).as_bytes()
}

/// Finds the next FastCDC-256K cut point in `data` (must be non-empty).
#[inline]
pub fn next_cut(data: &[u8]) -> usize {
    let n = data.len();
    if n <= MIN_CHUNK_BYTES {
        return n;
    }
    let gear = gear_table();
    let max_pos = n.min(MAX_CHUNK_BYTES);
    let mid_pos = max_pos.min(AVG_CHUNK_BYTES);
    let mut fp: u64 = 0;
    let mut i = MIN_CHUNK_BYTES;
    while i < mid_pos {
        fp = (fp << 1).wrapping_add(gear[data[i] as usize]);
        if (fp & MASK_S) == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < max_pos {
        fp = (fp << 1).wrapping_add(gear[data[i] as usize]);
        if (fp & MASK_L) == 0 {
            return i + 1;
        }
        i += 1;
    }
    max_pos
}

/// Chunks an in-memory slice with FastCDC-256K.
pub fn chunk_bytes(data: &[u8], mut on_chunk: impl FnMut(&[u8])) {
    let mut offset = 0usize;
    while offset < data.len() {
        let cut = next_cut(&data[offset..]);
        on_chunk(&data[offset..offset + cut]);
        offset += cut;
    }
}

/// Streams a `Read` source through FastCDC-256K using bounded RAM (~2 MiB) regardless of file size.
pub fn chunk_reader<R: Read>(
    reader: &mut R,
    mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let mut buf = Vec::with_capacity(MAX_CHUNK_BYTES * 2);
    let mut tmp = vec![0u8; 512 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        buf.extend_from_slice(&tmp[..n]);
        let mut offset = 0usize;
        while buf.len() - offset >= MAX_CHUNK_BYTES {
            let cut = next_cut(&buf[offset..]);
            on_chunk(&buf[offset..offset + cut])?;
            offset += cut;
        }
        if offset > 0 {
            buf.drain(..offset);
        }
    }
    let mut offset = 0usize;
    while offset < buf.len() {
        let cut = next_cut(&buf[offset..]);
        on_chunk(&buf[offset..offset + cut])?;
        offset += cut;
    }
    Ok(total)
}

struct ZstdThreadCtx {
    cctx: *mut zstd_sys::ZSTD_CCtx,
    dctx: *mut zstd_sys::ZSTD_DCtx,
    probe_buf: Vec<u8>,
}

impl ZstdThreadCtx {
    fn new() -> Self {
        // SAFETY: standard zstd context constructors; freed in Drop.
        let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
        let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
        assert!(!cctx.is_null() && !dctx.is_null(), "zstd ctx alloc failed");
        Self {
            cctx,
            dctx,
            probe_buf: Vec::with_capacity(16 * 1024),
        }
    }
}

impl Drop for ZstdThreadCtx {
    fn drop(&mut self) {
        // SAFETY: allocated by ZSTD_create*Ctx above.
        unsafe {
            zstd_sys::ZSTD_freeCCtx(self.cctx);
            zstd_sys::ZSTD_freeDCtx(self.dctx);
        }
    }
}

thread_local! {
    static ZSTD_CTX: RefCell<ZstdThreadCtx> = RefCell::new(ZstdThreadCtx::new());
}

/// Compresses `raw` using `preferred` codec (with 8 KiB incompressible probe).
/// Returns `(actual_codec, stored_bytes)`. Falls back to `Codec::None` if data does not shrink.
pub fn compress_chunk(preferred: Codec, raw: &[u8]) -> Result<(Codec, Vec<u8>)> {
    if preferred == Codec::None || raw.len() < 64 {
        return Ok((Codec::None, raw.to_vec()));
    }
    match preferred {
        Codec::None => Ok((Codec::None, raw.to_vec())),
        Codec::Lz4 => {
            if raw.len() > PROBE_MIN_CHUNK {
                let probe = lz4_flex::block::compress(&raw[..PROBE_BYTES]);
                if probe.len() * 100 >= PROBE_BYTES * 95 {
                    return Ok((Codec::None, raw.to_vec()));
                }
            }
            let out = lz4_flex::block::compress(raw);
            if out.len() < raw.len() {
                Ok((Codec::Lz4, out))
            } else {
                Ok((Codec::None, raw.to_vec()))
            }
        }
        Codec::Zstd1 => ZSTD_CTX.with(|cell| {
            let mut ctx = cell.borrow_mut();
            if raw.len() > PROBE_MIN_CHUNK {
                let bound = unsafe { zstd_sys::ZSTD_compressBound(PROBE_BYTES) };
                if ctx.probe_buf.len() < bound {
                    ctx.probe_buf.resize(bound, 0);
                }
                let res = unsafe {
                    zstd_sys::ZSTD_compressCCtx(
                        ctx.cctx,
                        ctx.probe_buf.as_mut_ptr().cast::<c_void>(),
                        bound,
                        raw.as_ptr().cast::<c_void>(),
                        PROBE_BYTES,
                        1,
                    )
                };
                if unsafe { zstd_sys::ZSTD_isError(res) } == 0 && res * 100 >= PROBE_BYTES * 95 {
                    return Ok((Codec::None, raw.to_vec()));
                }
            }
            let bound = unsafe { zstd_sys::ZSTD_compressBound(raw.len()) };
            let mut out = vec![0u8; bound];
            let res = unsafe {
                zstd_sys::ZSTD_compressCCtx(
                    ctx.cctx,
                    out.as_mut_ptr().cast::<c_void>(),
                    bound,
                    raw.as_ptr().cast::<c_void>(),
                    raw.len(),
                    1,
                )
            };
            if unsafe { zstd_sys::ZSTD_isError(res) } != 0 {
                return Err(Error::Other("zstd compress failed".into()));
            }
            if res < raw.len() {
                out.truncate(res);
                Ok((Codec::Zstd1, out))
            } else {
                Ok((Codec::None, raw.to_vec()))
            }
        }),
    }
}

/// Decompresses `stored` directly into `dst` (which must have length `raw_len`).
pub fn decompress_chunk_into(codec: Codec, stored: &[u8], dst: &mut [u8]) -> Result<()> {
    let raw_len = dst.len();
    match codec {
        Codec::None => {
            if stored.len() != raw_len {
                return Err(Error::Corrupt(format!(
                    "raw chunk length mismatch: stored {} != expected {}",
                    stored.len(),
                    raw_len
                )));
            }
            dst.copy_from_slice(stored);
            Ok(())
        }
        Codec::Lz4 => {
            let n = lz4_flex::block::decompress_into(stored, dst)
                .map_err(|e| Error::Corrupt(format!("lz4 decompress failed: {e}")))?;
            if n != raw_len {
                return Err(Error::Corrupt(format!(
                    "lz4 decoded length {n} != expected {raw_len}"
                )));
            }
            Ok(())
        }
        Codec::Zstd1 => ZSTD_CTX.with(|cell| {
            let ctx = cell.borrow_mut();
            let res = unsafe {
                zstd_sys::ZSTD_decompressDCtx(
                    ctx.dctx,
                    dst.as_mut_ptr().cast::<c_void>(),
                    raw_len,
                    stored.as_ptr().cast::<c_void>(),
                    stored.len(),
                )
            };
            if unsafe { zstd_sys::ZSTD_isError(res) } != 0 || res != raw_len {
                return Err(Error::Corrupt(format!(
                    "zstd decompress failed or length mismatch (expected {raw_len})"
                )));
            }
            Ok(())
        }),
    }
}

/// Decompresses `stored` directly into a freshly allocated `Arc<[u8]>` in a single allocation
/// (no intermediate `Vec<u8>` -> `Arc::from` full-chunk memcpy, MSRV 1.85 compatible, zero `unsafe`).
pub fn decompress_chunk_arc(
    codec: Codec,
    stored: &[u8],
    raw_len: usize,
) -> Result<std::sync::Arc<[u8]>> {
    // `repeat_n` implements `TrustedLen`, so `Arc<[u8]>::from_iter` allocates the `ArcInner<[u8]>`
    // directly in a single allocation with refcount 1.
    let mut arc: std::sync::Arc<[u8]> = std::iter::repeat_n(0u8, raw_len).collect();
    let dst = std::sync::Arc::get_mut(&mut arc).expect("freshly allocated Arc is unique");
    decompress_chunk_into(codec, stored, dst)?;
    Ok(arc)
}

/// Decompresses `stored` into exact `raw_len` bytes according to `codec`.
pub fn decompress_chunk(codec: Codec, stored: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; raw_len];
    decompress_chunk_into(codec, stored, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn hash32_hasher_independent_of_shard_prefix_byte() {
        use std::hash::{BuildHasher, Hasher};
        let bh = Hash32BuildHasher::default();
        let mut h1 = [0u8; 32];
        let mut h2 = [0u8; 32];
        // Identical first 8 bytes (same shard `hash[0] & 63`), differing only in bytes[8..32].
        h1[0..8].copy_from_slice(&[0x2A; 8]);
        h2[0..8].copy_from_slice(&[0x2A; 8]);
        h1[8] = 1;
        h2[8] = 2;
        let mut s1 = bh.build_hasher();
        s1.write(&h1);
        let mut s2 = bh.build_hasher();
        s2.write(&h2);
        assert_ne!(s1.finish(), s2.finish());
    }

    #[test]
    fn compress_decompress_roundtrip_all_codecs() {
        let compressible = b"Unity Prefab YAML line repeated many times!\n".repeat(4000);
        for codec in [Codec::None, Codec::Lz4, Codec::Zstd1] {
            let (actual, stored) = compress_chunk(codec, &compressible).unwrap();
            if codec != Codec::None {
                assert_eq!(actual, codec);
                assert!(stored.len() < compressible.len() / 5);
            }
            let back = decompress_chunk(actual, &stored, compressible.len()).unwrap();
            assert_eq!(back, compressible);
        }
    }

    #[test]
    fn chunk_reader_matches_chunk_bytes_exactly() {
        let mut data = vec![0u8; 2_500_000];
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        for b in &mut data {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *b = (state >> 56) as u8;
        }
        let mut slice_chunks = Vec::new();
        chunk_bytes(&data, |c| slice_chunks.push((blake3_hash(c), c.len())));

        let mut reader_chunks = Vec::new();
        let total = chunk_reader(&mut Cursor::new(&data), |c| {
            reader_chunks.push((blake3_hash(c), c.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(total, data.len() as u64);
        assert_eq!(slice_chunks, reader_chunks);
    }
}
