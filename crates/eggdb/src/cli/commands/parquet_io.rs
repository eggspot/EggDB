use super::copy_io::StreamCopy;
use super::sql::{exit_for, print_error};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_parquet::{Cell, Column, ColumnType};
use eggdb_sql::{QueryResult, SqlType, Value};
use std::cell::Cell as Flag;
use std::path::Path;

const DAYS_FROM_1970_TO_2000: i64 = 10_957;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const MAX_PARQUET_BYTES: u64 = 4 << 30;

fn quoted_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn date_text(days_since_1970: i64) -> String {
    let (year, month, day) = civil_from_days(days_since_1970);
    format!("{year:04}-{month:02}-{day:02}")
}

fn timestamp_text(micros: i64, utc: bool) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let seconds = of_day / 1_000_000;
    format!(
        "{} {:02}:{:02}:{:02}.{:06}{}",
        date_text(days),
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60,
        of_day % 1_000_000,
        if utc { "+00" } else { "" }
    )
}

fn decimal_text(unscaled: i128, scale: u32) -> String {
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let sign = if unscaled < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

fn sql_type_of(column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Boolean => "boolean".to_owned(),
        ColumnType::Int32 => "integer".to_owned(),
        ColumnType::Int64 => "bigint".to_owned(),
        ColumnType::Float => "real".to_owned(),
        ColumnType::Double => "double precision".to_owned(),
        ColumnType::Text => "text".to_owned(),
        ColumnType::Binary => "bytea".to_owned(),
        ColumnType::Date => "date".to_owned(),
        ColumnType::Timestamp { utc: true } => "timestamptz".to_owned(),
        ColumnType::Timestamp { utc: false } => "timestamp".to_owned(),
        ColumnType::Decimal { precision, scale } => format!("numeric({}, {})", precision.max(1), scale.min(precision.max(1))),
    }
}

fn csv_field(out: &mut String, column_type: ColumnType, cell: &Cell) {
    let quoted = |out: &mut String, text: &str| {
        out.push('"');
        out.push_str(&text.replace('"', "\"\""));
        out.push('"');
    };
    match cell {
        Cell::Null => {}
        Cell::Boolean(flag) => out.push(if *flag { 't' } else { 'f' }),
        Cell::Int(value) => match column_type {
            ColumnType::Date => out.push_str(&date_text(*value)),
            ColumnType::Timestamp { utc } => out.push_str(&timestamp_text(*value, utc)),
            _ => out.push_str(&value.to_string()),
        },
        Cell::Float(value) if value.is_nan() => out.push_str("NaN"),
        Cell::Float(value) if value.is_infinite() => out.push_str(if *value > 0.0 { "Infinity" } else { "-Infinity" }),
        Cell::Float(value) => out.push_str(&value.to_string()),
        Cell::Decimal(unscaled) => {
            let scale = match column_type {
                ColumnType::Decimal { scale, .. } => scale,
                _ => 0,
            };
            out.push_str(&decimal_text(*unscaled, scale));
        }
        Cell::Bytes(bytes) => match column_type {
            ColumnType::Text => quoted(out, &String::from_utf8_lossy(bytes)),
            _ => {
                let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                quoted(out, &format!("\\x{hex}"));
            }
        },
    }
}

fn csv_of(columns: &[Column]) -> String {
    let rows = columns.first().map_or(0, |column| column.cells.len());
    let mut out = String::new();
    for row in 0..rows {
        for (at, column) in columns.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            csv_field(&mut out, column.column_type, &column.cells[row]);
        }
        out.push('\n');
    }
    out
}

fn column_of(name: &str, sql_type: Option<SqlType>, values: Vec<Value>) -> Column {
    let column_type = match sql_type {
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
    };
    let cells = values
        .into_iter()
        .map(|value| match (column_type, value) {
            (_, Value::Null) => Cell::Null,
            (_, Value::Boolean(flag)) => Cell::Boolean(flag),
            (_, Value::SmallInt(number)) => Cell::Int(i64::from(number)),
            (_, Value::Integer(number)) => Cell::Int(i64::from(number)),
            (_, Value::BigInt(number)) => Cell::Int(number),
            (_, Value::Real(number)) => Cell::Float(f64::from(number)),
            (_, Value::Double(number)) => Cell::Float(number),
            (ColumnType::Date, Value::Date(days)) => Cell::Int(i64::from(days) + DAYS_FROM_1970_TO_2000),
            (ColumnType::Timestamp { .. }, Value::Timestamp(micros) | Value::TimestampTz(micros)) => {
                Cell::Int(micros.saturating_add(DAYS_FROM_1970_TO_2000 * MICROS_PER_DAY))
            }
            (ColumnType::Binary, Value::Bytea(bytes)) => Cell::Bytes(bytes),
            (_, other) => Cell::Bytes(other.to_text().unwrap_or_default().into_bytes()),
        })
        .collect();
    Column { name: name.to_owned(), column_type, nullable: true, cells }
}

