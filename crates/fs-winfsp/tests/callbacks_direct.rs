//! The two step-④ callbacks, called **directly** — no WinFsp, no mount.
//!
//! This is the "pure Rust criterion first" step: the bodies are exactly what WinFsp will call, so a wrong structure or a
//! missing size check shows up here rather than as "the drive shows 0 bytes".

use std::ffi::c_void;
use std::mem::size_of;

use fs_winfsp::abi::{FileInfo, VolumeInfo};
use fs_winfsp::callbacks::{configure, get_file_info, get_volume_info, EntryContext};
use fs_winfsp::{Attributes, Kind, NodeInfo};

fn node(file_id: u64) -> NodeInfo {
    NodeInfo {
        file_id,
        kind: Kind::File,
        size: 1234,
        times: fs_winfsp::file_info::times(
            1_793_000_000_000_000_000,
            1_793_000_001_000_000_000,
            1_793_000_002_000_000_000,
            1_793_000_003_000_000_000,
        ),
        attributes: Attributes(0x80),
    }
}

fn lookup(file_id: u64) -> Option<NodeInfo> {
    (file_id == 7).then(|| node(file_id))
}

/// The two callbacks, in **one** test: the configuration is process-wide (`OnceLock`), so two tests would race for it.
///
/// That is not a testing inconvenience, it is the production shape — one mount per process — and writing it as one test keeps
/// the test honest about it instead of hiding it behind serial execution flags.
#[test]
fn the_two_callbacks_answer_from_configuration_and_from_the_entry_context() {
    let volume = VolumeInfo {
        total_size: 4096,
        free_size: 1024,
        volume_label_length: 8,
        volume_label: {
            let mut label = [0u16; 32];
            label[..4].copy_from_slice(&[b'w' as u16, b'c' as u16, b'a' as u16, b's' as u16]);
            label
        },
    };
    assert!(
        configure(&volume, lookup),
        "cell configure: the first configuration in this process succeeds"
    );

    // Volume: the configured record is copied verbatim; a short buffer is refused rather than half-filled.
    let mut out = VolumeInfo::default();
    let status = unsafe {
        get_volume_info(
            (&mut out as *mut VolumeInfo).cast::<c_void>(),
            size_of::<VolumeInfo>() as u32,
        )
    };
    assert_eq!(status, 0, "cell volume status: STATUS_SUCCESS");
    assert_eq!(out, volume, "cell volume record: copied verbatim");

    let mut small = [0u8; 8];
    let status =
        unsafe { get_volume_info(small.as_mut_ptr().cast::<c_void>(), small.len() as u32) };
    assert_ne!(status, 0, "cell short buffer: refused, not truncated");

    // File info: answered from the `FileContext`, not from a path or a name lookup.
    let context = EntryContext { file_id: 7 };
    let mut out = FileInfo::zeroed();
    let status = unsafe {
        get_file_info(
            (&context as *const EntryContext).cast::<c_void>(),
            (&mut out as *mut FileInfo).cast::<c_void>(),
        )
    };
    assert_eq!(status, 0, "cell file status: the entry is known");
    assert_eq!(
        out.file_size, 1234,
        "cell size: from the entry, not from a path"
    );
    assert_eq!(
        out.index_number, 7,
        "cell identity: the FileId is the FileIndex"
    );

    // An unknown id is an error, and the buffer is left untouched — a zeroed record would show as an empty file.
    let unknown = EntryContext { file_id: 99 };
    let mut out = FileInfo::zeroed();
    let status = unsafe {
        get_file_info(
            (&unknown as *const EntryContext).cast::<c_void>(),
            (&mut out as *mut FileInfo).cast::<c_void>(),
        )
    };
    assert_ne!(status, 0, "cell unknown: refused");
    assert_eq!(out, FileInfo::zeroed(), "cell unknown: buffer untouched");

    // A null context or a null output is refused before anything is dereferenced.
    assert_ne!(
        unsafe {
            get_file_info(
                std::ptr::null(),
                (&mut out as *mut FileInfo).cast::<c_void>(),
            )
        },
        0,
        "cell null context: refused"
    );
    assert_ne!(
        unsafe { get_volume_info(std::ptr::null_mut(), 88) },
        0,
        "cell null out: refused"
    );
}
