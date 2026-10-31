use crate::{Bson, Document, MAX_DEPTH, compare, new_object_id, read_document, relaxed_json};
use std::cmp::Ordering;

fn sample() -> Document {
    let mut inner = Document::new();
    inner.insert("city", Bson::String("London".to_owned()));
    Document::from_entries(vec![
        ("_id".to_owned(), Bson::ObjectId([1; 12])),
        ("name".to_owned(), Bson::String("Ada".to_owned())),
        ("age".to_owned(), Bson::Int32(36)),
        ("score".to_owned(), Bson::Double(9.5)),
        ("big".to_owned(), Bson::Int64(1 << 40)),
        ("address".to_owned(), Bson::Document(inner)),
        ("tags".to_owned(), Bson::Array(vec![Bson::String("a".to_owned()), Bson::Null])),
        ("photo".to_owned(), Bson::Binary { subtype: 0, bytes: vec![1, 2, 3] }),
        ("active".to_owned(), Bson::Boolean(true)),
        ("born".to_owned(), Bson::DateTime(-4_000_000_000_000)),
        ("pattern".to_owned(), Bson::Regex { pattern: "^a".to_owned(), options: "i".to_owned() }),
        ("stamp".to_owned(), Bson::Timestamp { time: 7, increment: 2 }),
        ("price".to_owned(), Bson::Decimal128([0; 16])),
        ("low".to_owned(), Bson::MinKey),
        ("high".to_owned(), Bson::MaxKey),
        ("code".to_owned(), Bson::JavaScriptWithScope { code: "x".to_owned(), scope: Document::new() }),
    ])
}

#[test]
fn a_document_round_trips_through_its_bytes() {
    let document = sample();
    let bytes = document.to_bytes();
    assert_eq!(Document::from_bytes(&bytes).unwrap(), document);
}

#[test]
fn the_specification_example_reads() {
    let bytes = b"\x16\x00\x00\x00\x02hello\x00\x06\x00\x00\x00world\x00\x00";
    let document = Document::from_bytes(bytes).unwrap();
    assert_eq!(document.get_str("hello"), Some("world"));
    assert_eq!(document.to_bytes(), bytes);
}

#[test]
fn every_truncation_and_corruption_is_an_error_not_a_panic() {
    let bytes = sample().to_bytes();
    for length in 0..bytes.len() {
        assert!(Document::from_bytes(&bytes[..length]).is_err(), "length {length}");
    }
    for at in 0..bytes.len() {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut corrupted = bytes.clone();
            corrupted[at] ^= flip;
            let _ = Document::from_bytes(&corrupted);
        }
    }
}

#[test]
fn hostile_lengths_and_depths_are_refused() {
    assert!(Document::from_bytes(&[0xff, 0xff, 0xff, 0x7f, 0]).is_err());
    assert!(Document::from_bytes(&[4, 0, 0, 0, 0]).is_err());
    assert!(Document::from_bytes(b"\x0d\x00\x00\x00\x02a\x00\x00\x00\x00\x00\x00").is_err());
    assert!(Document::from_bytes(b"\x0c\x00\x00\x00\x08a\x00\x02\x00\x00\x00\x00").is_err());
    let mut nested = Document::new();
    for _ in 0..=MAX_DEPTH + 1 {
        let mut outer = Document::new();
        outer.insert("a", Bson::Document(nested));
        nested = outer;
    }
    assert!(Document::from_bytes(&nested.to_bytes()).is_err());
}

#[test]
fn read_document_reports_the_bytes_used() {
    let mut bytes = sample().to_bytes();
    let length = bytes.len();
    bytes.extend_from_slice(b"rest");
    assert_eq!(read_document(&bytes).unwrap().1, length);
    assert!(Document::from_bytes(&bytes).is_err());
}

#[test]
fn types_compare_in_the_documented_order() {
    let ordered = [
        Bson::MinKey,
        Bson::Null,
        Bson::Int32(1),
        Bson::String("a".to_owned()),
        Bson::Document(Document::new()),
        Bson::Array(Vec::new()),
        Bson::Binary { subtype: 0, bytes: Vec::new() },
        Bson::ObjectId([0; 12]),
        Bson::Boolean(false),
        Bson::DateTime(0),
        Bson::Timestamp { time: 0, increment: 0 },
        Bson::Regex { pattern: String::new(), options: String::new() },
        Bson::MaxKey,
    ];
    for pair in ordered.windows(2) {
        assert_eq!(compare(&pair[0], &pair[1]), Ordering::Less, "{pair:?}");
    }
}

#[test]
fn numbers_compare_by_value_across_types() {
    assert_eq!(compare(&Bson::Int32(2), &Bson::Double(2.0)), Ordering::Equal);
    assert_eq!(compare(&Bson::Int64(2), &Bson::Double(2.5)), Ordering::Less);
    assert_eq!(compare(&Bson::Int64(i64::MAX), &Bson::Double(9.3e18)), Ordering::Less);
    assert_eq!(compare(&Bson::Int64(i64::MAX), &Bson::Int64(i64::MAX - 1)), Ordering::Greater);
    assert_eq!(compare(&Bson::Double(f64::NAN), &Bson::Int32(i32::MIN)), Ordering::Less);
}

#[test]
fn object_ids_are_unique_and_start_with_the_time() {
    let first = new_object_id(0x6500_0000, || [9; 5], 10);
    let second = new_object_id(0x6500_0000, || [8; 5], 10);
    assert_ne!(first, second);
    assert_eq!(&first[..4], &[0x65, 0, 0, 0]);
    assert_eq!(first[4..9], second[4..9]);
}

#[test]
fn relaxed_extended_json_shows_each_type() {
    let text = relaxed_json(&sample());
    assert!(text.starts_with("{\"_id\": {\"$oid\": \"010101010101010101010101\"}, \"name\": \"Ada\", \"age\": 36, \"score\": 9.5"), "{text}");
    assert!(text.contains("\"photo\": {\"$binary\": {\"base64\": \"AQID\", \"subType\": \"00\"}}"), "{text}");
    assert!(text.contains("\"born\": {\"$date\": {\"$numberLong\": \"-4000000000000\"}}"), "{text}");
}
