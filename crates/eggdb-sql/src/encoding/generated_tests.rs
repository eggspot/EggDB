use super::{
    AFFINITY_FORMAT_VERSION, Column, GENERATED_COLUMNS_FORMAT_VERSION, GeneratedColumn,
    TableDefinition, generated_without_column,
};
use crate::types::SqlType;

fn column(name: &str) -> Column {
    Column {
        name: name.into(),
        sql_type: SqlType::Integer,
        not_null: false,
        default: None,
        auto_increment: false,
    }
}

fn generated_table() -> TableDefinition {
    TableDefinition {
        id: 4,
        name: "t".into(),
        columns: vec![column("a"), column("b"), column("total"), column("doubled")],
        generated: vec![
            GeneratedColumn {
                column: 2,
                dialect: crate::Dialect::PostgreSql,
                expression: "a + b".into(),
                stored: true,
            },
            GeneratedColumn {
                column: 3,
                dialect: crate::Dialect::MySql,
                expression: "`a` * 2".into(),
                stored: false,
            },
        ],
        ..TableDefinition::default()
    }
}

fn damaged(bytes: &[u8]) -> bool {
    TableDefinition::decode("t", bytes).is_err_and(|error| error.state().code() == "XX001")
}

#[test]
fn a_table_with_generated_columns_is_version_14_and_round_trips() {
    let table = generated_table();
    let encoded = table.encode();
    assert_eq!(encoded[0], GENERATED_COLUMNS_FORMAT_VERSION);
    assert_eq!(GENERATED_COLUMNS_FORMAT_VERSION, 14);
    assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
}

#[test]
fn a_table_without_generated_columns_keeps_its_older_version() {
    let mut table = generated_table();
    table.generated.clear();
    let encoded = table.encode();
    assert!(encoded[0] < GENERATED_COLUMNS_FORMAT_VERSION);
    assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
}

#[test]
fn a_record_of_the_next_version_is_refused_by_its_version() {
    let mut encoded = generated_table().encode();
    encoded[0] = 17;
    let error = TableDefinition::decode("t", &encoded).unwrap_err();
    assert_eq!(error.state().code(), "0A000");
    assert!(error.message().contains("format version 17"), "{error}");
}

#[test]
fn a_damaged_generated_section_is_refused() {
    let encoded = generated_table().encode();
    for length in 1..encoded.len() {
        assert!(
            TableDefinition::decode("t", &encoded[..length]).is_err(),
            "{length}"
        );
    }
    let mut lowered = encoded.clone();
    lowered[0] = AFFINITY_FORMAT_VERSION;
    assert!(damaged(&lowered));
    let mut unknown_kind = encoded.clone();
    let kind_at = unknown_kind.len() - (1 + 1 + "`a` * 2".len()) - 1;
    unknown_kind[kind_at] = 9;
    assert!(damaged(&unknown_kind));
}

#[test]
fn a_generated_column_with_a_default_or_out_of_range_is_refused() {
    let mut table = generated_table();
    table.columns[2].default = Some("1".into());
    assert!(damaged(&table.encode()));
    let mut table = generated_table();
    table.generated[1].column = 9;
    assert!(damaged(&table.encode()));
    let mut table = generated_table();
    table.generated.swap(0, 1);
    assert!(damaged(&table.encode()));
}

#[test]
fn dropping_a_column_shifts_the_generated_positions() {
    let table = generated_table();
    let shifted = generated_without_column(&table.generated, 0);
    assert_eq!(
        shifted.iter().map(|entry| entry.column).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let without = generated_without_column(&table.generated, 2);
    assert_eq!(without.len(), 1);
    assert_eq!(without[0].column, 2);
}

#[test]
fn every_changed_byte_of_a_version_14_record_is_an_error_or_a_clean_decode() {
    let encoded = generated_table().encode();
    let mut plain = generated_table();
    plain.generated.clear();
    let section_start = plain.encode().len().saturating_sub(1);
    for at in (0..encoded.len()).filter(|&at| at == 0 || at >= section_start) {
        for value in [0u8, 1, 2, 3, 4, 7, 8, 13, 14, 15, 0x7F, 0x80, 0xFE, 0xFF] {
            let mut changed = encoded.clone();
            changed[at] = value;
            let Ok(decoded) = TableDefinition::decode("t", &changed) else {
                continue;
            };
            for entry in &decoded.generated {
                assert!(entry.column < decoded.columns.len(), "{at}: {value}");
                assert!(!entry.expression.is_empty(), "{at}: {value}");
            }
            assert_eq!(
                TableDefinition::decode("t", &decoded.encode()).as_ref(),
                Ok(&decoded),
                "{at}: {value}"
            );
        }
    }
}

const PINNED_VERSION_14: &[u8] = &[
    14, 4, 0, 0, 0, 4, 1, 97, 2, 0, 0, 1, 98, 2, 0, 0, 5, 116, 111, 116, 97, 108, 2, 0, 0, 7, 100,
    111, 117, 98, 108, 101, 100, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 1, 0, 5, 97, 32, 43, 32,
    98, 3, 0, 1, 7, 96, 97, 96, 32, 42, 32, 50,
];

const PINNED_WITHOUT_GENERATED: &[u8] = &[
    1, 4, 0, 0, 0, 4, 1, 97, 2, 0, 0, 1, 98, 2, 0, 0, 5, 116, 111, 116, 97, 108, 2, 0, 0, 7, 100,
    111, 117, 98, 108, 101, 100, 2, 0, 0, 0,
];

#[test]
fn a_pinned_version_14_record_decodes_and_encodes_byte_for_byte() {
    let table = generated_table();
    assert_eq!(table.encode(), PINNED_VERSION_14);
    assert_eq!(TableDefinition::decode("t", PINNED_VERSION_14), Ok(table));
}

#[test]
fn a_table_without_generated_columns_keeps_its_pinned_older_bytes() {
    let mut table = generated_table();
    table.generated.clear();
    assert_eq!(table.encode(), PINNED_WITHOUT_GENERATED);
    assert!(PINNED_WITHOUT_GENERATED[0] < GENERATED_COLUMNS_FORMAT_VERSION);
    assert_eq!(
        TableDefinition::decode("t", PINNED_WITHOUT_GENERATED),
        Ok(table)
    );
}
