//! Manual import memory probe; does not require a live WinFsp mount.
//!
//! cargo test -p fs-dedup --test import_memory -- --ignored --test-threads=1 --nocapture
//!
//! Measures peak live Rust allocation bytes above the pre-import baseline, NOT process
//! RSS. Native SQLite/zstd allocations and OS page caches are intentionally not counted.
//! There are no throughput assertions: timing depends on the host and test profile.

use fs_dedup::{Codec, DedupStore, StoreConfig};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const MIB: usize = 1024 * 1024;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

fn add_live(bytes: usize) {
    let current = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(current, Ordering::Relaxed);
}

// SAFETY: every pointer/layout is passed unchanged to System; accounting does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add_live(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            add_live(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: pointer and layout describe the caller's live allocation.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: arguments satisfy GlobalAlloc::realloc's contract and are forwarded unchanged.
        let new_pointer = unsafe { System.realloc(pointer, layout, size) };
        if !new_pointer.is_null() {
            if size >= layout.size() {
                add_live(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "manual allocation probe: creates 72 MiB of source data across two isolated runs"]
fn report_import_peak_heap_for_unique_one_mib_files() {
    for count in [8, 64] {
        let temp = TempDir(std::env::temp_dir().join(format!(
            "fs-dedup-import-memory-{}-{count}",
            std::process::id()
        )));
        std::fs::create_dir(&temp.0).unwrap();
        let source = temp.0.join("source");
        std::fs::create_dir(&source).unwrap();
        let mut data = vec![0u8; MIB];
        let mut seed = 0x9876_5432_10ab_cdefu64;
        for index in 0..count {
            // Unique deterministic data: no accidental repeated chunks across source files.
            for chunk in data.chunks_exact_mut(8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                chunk.copy_from_slice(&seed.to_le_bytes());
            }
            std::fs::write(source.join(format!("{index}.bin")), &data).unwrap();
        }
        drop(data);
        let store = DedupStore::init(
            temp.0.join("store"),
            StoreConfig {
                codec: Codec::None,
                ..StoreConfig::default()
            },
        )
        .unwrap();

        let baseline = LIVE.load(Ordering::Relaxed);
        PEAK.store(baseline, Ordering::Relaxed);
        let start = Instant::now();
        let summary = store.import_dir(&source, "ws", 2, None).unwrap();
        let peak_delta = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
        eprintln!(
            "files={count}, logical={} MiB, peak Rust heap delta={:.2} MiB, elapsed={:?}",
            summary.logical_bytes / MIB as u64,
            peak_delta as f64 / MIB as f64,
            start.elapsed(),
        );
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.files, count);
        assert_eq!(summary.logical_bytes, count * MIB as u64);
    }
}
