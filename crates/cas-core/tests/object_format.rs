//! Integration tests for the version-1 object format.
//!
//! These tests exercise only the public surface of `cas-core`, the same one a
//! Store or CLI will use.

use cas_core::{
    verify_id, EnvelopeError, IdParseError, Object, ObjectId, ObjectIdHasher, Role,
    ENVELOPE_HEADER_LEN, MAGIC, MAX_OBJECT_LEN, MAX_VALUE_LEN, VERSION,
};

/// A frame with the fields supplied directly, so a test can build bytes no
/// constructor would ever produce.
fn frame(role: u8, payload_len: u32, value_len: u32, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_HEADER_LEN + value.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(role);
    out.extend_from_slice(&payload_len.to_be_bytes());
    out.extend_from_slice(&value_len.to_be_bytes());
    out.extend_from_slice(value);
    out
}

#[test]
fn roundtrip_preserves_role_and_value() {
    let object = Object::new(Role::Bytes, b"hello, world").expect("framed");
    let bytes = object.to_vec();
    assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN + 12);
    assert_eq!(&bytes[0..4], &MAGIC);
    assert_eq!(bytes[4], VERSION);
    assert_eq!(bytes[5], Role::Bytes.tag());

    let decoded = Object::decode(&bytes).expect("decodes");
    assert_eq!(decoded.role(), Role::Bytes);
    assert_eq!(decoded.value(), b"hello, world");
    assert_eq!(decoded.envelope(), object.envelope());
}

#[test]
fn roundtrip_covers_every_role_and_value_shape() {
    let shapes: [&[u8]; 4] = [b"", b"\x00", b"\x00\x00\xff\x7f", b"payload-bytes"];
    for role in Role::ALL {
        for value in shapes {
            let object = Object::new(role, value).expect("framed");
            let bytes = object.to_vec();
            let decoded = Object::decode(&bytes).expect("decodes");
            assert_eq!(decoded.role(), role, "role {role} round-trip");
            assert_eq!(decoded.value(), value, "value {value:?} round-trip");
        }
    }
}

#[test]
fn frame_into_caller_buffer_reports_written_length() {
    let object = Object::new(Role::Chunk, b"abcdef").expect("framed");
    let mut buffer = vec![0u8; 64];
    let written = object.frame(&mut buffer);
    assert_eq!(written, ENVELOPE_HEADER_LEN + 6);
    assert_eq!(&buffer[..written], object.to_vec().as_slice());
    assert!(buffer[written..].iter().all(|byte| *byte == 0));
}

#[test]
fn decoded_object_hashes_to_the_same_identity() {
    for role in Role::ALL {
        let object = Object::new(role, b"same value").expect("framed");
        let bytes = object.to_vec();
        let decoded = Object::decode(&bytes).expect("decodes");
        assert_eq!(
            decoded.id().expect("hashed"),
            object.id().expect("hashed"),
            "role {role}"
        );
    }
}

