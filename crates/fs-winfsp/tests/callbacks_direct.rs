//! The step-④ callbacks, called **directly** — no WinFsp, no mount, no process-wide configuration.
//!
//! This used to be **one** test, because the configuration lived in process-wide statics and two tests would have raced for it
//! (that is process 302's original form). Now that the answers come from an **`Adapter` handed to the function**, each case gets
//! its own test and its own adapter — the criterion is the same, but the tests no longer share state, so a failure names itself.

use std::ffi::c_void;

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::{FileInfo, VolumeInfo};
use fs_winfsp::callbacks::NOT_IMPLEMENTED;
use fs_winfsp::callbacks::{
    get_file_info_shell, get_volume_info_shell, EntryContext, OURS_INVALID_PARAMETER,
};
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// The metadata the fixture knows: one file, id 7, 1234 bytes.
fn node() -> NodeInfo {
    NodeInfo {
        file_id: 7,
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

/// A one-entry fixture: the answers are the same ones the old closures gave, now behind the trait the mount uses.
struct OneFile;

impl WorkspaceOps for OneFile {
    fn create_or_open(&self, path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome> {
        if disposition != Disposition::Open {
            return Err(OpsError::ReadOnly);
        }
        if path != b"/a.txt" {
            return Err(OpsError::NotFound);
        }
        let info = node();
        Ok(OpenOutcome {
            file_id: info.file_id,
            kind: info.kind,
            action: fs_semantics::disposition::Action::Open,
            size: info.size,
            times: info.times,
            attributes: info.attributes,
            binding: (info.file_id, Vec::new()),
            replaced: None,
        })
    }

    fn mkdir(&self, _path: &[u8]) -> OpsResult<u64> {
        Err(OpsError::ReadOnly)
    }

    fn read(&self, _file_id: u64, _offset: u64, _len: usize) -> OpsResult<Vec<u8>> {
        Err(OpsError::ReadOnly)
    }

    fn write(&self, _file_id: u64, _offset: u64, _bytes: &[u8]) -> OpsResult<usize> {
        Err(OpsError::ReadOnly)
    }

    fn set_size(&self, _file_id: u64, _len: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        if dir_id != 0 {
            return Ok(Vec::new());
        }
        Ok(vec![DirEntry {
            name: b"a.txt".to_vec(),
            kind: Kind::File,
            file_id: 7,
        }])
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        if file_id == 7 {
            Ok(node())
        } else {
            Err(OpsError::NotFound)
        }
    }

    fn set_times(
        &self,
        _file_id: u64,
        _accessed: Option<i64>,
        _written: Option<i64>,
        _changed: Option<i64>,
        _created: Option<i64>,
    ) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }

    fn set_attributes(&self, _file_id: u64, _attributes: Attributes) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }

    fn rename(&self, _from: &[u8], _to: &[u8], _replace: bool) -> OpsResult<RenameOutcome> {
        Err(OpsError::ReadOnly)
    }

    fn remove_at(&self, _parent: u64, _name: &[u8], _expected_id: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }

    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

const VOLUME: VolumeInfo = VolumeInfo {
    total_size: 4096,
    free_size: 1024,
    volume_label_length: 8,
    volume_label: [
        b'w' as u16,
        b'c' as u16,
        b'a' as u16,
        b's' as u16,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ],
};

fn adapter() -> DynAdapter {
    DynAdapter::with_config(
        Box::new(OneFile) as DynOps,
        fs_winfsp::adapter::AdapterConfig {
            volume: Some(VOLUME),
            ..fs_winfsp::adapter::AdapterConfig::default()
        },
    )
}

#[test]
fn the_volume_record_is_copied_verbatim() {
    let adapter = adapter();
    let mut out = VolumeInfo::default();
    let status = unsafe {
        get_volume_info_shell(
            Some(&adapter),
            (&mut out as *mut VolumeInfo).cast::<c_void>(),
        )
    };
    assert_eq!(status, 0, "cell status: STATUS_SUCCESS");
    assert_eq!(out, VOLUME, "cell record: copied verbatim");
}

#[test]
fn the_volume_answer_is_the_whole_structure_because_there_is_no_length() {
    // `GetVolumeInfo` takes **two** arguments (`winfsp.h:206-207`); the dispatcher hands over an 88-byte stack local it has just zeroed
    // and copies all 88 bytes back (`fsop.c:1245-1260`). The old "short buffer" case tested a phantom: the length we were reading was
    // stack residue, and every answer derived from it was answering a constraint that did not exist.
    const SENTINEL: u8 = 0xAA;
    let adapter = adapter();
    let mut raw = [SENTINEL; 96];
    let status =
        unsafe { get_volume_info_shell(Some(&adapter), raw.as_mut_ptr().cast::<c_void>()) };
    assert_eq!(status, 0, "cell whole: answered");
    let written = VolumeInfo {
        total_size: u64::from_ne_bytes(raw[..8].try_into().expect("eight bytes")),
        free_size: u64::from_ne_bytes(raw[8..16].try_into().expect("eight bytes")),
        volume_label_length: u16::from_ne_bytes(raw[16..18].try_into().expect("two bytes")),
        volume_label: [0; 32],
    };
    assert_eq!(
        (
            written.total_size,
            written.free_size,
            written.volume_label_length
        ),
        (
            VOLUME.total_size,
            VOLUME.free_size,
            VOLUME.volume_label_length
        ),
        "cell whole: all of it, not a prefix chosen by a number we made up"
    );
    assert_eq!(
        raw[88], SENTINEL,
        "cell whole: and exactly 88 bytes — the byte after the structure is untouched"
    );
}

#[test]
fn a_null_buffer_is_refused_for_the_volume() {
    let adapter = adapter();
    let status = unsafe { get_volume_info_shell(Some(&adapter), std::ptr::null_mut()) };
    assert_eq!(status, OURS_INVALID_PARAMETER, "cell null out: refused");
}

#[test]
fn the_file_record_is_answered_from_the_entry_context() {
    let adapter = adapter();
    let context = EntryContext {
        file_id: 7,
        handle: fs_winfsp::handles::HandleId::from_raw(1),
    };
    let mut out = FileInfo::zeroed();
    let status = unsafe {
        get_file_info_shell(
            Some(&adapter),
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
}

#[test]
fn an_unknown_id_is_refused_and_leaves_the_buffer_untouched() {
    let adapter = adapter();
    let unknown = EntryContext {
        file_id: 99,
        handle: fs_winfsp::handles::HandleId::from_raw(2),
    };
    let mut out = FileInfo::zeroed();
    let status = unsafe {
        get_file_info_shell(
            Some(&adapter),
            (&unknown as *const EntryContext).cast::<c_void>(),
            (&mut out as *mut FileInfo).cast::<c_void>(),
        )
    };
    assert_eq!(status, NOT_IMPLEMENTED, "cell unknown: refused");
    assert_eq!(out, FileInfo::zeroed(), "cell unknown: buffer untouched");
}

#[test]
fn null_pointers_are_refused_before_anything_is_dereferenced() {
    let adapter = adapter();
    let mut out = FileInfo::zeroed();
    assert_eq!(
        unsafe {
            get_file_info_shell(
                Some(&adapter),
                std::ptr::null(),
                (&mut out as *mut FileInfo).cast::<c_void>(),
            )
        },
        NOT_IMPLEMENTED,
        "cell null context: refused"
    );
    // No adapter at all (a registry miss) is also a refusal, and it must not borrow anybody else's answers.
    assert_eq!(
        unsafe {
            get_volume_info_shell(
                None,
                (&mut VolumeInfo::default() as *mut VolumeInfo).cast::<c_void>(),
            )
        },
        NOT_IMPLEMENTED,
        "cell no mount: refused"
    );
}
