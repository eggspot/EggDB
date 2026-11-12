use crate::error::{SqlError, SqlResult, SqlState};
use crate::json_functions::RecordSink;
use crate::types::{Conversion, SqlType, Value, convert};
use eggdb_parquet::{Cell, Column, ColumnType};

const MOST_FILE_BYTES: usize = 1 << 30;
const DAYS_1970_TO_2000: i64 = 10_957;
const MICROS_1970_TO_2000: i64 = 946_684_800_000_000;

fn decimal_text(unscaled: i128, scale: u32) -> String {
    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let text = if scale == 0 {
        digits
    } else if digits.len() > scale {
        format!("{}.{}", &digits[..digits.len() - scale], &digits[digits.len() - scale..])
    } else {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    };
    if negative { format!("-{text}") } else { text }
}

fn value_of(column: &Column, cell: &Cell) -> Value {
    match (cell, column.column_type) {
        (Cell::Null, _) => Value::Null,
        (Cell::Boolean(flag), _) => Value::Boolean(*flag),
        (Cell::Int(days), ColumnType::Date) => i32::try_from(*days - DAYS_1970_TO_2000).map_or(Value::Null, Value::Date),
        (Cell::Int(micros), ColumnType::Timestamp { utc: true }) => Value::TimestampTz(micros - MICROS_1970_TO_2000),
        (Cell::Int(micros), ColumnType::Timestamp { utc: false }) => Value::Timestamp(micros - MICROS_1970_TO_2000),
        (Cell::Int(whole), _) => Value::BigInt(*whole),
        (Cell::Float(number), _) => Value::Double(*number),
        (Cell::Bytes(bytes), ColumnType::Text) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        (Cell::Bytes(bytes), _) => Value::Bytea(bytes.clone()),
        (Cell::Decimal(unscaled), ColumnType::Decimal { scale, .. }) => Value::Text(decimal_text(*unscaled, scale)),
        (Cell::Decimal(unscaled), _) => Value::Text(unscaled.to_string()),
    }
}

/// The rows of `read_parquet(path) AS t(name type, ...)`: the file read through the session's
/// file reader, each declared column taken from the file's column of that name (matched
/// exactly, then without regard to case; `NULL` when the file has none) and cast to its type.
pub(crate) fn rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let Some(path) = values.first() else {
        return Err(crate::json_to_record::definition_list_required());
    };
    let Some(path) = path.to_text() else {
        return Ok(());
    };
    let bytes = crate::new_files::read_file(&path, MOST_FILE_BYTES)?;
    let columns = eggdb_parquet::read(&bytes).map_err(|error| SqlError::new(SqlState::DATA_CORRUPTED, format!("{path}: {}", error.0)))?;
    let chosen: Vec<Option<&Column>> = names
        .iter()
        .map(|name| {
            columns
                .iter()
                .find(|column| column.name == *name)
                .or_else(|| columns.iter().find(|column| column.name.eq_ignore_ascii_case(name)))
        })
        .collect();
    let count = columns.first().map_or(0, |column| column.cells.len());
    crate::memory::check_value_bytes(bytes.len())?;
    for row in 0..count {
        let mut record = Vec::with_capacity(names.len());
        for (at, column) in chosen.iter().enumerate() {
            let value = column
                .and_then(|column| column.cells.get(row).map(|cell| value_of(column, cell)))
                .unwrap_or(Value::Null);
            let declared = types.get(at + 1).copied().flatten();
            record.push(match declared {
                Some(sql_type) if !value.is_null() && value.sql_type() != Some(sql_type) => {
                    convert(value, sql_type, Conversion::Explicit)?
                }
                _ => value,
            });
        }
        sink(record)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::memory_store::MemoryStore;
    use crate::{Dialect, Session};

    struct MemoryFile;

    static FILE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static READER: MemoryFile = MemoryFile;

    impl crate::new_files::FileReader for MemoryFile {
        fn read_file(&self, path: &str, _most_bytes: usize) -> Result<Vec<u8>, String> {
            if path != "people.parquet" {
                return Err(format!("{path}: no such file"));
            }
            Ok(FILE.get_or_init(|| {
                eggdb_parquet::write(&[
                    Column {
                        name: "id".to_owned(),
                        column_type: ColumnType::Int64,
                        nullable: false,
                        cells: vec![Cell::Int(1), Cell::Int(2)],
                    },
                    Column {
                        name: "Name".to_owned(),
                        column_type: ColumnType::Text,
                        nullable: true,
                        cells: vec![Cell::Bytes(b"egg".to_vec()), Cell::Null],
                    },
                ])
                .expect("writes")
            })
            .clone())
        }
    }

    #[test]
    fn read_parquet_reads_declared_columns_through_the_session_reader() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::PostgreSql);
        let query = "SELECT id, name, missing FROM read_parquet('people.parquet') AS t(id int, name text, missing text) ORDER BY id";
        assert!(session.execute(query, &mut |_| {}).is_err(), "a session without a reader reads no file");
        session.set_file_reader(&READER);
        let mut rows = Vec::new();
        session.execute(query, &mut |result| rows.extend(result.rows)).expect("reads");
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Text("egg".to_owned()), Value::Null],
                vec![Value::Integer(2), Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn decimals_keep_their_scale() {
        assert_eq!(decimal_text(12345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
    }
}
