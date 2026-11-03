use super::dynamic_keys::{BLOB_CLASS, NUMBER_CLASS, TEXT_CLASS};
use super::{
    AFFINITY_FORMAT_VERSION, BASE_TYPES_FORMAT_VERSION, Column, DEFINED_TYPES_FORMAT_VERSION,
    IndexDefinition, NULL_IN_KEY, PRESENT, Reader, TableDefinition, decode_key_values, decode_row,
    encode_column_key, encode_row, skip_key_values,
};
use crate::type_affinity::TypeAffinity;
use crate::types::{SqlType, Value, integer_against_real};
use std::cmp::Ordering;

const TYPES: [SqlType; 4] = [
    SqlType::Dynamic(TypeAffinity::Text),
    SqlType::Dynamic(TypeAffinity::Numeric),
    SqlType::Dynamic(TypeAffinity::Real),
    SqlType::Dynamic(TypeAffinity::Blob),
];

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn key_of(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_column_key(&mut out, value, SqlType::DYNAMIC);
    out
}

fn number(nearest: [u8; 8], offset: [u8; 2]) -> Vec<u8> {
    [&[PRESENT, NUMBER_CLASS][..], &nearest, &offset].concat()
}

#[test]
fn dynamic_keys_match_their_test_vectors() {
    let zero = number([0x80, 0, 0, 0, 0, 0, 0, 0], [0x80, 0]);
    let one = number([0xBF, 0xF0, 0, 0, 0, 0, 0, 0], [0x80, 0]);
    let vectors: Vec<(Value, Vec<u8>)> = vec![
        (Value::Null, vec![NULL_IN_KEY]),
        (Value::BigInt(0), zero.clone()),
        (Value::Double(0.0), zero.clone()),
        (Value::Double(-0.0), zero),
        (Value::BigInt(1), one.clone()),
        (Value::Double(1.0), one.clone()),
        (Value::Boolean(true), one.clone()),
        (Value::Integer(1), one),
        (
            Value::BigInt(-1),
            number([0x40, 0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], [0x80, 0]),
        ),
        (
            Value::Double(2.5),
            number([0xC0, 0x04, 0, 0, 0, 0, 0, 0], [0x80, 0]),
        ),
        (
            Value::Numeric(crate::numeric::Numeric::parse("2.5").unwrap()),
            number([0xC0, 0x04, 0, 0, 0, 0, 0, 0], [0x80, 0]),
        ),
        (
            Value::BigInt(9_007_199_254_740_993),
            number([0xC3, 0x40, 0, 0, 0, 0, 0, 0], [0x80, 0x01]),
        ),
        (
            Value::BigInt(i64::MAX),
            number([0xC3, 0xE0, 0, 0, 0, 0, 0, 0], [0x7F, 0xFF]),
        ),
        (
            Value::BigInt(i64::MIN),
            number([0x3C, 0x1F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], [0x80, 0]),
        ),
        (Value::Text(String::new()), vec![PRESENT, TEXT_CLASS, 0, 1]),
        (
            Value::Text("ab".into()),
            vec![PRESENT, TEXT_CLASS, b'a', b'b', 0, 1],
        ),
        (
            Value::Text("a\0".into()),
            vec![PRESENT, TEXT_CLASS, b'a', 0, 0xFF, 0, 1],
        ),
        (
            Value::Bytea(vec![0, 0xFF]),
            vec![PRESENT, BLOB_CLASS, 0, 0xFF, 0xFF, 0, 1],
        ),
    ];
    assert_eq!((NUMBER_CLASS, TEXT_CLASS, BLOB_CLASS), (1, 2, 3));
    for (value, expected) in vectors {
        assert_eq!(key_of(&value), expected, "{value:?}");
        for sql_type in TYPES {
            let mut decoded = Vec::new();
            assert_eq!(
                decode_key_values(&expected, [sql_type], &mut decoded),
                Ok(expected.len()),
                "{value:?}"
            );
            assert_eq!(key_of(&decoded[0]), expected, "{value:?}");
            assert_eq!(skip_key_values(&expected, [sql_type]), Ok(expected.len()));
        }
    }
}

