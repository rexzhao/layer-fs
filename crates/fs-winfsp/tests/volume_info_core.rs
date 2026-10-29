//! The `GetVolumeInfo` **body**, testable without a mount (361: the pure core goes in a testable function, the `unsafe` shell
//! keeps only the process-wide lookup).
//!
//! Four cells, and each one is a different answer — including the one that used to be a bug: a buffer that is too small is
//! "too small", never "unsupported" (WinFsp retries the former and treats the latter as terminal).

use std::ffi::c_void;

use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::callbacks::{
    get_volume_info_for, NOT_IMPLEMENTED, OURS_BUFFER_TOO_SMALL, OURS_INVALID_PARAMETER,
};

fn volume(total: u64) -> VolumeInfo {
    VolumeInfo {
        total_size: total,
        free_size: total / 2,
        volume_label_length: 0,
        volume_label: [0; 32],
    }
}

#[test]
fn a_volume_is_written_into_a_large_enough_buffer() {
    let mut out = VolumeInfo {
        total_size: 0,
        free_size: 0,
        volume_label_length: 0,
        volume_label: [0; 32],
    };
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("capacity fits u32");
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            None,
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell ok: a full-size buffer succeeds");
    assert_eq!(
        out.total_size,
        1 << 20,
        "cell ok: and the volume is the one that was handed in"
    );
    assert_eq!(
        out.free_size,
        1 << 19,
        "cell ok: every field travelled, not just the first"
    );
}

#[test]
fn a_short_buffer_gets_the_prefix_that_fits() {
    // One byte short of the whole structure: the answer is the prefix, and the byte **past** the buffer is never touched (the sentinel
    // stays 0xAA). This replaces an earlier expectation that said "too small is refused" — true of the old single-phase body, and the
    // reason the volume showed no size: the dispatcher asks with eight bytes, not with 88.
    const SENTINEL: u8 = 0xAA;
    let mut raw = [SENTINEL; 96];
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits") - 1;
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            None,
            raw.as_mut_ptr().cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell short: answered, not refused");
    let prefix = u64::from_ne_bytes(raw[..8].try_into().expect("eight bytes"));
    assert_eq!(
        prefix,
        1 << 20,
        "cell short: the prefix carries `TotalSize`"
    );
    assert_eq!(
        raw[capacity as usize], SENTINEL,
        "cell short: the first byte **past** the buffer is untouched — a prefix is not an overrun"
    );
}

#[test]
fn no_configured_volume_is_refused_and_that_is_a_different_answer_from_a_short_buffer() {
    let mut out = volume(0);
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits");
    let status = unsafe {
        get_volume_info_for(
            None,
            None,
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, NOT_IMPLEMENTED, "cell absent: no volume configured");
    assert_ne!(
        status, OURS_BUFFER_TOO_SMALL,
        "cell absent: **a missing volume and a small buffer must not share an answer**"
    );
    assert_eq!(out.total_size, 0, "cell absent: nothing was written");
}

#[test]
fn a_null_buffer_is_refused_before_anything_else() {
    let status =
        unsafe { get_volume_info_for(Some(volume(1 << 20)), None, std::ptr::null_mut(), 4096) };
    assert_eq!(
        status, OURS_INVALID_PARAMETER,
        "cell null: the pointer is checked first"
    );
}

/// The **shell** asks the mount, not the process (352): an adapter with a volume answers it, and `None` — the registry miss —
/// answers "no volume" instead of borrowing somebody else's configuration.
#[test]
fn the_shell_reads_the_volume_from_the_mount_it_was_given() {
    use fs_winfsp::adapter::AdapterConfig;
    use fs_winfsp::callbacks::get_volume_info_shell;
    use fs_winfsp::mock::MockBackend;
    use fs_winfsp::ops::DynOps;
    use fs_winfsp::DynAdapter;

    let configured: DynAdapter = DynAdapter::with_config(
        Box::new(MockBackend::new()) as DynOps,
        AdapterConfig {
            volume: Some(volume(1 << 40)),
            ..AdapterConfig::default()
        },
    );
    let bare: DynAdapter = DynAdapter::new(Box::new(MockBackend::new()) as DynOps);

    let read = |adapter: Option<&DynAdapter>| {
        let mut out = volume(0);
        let status =
            unsafe { get_volume_info_shell(adapter, std::ptr::addr_of_mut!(out).cast::<c_void>()) };
        (status, out.total_size)
    };

    assert_eq!(
        read(Some(&configured)),
        (0, 1 << 40),
        "cell configured: the mount's own volume comes back"
    );
    assert_eq!(
        read(Some(&bare)),
        (NOT_IMPLEMENTED, 0),
        "cell bare: a mount without a volume answer refuses — it does not borrow"
    );
    assert_eq!(
        read(None),
        (NOT_IMPLEMENTED, 0),
        "cell miss: **no adapter is not a reason to invent a volume**"
    );
}

/// **C1** — the capacity is the **host volume's**, asked on every call, not a number captured at mount time.
///
/// The placeholder this replaces was `1 GiB / 512 MiB`; asserting "not that" would be a weak criterion, so the source is made to answer with an unmistakable pair and
/// the test also changes it between two calls: a captured snapshot cannot follow.
#[test]
fn the_capacity_is_asked_for_on_every_call() {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TOTAL: AtomicU64 = AtomicU64::new(7 * 1024 * 1024 * 1024);
    static FREE: AtomicU64 = AtomicU64::new(3 * 1024 * 1024 * 1024);

    fn source() -> Option<(u64, u64)> {
        Some((TOTAL.load(Ordering::SeqCst), FREE.load(Ordering::SeqCst)))
    }

    let mut out: VolumeInfo = unsafe { std::mem::zeroed() };
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits");
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            Some(&source),
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell C1: the call succeeds");
    assert_eq!(
        out.total_size,
        7 * 1024 * 1024 * 1024,
        "cell C1: total comes from the source"
    );
    assert_eq!(
        out.free_size,
        3 * 1024 * 1024 * 1024,
        "cell C1: free comes from the source"
    );

    // A second call with different numbers: a snapshot taken at mount time could not follow.
    TOTAL.store(9 * 1024 * 1024 * 1024, Ordering::SeqCst);
    FREE.store(1, Ordering::SeqCst);
    let mut again: VolumeInfo = unsafe { std::mem::zeroed() };
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            Some(&source),
            std::ptr::addr_of_mut!(again).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell C1: the second call succeeds");
    println!(
        "C1 reading: second call total={} free={}",
        again.total_size, again.free_size
    );
    assert_eq!(
        again.total_size,
        9 * 1024 * 1024 * 1024,
        "cell C1: the numbers moved with the source"
    );
    assert_eq!(
        again.free_size, 1,
        "cell C1: free space is read again, not remembered"
    );
}

/// **C1b** — a source that cannot answer leaves the configured numbers alone instead of inventing zeros.
#[test]
fn a_silent_capacity_source_keeps_the_configured_numbers() {
    fn silent() -> Option<(u64, u64)> {
        None
    }
    let mut out: VolumeInfo = unsafe { std::mem::zeroed() };
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits");
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            Some(&silent),
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell C1b: the call still succeeds");
    assert_eq!(
        out.total_size,
        1 << 20,
        "cell C1b: the configured total survives"
    );
}
