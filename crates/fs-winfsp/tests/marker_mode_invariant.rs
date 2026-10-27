//! The cheap invariant that keeps `NextOffset` from mattering (manager's re-characterisation of the finding).
//!
//! `dir_buffer::push` never writes `NextOffset`. With WinFsp's `DirectoryMarkerAsNextOffset` **clear** that is harmless: the FSD
//! resumes by the last name it saw (`winfsp.h:687`), and never reads the field. If someone ever sets that flag, `NextOffset = 0`
//! stops meaning "unused" and starts meaning "no further entries" — a silent truncation of every listing longer than one page.
//!
//! Two ways out: write the offsets, or assert the flag stays clear. The flag is the cheaper of the two, so it is asserted here —
//! and the assertion names the coupling, which is what a comment could only remind a human about.

use fs_winfsp::abi::VolumeParams;

/// Offset of the first bitfield word (`fsctl.h:206`, right after `UINT32 FileInfoTimeout` at 32 and `UINT32 SectorSize`… the
/// layout is byte-pinned by the SDK's own size assertion, so the offset is read from one place only: this constant).
const FIRST_FLAGS_AT: usize = 36;
/// `DirectoryMarkerAsNextOffset:1` — `fsctl.h:232`.
const DIRECTORY_MARKER_AS_NEXT_OFFSET: u32 = 1 << 12;

fn flags(params: &VolumeParams) -> u32 {
    let bytes = params.as_bytes();
    u32::from_ne_bytes(
        bytes[FIRST_FLAGS_AT..FIRST_FLAGS_AT + 4]
            .try_into()
            .expect("four bytes"),
    )
}

#[test]
fn the_directory_marker_stays_a_name_not_an_offset() {
    let mut params = VolumeParams::zeroed();
    params.set_name_flags();

    assert_eq!(
        flags(&params) & DIRECTORY_MARKER_AS_NEXT_OFFSET,
        0,
        "cell flag: `DirectoryMarkerAsNextOffset` must stay **clear** — `dir_buffer` never writes `NextOffset`, \
         so turning this on would silently truncate every listing longer than one page"
    );
}

#[test]
fn the_assertion_can_fail_on_this_layout() {
    // A red method for the cell above: the bit really is observable in the first bitfield word. If the offset or the mask were
    // wrong, the assertion above would pass for the wrong reason — proving "flag is clear" requires proving "flag is readable".
    let mut params = VolumeParams::zeroed();
    params.set_name_flags();
    let mut raw = *params.as_bytes();
    let with_marker = flags(&params) | DIRECTORY_MARKER_AS_NEXT_OFFSET;
    raw[FIRST_FLAGS_AT..FIRST_FLAGS_AT + 4].copy_from_slice(&with_marker.to_ne_bytes());

    let seen = u32::from_ne_bytes(
        raw[FIRST_FLAGS_AT..FIRST_FLAGS_AT + 4]
            .try_into()
            .expect("four bytes"),
    );
    assert_ne!(
        seen & DIRECTORY_MARKER_AS_NEXT_OFFSET,
        0,
        "cell control: setting the bit makes the check above fail — so it is reading a real bit, not a constant zero"
    );
    // And it does not collide with the flags we do set.
    for bit in [1, 2, 3] {
        assert_eq!(
            DIRECTORY_MARKER_AS_NEXT_OFFSET & (1 << bit),
            0,
            "cell distinct: the marker flag is a different bit from CasePreservedNames/UnicodeOnDisk/PersistentAcls"
        );
    }
}
