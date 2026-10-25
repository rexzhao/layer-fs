//! `time` tests (P3-1 §1): the two representable boundaries, the 100 ns floor, and round-trip identity.

use fs_windows::error::Failure;
use fs_windows::time::{
    filetime_to_unix_ns, unix_ns_to_filetime, FILETIME_EPOCH_OFFSET_SECS, TICKS_PER_SECOND,
};

/// The 1601 epoch is **not representable** in `i64` nanoseconds, so FILETIME 0 has no Unix counterpart.
///
/// This is the fact that made the first implementation wrong for every input (it computed the offset in
/// `i64`, where 1.16e19 ns overflows). Stating it as an assertion keeps the wider arithmetic honest: the
/// earliest instant this model can express is `i64::MIN`, whose FILETIME value is well above zero.
#[test]
fn the_1601_epoch_is_outside_i64_and_that_is_why_the_arithmetic_is_wider() {
    let offset_ns = i128::from(FILETIME_EPOCH_OFFSET_SECS) * -1_000_000_000;
    assert!(
        i64::try_from(offset_ns).is_err(),
        "the 1601 epoch does not fit in i64 nanoseconds"
    );
    assert_eq!(
        filetime_to_unix_ns(0),
        Err(Failure::InvalidParameter),
        "cell filetime=0: the 1601 epoch cannot be an i64 nanosecond timestamp"
    );

    let earliest = unix_ns_to_filetime(i64::MIN).expect("i64::MIN is after 1601");
    let expected =
        (i128::from(i64::MIN) + i128::from(FILETIME_EPOCH_OFFSET_SECS) * 1_000_000_000) / 100;
    assert_eq!(
        u64::try_from(expected).expect("fits"),
        earliest,
        "cell instant=i64::MIN: the earliest representable FILETIME"
    );
    assert!(earliest > 0, "even the earliest i64 instant is after 1601");
}

/// Nothing this API can be given is before 1601 (`i64` cannot reach it), so the refusal path is about
/// inputs that would land outside the representable range rather than about clamping.
#[test]
fn inputs_that_cannot_be_a_filetime_are_refused_not_clamped() {
    // i64::MIN is representable; anything the store cannot hold is caught on the way back.
    assert!(unix_ns_to_filetime(i64::MIN).is_ok());
    assert_eq!(
        filetime_to_unix_ns(u64::MAX),
        Err(Failure::InvalidParameter),
        "cell filetime=u64::MAX: beyond i64 nanoseconds"
    );
    // A FILETIME whose nanosecond value exceeds i64 is refused rather than wrapped into the past.
    let just_over = u64::try_from(
        (i128::from(i64::MAX) + i128::from(FILETIME_EPOCH_OFFSET_SECS) * 1_000_000_000) / 100 + 1,
    )
    .expect("fits");
    assert_eq!(
        filetime_to_unix_ns(just_over),
        Err(Failure::InvalidParameter),
        "cell filetime=just_over_i64: refused, not wrapped"
    );
}

/// Conversions **floor** to the 100 ns tick; they never round up.
#[test]
fn sub_tick_precision_is_floored() {
    let ns_1970 = 0;
    let base = unix_ns_to_filetime(ns_1970).expect("1970 is representable");
    for extra in 1..100i64 {
        assert_eq!(
            unix_ns_to_filetime(extra),
            Ok(base),
            "cell extra_ns={extra}: everything below one tick floors to the same value"
        );
    }
    assert_eq!(
        unix_ns_to_filetime(100),
        Ok(base + 1),
        "cell extra_ns=100: exactly one tick"
    );
}

/// A whole second is `TICKS_PER_SECOND` ticks, which pins the unit rather than assuming it.
#[test]
fn one_second_is_ten_million_ticks() {
    let base = unix_ns_to_filetime(0).expect("epoch");
    assert_eq!(
        unix_ns_to_filetime(1_000_000_000),
        Ok(base + u64::try_from(TICKS_PER_SECOND).expect("fits")),
        "cell span=1s"
    );
}

/// Round trip at tick granularity, including a value near the `u64` ceiling of FILETIME.
#[test]
fn round_trips_at_tick_granularity() {
    for ns in [0i64, 1_000, 1_234_567_800, 2_147_483_647_000_000_000] {
        let filetime = unix_ns_to_filetime(ns).expect("representable");
        let back = filetime_to_unix_ns(filetime).expect("round trips");
        assert_eq!(
            back,
            ns - ns % 100,
            "cell ns={ns}: the round trip must land on the floored tick"
        );
    }
    assert_eq!(
        filetime_to_unix_ns(u64::MAX),
        Err(Failure::InvalidParameter),
        "cell filetime=u64::MAX: outside i64 nanoseconds, refused rather than wrapped"
    );
}

/// Local time zones do not participate: the same instant converts identically regardless of environment.
#[test]
fn conversion_does_not_depend_on_the_local_time_zone() {
    let ns = 1_700_000_000_123_456_700;
    let first = unix_ns_to_filetime(ns).expect("representable");
    std::env::set_var("TZ", "Pacific/Kiritimati");
    let second = unix_ns_to_filetime(ns).expect("representable");
    std::env::remove_var("TZ");
    assert_eq!(first, second, "cell ns={ns}: the conversion is UTC-only");
}
