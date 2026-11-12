use eggdb_parquet::{Cell, Column, ColumnType};
use eggdb_sql::{QueryResult, SqlType, Value};

const DAYS_FROM_1970_TO_2000: i64 = 10_957;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn column_type(sql_type: Option<&SqlType>) -> ColumnType {
    match sql_type {
        Some(SqlType::Boolean) => ColumnType::Boolean,
        Some(SqlType::SmallInt | SqlType::Integer) => ColumnType::Int32,
        Some(SqlType::BigInt) => ColumnType::Int64,
        Some(SqlType::Real) => ColumnType::Float,
        Some(SqlType::Double) => ColumnType::Double,
        Some(SqlType::Date) => ColumnType::Date,
        Some(SqlType::Timestamp(_)) => ColumnType::Timestamp { utc: false },
        Some(SqlType::TimestampTz(_)) => ColumnType::Timestamp { utc: true },
        Some(SqlType::Bytea) => ColumnType::Binary,
        _ => ColumnType::Text,
    }
}

fn cell(column_type: ColumnType, value: Value) -> Cell {
    match (column_type, value) {
        (_, Value::Null) => Cell::Null,
        (ColumnType::Boolean, Value::Boolean(flag)) => Cell::Boolean(flag),
        (ColumnType::Int32 | ColumnType::Int64, Value::SmallInt(number)) => Cell::Int(i64::from(number)),
        (ColumnType::Int32 | ColumnType::Int64, Value::Integer(number)) => Cell::Int(i64::from(number)),
        (ColumnType::Int64, Value::BigInt(number)) => Cell::Int(number),
        (ColumnType::Float | ColumnType::Double, Value::Real(number)) => Cell::Float(f64::from(number)),
        (ColumnType::Float | ColumnType::Double, Value::Double(number)) => Cell::Float(number),
        (ColumnType::Date, Value::Date(days)) => Cell::Int(i64::from(days) + DAYS_FROM_1970_TO_2000),
        (ColumnType::Timestamp { .. }, Value::Timestamp(micros) | Value::TimestampTz(micros)) => {
            Cell::Int(micros.saturating_add(DAYS_FROM_1970_TO_2000 * MICROS_PER_DAY))
        }
        (ColumnType::Binary, Value::Bytea(bytes)) => Cell::Bytes(bytes),
        (ColumnType::Text, other) => Cell::Bytes(other.to_text().unwrap_or_default().into_bytes()),
        (_, _) => Cell::Null,
    }
}

/// A result's rows as Arrow columns, typed by the result's column types (text for the rest).
pub fn columns_of(result: QueryResult) -> Vec<Column> {
    let described = result.columns.unwrap_or_default();
    let types: Vec<ColumnType> = described.iter().map(|column| column_type(column.sql_type.as_ref())).collect();
    let mut cells: Vec<Vec<Cell>> = types.iter().map(|_| Vec::with_capacity(result.rows.len())).collect();
    for row in result.rows {
        for (at, value) in row.into_iter().enumerate() {
            if let (Some(column), Some(&kind)) = (cells.get_mut(at), types.get(at)) {
                column.push(cell(kind, value));
            }
        }
    }
    described
        .into_iter()
        .zip(types)
        .zip(cells)
        .map(|((column, column_type), cells)| Column {
            name: column.name,
            column_type,
            nullable: true,
            cells,
        })
        .collect()
}

/// Text columns of the given names and rows, for the catalog commands.
pub(crate) fn text_columns(names: &[&str], rows: &[Vec<Option<String>>]) -> Vec<Column> {
    names
        .iter()
        .enumerate()
        .map(|(at, name)| Column {
            name: (*name).to_owned(),
            column_type: ColumnType::Text,
            nullable: true,
            cells: rows
                .iter()
                .map(|row| match row.get(at).cloned().flatten() {
                    Some(text) => Cell::Bytes(text.into_bytes()),
                    None => Cell::Null,
                })
                .collect(),
        })
        .collect()
}
