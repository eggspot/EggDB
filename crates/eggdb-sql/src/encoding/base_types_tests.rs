use super::{
    BASE_TYPES_FORMAT_VERSION, CITEXT_TAG, Column, DEFINED_TYPES_FORMAT_VERSION, FORMAT_VERSION,
    HSTORE_TAG, Reader, TableDefinition, decode_key_values, decode_row, decode_user_type,
    encode_key_value, encode_row, encode_user_type, skip_key_values,
};
use crate::types::{DefinedType, SqlType, Value};
use crate::user_types::{Attribute, DomainDefinition, TypeBody, UserType};

fn column(name: &str, sql_type: SqlType) -> Column {
    Column {
        name: name.into(),
        sql_type,
        not_null: false,
        default: None,
        auto_increment: false,
    }
}

fn table_with(columns: Vec<Column>) -> TableDefinition {
    TableDefinition {
        id: 9,
        name: "t".into(),
        columns,
        primary_key: vec![0],
        ..TableDefinition::default()
    }
}

fn base_table() -> TableDefinition {
    table_with(vec![
        column("id", SqlType::BigInt),
        column("email", SqlType::Defined(DefinedType::CITEXT)),
        column("attributes", SqlType::Defined(DefinedType::HSTORE)),
    ])
}

fn hstore(text: &str) -> Value {
    crate::hstore::value(crate::hstore::parse_text(text).unwrap())
}

fn damaged(bytes: &[u8]) -> bool {
    TableDefinition::decode("t", bytes).is_err_and(|error| error.state().code() == "XX001")
}

#[test]
fn a_table_using_citext_or_hstore_is_version_11_and_round_trips() {
    let table = base_table();
    let encoded = table.encode();
    assert_eq!(encoded[0], BASE_TYPES_FORMAT_VERSION);
    assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
    let citext_type = [23, 5, 0x28, 0x23, 0, 0];
    let hstore_type = [23, 5, 0x2A, 0x23, 0, 0];
    assert!(encoded.windows(6).any(|bytes| bytes == citext_type));
    assert!(encoded.windows(6).any(|bytes| bytes == hstore_type));
    for cut in 0..encoded.len() {
        assert!(damaged(&encoded[..cut]), "cut at {cut}");
    }
    for at in 0..encoded.len() {
        for flip in [0x01u8, 0x80, 0xFF] {
            let mut changed = encoded.clone();
            changed[at] ^= flip;
            let _ = TableDefinition::decode("t", &changed);
        }
    }
}

#[test]
fn tables_without_the_new_types_keep_writing_their_older_versions_byte_for_byte() {
    let plain = table_with(vec![column("id", SqlType::BigInt)]);
    assert_eq!(
        plain.encode(),
        [1, 9, 0, 0, 0, 1, 2, b'i', b'd', 3, 0, 0, 1, 0]
    );
    let with_array = table_with(vec![
        column("id", SqlType::BigInt),
        column("tags", SqlType::Array(crate::arrays::ArrayElement::Text)),
    ]);
    let encoded = with_array.encode();
    assert_eq!(encoded[0], DEFINED_TYPES_FORMAT_VERSION);
    let mut claimed_newer = encoded.clone();
    claimed_newer[0] = BASE_TYPES_FORMAT_VERSION;
    assert!(damaged(&claimed_newer));
}

#[test]
fn a_version_11_table_cannot_pass_as_version_10_and_the_older_reader_refuses_it_cleanly() {
    let mut encoded = base_table().encode();
    let mut older_reader = Reader::new(&encoded, "table definition");
    let error = older_reader
        .version_up_to(DEFINED_TYPES_FORMAT_VERSION)
        .unwrap_err();
    assert_eq!(error.state().code(), "0A000");
    assert!(error.message().contains("format version 11"), "{error}");
    encoded[0] = DEFINED_TYPES_FORMAT_VERSION;
    assert!(damaged(&encoded));
}

#[test]
fn a_base_kind_with_any_other_oid_is_damage() {
    let encoded = base_table().encode();
    let at = encoded
        .windows(6)
        .position(|bytes| bytes == [23, 5, 0x28, 0x23, 0, 0])
        .unwrap();
    for oid in [25u32, 9001, 9003, 16_384, crate::records::RECORD_OID] {
        let mut changed = encoded.clone();
        changed[at + 2..at + 6].copy_from_slice(&oid.to_le_bytes());
        assert!(damaged(&changed), "oid {oid}");
    }
}

