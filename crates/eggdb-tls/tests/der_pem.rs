//! DER and PEM: what the writer produces reads back, and hostile input is refused with an
//! error, never a panic.

use eggdb_tls::der::{DerError, DerReader, DerWriter, tag, unsigned_integer_magnitude};
use eggdb_tls::oid;
use eggdb_tls::pem::{PemError, pem_decode_all, pem_encode};

#[test]
fn a_written_structure_reads_back() {
    let mut writer = DerWriter::new();
    writer.write_sequence(|sequence| {
        sequence.write_small_unsigned_integer(2);
        sequence.write_unsigned_integer(&[0x00, 0x00, 0x80, 0x01]);
        sequence.write_object_identifier(oid::PRIME256V1);
        sequence.write_boolean(true);
        sequence.write_null();
        sequence.write_bit_string(&[1, 2, 3]);
        sequence.write_octet_string(&[0x55; 300]);
        sequence.write_constructed(tag::context_constructed(3), |inner| {
            inner.write_small_unsigned_integer(0);
        });
    });
    let bytes = writer.into_bytes();
    let mut outer = DerReader::new(&bytes);
    let mut sequence = outer.read_sequence().unwrap();
    outer.finish().unwrap();
    assert_eq!(sequence.read_small_unsigned_integer().unwrap(), 2);
    assert_eq!(sequence.read_unsigned_integer().unwrap(), [0x80, 0x01]);
    assert_eq!(sequence.read_object_identifier().unwrap(), oid::PRIME256V1);
    assert!(sequence.read_boolean().unwrap());
    sequence.read_null().unwrap();
    assert_eq!(sequence.read_bit_string().unwrap(), [1, 2, 3]);
    assert_eq!(sequence.read_octet_string().unwrap(), [0x55; 300]);
    assert_eq!(sequence.read_optional(tag::context_constructed(1)).unwrap(), None);
    let mut explicit = sequence
        .read_constructed(tag::context_constructed(3))
        .unwrap();
    assert_eq!(explicit.read_small_unsigned_integer().unwrap(), 0);
    explicit.finish().unwrap();
    sequence.finish().unwrap();
}

#[test]
fn an_element_keeps_its_whole_encoding() {
    let bytes = [0x30, 0x03, 0x02, 0x01, 0x05, 0xff];
    let mut reader = DerReader::new(&bytes);
    let element = reader.read_element().unwrap();
    assert_eq!(element.tag, tag::SEQUENCE);
    assert_eq!(element.contents, [0x02, 0x01, 0x05]);
    assert_eq!(element.encoded, &bytes[..5]);
    assert_eq!(reader.finish(), Err(DerError::TrailingData));
}

#[test]
fn encodings_der_forbids_are_refused() {
    let cases: [(&[u8], DerError); 9] = [
        (&[0x30], DerError::Truncated),
        (&[0x30, 0x05, 0x00], DerError::Truncated),
        (&[0x1f, 0x01, 0x00], DerError::HighTagNumber),
        (&[0x30, 0x80, 0x00, 0x00], DerError::LengthEncoding),
        (&[0x04, 0x81, 0x05, 1, 2, 3, 4, 5], DerError::LengthEncoding),
        (&[0x04, 0x82, 0x00, 0x80], DerError::LengthEncoding),
        (&[0x04, 0x85, 1, 0, 0, 0, 0], DerError::LengthEncoding),
        (&[0x04, 0x84, 0xff, 0xff, 0xff, 0xff], DerError::Truncated),
        (&[0x04, 0x81], DerError::Truncated),
    ];
    for (bytes, expected) in cases {
        assert_eq!(DerReader::new(bytes).read_element().unwrap_err(), expected, "{bytes:02x?}");
    }
}

#[test]
fn integers_must_be_minimal_and_unsigned_where_required() {
    assert_eq!(unsigned_integer_magnitude(&[]), Err(DerError::IntegerEncoding));
    assert_eq!(unsigned_integer_magnitude(&[0x00, 0x01]), Err(DerError::IntegerEncoding));
    assert_eq!(unsigned_integer_magnitude(&[0x80]), Err(DerError::IntegerEncoding));
    assert_eq!(unsigned_integer_magnitude(&[0x00]).unwrap(), [0x00]);
    assert_eq!(unsigned_integer_magnitude(&[0x00, 0x80]).unwrap(), [0x80]);
    let mut writer = DerWriter::new();
    writer.write_unsigned_integer(&[]);
    writer.write_unsigned_integer(&[0xff]);
    assert_eq!(writer.into_bytes(), [0x02, 0x01, 0x00, 0x02, 0x02, 0x00, 0xff]);
    let too_big = [0x02, 0x09, 0x01, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        DerReader::new(&too_big).read_small_unsigned_integer(),
        Err(DerError::IntegerEncoding)
    );
}

