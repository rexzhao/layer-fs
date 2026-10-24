//! Acceptance D, measured without the event probe (ADR-008 §5).
//!
//! `sync-probe` also enables `rusqlite/trace`, whose callback runs for every statement — 1000
//! INSERTs inside one transaction means 1000 trace callbacks inside the window being measured.
//! This target therefore requires `lock-probe` only (timestamps, no trace), so the numbers are
//! about the transaction, not about the instrumentation.
//!
//! Run it as:
//!
//! ```text
//! cargo test --release -p cas-store --features lock-probe --test lock_hold -- --ignored --nocapture
//! ```
#![cfg(feature = "lock-probe")]

use std::path::PathBuf;

use cas_core::Role;
use cas_store::{probe, OpenMode, Store};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cas-store-lock-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `S`, `H(10)`, `H(1000)`, and the marginal lock cost per object.
#[test]
#[ignore = "timing: run explicitly with --ignored --nocapture and paste the numbers"]
fn the_transaction_does_not_grow_with_the_object_count() {
    let _guard = probe::serialised();
    let root = TempRoot::new("hold");
    let store = Store::open(root.store(), OpenMode::Create).expect("open");

    let mut sync_samples = Vec::new();
    for _ in 0..20 {
        let file = root.store().join("calibration.bin");
        std::fs::write(&file, vec![0u8; 4096]).expect("write");
        let handle = std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .expect("open");
        let started = std::time::Instant::now();
        handle.sync_all().expect("sync");
        sync_samples.push(started.elapsed().as_nanos() as f64);
    }
    sync_samples.sort_by(|left, right| left.partial_cmp(right).expect("finite"));
    let sync = sync_samples[sync_samples.len() / 2];

    let measure = |objects: usize, round: usize| -> f64 {
        let mut batch = store.batch();
        for index in 0..objects {
            let mut value = vec![0u8; 16 * 1024];
            value[..8].copy_from_slice(&(round as u64).to_le_bytes());
            value[8..16].copy_from_slice(&(index as u64).to_le_bytes());
            batch.put(Role::Bytes, &value).expect("put");
        }
        probe::reset_timings();
        batch.commit().expect("commit");
        assert_eq!(
            probe::transaction_count(),
            1,
            "one batch is one transaction"
        );
        probe::max_transaction_nanos() as f64
    };

    let mut h10 = Vec::new();
    let mut h1000 = Vec::new();
    for round in 0..6 {
        let small = measure(10, round);
        let large = measure(1000, round);
        eprintln!(
            "round {round}: H(10) = {small:.0} ns, H(1000) = {large:.0} ns, S = {sync:.0} ns"
        );
        if round > 0 {
            h10.push(small);
            h1000.push(large);
        }
    }
    let median = |mut values: Vec<f64>| {
        values.sort_by(|left, right| left.partial_cmp(right).expect("finite"));
        values[values.len() / 2]
    };
    let h10 = median(h10);
    let h1000 = median(h1000);
    let marginal = (h1000 - h10) / 990.0;
    eprintln!(
        "lock-probe: S = {sync:.0} ns, H(10) = {h10:.0} ns, H(1000) = {h1000:.0} ns, \
         m = {marginal:.1} ns/object, 100*S = {:.0} ns",
        100.0 * sync
    );

    if h1000 >= h10 {
        assert!(
            marginal <= sync / 10.0,
            "marginal lock cost {marginal:.1} ns/object is not far below one fsync ({sync:.0} ns)"
        );
    }
    assert!(
        h1000 <= 100.0 * sync,
        "H(1000) = {h1000:.0} ns exceeds 100 fsyncs"
    );
}
