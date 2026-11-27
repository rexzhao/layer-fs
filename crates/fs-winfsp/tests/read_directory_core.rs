//! The `ReadDirectory` **body**, testable without a mount (361's second cut).
//!
//! The three failures this core has actually produced are the three cells that matter: a listing whose count is not written reads
//! as an empty directory; a resume marker that is not translated lists the same page twice; and a record that does not fit must
//! **stop** the page rather than be written half-way.

use std::ffi::c_void;

use fs_semantics::attributes::DIRECTORY;
use fs_winfsp::callbacks::{read_directory_from, read_directory_shell, ListEntry};
use fs_winfsp::ops::NodeInfo;
use fs_winfsp::Kind;

/// The shell's **miss** path: no mount means an empty page, never another mount's listing (352).
///
/// `read_directory_shell` also proves the liveness question is asked at all — a `None` adapter is the registry miss that the
/// `contains == false` assertion argues cannot happen at teardown, and the counter is what makes it visible if it does.
#[test]
fn the_shell_lists_nothing_when_there_is_no_mount() {
    let entry =
        fs_winfsp::callbacks::register_context_for_test(fs_winfsp::callbacks::EntryContext {
            file_id: 0,
            handle: fs_winfsp::handles::HandleId::from_raw(0),
        });
    let context = entry;
    let mut buffer = vec![0u8; 4096];
    let mut transferred = u32::MAX;

    let status = unsafe {
        read_directory_shell(
            None,
            context,
            buffer.as_mut_ptr().cast::<c_void>(),
            std::ptr::null(),
            4096,
            std::ptr::addr_of_mut!(transferred),
        )
    };

    assert_eq!(status, 0, "cell miss: an empty page, not an error status");
    assert_eq!(transferred, 0, "cell miss: and the count says so");
    assert!(
        buffer.iter().all(|byte| *byte == 0),
        "cell miss: nothing was written — **no fallback to whoever was configured last**"
    );
}

fn info(kind: Kind) -> NodeInfo {
    NodeInfo {
        file_id: 7,
        kind,
        size: 4,
        times: fs_winfsp::ops::Times::default(),
        attributes: fs_winfsp::ops::Attributes::default(),
    }
}

fn entries(names: &[&str]) -> Vec<ListEntry> {
    names
        .iter()
        .map(|name| (name.encode_utf16().collect::<Vec<u16>>(), info(Kind::File)))
        .collect()
}

/// Runs one page and reports `(status, bytes_transferred, names in the buffer)`.
///
/// Reading the names back out of the buffer is the point: asserting only "status 0" is what made the empty listing look healthy
/// for a day.
fn page(entries: &[ListEntry], capacity: usize, marker: Option<&[u16]>) -> (i32, u32, Vec<String>) {
    let mut buffer = vec![0u8; capacity];
    let mut transferred = u32::MAX;
    // WinFsp passes a **NUL-terminated** UTF-16 name; a slice without the terminator would be read past its end.
    let mut terminated = marker.map(|name| {
        let mut units = name.to_vec();
        units.push(0);
        units
    });
    let marker_ptr = terminated
        .as_mut()
        .map_or(std::ptr::null(), |units| units.as_ptr());
    let status = unsafe {
        read_directory_from(
            entries,
            buffer.as_mut_ptr().cast::<c_void>(),
            marker_ptr,
            u32::try_from(capacity).expect("capacity fits"),
            std::ptr::addr_of_mut!(transferred),
        )
    };
    // Walk the records the way WinFsp does: NextOffset byte at 80, NameLength at 64, Name at 104.
    let mut names = Vec::new();
    let mut at = 0usize;
    while at + 104 <= capacity {
        // `Size` (u16 at 0) is the **whole** record incl. name; the name is what is left after the 104-byte header. A zero
        // size means "no record here", which is how a page ends.
        let size = u16::from_ne_bytes([buffer[at], buffer[at + 1]]) as usize;
        if size == 0 {
            break;
        }
        let name_len = size - 104;
        let next =
            u32::from_ne_bytes(buffer[at + 80..at + 84].try_into().expect("4 bytes")) as usize;
        let name = String::from_utf16_lossy(
            &buffer[at + 104..at + 104 + name_len]
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                .collect::<Vec<u16>>(),
        );
        names.push(name);
        // Records are packed contiguously, 8-aligned — `NextOffset` is **not** what the framing uses (it is never written), so
        // the walker advances by the record's padded size. See the note in the module docs.
        at += fs_winfsp::abi::dir_info::aligned(size);
        let _ = next;
    }
    (status, transferred, names)
}

#[test]
fn a_listing_is_written_and_its_count_too() {
    let all = entries(&["alpha.txt", "beta"]);
    let (status, transferred, names) = page(&all, 4096, None);
    assert_eq!(status, 0, "cell ok: the page succeeds");
    assert!(
        transferred > 0,
        "cell count: **the count is the only evidence entries exist**"
    );
    assert_eq!(
        names,
        vec!["alpha.txt".to_string(), "beta".to_string()],
        "cell names: the entries, in the order handed in"
    );
}