#[test]
fn identity_is_lowercase_hex_and_parses_back() {
    let id = Object::new(Role::Bytes, b"x")
        .expect("framed")
        .id()
        .expect("hashed");
    let text = id.to_string();
    assert_eq!(text.len(), 64);
    assert!(text
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(text.parse::<ObjectId>().expect("parses"), id);
    // Uppercase input is accepted and yields the same digest.
    assert_eq!(text.to_uppercase().parse::<ObjectId>().expect("parses"), id);
}

#[test]
fn id_parse_rejects_wrong_length_and_non_hex() {
    assert_eq!(
        "abc".parse::<ObjectId>(),
        Err(IdParseError::Length { actual: 3 })
    );
    let mut text = Object::new(Role::Bytes, b"x")
        .expect("framed")
        .id()
        .expect("hashed")
        .to_string();
    text.replace_range(5..6, "g");
    assert_eq!(
        text.parse::<ObjectId>(),
        Err(IdParseError::NotHex {
            offset: 5,
            found: 'g'
        })
    );
}

#[test]
fn id_parse_reports_a_length_in_bytes_not_characters() {
    // 64 multi-byte characters: 64 characters but 128 bytes. The old message
    // read "expected 64, found 64 characters" while failing, which was
    // misleading; the length is a byte rule.
    let text = "é".repeat(64);
    assert_eq!(text.chars().count(), 64);
    assert_eq!(text.len(), 128);
    assert_eq!(
        text.parse::<ObjectId>(),
        Err(IdParseError::Length { actual: 128 })
    );

    // A too-short multi-byte string reports its byte count too.
    assert_eq!(
        "é".parse::<ObjectId>(),
        Err(IdParseError::Length { actual: 2 })
    );

    // The message names the rule and the unit it measured.
    let message = IdParseError::Length { actual: 128 }.to_string();
    assert!(message.contains("64 ASCII hex characters"), "{message}");
    assert!(message.contains("128 bytes"), "{message}");
}

#[test]
fn id_parse_checks_length_before_content() {
    // A multi-byte character swapped in for one ASCII digit changes the byte
    // length (64 -> 65), so the byte-length rule fires first. This documents
    // the order: length, then hex content.
    let mut text = "0".repeat(64);
    text.replace_range(0..1, "é");
    assert_eq!(
        text.parse::<ObjectId>(),
        Err(IdParseError::Length { actual: 65 })
    );

    // Every byte of a correctly sized string is then checked.
    for offset in [0usize, 31, 63] {
        let mut text = "0".repeat(64);
        text.replace_range(offset..offset + 1, "z");
        assert_eq!(
            text.parse::<ObjectId>(),
            Err(IdParseError::NotHex { offset, found: 'z' })
        );
    }
}

#[test]
fn same_value_under_different_roles_has_different_identity() {
    let value = b"identical payload";
    let mut ids = Vec::new();
    for role in Role::ALL {
        ids.push(
            Object::new(role, value)
                .expect("framed")
                .id()
                .expect("hashed"),
        );
    }
    for (index, left) in ids.iter().enumerate() {
        for (other, right) in ids.iter().enumerate() {
            if index != other {
                assert_ne!(
                    left,
                    right,
                    "roles {} and {}",
                    Role::ALL[index],
                    Role::ALL[other]
                );
            }
        }
    }
}

#[test]
fn same_role_and_value_has_identical_identity() {
    let left = Object::new(Role::Bytes, b"dup")
        .expect("framed")
        .id()
        .expect("hashed");
    let right = Object::new(Role::Bytes, b"dup")
        .expect("framed")
        .id()
        .expect("hashed");
    assert_eq!(left, right);
}

#[test]
fn streaming_identity_matches_whole_buffer_identity() {
    let value: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let whole = ObjectId::hash(Role::Chunk, &value).expect("hashed");

    for chunk_size in [1usize, 3, 7, 64, 4095, 4096] {
        let mut stream = ObjectIdHasher::start();
        stream
            .update_envelope(Role::Chunk, value.len())
            .expect("envelope accepted");
        for chunk in value.chunks(chunk_size) {
            stream.update_value(chunk);
        }
        assert_eq!(
            stream.finish().expect("complete stream"),
            whole,
            "chunk size {chunk_size}"
        );
    }
}

#[test]
fn streaming_identity_matches_an_empty_value() {
    let mut stream = ObjectIdHasher::start();
    stream
        .update_envelope(Role::Bytes, 0)
        .expect("envelope accepted");
    assert_eq!(
        stream.finish().expect("complete stream"),
        ObjectId::hash(Role::Bytes, b"").expect("hashed")
    );
}

#[test]
fn streaming_identity_matches_the_framed_object_identity() {
    let value = b"stream me";
    let object = Object::new(Role::DirectoryLeaf, value).expect("framed");
    let mut stream = ObjectIdHasher::start();
    stream
        .update_envelope(Role::DirectoryLeaf, value.len())
        .expect("envelope accepted");
    stream.update_value(&value[..4]);
    stream.update_value(&value[4..]);
    assert_eq!(
        stream.finish().expect("complete stream"),
        object.id().expect("hashed")
    );
}

#[test]
fn streaming_envelope_refuses_an_oversized_value() {
    let mut stream = ObjectIdHasher::start();
    assert_eq!(
        stream.update_envelope(Role::Bytes, MAX_VALUE_LEN + 1),
        Err(EnvelopeError::TooLarge {
            declared: MAX_OBJECT_LEN + 1,
            limit: MAX_OBJECT_LEN,
        })
    );
}

#[test]
fn streaming_finish_rejects_too_few_value_bytes() {
    let mut stream = ObjectIdHasher::start();
    stream
        .update_envelope(Role::Bytes, 10)
        .expect("envelope accepted");
    stream.update_value(b"short");
    assert_eq!(
        stream.finish(),
        Err(EnvelopeError::ValueLengthMismatch {
            declared: 10,
            fed: 5,
        })
    );
}

#[test]
fn streaming_finish_rejects_too_many_value_bytes() {
    let mut stream = ObjectIdHasher::start();
    stream
        .update_envelope(Role::Bytes, 2)
        .expect("envelope accepted");
    stream.update_value(b"three");
    assert_eq!(
        stream.finish(),
        Err(EnvelopeError::ValueLengthMismatch {
            declared: 2,
            fed: 5,
        })
    );
}

#[test]
fn streaming_finish_rejects_a_declared_zero_with_bytes_fed() {
    let mut stream = ObjectIdHasher::start();
    stream
        .update_envelope(Role::Bytes, 0)
        .expect("envelope accepted");
    stream.update_value(b"x");
    assert_eq!(
        stream.finish(),
        Err(EnvelopeError::ValueLengthMismatch {
            declared: 0,
            fed: 1,
        })
    );
}

#[test]
fn streaming_finish_rejects_a_missing_envelope() {
    let stream = ObjectIdHasher::start();
    assert_eq!(
        stream.finish(),
        Err(EnvelopeError::Truncated {
            needed: ENVELOPE_HEADER_LEN,
            actual: 0,
        })
    );
}

#[test]
fn decode_rejects_empty_and_short_input() {
    assert_eq!(
        Object::decode(b""),
        Err(EnvelopeError::Truncated {
            needed: ENVELOPE_HEADER_LEN,
            actual: 0
        })
    );
    let bytes = Object::new(Role::Bytes, b"v").expect("framed").to_vec();
    assert_eq!(
        Object::decode(&bytes[..ENVELOPE_HEADER_LEN - 1]),
        Err(EnvelopeError::Truncated {
            needed: ENVELOPE_HEADER_LEN,
            actual: ENVELOPE_HEADER_LEN - 1
        })
    );
}

#[test]
fn decode_rejects_wrong_magic() {
    let mut bytes = Object::new(Role::Bytes, b"v").expect("framed").to_vec();
    bytes[1] = b'X';
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::BadMagic { found: *b"WXAS" })
    );
}

