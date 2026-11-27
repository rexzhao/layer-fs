//! Fast LZ4 / Zstd-1 / Zstd-3 chunk compression benchmark with incompressible probe.

use std::ffi::c_void;
use std::time::Instant;

/// Probe width for skipping incompressible high-entropy chunks (matches LayerFS's 8 KiB probe).
const PROBE_BYTES: usize = 8 * 1024;
/// Only run the probe on chunks strictly larger than twice the probe width.
const PROBE_MIN_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct CodecMetrics {
    pub sampled_chunks: u64,
    pub sampled_raw_bytes: u64,
    pub stored_bytes: u64,
    pub probe_skipped_chunks: u64,
    pub probe_skipped_bytes: u64,
    pub compress_ns: u64,
    pub decompress_ns: u64,
    pub decompressed_bytes: u64,
}

impl CodecMetrics {
    pub fn merge_from(&mut self, other: &Self) {
        self.sampled_chunks += other.sampled_chunks;
        self.sampled_raw_bytes += other.sampled_raw_bytes;
        self.stored_bytes += other.stored_bytes;
        self.probe_skipped_chunks += other.probe_skipped_chunks;
        self.probe_skipped_bytes += other.probe_skipped_bytes;
        self.compress_ns += other.compress_ns;
        self.decompress_ns += other.decompress_ns;
        self.decompressed_bytes += other.decompressed_bytes;
    }

    pub fn ratio(&self) -> f64 {
        if self.sampled_raw_bytes == 0 {
            1.0
        } else {
            self.stored_bytes as f64 / self.sampled_raw_bytes as f64
        }
    }

    pub fn compress_mb_s(&self) -> f64 {
        if self.compress_ns == 0 {
            0.0
        } else {
            (self.sampled_raw_bytes as f64 / (1024.0 * 1024.0))
                / (self.compress_ns as f64 / 1_000_000_000.0)
        }
    }

