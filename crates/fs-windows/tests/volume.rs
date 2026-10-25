//! `volume` tests (P3-1 §1): the numbers come from **one** source, and that source is documented.

use fs_windows::volume::{info, FREE_BYTES, TOTAL_BYTES};

#[test]
fn the_reported_numbers_are_the_constants() {
    let volume = info();
    assert_eq!(volume.total_bytes, TOTAL_BYTES, "cell total");
    assert_eq!(volume.free_bytes, FREE_BYTES, "cell free");
    assert_eq!(volume.sectors_per_unit, 1, "cell sectors_per_unit");
    assert_eq!(volume.bytes_per_sector, 4096, "cell bytes_per_sector");
}

/// A single source: the function is pure, so two calls cannot disagree, and nothing is read from the host disk.
#[test]
fn the_numbers_are_stable_and_not_measured() {
    assert_eq!(
        info(),
        info(),
        "cell call=2: the same numbers, not a measurement"
    );

    // Writing a large file must not change what the volume reports: if it did, this layer would be reading
    // the host's free space, which is exactly the mistake the module documents.
    let before = info();
    let scratch = std::env::temp_dir().join("fs-windows-volume-probe.bin");
    std::fs::write(&scratch, vec![0u8; 1 << 20]).expect("write a scratch file");
    let after = info();
    let _ = std::fs::remove_file(&scratch);
    assert_eq!(
        before, after,
        "cell host_disk=changed: the report must not follow the host"
    );
}

/// Windows requires free ≤ total; keeping them equal is a documented choice, not an accident.
#[test]
fn free_never_exceeds_total_and_the_equality_is_documented() {
    let volume = info();
    assert!(volume.free_bytes <= volume.total_bytes, "cell free>total");
    assert_eq!(
        volume.free_bytes, volume.total_bytes,
        "cell free==total: a workspace is a shared view, so 'free' is a policy value equal to the total"
    );
    assert_eq!(TOTAL_BYTES, 1 << 40, "cell total: 1 TiB, stated once");
}