#[test]
fn decode_rejects_wrong_version() {
    let mut bytes = Object::new(Role::Bytes, b"v").expect("framed").to_vec();
    bytes[4] = 2;
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::UnsupportedVersion { found: 2 })
    );
    bytes[4] = 0;
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::UnsupportedVersion { found: 0 })
    );
}

#[test]
fn decode_rejects_unknown_role() {
    for tag in [0u8, 9, 200, 255] {
        let bytes = frame(tag, 0, 1, b"v");
        assert_eq!(
            Object::decode(&bytes),
            Err(EnvelopeError::UnknownRole { found: tag })
        );
    }
}

#[test]
fn decode_rejects_truncated_value() {
    // Header claims 5 value bytes, only 3 present.
    let bytes = frame(Role::Bytes.tag(), 0, 5, b"abc");
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::LengthMismatch {
            declared: ENVELOPE_HEADER_LEN as u64 + 5,
            actual: ENVELOPE_HEADER_LEN as u64 + 3,
        })
    );
}

#[test]
fn decode_rejects_a_trailing_byte() {
    let mut bytes = Object::new(Role::Bytes, b"v").expect("framed").to_vec();
    bytes.push(0);
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::LengthMismatch {
            declared: ENVELOPE_HEADER_LEN as u64 + 1,
            actual: ENVELOPE_HEADER_LEN as u64 + 2,
        })
    );
}

#[test]
fn decode_rejects_declared_value_beyond_the_cap() {
    // Declared value alone exceeds MAX_VALUE_LEN; the input is a header only,
    // so the size rule must fire before the length-vs-input rule.
    let bytes = frame(Role::Bytes.tag(), 0, (MAX_VALUE_LEN as u32) + 1, b"");
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::TooLarge {
            declared: MAX_OBJECT_LEN + 1,
            limit: MAX_OBJECT_LEN,
        })
    );
}

#[test]
fn decode_rejects_a_self_consistent_nonzero_payload() {
    // payload_len=3, value_len=1, with all 3 payload bytes present: the total
    // matches the declared length, so only the version-1 payload rule can
    // refuse it. Without that rule the payload bytes would be silently skipped.
    let mut bytes = frame(Role::Bytes.tag(), 3, 1, b"v");
    bytes.extend_from_slice(b"raw");
    assert_eq!(bytes.len() as u64, ENVELOPE_HEADER_LEN as u64 + 1 + 3);
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::NonZeroPayload { found: 3 })
    );
}

