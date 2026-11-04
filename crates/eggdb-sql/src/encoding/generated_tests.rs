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
fn an_older_reader_refuses_a_version_14_record_by_its_version() {
    let mut encoded = generated_table().encode();
    encoded[0] = GENERATED_COLUMNS_FORMAT_VERSION + 1;
    let error = TableDefinition::decode("t", &encoded).unwrap_err();
    assert_eq!(error.state().code(), "0A000");
    assert!(error.message().contains("format version 15"), "{error}");
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
