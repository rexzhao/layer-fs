//! Criterion ④ for `open`/`create` going through the mount's backend (P3-4 batch (i), (b)-2a).
//!
//! The behaviour changed shape: it used to be "ask the resolver, then ask the lookup", and it is now "ask the backend for
//! `Disposition::Open`, then read current metadata". A shape change needs its own assertions — a still-green suite is not evidence
//! that the new path preserves the promises the old one made:
//!
//! 1. an **existing** name still answers the same id;
//! 2. a **missing** name is `NotFound` — not `InvalidRecord`, and certainly not a creation;
//! 3. the store is **unchanged** by both (no new entry, no size change) — this is the cell that pins down "Open does not create".

use std::ffi::c_void;

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{open_shell, OURS_OBJECT_NAME_NOT_FOUND};
use fs_winfsp::mock::MockBackend;
use fs_winfsp::ops::{DynOps, WorkspaceOps};
use fs_winfsp::DynAdapter;

/// The UTF-16, NUL-terminated form WinFsp passes.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Runs one `open` and reports `(status, size in the returned record)`.
fn open(adapter: &DynAdapter, path: &str) -> (i32, u64) {
    let name = wide(path);
    let mut record = FileInfo {
        file_attributes: 0,
        ..FileInfo::default()
    };
    let mut context: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        open_shell(
            Some(adapter),
            name.as_ptr(),
            std::ptr::addr_of_mut!(context),
            std::ptr::addr_of_mut!(record),
        )
    };
    if !context.is_null() {
        // Release it the way `close` would: the shell allocated it, so a test that opened must free.
        fs_winfsp::callbacks::release_context_for_test(context);
    }
    (status, record.file_size)
}

fn store_shape(adapter: &DynAdapter) -> (usize, Vec<(Vec<u8>, u64)>) {
    let listing = adapter.backend().read_dir(0).expect("list root");
    let mut shape: Vec<(Vec<u8>, u64)> = listing
        .iter()
        .map(|entry| {
            let size = adapter
                .backend()
                .get_info(entry.file_id)
                .map(|info| info.size)
                .unwrap_or(u64::MAX);
            (entry.name.clone(), size)
        })
        .collect();
    shape.sort();
    (listing.len(), shape)
}

fn adapter_with_file() -> DynAdapter {
    let adapter: DynAdapter = DynAdapter::new(Box::new(MockBackend::new()) as DynOps);
    adapter
        .backend()
        .create_or_open(b"/alpha.txt", Disposition::Create)
        .expect("create the file through the same backend the shell will use");
    adapter
        .backend()
        .write(1, 0, b"hello")
        .expect("write 5 bytes");
    adapter
}

#[test]
fn an_existing_name_answers_the_same_entry() {
    let adapter = adapter_with_file();
    let before = store_shape(&adapter);
    let (status, size) = open(&adapter, "/alpha.txt");

    assert_eq!(status, 0, "cell existing: the open succeeds");
    assert_eq!(
        size, 5,
        "cell existing: the record describes the current size, not a remembered one"
    );
    assert_eq!(
        store_shape(&adapter),
        before,
        "cell existing: and the store is untouched"
    );
}

#[test]
fn a_missing_name_is_not_found_and_is_not_created() {
    let adapter = adapter_with_file();
    let before = store_shape(&adapter);
    let (status, _) = open(&adapter, "/nope.txt");

    assert_eq!(
        status, OURS_OBJECT_NAME_NOT_FOUND,
        "cell missing: `Open` means \"must exist\", so a missing name is OBJECT_NAME_NOT_FOUND"
    );
    assert_eq!(
        store_shape(&adapter),
        before,
        "cell missing: **Open does not create** — the listing and every size are unchanged"
    );
}

#[test]
fn no_mount_is_refused_rather_than_answered() {
    let name = wide("/alpha.txt");
    let mut record = FileInfo::default();
    let mut context: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        open_shell(
            None,
            name.as_ptr(),
            std::ptr::addr_of_mut!(context),
            std::ptr::addr_of_mut!(record),
        )
    };
    assert_ne!(status, 0, "cell miss: no mount is a refusal (352)");
    assert!(
        context.is_null(),
        "cell miss: and no FileContext is handed out"
    );
}