#[test]
fn a_marker_resumes_after_that_name() {
    let all = entries(&["alpha.txt", "beta", "gamma"]);
    let marker: Vec<u16> = "alpha.txt".encode_utf16().collect();
    let (status, _, names) = page(&all, 4096, Some(&marker));
    assert_eq!(status, 0);
    assert_eq!(
        names,
        vec!["beta".to_string(), "gamma".to_string()],
        "cell resume: the marker names the **last entry of the previous page**, so the page starts after it"
    );
}

#[test]
fn a_deleted_marker_advances_past_it_instead_of_looping() {
    let all = entries(&["alpha.txt", "gamma"]);
    let deleted_mid: Vec<u16> = "beta".encode_utf16().collect();
    let (_, _, names) = page(&all, 4096, Some(&deleted_mid));
    assert_eq!(
        names,
        vec!["gamma".to_string()],
        "deleted marker in the middle advances to the first entry strictly after it"
    );

    let deleted_end: Vec<u16> = "not-here".encode_utf16().collect();
    let (_, transferred, names) = page(&all, 4096, Some(&deleted_end));
    assert_eq!(transferred, 0);
    assert!(
        names.is_empty(),
        "deleted marker past the last entry finishes enumeration instead of looping back to 0"
    );
}

#[test]
fn a_record_that_does_not_fit_stops_the_page_instead_of_half_writing() {
    // Room for exactly one record: the second must be left out entirely, and the count must describe what was written.
    let all = entries(&["alpha.txt", "beta"]);
    let (status, transferred, names) = page(&all, 160, None);
    assert_eq!(status, 0, "cell full: a full buffer is not an error");
    assert_eq!(
        names.len(),
        1,
        "cell full: **one whole record**, never one and a half"
    );
    assert_eq!(names[0], "alpha.txt");
    assert!(
        transferred > 0,
        "cell count: and the count matches the one record actually written"
    );
}

#[test]
fn an_empty_directory_is_a_success_with_nothing_transferred() {
    let (status, transferred, names) = page(&[], 4096, None);
    assert_eq!(status, 0, "cell empty: an empty directory is not a failure");
    assert_eq!(transferred, 0, "cell empty: nothing was transferred");
    assert!(names.is_empty(), "cell empty: and nothing was written");
}

#[test]
fn the_derived_attribute_bits_do_not_stop_a_listing() {
    // One directory among files: the info records differ, and a core that built the record itself instead of going through
    // `file_info` would show up here.
    let mut all = entries(&["beta"]);
    all.insert(0, ("sub".encode_utf16().collect(), info(Kind::Directory)));
    let (status, _, names) = page(&all, 4096, None);
    assert_eq!(status, 0);
    assert_eq!(names, vec!["sub".to_string(), "beta".to_string()]);
    assert_eq!(
        DIRECTORY, 0x10,
        "cell attributes: the bit the record carries for a directory is the one Win32 defines"
    );
}

#[test]
fn binary_search_pagination_handles_dots_and_pre_dot_names() {
    // Leading `.` and `..` followed by sorted real entries including names < '.' in UTF-16 (`!`, `#`, `-`).
    let mut expected: Vec<String> = vec![
        ".".to_owned(),
        "..".to_owned(),
        "!bang.txt".to_owned(),
        "#hash.txt".to_owned(),
        "-dash.txt".to_owned(),
        ".hidden".to_owned(),
    ];
    for i in 0..120 {
        expected.push(format!("file_{i:03}.txt"));
    }
    let all: Vec<ListEntry> = expected
        .iter()
        .map(|n| {
            let kind = if n == "." || n == ".." {
                Kind::Directory
            } else {
                Kind::File
            };
            (n.encode_utf16().collect(), info(kind))
        })
        .collect();

    let mut collected = Vec::new();
    let mut marker: Option<Vec<u16>> = None;
    let mut pages = 0usize;
    loop {
        let (status, transferred, names) = page(&all, 512, marker.as_deref());
        assert_eq!(status, 0);
        if transferred == 0 || names.is_empty() {
            break;
        }
        pages += 1;
        assert!(pages < 200, "pagination must terminate without looping");
        marker = Some(names.last().unwrap().encode_utf16().collect());
        collected.extend(names);
    }
    assert!(pages > 10, "expected multi-page traversal, got {pages}");
    assert_eq!(collected, expected);
}

