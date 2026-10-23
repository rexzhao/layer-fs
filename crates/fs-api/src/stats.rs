//! Counters the API exposes (ADR-004 §6).

/// Read-amplification counters for one [`crate::Fs`].
///
/// This is a **production metric**, not a test hook: spec §5.6 requires a read
/// amplification measure, and the number of object fetches a small read causes
/// is its smallest useful form. T5 reuses it directly, so there is no feature
/// gate around it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// How many object fetches have happened since the handle opened.
    ///
    /// A range read should touch only the blocks its range intersects.
    pub object_gets: u64,
}

/// Storage and namespace totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Published object count.
    pub objects: u64,
    /// Publication records in this namespace.
    pub root_records: u64,
}
