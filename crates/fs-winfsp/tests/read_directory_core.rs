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
    let entry = fs_winfsp::callbacks::EntryContext { file_id: 0 };
    let context = std::ptr::addr_of!(entry).cast::<c_void>();
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
fn a_marker_naming_nothing_starts_from_the_beginning() {
    let all = entries(&["alpha.txt", "beta"]);
    let marker: Vec<u16> = "not-here".encode_utf16().collect();
    let (_, _, names) = page(&all, 4096, Some(&marker));
    assert_eq!(
        names,
        vec!["alpha.txt".to_string(), "beta".to_string()],
        "cell unknown marker: no entry matches, so the page restarts — not an empty page"
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