#[test]
fn an_integer_and_a_real_compare_exactly() {
    let two_to_53 = 9_007_199_254_740_992.0;
    assert_eq!(
        integer_against_real(9_007_199_254_740_993, two_to_53),
        Ordering::Greater
    );
    assert_eq!(
        integer_against_real(9_007_199_254_740_992, two_to_53),
        Ordering::Equal
    );
    assert_eq!(integer_against_real(3, 3.5), Ordering::Less);
    assert_eq!(integer_against_real(-3, -3.5), Ordering::Greater);
    assert_eq!(integer_against_real(0, -0.0), Ordering::Equal);
    assert_eq!(
        integer_against_real(i64::MAX, 9_223_372_036_854_775_808.0),
        Ordering::Less
    );
    assert_eq!(
        integer_against_real(i64::MIN, -9_223_372_036_854_775_808.0),
        Ordering::Equal
    );
    assert_eq!(
        integer_against_real(i64::MIN, f64::NEG_INFINITY),
        Ordering::Greater
    );
    assert_eq!(
        integer_against_real(i64::MAX, f64::INFINITY),
        Ordering::Less
    );
    assert_eq!(integer_against_real(0, f64::NAN), Ordering::Less);
}

fn random_value(random: &mut Random) -> Value {
    let bits = random.next();
    let pick = random.next() % 12;
    match pick {
        0 => Value::Null,
        1 => Value::BigInt((bits % 21) as i64 - 10),
        2 => Value::Double(((bits % 41) as f64 - 20.0) / 4.0),
        3 => Value::BigInt((1i64 << 53) + (bits % 9) as i64 - 4),
        4 => Value::Double(9_007_199_254_740_992.0 + 2.0 * ((bits % 5) as f64 - 2.0)),
        5 => Value::BigInt(i64::MAX - (bits % 3000) as i64),
        6 => Value::BigInt(i64::MIN + (bits % 3000) as i64),
        7 => Value::Double(f64::from_bits(bits)),
        8 => Value::BigInt(bits as i64),
        9 => {
            let alphabet = ['a', 'b', '\0', 'é', '\u{1F600}'];
            let length = (bits % 4) as usize;
            Value::Text(
                (0..length)
                    .map(|at| alphabet[((bits >> (8 * at + 8)) % 5) as usize])
                    .collect(),
            )
        }
        _ => {
            let length = (bits % 4) as usize;
            let choices = [0u8, 1, 0xFF, 0x7F];
            Value::Bytea(
                (0..length)
                    .map(|at| choices[((bits >> (8 * at + 8)) % 4) as usize])
                    .collect(),
            )
        }
    }
}

fn rank(value: &Value) -> u8 {
    match value {
        Value::Null => 3,
        Value::Text(_) => 1,
        Value::Bytea(_) => 2,
        _ => 0,
    }
}

