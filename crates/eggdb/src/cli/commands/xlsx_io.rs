use super::Session;
use super::parquet_io::{date_text, decimal_text, timestamp_text};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_content::{SheetCell, read_sheet, write_sheet};
use eggdb_parquet::{Cell, Column, ColumnType};
use std::path::Path;

const MAX_XLSX_BYTES: u64 = 1 << 30;
const MAX_XLSX_PARTS_BYTES: usize = 2 << 30;
const MAX_COLUMNS: usize = 1600;
const MICROS_PER_DAY: i64 = 86_400_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Nothing,
    Boolean,
    Integer,
    Double,
    Date,
    Timestamp,
    Text,
}

fn kind_of(cell: &SheetCell) -> Seen {
    match cell {
        SheetCell::Empty => Seen::Nothing,
        SheetCell::Boolean(_) => Seen::Boolean,
        SheetCell::Number(text) if text.parse::<i64>().is_ok() => Seen::Integer,
        SheetCell::Number(_) => Seen::Double,
        SheetCell::Date(_) => Seen::Date,
        SheetCell::Timestamp(_) => Seen::Timestamp,
        SheetCell::Text(_) => Seen::Text,
    }
}

fn widened(held: Seen, found: Seen) -> Seen {
    match (held, found) {
        (held, Seen::Nothing) => held,
        (Seen::Nothing, found) => found,
        (held, found) if held == found => held,
        (Seen::Integer, Seen::Double) | (Seen::Double, Seen::Integer) => Seen::Double,
        (Seen::Date, Seen::Timestamp) | (Seen::Timestamp, Seen::Date) => Seen::Timestamp,
        _ => Seen::Text,
    }
}

fn sheet_text(cell: &SheetCell) -> String {
    match cell {
        SheetCell::Empty => String::new(),
        SheetCell::Number(text) | SheetCell::Text(text) => text.clone(),
        SheetCell::Boolean(flag) => flag.to_string(),
        SheetCell::Date(days) => date_text(*days),
        SheetCell::Timestamp(micros) => timestamp_text(*micros, false),
    }
}

fn table_cell(kind: Seen, cell: &SheetCell) -> Cell {
    match (kind, cell) {
        (_, SheetCell::Empty) => Cell::Null,
        (Seen::Boolean, SheetCell::Boolean(flag)) => Cell::Boolean(*flag),
        (Seen::Integer, SheetCell::Number(text)) => Cell::Int(text.parse().unwrap_or(0)),
        (Seen::Double, SheetCell::Number(text)) => Cell::Float(text.parse().unwrap_or(0.0)),
        (Seen::Date, SheetCell::Date(days)) => Cell::Int(*days),
        (Seen::Timestamp, SheetCell::Date(days)) => Cell::Int(days.saturating_mul(MICROS_PER_DAY)),
        (Seen::Timestamp, SheetCell::Timestamp(micros)) => Cell::Int(*micros),
        (_, other) => Cell::Bytes(sheet_text(other).into_bytes()),
    }
}

fn header_names(header: &[SheetCell], width: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(width);
    for at in 0..width {
        let given = header.get(at).map(sheet_text).unwrap_or_default();
        let given = given.trim();
        let base = if given.is_empty() {
            format!("column{}", at + 1)
        } else {
            given.to_owned()
        };
        let mut name = base.clone();
        let mut suffix = 2;
        while names.iter().any(|known| known.eq_ignore_ascii_case(&name)) {
            name = format!("{base}_{suffix}");
            suffix += 1;
        }
        names.push(name);
    }
    names
}

/// The columns of a worksheet's rows: the first row names them (a blank heading becomes
/// `column<n>`, a repeated one gains `_2`, `_3`), every later row that holds a value is a table
/// row, and each column is typed by every value it holds (booleans, integers, doubles, dates,
/// timestamps, else text).
pub(super) fn columns_of(rows: &[Vec<SheetCell>]) -> Result<Vec<Column>, String> {
    let Some((header, body)) = rows.split_first() else {
        return Ok(Vec::new());
    };
    let body: Vec<&Vec<SheetCell>> = body
        .iter()
        .filter(|row| row.iter().any(|cell| *cell != SheetCell::Empty))
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width > MAX_COLUMNS {
        return Err(format!("it has {width} columns; a table holds at most {MAX_COLUMNS}"));
    }
    let mut kinds = vec![Seen::Nothing; width];
    for row in &body {
        for (at, cell) in row.iter().enumerate() {
            kinds[at] = widened(kinds[at], kind_of(cell));
        }
    }
    Ok(header_names(header, width)
        .into_iter()
        .zip(&kinds)
        .enumerate()
        .map(|(at, (name, kind))| Column {
            name,
            column_type: match kind {
                Seen::Boolean => ColumnType::Boolean,
                Seen::Integer => ColumnType::Int64,
                Seen::Double => ColumnType::Double,
                Seen::Date => ColumnType::Date,
                Seen::Timestamp => ColumnType::Timestamp { utc: false },
                Seen::Text | Seen::Nothing => ColumnType::Text,
            },
            nullable: true,
            cells: body
                .iter()
                .map(|row| table_cell(*kind, row.get(at).unwrap_or(&SheetCell::Empty)))
                .collect(),
        })
        .collect())
}

