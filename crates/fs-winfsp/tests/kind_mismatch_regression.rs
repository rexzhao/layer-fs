//! Regression for the "file replaced a directory" accident (`72db813`).
//!
//! The real trace showed three creates: file, **directory**, then a *file-style* open of the same directory name
//! (`CreateOptions = 0x60`, no `FILE_DIRECTORY_FILE`). That third call went through the file door and the backend created an empty **file** under
//! the directory's name — after which every child was "not found". The fix checks the kind the caller asked for against the kind it got.
//!
//! The criterion is deliberately **negative**: the file-style open must be refused **and must create nothing**. A test that only asserted the
//! status would pass for an implementation that still clobbered the entry and then reported the error afterwards.

use std::ffi::c_void;

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    create_shell, split_create_options, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
    OURS_IS_A_DIRECTORY,
};
use fs_winfsp::mock::MockBackend;
use fs_winfsp::ops::Path;
use fs_winfsp::ops::{DynOps, WorkspaceOps};
use fs_winfsp::DynAdapter;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Packs a disposition and options the way the FSD does (`fsctl.h:351`).
fn pack(disposition: u32, options: u32) -> u32 {
    ((disposition & 0xff) << 24) | (options & 0x00FF_FFFF)
}

fn open_with(adapter: &DynAdapter, path: &str, options: u32) -> i32 {
    let name = wide(path);
    let mut record = FileInfo::zeroed();
    let mut context: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        create_shell(
            Some(adapter),
            name.as_ptr(),
            options,
            std::ptr::addr_of_mut!(context),
            std::ptr::addr_of_mut!(record),
        )
    };
    if !context.is_null() {
        fs_winfsp::callbacks::release_context_for_test(context);
    }
    status
}

#[test]
fn a_file_style_open_of_a_directory_is_refused_and_creates_nothing() {
    let adapter: DynAdapter = DynAdapter::new(Box::new(MockBackend::new()) as DynOps);
    // The directory the accident clobbered.
    let dir = adapter.backend().mkdir(&Path::new(b"/d")).expect("mkdir");
    let before: Vec<Vec<u8>> = adapter
        .backend()
        .read_dir(0)
        .expect("list root")
        .into_iter()
        .map(|entry| entry.name)
        .collect();

    // `FILE_OPEN` + `FILE_NON_DIRECTORY_FILE` — the third create in the real trace.
    let status = open_with(&adapter, "/d", pack(1, FILE_NON_DIRECTORY_FILE));

    assert_eq!(
        status, OURS_IS_A_DIRECTORY,
        "cell status: a file-style open of a directory is `FILE_IS_A_DIRECTORY` (`ntstatus.h:3773`)"
    );
    let after: Vec<Vec<u8>> = adapter
        .backend()
        .read_dir(0)
        .expect("list root")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        after, before,
        "cell store: **nothing was created** — a refused open must not clobber the entry it refused (382)"
    );
    let info = adapter
        .backend()
        .get_info(dir)
        .expect("the directory survives");
    assert_eq!(
        info.kind,
        fs_winfsp::Kind::Directory,
        "cell kind: the name is still a directory, not an empty file of the same name"
    );
}

#[test]
fn a_directory_style_open_of_a_file_is_refused() {
    let adapter: DynAdapter = DynAdapter::new(Box::new(MockBackend::new()) as DynOps);
    adapter
        .backend()
        .create_or_open(&Path::new(b"/f.txt"), Disposition::Create)
        .expect("create the file");

    let status = open_with(&adapter, "/f.txt", pack(1, FILE_DIRECTORY_FILE));

    assert_eq!(
        status,
        fs_winfsp::callbacks::OURS_NOT_A_DIRECTORY,
        "cell status: the other direction has its own code (`STATUS_NOT_A_DIRECTORY`, `ntstatus.h:4444`)"
    );
}

#[test]
fn the_two_kind_codes_are_the_ones_the_sdk_defines() {
    // Read from `ntstatus.h`, not from memory: the first version of `IS_A_DIRECTORY` was `0xC0000102`, which is
    // `STATUS_FILE_CORRUPT_ERROR` — a plausible-looking number that would have shown up in L2 as a difference from NTFS.
    assert_eq!(
        fs_winfsp::callbacks::OURS_IS_A_DIRECTORY as u32,
        0xC000_00BA,
        "cell is-a-directory: `ntstatus.h:3773`"
    );
    assert_eq!(
        fs_winfsp::callbacks::OURS_NOT_A_DIRECTORY as u32,
        0xC000_0103,
        "cell not-a-directory: `ntstatus.h:4444`"
    );
    assert_ne!(
        fs_winfsp::callbacks::OURS_IS_A_DIRECTORY as u32,
        0xC000_0102,
        "cell not-corrupt: `0xC0000102` is `STATUS_FILE_CORRUPT_ERROR` (`ntstatus.h:4435`), not a kind mismatch"
    );
    // And the split still reads both halves.
    let parts = split_create_options(pack(2, FILE_DIRECTORY_FILE));
    assert_eq!(
        parts.options & FILE_DIRECTORY_FILE,
        FILE_DIRECTORY_FILE,
        "cell split: the directory option survives the round trip"
    );
}