    pub fn decompress_mb_s(&self) -> f64 {
        if self.decompress_ns == 0 {
            0.0
        } else {
            (self.decompressed_bytes as f64 / (1024.0 * 1024.0))
                / (self.decompress_ns as f64 / 1_000_000_000.0)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct CompressionSuite {
    pub lz4: CodecMetrics,
    pub zstd1: CodecMetrics,
    pub zstd3: CodecMetrics,
}

impl CompressionSuite {
    pub fn merge_from(&mut self, other: &Self) {
        self.lz4.merge_from(&other.lz4);
        self.zstd1.merge_from(&other.zstd1);
        self.zstd3.merge_from(&other.zstd3);
    }
}

/// Reusable per-worker compression contexts so benchmarking allocates zero contexts per chunk.
pub struct WorkerCompressor {
    cctx: *mut zstd_sys::ZSTD_CCtx,
    dctx: *mut zstd_sys::ZSTD_DCtx,
    out_buf: Vec<u8>,
    dec_buf: Vec<u8>,
    pub suite: CompressionSuite,
}

impl WorkerCompressor {
    pub fn new() -> Self {
        // SAFETY: standard zstd context constructors; freed in Drop.
        let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
        let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
        assert!(!cctx.is_null() && !dctx.is_null(), "zstd context allocation failed");
        Self {
            cctx,
            dctx,
            out_buf: Vec::with_capacity(512 * 1024),
            dec_buf: Vec::with_capacity(512 * 1024),
            suite: CompressionSuite::default(),
        }
    }

    /// Benchmarks LZ4, Zstd-1, and Zstd-3 on one unique chunk (including decompression speed
    /// on compressed chunks).
    pub fn bench_chunk(&mut self, raw: &[u8]) {
        if raw.is_empty() {
            return;
        }
        self.bench_lz4(raw);
        self.bench_zstd(raw, 1);
        self.bench_zstd(raw, 3);
    }

    fn bench_lz4(&mut self, raw: &[u8]) {
        let m = &mut self.suite.lz4;
        m.sampled_chunks += 1;
        m.sampled_raw_bytes += raw.len() as u64;

        let t0 = Instant::now();
        if raw.len() > PROBE_MIN_CHUNK {
            let probe = lz4_flex::block::compress(&raw[..PROBE_BYTES]);
            if probe.len() * 100 >= PROBE_BYTES * 95 {
                m.compress_ns += t0.elapsed().as_nanos() as u64;
                m.probe_skipped_chunks += 1;
                m.probe_skipped_bytes += raw.len() as u64;
                m.stored_bytes += raw.len() as u64;
                return;
            }
        }
        let max_len = lz4_flex::block::get_maximum_output_size(raw.len());
        if self.out_buf.len() < max_len {
            self.out_buf.resize(max_len, 0);
        }
        let comp_len = lz4_flex::block::compress_into(raw, &mut self.out_buf).unwrap_or(raw.len());
        m.compress_ns += t0.elapsed().as_nanos() as u64;

        if comp_len < raw.len() {
            m.stored_bytes += comp_len as u64;
            if self.dec_buf.len() < raw.len() {
                self.dec_buf.resize(raw.len(), 0);
            }
            let d0 = Instant::now();
            let _ = lz4_flex::block::decompress_into(
                &self.out_buf[..comp_len],
                &mut self.dec_buf[..raw.len()],
            );
            m.decompress_ns += d0.elapsed().as_nanos() as u64;
            m.decompressed_bytes += raw.len() as u64;
        } else {
            m.stored_bytes += raw.len() as u64;
        }
    }

    fn bench_zstd(&mut self, raw: &[u8], level: i32) {
        let m = if level == 1 {
            &mut self.suite.zstd1
        } else {
            &mut self.suite.zstd3
        };
        m.sampled_chunks += 1;
        m.sampled_raw_bytes += raw.len() as u64;

        let t0 = Instant::now();
        if raw.len() > PROBE_MIN_CHUNK {
            let probe_bound = unsafe { zstd_sys::ZSTD_compressBound(PROBE_BYTES) };
            if self.out_buf.len() < probe_bound {
                self.out_buf.resize(probe_bound, 0);
            }
            // SAFETY: valid cctx, valid input/output slices.
            let probe_res = unsafe {
                zstd_sys::ZSTD_compressCCtx(
                    self.cctx,
                    self.out_buf.as_mut_ptr().cast::<c_void>(),
                    probe_bound,
                    raw.as_ptr().cast::<c_void>(),
                    PROBE_BYTES,
                    level,
                )
            };
            if unsafe { zstd_sys::ZSTD_isError(probe_res) } == 0
                && probe_res * 100 >= PROBE_BYTES * 95
            {
                m.compress_ns += t0.elapsed().as_nanos() as u64;
                m.probe_skipped_chunks += 1;
                m.probe_skipped_bytes += raw.len() as u64;
                m.stored_bytes += raw.len() as u64;
                return;
            }
        }

        let bound = unsafe { zstd_sys::ZSTD_compressBound(raw.len()) };
        if self.out_buf.len() < bound {
            self.out_buf.resize(bound, 0);
        }
        // SAFETY: valid cctx and buffers sized to ZSTD_compressBound(raw.len()).
        let res = unsafe {
            zstd_sys::ZSTD_compressCCtx(
                self.cctx,
                self.out_buf.as_mut_ptr().cast::<c_void>(),
                bound,
                raw.as_ptr().cast::<c_void>(),
                raw.len(),
                level,
            )
        };
        m.compress_ns += t0.elapsed().as_nanos() as u64;

        if unsafe { zstd_sys::ZSTD_isError(res) } == 0 && res < raw.len() {
            m.stored_bytes += res as u64;
            if self.dec_buf.len() < raw.len() {
                self.dec_buf.resize(raw.len(), 0);
            }
            let d0 = Instant::now();
            // SAFETY: valid dctx, compressed frame of `res` bytes, destination of `raw.len()`.
            let _ = unsafe {
                zstd_sys::ZSTD_decompressDCtx(
                    self.dctx,
                    self.dec_buf.as_mut_ptr().cast::<c_void>(),
                    raw.len(),
                    self.out_buf.as_ptr().cast::<c_void>(),
                    res,
                )
            };
            m.decompress_ns += d0.elapsed().as_nanos() as u64;
            m.decompressed_bytes += raw.len() as u64;
        } else {
            m.stored_bytes += raw.len() as u64;
        }
    }
}

impl Drop for WorkerCompressor {
    fn drop(&mut self) {
        // SAFETY: contexts allocated by ZSTD_create*Ctx in new().
        unsafe {
            zstd_sys::ZSTD_freeCCtx(self.cctx);
            zstd_sys::ZSTD_freeDCtx(self.dctx);
        }
    }
}