impl Session<'_> {
    pub(super) fn import_parquet(&mut self, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let size = std::fs::metadata(source).map_err(|error| usage(format!("cannot read {shown}: {error}")))?.len();
        if size > MAX_PARQUET_BYTES {
            return Err(usage(format!("{shown} is larger than 4 GiB, the most eggdb import --parquet reads at once")));
        }
        let bytes = std::fs::read(source).map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let columns = eggdb_parquet::read(&bytes)
            .map_err(|error| Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError))?;
        let rows = columns.first().map_or(0, |column| column.cells.len()) as u64;
        let definitions: Vec<String> = columns
            .iter()
            .map(|column| format!("{} {}", quoted_name(&column.name), sql_type_of(column.column_type)))
            .collect();
        let names: Vec<String> = columns.iter().map(|column| quoted_name(&column.name)).collect();
        let create = format!("CREATE TABLE IF NOT EXISTS {table} ({})", definitions.join(", "));
        let copy = format!("COPY {table} ({}) FROM STDIN (FORMAT csv)", names.join(", "));
        let csv = csv_of(&columns);
        let db = self.open(Access::Write { create: self.request.create }, None)?;
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::new(&store);
        let copied_out = Flag::new(false);
        let mut input = csv.as_bytes();
        let outcome = session.execute("BEGIN", &mut |_| {}).and_then(|()| session.execute(&create, &mut |_| {})).and_then(|()| {
            let mut channel = StreamCopy { input: Some(&mut input), output: None, copied_out: &copied_out };
            session.execute_with_copy(&copy, &mut |_: QueryResult| true, &mut channel)
        });
        let outcome = match outcome {
            Ok(()) => session.execute("COMMIT", &mut |_| {}),
            Err(error) => {
                let _ = session.execute("ROLLBACK", &mut |_| {});
                Err(error)
            }
        };
        let closed = session.close();
        let exit = match outcome.and(closed.map(|_| ())) {
            Ok(()) => Exit::Done,
            Err(error) => {
                print_error(&mut self.messages, &error, &copy);
                self.messages.text("Nothing was imported: the rows load in one transaction.\n");
                exit_for(&error)
            }
        };
        self.close(db)?;
        if exit != Exit::Done {
            return Ok(exit);
        }
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new().string("file", &self.file_display).string("from", &shown).string("table", &table).number("rows", rows).line(),
            ),
            _ => self.out.text(&format!("imported {} rows from {shown} into {table} in {}\n", render::count(rows), self.file)),
        }
        Ok(Exit::Done)
    }

    pub(super) fn export_parquet(&mut self, target: &Path) -> Result<Exit, Failure> {
        let shown = render::text(target.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let query = format!("SELECT * FROM {table}");
        let db = self.open(Access::ReadOnly, None)?;
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::new(&store);
        let mut found = None;
        let outcome = session.execute(&query, &mut |result| found = Some(result));
        let _ = session.close();
        if let Err(error) = outcome {
            print_error(&mut self.messages, &error, &query);
            self.close(db)?;
            return Ok(exit_for(&error));
        }
        self.close(db)?;
        let (described, found_rows) = found.map_or((None, Vec::new()), |result: QueryResult| (result.columns, result.rows));
        let names: Vec<(String, Option<SqlType>)> =
            described.unwrap_or_default().into_iter().map(|column| (column.name, column.sql_type)).collect();
        let rows = found_rows.len() as u64;
        let mut by_column: Vec<Vec<Value>> = vec![Vec::with_capacity(found_rows.len()); names.len()];
        for row in found_rows {
            for (slot, value) in by_column.iter_mut().zip(row) {
                slot.push(value);
            }
        }
        let columns: Vec<Column> = names.iter().zip(by_column).map(|((name, sql_type), values)| column_of(name, *sql_type, values)).collect();
        let bytes = eggdb_parquet::write(&columns)
            .map_err(|error| Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError))?;
        std::fs::write(target, bytes)
            .map_err(|error| Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError))?;
        match self.format {
            Format::Json => self.out.text(&JsonObject::new().string("table", &table).string("to", &shown).number("rows", rows).line()),
            _ => self.out.text(&format!("exported {} rows of {table} to {shown}\n", render::count(rows))),
        }
        Ok(Exit::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::{date_text, decimal_text, timestamp_text};

    #[test]
    fn values_render_as_postgresql_reads_them() {
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(date_text(19_783), "2024-03-01");
        assert_eq!(date_text(-1), "1969-12-31");
        assert_eq!(timestamp_text(1_000_001, true), "1970-01-01 00:00:01.000001+00");
        assert_eq!(decimal_text(12_345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
    }
}