#[test]
fn read_directory_shell_caches_snapshot_across_pages() {
    use fs_semantics::disposition::Disposition;
    use fs_winfsp::ops::{
        Attributes, DirEntry, DirOutcome, OpenOutcome, OpsResult, Path as OpsPath, RenameOutcome,
        WorkspaceOps,
    };
    use std::sync::Arc;

    struct SharedMock(Arc<fs_winfsp::MockBackend>);
    impl WorkspaceOps for SharedMock {
        fn create_or_open(&self, path: &OpsPath<'_>, d: Disposition) -> OpsResult<OpenOutcome> {
            self.0.create_or_open(path, d)
        }
        fn mkdir(&self, path: &OpsPath<'_>) -> OpsResult<u64> {
            self.0.mkdir(path)
        }
        fn open_dir(&self, path: &OpsPath<'_>) -> OpsResult<u64> {
            self.0.open_dir(path)
        }
        fn dir_binding(&self, path: &OpsPath<'_>) -> Option<DirOutcome> {
            self.0.dir_binding(path)
        }
        fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
            self.0.read(file_id, offset, len)
        }
        fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
            self.0.write(file_id, offset, bytes)
        }
        fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
            self.0.set_size(file_id, len)
        }
        fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
            self.0.read_dir(dir_id)
        }
        fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
            self.0.get_info(file_id)
        }
        fn set_times(
            &self,
            file_id: u64,
            a: Option<i64>,
            w: Option<i64>,
            c: Option<i64>,
            cr: Option<i64>,
        ) -> OpsResult<()> {
            self.0.set_times(file_id, a, w, c, cr)
        }
        fn set_attributes(&self, file_id: u64, attr: Attributes) -> OpsResult<()> {
            self.0.set_attributes(file_id, attr)
        }
        fn rename(&self, from: &OpsPath<'_>, to: &OpsPath<'_>, r: bool) -> OpsResult<RenameOutcome> {
            self.0.rename(from, to, r)
        }
        fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
            self.0.remove_at(parent, name, expected_id)
        }
        fn flush(&self) -> OpsResult<()> {
            self.0.flush()
        }
    }

    let mock = Arc::new(fs_winfsp::MockBackend::new());
    let sub_id = mock.mkdir(&OpsPath::new(b"/sub")).unwrap();
    for i in 0..40 {
        let p = format!("/sub/item_{i:02}.txt");
        mock.create_or_open(&OpsPath::new(p.as_bytes()), Disposition::Create)
            .unwrap();
    }

    let dyn_ops: fs_winfsp::ops::DynOps = Box::new(SharedMock(mock.clone()));
    let adapter = fs_winfsp::DynAdapter::new(dyn_ops);
    let context = fs_winfsp::callbacks::register_context_for_test(
        fs_winfsp::callbacks::EntryContext {
            file_id: sub_id,
            handle: fs_winfsp::handles::HandleId::from_raw(99),
        },
    );

    let read_shell_page = |marker: Option<&[u16]>| -> (u32, Vec<String>) {
        let capacity = 512usize;
        let mut buffer = vec![0u8; capacity];
        let mut transferred = u32::MAX;
        let mut terminated = marker.map(|m| {
            let mut u = m.to_vec();
            u.push(0);
            u
        });
        let marker_ptr = terminated
            .as_mut()
            .map_or(std::ptr::null(), |u| u.as_ptr());
        let status = unsafe {
            read_directory_shell(
                Some(&adapter),
                context,
                buffer.as_mut_ptr().cast::<c_void>(),
                marker_ptr,
                capacity as u32,
                std::ptr::addr_of_mut!(transferred),
            )
        };
        assert_eq!(status, 0);
        let mut names = Vec::new();
        let mut at = 0usize;
        while at + 104 <= capacity {
            let size = u16::from_ne_bytes([buffer[at], buffer[at + 1]]) as usize;
            if size == 0 {
                break;
            }
            let name_len = size - 104;
            let name = String::from_utf16_lossy(
                &buffer[at + 104..at + 104 + name_len]
                    .chunks_exact(2)
                    .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                    .collect::<Vec<u16>>(),
            );
            names.push(name);
            at += fs_winfsp::abi::dir_info::aligned(size);
        }
        (transferred, names)
    };

    let mut collected = Vec::new();
    let mut marker: Option<Vec<u16>> = None;
    let mut pages = 0usize;
    loop {
        let (transferred, names) = read_shell_page(marker.as_deref());
        if transferred == 0 || names.is_empty() {
            break;
        }
        pages += 1;
        marker = Some(names.last().unwrap().encode_utf16().collect());
        collected.extend(names);
    }

    assert!(pages > 5, "expected multiple pages, got {pages}");
    assert_eq!(collected.len(), 42, "`.` + `..` + 40 files");
    assert_eq!(collected[0], ".");
    assert_eq!(collected[1], "..");

    let read_dir_calls = mock
        .calls()
        .into_iter()
        .filter(|c| matches!(c, fs_winfsp::Call::ReadDir(id) if *id == sub_id))
        .count();
    assert_eq!(
        read_dir_calls, 1,
        "paginated ReadDirectory must call backend.read_dir exactly once per enumeration, not once per page"
    );

    // Restart scan (`marker = None`) refreshes the snapshot cleanly.
    let (transferred, _) = read_shell_page(None);
    assert!(transferred > 0);
    let read_dir_calls_after_restart = mock
        .calls()
        .into_iter()
        .filter(|c| matches!(c, fs_winfsp::Call::ReadDir(id) if *id == sub_id))
        .count();
    assert_eq!(read_dir_calls_after_restart, 2);

    fs_winfsp::callbacks::release_context_for_test(context);
}
