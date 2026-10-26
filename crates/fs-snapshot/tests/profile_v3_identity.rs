//! The v3 profile's **identity**, pinned literally (ADR-005-v3 draft §2.3; P3-2 card §3.1 last row).
//!
//! `profile_id = BLAKE3("wcas/snapshot-profile/v1\0" || description)`, so the description string *is* the identity: one
//! reflowed line in `description()` silently creates a profile that no existing store matches. That is why the expected
//! string is written out here in full rather than built from the same code it is testing — a test that concatenated the
//! same pieces would agree with any change.

use fs_snapshot::Profile;

/// ADR-005-v3 draft §2.3, verbatim (line breaks in the document are not part of the string).
const V3_DESCRIPTION: &str = concat!(
    "wcas/snapshot/v3;fmt=1;inode_fmt=2;mask_bits=6;min_entries=4;max_page_bytes=16384;",
    "cdc=fastcdc-nc2;cdc_min=8192;cdc_avg=16384;cdc_max=32768;gear=blake3:wcas/gear/v1;",
    "content_role=chunk;max_object_value=16777216;name_max=255;hash=blake3;domains=inode,dir,extent"
);

/// The description is exactly the ADR's string, character for character.
#[test]
fn the_v3_description_is_the_adr_string() {
    assert_eq!(
        Profile::V3.description(),
        V3_DESCRIPTION,
        "cell description: any character change here is a new profile id, so this must match §2.3 verbatim"
    );
}

/// The id is the BLAKE3 of the domain-separated description, computed here from the literal.
#[test]
fn the_v3_id_is_the_hash_of_that_string() {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"wcas/snapshot-profile/v1\0");
    hasher.update(V3_DESCRIPTION.as_bytes());
    let expected = *hasher.finalize().as_bytes();
    assert_eq!(
        Profile::V3.id(),
        expected,
        "cell id: the profile id must be the hash of the literal §2.3 string"
    );
}

/// v2's id is **unchanged** — the frozen fixture store is opened with it, so a change here would break a committed
/// artifact rather than a test.
#[test]
fn the_v2_id_is_unchanged_and_still_current() {
    assert_eq!(
        Profile::CURRENT,
        Profile::V3,
        "cell current: since G3 this build **writes v3**; v2 remains readable (the frozen fixture proves it)"
    );
    assert_eq!(
        Profile::V2.description(),
        concat!(
            "wcas/snapshot/v2;fmt=1;mask_bits=6;min_entries=4;max_page_bytes=16384;",
            "cdc=fastcdc-nc2;cdc_min=8192;cdc_avg=16384;cdc_max=32768;gear=blake3:wcas/gear/v1;",
            "content_role=chunk;max_object_value=16777216;name_max=255;hash=blake3;domains=inode,dir,extent"
        ),
        "cell v2: the v2 description is frozen — the committed v2 fixture store depends on its id"
    );
}

/// Only the inode page format moved: `fmt` is still 1 in v3, `inode_fmt` is 2. A v3 profile that also changed `fmt`
/// would make directory/extent pages unreadable by v2 readers for no reason.
#[test]
fn v3_changes_only_the_inode_page_format() {
    assert_eq!(
        Profile::V3.fmt,
        1,
        "cell fmt: directory/extent/root pages keep fmt = 1"
    );
    assert_eq!(
        Profile::V3.inode_fmt,
        2,
        "cell inode_fmt: inode pages become fmt = 2"
    );
    assert_eq!(Profile::V1.inode_fmt, 1, "cell v1: inode pages are fmt = 1");
    assert_eq!(Profile::V2.inode_fmt, 1, "cell v2: inode pages are fmt = 1");
    // Everything except the record format is identical, so a v3 encoder differs from v2 only in the record bytes.
    assert_eq!(
        Profile::V3.max_page_bytes,
        Profile::V2.max_page_bytes,
        "cell capacity: the page limit is unchanged; records grow into it"
    );
    assert_eq!(
        Profile::V3.mask_bits,
        Profile::V2.mask_bits,
        "cell split: same key-hash rule"
    );
}
