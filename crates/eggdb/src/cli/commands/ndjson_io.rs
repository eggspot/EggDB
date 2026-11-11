use super::Session;
use super::parquet_io::{date_text, decimal_text, timestamp_text};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_parquet::{Cell, Column, ColumnType};
use eggdb_sql::json::{Json, parse_document};
use std::path::Path;

const MAX_NDJSON_BYTES: u64 = 4 << 30;
const MAX_COLUMNS: usize = 1600;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Nothing,
    Boolean,
    Integer,
    Double,
    Text,
}

fn widened(held: Seen, value: &Json) -> Seen {
    let found = match value {
        Json::Null => return held,
        Json::Boolean(_) => Seen::Boolean,
        Json::Number(text) if text.parse::<i64>().is_ok() => Seen::Integer,
        Json::Number(_) => Seen::Double,
        _ => Seen::Text,
    };
    match (held, found) {
        (Seen::Nothing, found) => found,
        (held, found) if held == found => held,
        (Seen::Integer, Seen::Double) | (Seen::Double, Seen::Integer) => Seen::Double,
        _ => Seen::Text,
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The columns of an NDJSON text: one object per line, columns in the order keys first
/// appear, each typed by every value it holds (integers, doubles, booleans, else text, with
/// nested objects and arrays kept as their JSON text).
pub(super) fn columns_of(text: &str) -> Result<Vec<Column>, String> {
    let mut names: Vec<String> = Vec::new();
    let mut kinds: Vec<Seen> = Vec::new();
    let mut rows: Vec<Vec<(usize, String, Json)>> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let document = parse_document(line)
            .map_err(|error| format!("line {}: {}", number + 1, error.message))?;
        let Json::Object(members) = document.value else {
            return Err(format!("line {}: each line must be a JSON object", number + 1));
        };
        let mut row = Vec::with_capacity(members.len());
        for (name, node) in members {
            let at = match names.iter().position(|known| *known == name) {
                Some(at) => at,
                None => {
                    if names.len() >= MAX_COLUMNS {
                        return Err(format!("more than {MAX_COLUMNS} distinct keys"));
                    }
                    names.push(name.clone());
                    kinds.push(Seen::Nothing);
                    names.len() - 1
                }
            };
            kinds[at] = widened(kinds[at], &node.value);
            let raw = line.get(node.start..node.end).unwrap_or_default().to_owned();
            row.push((at, raw, node.value));
        }
        rows.push(row);
    }
    let mut columns: Vec<Column> = names
        .into_iter()
        .zip(&kinds)
        .map(|(name, kind)| Column {
            name,
            column_type: match kind {
                Seen::Boolean => ColumnType::Boolean,
                Seen::Integer => ColumnType::Int64,
                Seen::Double => ColumnType::Double,
                Seen::Text | Seen::Nothing => ColumnType::Text,
            },
            nullable: true,
            cells: Vec::with_capacity(rows.len()),
        })
        .collect();
    for row in rows {
        let mut cells = vec![Cell::Null; columns.len()];
        for (at, raw, value) in row {
            cells[at] = match (columns[at].column_type, value) {
                (_, Json::Null) => Cell::Null,
                (ColumnType::Boolean, Json::Boolean(flag)) => Cell::Boolean(flag),
                (ColumnType::Int64, Json::Number(text)) => Cell::Int(text.parse().unwrap_or(0)),
                (ColumnType::Double, Json::Number(text)) => Cell::Float(text.parse().unwrap_or(0.0)),
                (_, Json::String(text)) => Cell::Bytes(text.into_bytes()),
                _ => Cell::Bytes(raw.into_bytes()),
            };
        }
        for (column, cell) in columns.iter_mut().zip(cells) {
            column.cells.push(cell);
        }
    }
    Ok(columns)
}

fn cell_json(column_type: ColumnType, cell: &Cell) -> String {
    match cell {
        Cell::Null => "null".to_owned(),
        Cell::Boolean(flag) => flag.to_string(),
        Cell::Int(value) => match column_type {
            ColumnType::Date => json_string(&date_text(*value)),
            ColumnType::Timestamp { utc } => json_string(&timestamp_text(*value, utc)),
            _ => value.to_string(),
        },
        Cell::Float(value) if value.is_finite() => value.to_string(),
        Cell::Float(value) => json_string(&value.to_string()),
        Cell::Decimal(unscaled) => {
            let scale = match column_type {
                ColumnType::Decimal { scale, .. } => scale,
                _ => 0,
            };
            decimal_text(*unscaled, scale)
        }
        Cell::Bytes(bytes) => match column_type {
            ColumnType::Text => json_string(&String::from_utf8_lossy(bytes)),
            _ => json_string(&format!(
                "\\x{}",
                bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            )),
        },
    }
}

/// The rows of `columns` as NDJSON, one object per line.
pub(super) fn ndjson_of(columns: &[Column]) -> String {
    let rows = columns.first().map_or(0, |column| column.cells.len());
    let mut out = String::new();
    for row in 0..rows {
        out.push('{');
        for (at, column) in columns.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            out.push_str(&json_string(&column.name));
            out.push(':');
            out.push_str(&cell_json(column.column_type, &column.cells[row]));
        }
        out.push_str("}\n");
    }
    out
}

impl Session<'_> {
    pub(super) fn import_ndjson(&mut self, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let size = std::fs::metadata(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?
            .len();
        if size > MAX_NDJSON_BYTES {
            return Err(usage(format!(
                "{shown} is larger than 4 GiB, the most eggdb import --ndjson reads at once"
            )));
        }
        let text = std::fs::read_to_string(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let columns = columns_of(&text).map_err(|error| {
            Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError)
        })?;
        if columns.is_empty() {
            return Err(Failure::database(
                format!("cannot import {shown}: it holds no JSON objects"),
                Exit::DatabaseError,
            ));
        }
        self.import_columns(&shown, &table, &columns)
    }

    pub(super) fn export_ndjson(&mut self, target: &Path) -> Result<Exit, Failure> {
        let shown = render::text(target.display().to_string().as_bytes());
        let (table, columns, rows) = match self.table_columns()? {
            Ok(found) => found,
            Err(exit) => return Ok(exit),
        };
        std::fs::write(target, ndjson_of(&columns)).map_err(|error| {
            Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError)
        })?;
        Ok(self.written(&table, &shown, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_types_widen_and_round_trip() {
        let columns = columns_of("{\"id\":1,\"score\":2,\"tags\":[\"a\"]}\n{\"id\":2,\"score\":2.5,\"ok\":true}\n").unwrap();
        let types: Vec<ColumnType> = columns.iter().map(|column| column.column_type).collect();
        assert_eq!(
            types,
            vec![ColumnType::Int64, ColumnType::Double, ColumnType::Text, ColumnType::Boolean]
        );
        let written = ndjson_of(&columns);
        assert!(written.starts_with("{\"id\":1,\"score\":2,\"tags\":\"[\\\"a\\\"]\",\"ok\":null}"), "{written}");
        assert!(columns_of("[1,2]").is_err());
    }
}