#[test]
fn malformed_primitives_are_refused() {
    let boolean = [0x01, 0x01, 0x01];
    assert_eq!(DerReader::new(&boolean).read_boolean(), Err(DerError::PrimitiveEncoding));
    let bits = [0x03, 0x02, 0x01, 0xff];
    assert_eq!(DerReader::new(&bits).read_bit_string(), Err(DerError::BitStringEncoding));
    assert_eq!(DerReader::new(&bits).read_flag_bit_string(), Err(DerError::BitStringEncoding));
    let flags = [0x03, 0x02, 0x05, 0xa0];
    assert_eq!(DerReader::new(&flags).read_flag_bit_string().unwrap(), [0xa0]);
    for identifier in [
        &[0x06, 0x00][..],
        &[0x06, 0x02, 0x2a, 0x86][..],
        &[0x06, 0x02, 0x80, 0x01][..],
    ] {
        assert_eq!(
            DerReader::new(identifier).read_object_identifier(),
            Err(DerError::ObjectIdentifierEncoding)
        );
    }
    assert_eq!(
        DerReader::new(&[0x02, 0x01, 0x00]).read_sequence().unwrap_err(),
        DerError::UnexpectedTag {
            expected: tag::SEQUENCE,
            found: Some(tag::INTEGER)
        }
    );
}

#[test]
fn every_truncation_of_a_structure_is_an_error_not_a_panic() {
    let mut writer = DerWriter::new();
    writer.write_sequence(|sequence| {
        sequence.write_object_identifier(oid::EC_PUBLIC_KEY);
        sequence.write_octet_string(&[7; 200]);
    });
    let bytes = writer.into_bytes();
    for length in 0..bytes.len() {
        assert!(DerReader::new(&bytes[..length]).read_sequence().is_err());
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..10_000 {
        let mut mutated = bytes.clone();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let position = (state as usize) % mutated.len();
        mutated[position] = (state >> 32) as u8;
        let mut reader = DerReader::new(&mutated);
        if let Ok(mut sequence) = reader.read_sequence() {
            let _ = sequence.read_object_identifier();
            let _ = sequence.read_octet_string();
            let _ = sequence.finish();
        }
    }
}

#[test]
fn pem_blocks_round_trip_and_text_between_them_is_ignored() {
    let first = pem_encode("CERTIFICATE", &[0x30, 0x03, 0x02, 0x01, 0x05]);
    let second = pem_encode("PRIVATE KEY", &[9; 100]);
    assert!(second.lines().all(|line| line.len() <= 64));
    let text = format!("subject=CN = EggDB\n{first}\r\nexplanatory text\n{second}");
    let blocks = pem_decode_all(&text).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].label, "CERTIFICATE");
    assert_eq!(blocks[0].contents, [0x30, 0x03, 0x02, 0x01, 0x05]);
    assert_eq!(blocks[1].label, "PRIVATE KEY");
    assert_eq!(blocks[1].contents, [9; 100]);
    assert!(pem_decode_all("no blocks at all").unwrap().is_empty());
}

#[test]
fn broken_pem_is_refused() {
    let unfinished = "-----BEGIN CERTIFICATE-----\nMAMCAQU=\n";
    assert_eq!(
        pem_decode_all(unfinished),
        Err(PemError::MissingEnd {
            label: "CERTIFICATE".to_string()
        })
    );
    let mismatched = "-----BEGIN CERTIFICATE-----\nMAMCAQU=\n-----END PRIVATE KEY-----\n";
    assert!(pem_decode_all(mismatched).is_err());
    let encrypted = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nMAMCAQU=\n-----END RSA PRIVATE KEY-----\n";
    assert_eq!(pem_decode_all(encrypted), Err(PemError::EncryptedHeaders));
    let not_base64 = "-----BEGIN CERTIFICATE-----\nMA*CAQU=\n-----END CERTIFICATE-----\n";
    assert!(matches!(pem_decode_all(not_base64), Err(PemError::Base64(_))));
}