fn sheet_cell(column_type: ColumnType, cell: &Cell) -> SheetCell {
    match cell {
        Cell::Null => SheetCell::Empty,
        Cell::Boolean(flag) => SheetCell::Boolean(*flag),
        Cell::Int(value) => match column_type {
            ColumnType::Date => SheetCell::Date(*value),
            ColumnType::Timestamp { .. } => SheetCell::Timestamp(*value),
            _ => SheetCell::Number(value.to_string()),
        },
        Cell::Float(value) if value.is_finite() => SheetCell::Number(value.to_string()),
        Cell::Float(value) => SheetCell::Text(value.to_string()),
        Cell::Decimal(unscaled) => {
            let scale = match column_type {
                ColumnType::Decimal { scale, .. } => scale,
                _ => 0,
            };
            SheetCell::Number(decimal_text(*unscaled, scale))
        }
        Cell::Bytes(bytes) => match column_type {
            ColumnType::Text => SheetCell::Text(String::from_utf8_lossy(bytes).into_owned()),
            _ => SheetCell::Text(format!(
                "\\x{}",
                bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            )),
        },
    }
}

/// A heading row of column names, then one row per table row.
pub(super) fn sheet_rows(columns: &[Column]) -> Vec<Vec<SheetCell>> {
    let count = columns.first().map_or(0, |column| column.cells.len());
    let mut rows = Vec::with_capacity(count + 1);
    rows.push(
        columns
            .iter()
            .map(|column| SheetCell::Text(column.name.clone()))
            .collect(),
    );
    for row in 0..count {
        rows.push(
            columns
                .iter()
                .map(|column| sheet_cell(column.column_type, &column.cells[row]))
                .collect(),
        );
    }
    rows
}

impl Session<'_> {
    pub(super) fn import_xlsx(&mut self, source: &Path) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let shown = render::text(source.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        let size = std::fs::metadata(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?
            .len();
        if size > MAX_XLSX_BYTES {
            return Err(usage(format!(
                "{shown} is larger than 1 GiB, the most eggdb import --xlsx reads at once"
            )));
        }
        let bytes = std::fs::read(source)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let failed =
            |error: String| Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError);
        let rows = read_sheet(&bytes, MAX_XLSX_PARTS_BYTES).map_err(|error| failed(error.0))?;
        let columns = columns_of(&rows).map_err(failed)?;
        if columns.is_empty() {
            return Err(failed("its first worksheet is empty".to_owned()));
        }
        self.import_columns(&shown, &table, &columns)
    }

    pub(super) fn export_xlsx(&mut self, target: &Path) -> Result<Exit, Failure> {
        let shown = render::text(target.display().to_string().as_bytes());
        let (table, columns, rows) = match self.table_columns()? {
            Ok(found) => found,
            Err(exit) => return Ok(exit),
        };
        let file = write_sheet(&table, &sheet_rows(&columns)).map_err(|error| {
            Failure::database(format!("cannot write {shown}: {}", error.0), Exit::DatabaseError)
        })?;
        std::fs::write(target, file).map_err(|error| {
            Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError)
        })?;
        Ok(self.written(&table, &shown, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_columns_take_headings_and_widen_types() {
        let rows = vec![
            vec![SheetCell::Text("id".into()), SheetCell::Empty, SheetCell::Text("ID".into())],
            vec![SheetCell::Number("1".into()), SheetCell::Date(10), SheetCell::Text("x".into())],
            vec![SheetCell::Empty, SheetCell::Empty, SheetCell::Empty],
            vec![SheetCell::Number("2.5".into()), SheetCell::Timestamp(5), SheetCell::Number("3".into())],
        ];
        let columns = columns_of(&rows).unwrap();
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(names, vec!["id", "column2", "ID_2"]);
        let types: Vec<ColumnType> = columns.iter().map(|column| column.column_type).collect();
        assert_eq!(
            types,
            vec![ColumnType::Double, ColumnType::Timestamp { utc: false }, ColumnType::Text]
        );
        assert_eq!(columns[0].cells.len(), 2);
        assert_eq!(columns[1].cells[0], Cell::Int(10 * MICROS_PER_DAY));
        assert_eq!(columns[2].cells[1], Cell::Bytes(b"3".to_vec()));
        let back = sheet_rows(&columns);
        assert_eq!(back[0][2], SheetCell::Text("ID_2".into()));
        assert_eq!(back[2][0], SheetCell::Number("2.5".into()));
    }
}
