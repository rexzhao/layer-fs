//! Cutting one stored extent at a declared edit boundary.
//!
//! A retained range is a slice of exactly one base extent: the payload identity is
//! preserved and only the source offset and length move. Offsets are checked
//! against the extent's own coverage, so a boundary that does not lie inside the
//! extent is rejected rather than silently clamped.
