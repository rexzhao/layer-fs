//! Byte-level primitives shared by every snapshot value (ADR-005 §3).
//!
//! All integers are big-endian and fixed width; the reader is written against the
//! same functions so an encode/decode pair cannot drift.

use crate::error::{malformed, SnapshotError, SnapshotResult};

/// A byte reader that refuses to read past the end and notices trailing bytes.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    /// The unread bytes, without consuming them.
    ///
    /// Needed by decoders whose length is only known **after** parsing (the v3 inode record: `security_len` decides how
    /// long the record is). They parse a peek, then `take` exactly what they consumed — `take(remaining())` would look
    /// equivalent for a single-record page and would silently swallow every following record.
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    pub(crate) fn at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }

    pub(crate) fn take(&mut self, count: usize) -> SnapshotResult<&'a [u8]> {
        if self.remaining() < count {
            return Err(malformed(format!(
                "need {count} bytes at offset {}, only {} remain",
                self.offset,
                self.remaining()
            )));
        }
        let slice = &self.bytes[self.offset..self.offset + count];
        self.offset += count;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> SnapshotResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> SnapshotResult<u32> {
        let raw = self.take(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    pub(crate) fn u64(&mut self) -> SnapshotResult<u64> {
        let raw = self.take(8)?;
        Ok(u64::from_be_bytes([
            raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
        ]))
    }

    pub(crate) fn i64(&mut self) -> SnapshotResult<i64> {
        Ok(self.u64()? as i64)
    }

    /// A 32-byte object id.
    pub(crate) fn oid(&mut self) -> SnapshotResult<cas_core::ObjectId> {
        let raw = self.take(32)?;
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(raw);
        Ok(cas_core::ObjectId::from_bytes(bytes))
    }

    /// A name: `u8 len | len bytes`, with `1 <= len <= name_max`.
    pub(crate) fn name(&mut self, name_max: usize) -> SnapshotResult<Vec<u8>> {
        let len = self.u8()? as usize;
        if len == 0 {
            return Err(malformed("a name may not be empty"));
        }
        if len > name_max {
            return Err(malformed(format!(
                "name length {len} exceeds the profile limit {name_max}"
            )));
        }
        Ok(self.take(len)?.to_vec())
    }

    /// Reads the value format byte and checks it.
    pub(crate) fn format(&mut self, expected: u8) -> SnapshotResult<()> {
        let found = self.u8()?;
        if found != expected {
            return Err(SnapshotError::UnsupportedFormat { found });
        }
        Ok(())
    }

    /// Fails when anything is left over: a value is exactly as long as its fields.
    pub(crate) fn finish(&self) -> SnapshotResult<()> {
        if !self.at_end() {
            return Err(malformed(format!(
                "{} trailing bytes after the last field",
                self.remaining()
            )));
        }
        Ok(())
    }
}

/// A byte writer mirroring [`Reader`].
#[derive(Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    #[cfg(test)]
    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn oid(&mut self, value: cas_core::ObjectId) {
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// The underlying buffer, for a codec that writes its own layout (the v3 inode record).
    ///
    /// Exists so the record encoder can append **in place**: allocating a `Vec` per record cost more than the four extra
    /// fields did on the 99,999-record baseline.
    pub(crate) fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip_big_endian() {
        let mut writer = Writer::new();
        writer.u8(0x7F);
        writer.u32(0x0102_0304);
        writer.u64(0x0102_0304_0506_0708);
        writer.i64(-2);
        let bytes = writer.finish();
        assert_eq!(bytes[0], 0x7F);
        assert_eq!(&bytes[1..5], &[1, 2, 3, 4], "big-endian u32");
        assert_eq!(&bytes[5..13], &[1, 2, 3, 4, 5, 6, 7, 8], "big-endian u64");

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u8().expect("u8"), 0x7F);
        assert_eq!(reader.u32().expect("u32"), 0x0102_0304);
        assert_eq!(reader.u64().expect("u64"), 0x0102_0304_0506_0708);
        assert_eq!(reader.i64().expect("i64"), -2);
        reader.finish().expect("no trailing bytes");
    }

    #[test]
    fn a_name_round_trips_and_rejects_empty_and_oversize() {
        // Names are written by `encode_name` (its length prefix is part of the
        // value layout, not of the writer), and read back by `Reader::name`.
        let bytes = crate::encode::encode_name(b"hello", 255).expect("write");
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.name(255).expect("read"), b"hello");

        assert!(crate::encode::encode_name(b"", 255).is_err(), "empty name");
        assert!(
            crate::encode::encode_name(&[b'x'; 256], 255).is_err(),
            "over the profile limit"
        );
    }

    #[test]
    fn a_truncated_value_is_malformed_not_panicking() {
        let mut reader = Reader::new(&[0, 0]);
        let error = reader.u32().expect_err("truncated");
        assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
    }

    #[test]
    fn trailing_bytes_are_malformed() {
        let reader = Reader::new(&[1, 2, 3]);
        let error = reader.finish().expect_err("trailing");
        assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
    }
}