#[test]
fn decode_rejects_a_declared_payload_before_checking_length() {
    // The payload rule is checked in the header, so an inconsistent payload
    // length is reported as a non-zero payload rather than a length mismatch.
    let bytes = frame(Role::Bytes.tag(), 3, 1, b"v");
    assert_eq!(
        Object::decode(&bytes),
        Err(EnvelopeError::NonZeroPayload { found: 3 })
    );
}

#[test]
fn read_header_rejects_a_nonzero_payload() {
    let mut bytes = frame(Role::FilesystemRoot.tag(), 0, 0, b"");
    bytes[6..10].copy_from_slice(&7u32.to_be_bytes());
    assert_eq!(
        Object::read_header(&bytes),
        Err(EnvelopeError::NonZeroPayload { found: 7 })
    );
}

#[test]
fn construction_rejects_a_nonzero_payload() {
    // `from_parts` would otherwise frame and hash as if no payload existed, so
    // it refuses the request instead of dropping the argument.
    assert_eq!(
        Object::from_parts(Role::Bytes, b"v", 1),
        Err(EnvelopeError::NonZeroPayload { found: 1 })
    );
    assert_eq!(
        Object::from_parts(Role::Bytes, b"v", u32::MAX),
        Err(EnvelopeError::NonZeroPayload { found: u32::MAX })
    );
}

#[test]
fn construction_accepts_an_explicit_zero_payload() {
    let object = Object::from_parts(Role::Bytes, b"v", 0).expect("zero payload");
    assert_eq!(object.envelope().payload_len, 0);
    assert_eq!(
        object.id().expect("hashed"),
        Object::new(Role::Bytes, b"v")
            .expect("framed")
            .id()
            .expect("hashed")
    );
}

#[test]
fn construction_rejects_a_value_beyond_the_cap() {
    let value = vec![0u8; MAX_VALUE_LEN + 1];
    assert_eq!(
        Object::new(Role::Bytes, &value),
        Err(EnvelopeError::TooLarge {
            declared: (MAX_VALUE_LEN + 1) as u64,
            limit: MAX_VALUE_LEN as u64,
        })
    );
}

#[test]
fn construction_accepts_a_value_exactly_at_the_cap() {
    let value = vec![0xA5u8; MAX_VALUE_LEN];
    let object = Object::new(Role::Bytes, &value).expect("at the cap");
    assert_eq!(object.envelope().framed_len(), MAX_OBJECT_LEN);
    let bytes = object.to_vec();
    let decoded = Object::decode(&bytes).expect("decodes");
    assert_eq!(decoded.value().len(), MAX_VALUE_LEN);
}

#[test]
fn verify_id_accepts_the_matching_object() {
    let object = Object::new(Role::InodeTable, b"rows").expect("framed");
    let bytes = object.to_vec();
    let verified = verify_id(object.id().expect("hashed"), &bytes).expect("verified");
    assert_eq!(verified.value(), b"rows");
}

#[test]
fn verify_id_reports_a_content_substitution() {
    let stored = Object::new(Role::Bytes, b"original").expect("framed");
    let expected = stored.id().expect("hashed");
    let other = Object::new(Role::Bytes, b"tampered").expect("framed");
    assert_eq!(
        verify_id(expected, &other.to_vec()),
        Err(EnvelopeError::IdentityMismatch {
            expected,
            actual: other.id().expect("hashed"),
        })
    );
}

#[test]
fn verify_id_reports_a_role_substitution() {
    // Same value, different role: the bytes are valid but are another object.
    let value = b"shared";
    let expected = Object::new(Role::Bytes, value)
        .expect("framed")
        .id()
        .expect("hashed");
    let other = Object::new(Role::DirectoryLeaf, value).expect("framed");
    assert!(matches!(
        verify_id(expected, &other.to_vec()),
        Err(EnvelopeError::IdentityMismatch { .. })
    ));
}

#[test]
fn read_header_accepts_a_header_without_the_value() {
    let object = Object::new(Role::FilesystemRoot, b"0123456789").expect("framed");
    let bytes = object.to_vec();
    let header = Object::read_header(&bytes[..ENVELOPE_HEADER_LEN]).expect("header");
    assert_eq!(header.role, Role::FilesystemRoot);
    assert_eq!(header.value_len, 10);
    assert_eq!(header.payload_len, 0);
    assert_eq!(header.framed_len(), bytes.len() as u64);
}