fn sqlite_order(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        _ if rank(left) != rank(right) => rank(left).cmp(&rank(right)),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (Value::Double(a), Value::Double(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::BigInt(a), Value::Double(b)) => integer_against_real(*a, *b),
        (Value::Double(a), Value::BigInt(b)) => integer_against_real(*b, *a).reverse(),
        (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
        (Value::Bytea(a), Value::Bytea(b)) => a.cmp(b),
        _ => Ordering::Equal,
    }
}

fn is_nan(value: &Value) -> bool {
    matches!(value, Value::Double(number) if number.is_nan())
}

#[test]
fn dynamic_keys_sort_as_sqlite_orders_values_and_decode_back_to_the_same_key() {
    let mut random = Random(0x0305_D1CE_5EED_0001);
    for _ in 0..50_000 {
        let left = random_value(&mut random);
        let right = random_value(&mut random);
        if is_nan(&left) || is_nan(&right) {
            continue;
        }
        let (left_key, right_key) = (key_of(&left), key_of(&right));
        assert_eq!(
            left_key.cmp(&right_key),
            sqlite_order(&left, &right),
            "{left:?} vs {right:?}"
        );
        let sql_type = TYPES[(random.next() % 4) as usize];
        let key = [left_key.clone(), right_key.clone(), vec![9, 9]].concat();
        let mut decoded = Vec::new();
        let used = decode_key_values(&key, [sql_type, sql_type], &mut decoded).unwrap();
        assert_eq!(used, key.len() - 2);
        assert_eq!(skip_key_values(&key, [sql_type, sql_type]), Ok(used));
        assert_eq!(key_of(&decoded[0]), left_key, "{left:?}");
        assert_eq!(key_of(&decoded[1]), right_key, "{right:?}");
        assert_eq!(decoded[0].is_null(), left.is_null());
        for cut in 0..used {
            let mut partial = Vec::new();
            assert!(decode_key_values(&key[..cut], [sql_type, sql_type], &mut partial).is_err());
            assert!(skip_key_values(&key[..cut], [sql_type, sql_type]).is_err());
        }
    }
}

#[test]
fn hostile_dynamic_keys_are_errors_never_panics_and_only_canonical_keys_are_accepted() {
    let mut random = Random(0xBAD_0305_0000_0001);
    let types = [SqlType::DYNAMIC, SqlType::Dynamic(TypeAffinity::Text)];
    for _ in 0..200_000 {
        let length = (random.next() % 24) as usize;
        let mut bytes: Vec<u8> = (0..length).map(|_| random.next() as u8).collect();
        if let Some(first) = bytes.first_mut()
            && random.next() % 4 != 0
        {
            *first = PRESENT;
        }
        if let Some(second) = bytes.get_mut(1)
            && random.next() % 4 != 0
        {
            *second = 1 + (random.next() % 3) as u8;
        }
        let mut values = Vec::new();
        let decoded = decode_key_values(&bytes, types, &mut values);
        let skipped = skip_key_values(&bytes, types);
        match decoded {
            Ok(used) => {
                assert_eq!(skipped, Ok(used));
                let mut again = Vec::new();
                for (value, sql_type) in values.iter().zip(types) {
                    encode_column_key(&mut again, value, sql_type);
                }
                assert_eq!(again, &bytes[..used], "{bytes:02X?}");
            }
            Err(_) => assert!(skipped.is_err(), "{bytes:02X?}"),
        }
    }
    for refused in [
        vec![PRESENT, 4, 0, 1],
        vec![PRESENT, 0, 0, 1],
        vec![0x03],
        number([0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], [0x80, 0]),
        number([0xBF, 0xF0, 0, 0, 0, 0, 0, 0], [0x80, 0x01]),
        number([0xC3, 0x40, 0, 0, 0, 0, 0, 0], [0x80, 0x02]),
        number([0xC3, 0xE0, 0, 0, 0, 0, 0, 0], [0x80, 0x01]),
        vec![PRESENT, TEXT_CLASS, 0xFF, 0, 1],
        vec![PRESENT, TEXT_CLASS, b'a', 0, 2],
        vec![PRESENT, BLOB_CLASS, 1, 2],
    ] {
        let mut values = Vec::new();
        assert!(
            decode_key_values(&refused, [SqlType::DYNAMIC], &mut values).is_err(),
            "{refused:02X?}"
        );
    }
}

fn column(name: &str, sql_type: SqlType) -> Column {
    Column {
        name: name.into(),
        sql_type,
        not_null: false,
        default: None,
        auto_increment: false,
    }
}

fn affinity_table() -> TableDefinition {
    TableDefinition {
        id: 9,
        name: "t".into(),
        columns: vec![
            column("id", SqlType::BigInt),
            column("body_text", SqlType::Dynamic(TypeAffinity::Text)),
            column("amount_numeric", SqlType::Dynamic(TypeAffinity::Numeric)),
            column("ratio_real", SqlType::Dynamic(TypeAffinity::Real)),
            column("anything_blob", SqlType::DYNAMIC),
        ],
        primary_key: vec![1],
        indexes: vec![IndexDefinition {
            id: 1,
            name: "t_amount".into(),
            unique: false,
            columns: vec![2, 4],
            ..IndexDefinition::default()
        }],
        ..TableDefinition::default()
    }
}

fn blob_only_table() -> TableDefinition {
    TableDefinition {
        id: 9,
        name: "t".into(),
        columns: vec![
            column("id", SqlType::BigInt),
            column("anything_blob", SqlType::DYNAMIC),
        ],
        primary_key: vec![0],
        ..TableDefinition::default()
    }
}

fn damaged(bytes: &[u8]) -> bool {
    TableDefinition::decode("t", bytes).is_err_and(|error| error.state().code() == "XX001")
}

fn type_after(encoded: &[u8], name: &str) -> usize {
    let mut named = vec![name.len() as u8];
    named.extend_from_slice(name.as_bytes());
    encoded
        .windows(named.len())
        .position(|bytes| bytes == named.as_slice())
        .unwrap()
        + named.len()
}

#[test]
fn a_table_with_affinities_is_version_13_and_its_type_tags_match_their_vectors() {
    let table = affinity_table();
    let encoded = table.encode();
    assert_eq!(encoded[0], AFFINITY_FORMAT_VERSION);
    assert_eq!(AFFINITY_FORMAT_VERSION, 13);
    assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
    for (name, tag) in [
        ("body_text", &[25, 1][..]),
        ("amount_numeric", &[25, 2]),
        ("ratio_real", &[25, 3]),
        ("anything_blob", &[24, 0]),
    ] {
        let at = type_after(&encoded, name);
        assert_eq!(&encoded[at..at + tag.len()], tag, "{name}");
    }
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
    for code in [0, 4, 5, 0xFF] {
        let mut changed = encoded.clone();
        changed[type_after(&encoded, "body_text") + 1] = code;
        assert!(damaged(&changed), "affinity code {code}");
    }
}

#[test]
fn a_version_13_table_is_refused_by_name_by_older_readers_and_cannot_pass_as_older() {
    let mut encoded = affinity_table().encode();
    let mut older_reader = Reader::new(&encoded, "table definition");
    let error = older_reader
        .version_up_to(BASE_TYPES_FORMAT_VERSION)
        .unwrap_err();
    assert_eq!(error.state().code(), "0A000");
    assert!(error.message().contains("format version 13"), "{error}");
    for older in [DEFINED_TYPES_FORMAT_VERSION, BASE_TYPES_FORMAT_VERSION] {
        encoded[0] = older;
        assert!(damaged(&encoded), "claimed version {older}");
    }
}

#[test]
fn blob_columns_keep_writing_version_10_with_tag_24_as_v0_2_0_did() {
    let table = blob_only_table();
    let mut encoded = table.encode();
    assert_eq!(encoded[0], DEFINED_TYPES_FORMAT_VERSION);
    assert_eq!(encoded[type_after(&encoded, "anything_blob")], 24);
    assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
    encoded[0] = AFFINITY_FORMAT_VERSION;
    assert!(
        damaged(&encoded),
        "a version 13 table needs a column with an affinity"
    );
}

#[test]
fn a_dynamic_column_of_any_affinity_reads_every_storage_class_in_rows() {
    let values = vec![
        Value::BigInt(-4),
        Value::Double(2.5),
        Value::Text("x".into()),
        Value::Bytea(vec![0, 1]),
        Value::Null,
    ];
    let encoded = encode_row(&values);
    for sql_type in TYPES {
        let columns: Vec<Column> = (0..values.len())
            .map(|at| column(&format!("c{at}"), sql_type))
            .collect();
        assert_eq!(decode_row(&encoded, &columns), Ok(values.clone()));
    }
}