#[test]
fn rows_of_citext_and_hstore_round_trip_and_hostile_bytes_are_damage() {
    let columns = base_table().columns;
    let values = vec![
        Value::BigInt(1),
        crate::citext::value("MiXeD@Example.com".into()),
        hstore(r#"b=>NULL, a=>1, "k\"ey"=>"v\\al""#),
    ];
    let encoded = encode_row(&values);
    assert_eq!(decode_row(&encoded, &columns), Ok(values));
    for cut in 0..encoded.len() {
        assert!(decode_row(&encoded[..cut], &columns).is_err());
    }
    let mut state = 0x5EED_0000_C17E_0028u64;
    for _ in 0..20_000 {
        let mut bytes = encoded.clone();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let at = (state as usize) % bytes.len();
        bytes[at] = (state >> 32) as u8;
        if let Ok(decoded) = decode_row(&bytes, &columns) {
            assert_eq!(decoded.len(), columns.len());
        }
    }
    let empty = vec![Value::Null, Value::Null, hstore("")];
    assert_eq!(decode_row(&encode_row(&empty), &columns), Ok(empty));
}

#[test]
fn hstore_rows_with_unsorted_duplicate_or_badly_flagged_pairs_are_damage() {
    let columns = vec![column("h", SqlType::Defined(DefinedType::HSTORE))];
    let row = |pairs: &[u8]| {
        let mut bytes = vec![FORMAT_VERSION, 1, HSTORE_TAG];
        bytes.extend_from_slice(pairs);
        bytes
    };
    let sorted = row(&[2, 1, b'a', 0, 2, b'a', b'b', 1, 1, b'x']);
    assert_eq!(
        decode_row(&sorted, &columns),
        Ok(vec![hstore("a=>NULL, ab=>x")])
    );
    for hostile in [
        row(&[2, 2, b'a', b'b', 0, 1, b'a', 0]),
        row(&[2, 1, b'a', 0, 1, b'a', 0]),
        row(&[1, 1, b'a', 2]),
        row(&[1, 1, b'a', 1, 5, b'x']),
        row(&[200, 1, b'a', 0]),
        row(&[1, 1, 0xFF, 0]),
    ] {
        assert!(decode_row(&hostile, &columns).is_err(), "{hostile:?}");
    }
}

#[test]
fn a_tag_that_does_not_match_its_column_type_is_damage() {
    let citext_column = vec![column("c", SqlType::Defined(DefinedType::CITEXT))];
    let text_column = vec![column("c", SqlType::Text)];
    let as_text = encode_row(&[Value::Text("a".into())]);
    let as_citext = encode_row(&[crate::citext::value("a".into())]);
    assert_eq!(as_citext[2], CITEXT_TAG);
    assert!(decode_row(&as_text, &citext_column).is_err());
    assert!(decode_row(&as_citext, &text_column).is_err());
    assert!(decode_row(&encode_row(&[hstore("a=>1")]), &citext_column).is_err());
}

#[test]
fn citext_and_hstore_inside_a_record_round_trip() {
    let columns = vec![column("r", SqlType::Defined(DefinedType::RECORD))];
    let record = crate::records::record_value(
        crate::records::RECORD_OID,
        vec![crate::citext::value("Ab".into()), hstore("x=>y")],
        vec!["c".into(), "h".into()],
    );
    let encoded = encode_row(std::slice::from_ref(&record));
    assert_eq!(decode_row(&encoded, &columns), Ok(vec![record]));
    for cut in 0..encoded.len() {
        assert!(decode_row(&encoded[..cut], &columns).is_err());
    }
}

#[test]
fn citext_keys_fold_case_so_case_variants_collide_and_order_ignores_case() {
    let key = |text: &str| {
        let mut out = Vec::new();
        encode_key_value(&mut out, &crate::citext::value(text.into()));
        out
    };
    assert_eq!(key("ABC"), key("abc"));
    assert_eq!(key("Straße"), key("STRASSE"));
    assert!(key("apple") < key("Banana"));
    assert!(key("Banana") < key("cherry"));
    let encoded = key("MiXeD");
    let citext = SqlType::Defined(DefinedType::CITEXT);
    let mut values = Vec::new();
    assert_eq!(
        decode_key_values(&encoded, [citext], &mut values),
        Ok(encoded.len())
    );
    assert_eq!(values, vec![crate::citext::value("mixed".into())]);
    assert_eq!(skip_key_values(&encoded, [citext]), Ok(encoded.len()));
    for cut in 0..encoded.len() {
        assert!(skip_key_values(&encoded[..cut], [citext]).is_err());
    }
}

fn base_user_types() -> Vec<UserType> {
    vec![
        UserType {
            oid: 20_010,
            name: "contact".into(),
            body: TypeBody::Composite(vec![
                Attribute {
                    name: "email".into(),
                    sql_type: SqlType::Defined(DefinedType::CITEXT),
                },
                Attribute {
                    name: "id".into(),
                    sql_type: SqlType::Integer,
                },
            ]),
        },
        UserType {
            oid: 20_011,
            name: "settings".into(),
            body: TypeBody::Domain(DomainDefinition {
                base: SqlType::Defined(DefinedType::HSTORE),
                not_null: false,
                default: None,
                checks: Vec::new(),
            }),
        },
    ]
}

#[test]
fn type_records_using_citext_or_hstore_are_version_2_and_the_older_reader_refuses_them() {
    for sample in base_user_types() {
        let mut bytes = encode_user_type(&sample);
        assert_eq!(bytes[0], 2);
        assert_eq!(decode_user_type(&sample.name, &bytes), Ok(sample.clone()));
        for cut in 0..bytes.len() {
            assert!(decode_user_type(&sample.name, &bytes[..cut]).is_err());
        }
        let mut older_reader = Reader::new(&bytes, "type definition");
        assert_eq!(
            older_reader
                .version_up_to(FORMAT_VERSION)
                .unwrap_err()
                .state()
                .code(),
            "0A000"
        );
        bytes[0] = 1;
        assert!(decode_user_type(&sample.name, &bytes).is_err());
    }
}

#[test]
fn a_type_record_of_the_base_kind_is_damage() {
    let mut bytes = encode_user_type(&base_user_types()[1]);
    bytes[1] = 5;
    assert!(decode_user_type("settings", &bytes).is_err());
}
