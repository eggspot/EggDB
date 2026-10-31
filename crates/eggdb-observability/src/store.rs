use crate::line_protocol::{FieldValue, Point};
use eggdb_sql::{KeyValueStore, QueryResult, Session, SqlError, Value};
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "metrics";
const ROWS_PER_INSERT: usize = 500;
const UNIX_EPOCH_FROM_2000_MICROS: i64 = 946_684_800_000_000;

pub(crate) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn table(measurement: &str) -> String {
    format!("{SCHEMA}.{}", identifier(measurement))
}

pub(crate) fn timestamp_literal(unix_micros: i64) -> String {
    let text = Value::TimestampTz(unix_micros.saturating_sub(UNIX_EPOCH_FROM_2000_MICROS)).to_text().unwrap_or_default();
    format!("{}::timestamptz", literal(&text))
}

pub(crate) fn run<S: KeyValueStore>(session: &mut Session<'_, S>, sql: &str) -> Result<Vec<QueryResult>, String> {
    let mut results = Vec::new();
    session.execute(sql, &mut |result| results.push(result)).map_err(|error: SqlError| error.message().to_owned())?;
    Ok(results)
}

fn rows<S: KeyValueStore>(session: &mut Session<'_, S>, sql: &str) -> Result<QueryResult, String> {
    run(session, sql)?.into_iter().rev().find(|result| result.columns.is_some()).ok_or_else(|| "the query returned no rows".to_owned())
}

/// Every column of every metrics table: table, column and `information_schema` data type.
pub(crate) fn catalog<S: KeyValueStore>(session: &mut Session<'_, S>) -> Result<BTreeMap<String, Vec<(String, String)>>, String> {
    let result = rows(
        session,
        &format!("SELECT table_name, column_name, data_type FROM information_schema.columns WHERE table_schema = {} ORDER BY table_name, ordinal_position", literal(SCHEMA)),
    )?;
    let mut tables: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for row in &result.rows {
        let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
        tables.entry(text(0)).or_default().push((text(1), text(2)));
    }
    Ok(tables)
}

fn sql_type(value: &FieldValue) -> &'static str {
    match value {
        FieldValue::Float(_) => "double precision",
        FieldValue::Integer(_) => "bigint",
        FieldValue::Unsigned(_) => "numeric",
        FieldValue::Text(_) => "text",
        FieldValue::Boolean(_) => "boolean",
    }
}

fn fits(existing: &str, value: &FieldValue) -> bool {
    matches!(
        (existing, value),
        ("double precision", FieldValue::Float(_) | FieldValue::Integer(_) | FieldValue::Unsigned(_))
            | ("bigint", FieldValue::Integer(_))
            | ("numeric", FieldValue::Integer(_) | FieldValue::Unsigned(_))
            | ("text", FieldValue::Text(_))
            | ("boolean", FieldValue::Boolean(_))
    )
}

fn value_sql(value: &FieldValue) -> String {
    match value {
        FieldValue::Float(number) => format!("{number:?}::double precision"),
        FieldValue::Integer(number) => number.to_string(),
        FieldValue::Unsigned(number) => format!("{number}::numeric"),
        FieldValue::Text(text) => literal(text),
        FieldValue::Boolean(flag) => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
    }
}

fn insert_all<S: KeyValueStore>(session: &mut Session<'_, S>, tables: &mut BTreeMap<String, Vec<(String, String)>>, by_measurement: &BTreeMap<&str, Vec<&Point>>) -> Result<(), String> {
    for (measurement, points) in by_measurement {
        let columns = tables.entry((*measurement).to_owned()).or_default();
        if columns.is_empty() {
            run(session, &format!("CREATE TABLE IF NOT EXISTS {} (\"time\" timestamptz NOT NULL)", table(measurement)))?;
            columns.push(("time".to_owned(), "timestamp with time zone".to_owned()));
        }
        let mut order: Vec<String> = Vec::new();
        for point in points {
            let tags = point.tags.iter().map(|(key, _)| (key, "text", None));
            let fields = point.fields.iter().map(|(key, value)| (key, sql_type(value), Some(value)));
            for (key, wanted, value) in tags.chain(fields) {
                let existing = columns.iter().find(|(name, _)| name == key).map(|(_, data_type)| data_type.clone());
                match existing {
                    Some(existing) => {
                        let compatible = match value {
                            Some(value) => fits(&existing, value),
                            None => existing == "text",
                        };
                        if !compatible {
                            return Err(format!("field type conflict: {key} of {measurement} is {existing}, not {wanted}"));
                        }
                    }
                    None => {
                        run(session, &format!("ALTER TABLE {} ADD COLUMN {} {wanted}", table(measurement), identifier(key)))?;
                        columns.push((key.clone(), wanted.to_owned()));
                    }
                }
                if !order.contains(key) {
                    order.push(key.clone());
                }
            }
        }
        let names: Vec<String> = std::iter::once("\"time\"".to_owned()).chain(order.iter().map(|key| identifier(key))).collect();
        for batch in points.chunks(ROWS_PER_INSERT) {
            let rows: Vec<String> = batch
                .iter()
                .map(|point| {
                    let mut cells = vec![timestamp_literal(point.time_ns.div_euclid(1000))];
                    for key in &order {
                        let cell = point
                            .tags
                            .iter()
                            .find(|(tag, _)| tag == key)
                            .map(|(_, value)| literal(value))
                            .or_else(|| point.fields.iter().find(|(field, _)| field == key).map(|(_, value)| value_sql(value)))
                            .unwrap_or_else(|| "NULL".to_owned());
                        cells.push(cell);
                    }
                    format!("({})", cells.join(", "))
                })
                .collect();
            run(session, &format!("INSERT INTO {} ({}) VALUES {}", table(measurement), names.join(", "), rows.join(", ")))?;
        }
    }
    Ok(())
}

/// Writes `points` into their measurements' tables, creating tables and columns as tags and
/// fields first appear, all in one transaction.
pub(crate) fn write<S: KeyValueStore>(session: &mut Session<'_, S>, points: &[Point]) -> Result<usize, String> {
    if points.is_empty() {
        return Ok(0);
    }
    run(session, &format!("CREATE SCHEMA IF NOT EXISTS {SCHEMA}"))?;
    let mut tables = catalog(session)?;
    let mut by_measurement: BTreeMap<&str, Vec<&Point>> = BTreeMap::new();
    for point in points {
        by_measurement.entry(point.measurement.as_str()).or_default().push(point);
    }
    run(session, "BEGIN")?;
    match insert_all(session, &mut tables, &by_measurement) {
        Ok(()) => {
            run(session, "COMMIT")?;
            Ok(points.len())
        }
        Err(error) => {
            let _ = run(session, "ROLLBACK");
            Err(error)
        }
    }
}

/// One series' samples: its labels (`__name__` first) and `(unix ms, value)` pairs in time order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Series {
    pub(crate) labels: Vec<(String, String)>,
    pub(crate) samples: Vec<(i64, f64)>,
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Double(number) => Some(*number),
        Value::Real(number) => Some(f64::from(*number)),
        Value::BigInt(number) => Some(*number as f64),
        Value::Integer(number) => Some(f64::from(*number)),
        Value::SmallInt(number) => Some(f64::from(*number)),
        Value::Boolean(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Numeric(_) => value.to_text().and_then(|text| text.parse().ok()),
        _ => None,
    }
}

/// The samples of metric `name` from `start_ms` (exclusive) to `end_ms` (inclusive), as series
/// split by their tag values. A metric is a table with a `value` column, or `<table>_<field>`.
pub(crate) fn read<S: KeyValueStore>(session: &mut Session<'_, S>, tables: &BTreeMap<String, Vec<(String, String)>>, name: &str, start_ms: i64, end_ms: i64) -> Result<Vec<Series>, String> {
    let located = match tables.get(name) {
        Some(columns) if columns.iter().any(|(column, _)| column == "value") => Some((name.to_owned(), "value".to_owned(), columns)),
        _ => name.char_indices().filter(|(_, character)| *character == '_').find_map(|(at, _)| {
            let (table, field) = (&name[..at], &name[at + 1..]);
            let columns = tables.get(table)?;
            columns.iter().any(|(column, data_type)| column == field && data_type != "text").then(|| (table.to_owned(), field.to_owned(), columns))
        }),
    };
    let Some((table_name, field, columns)) = located else {
        return Ok(Vec::new());
    };
    let tags: Vec<&String> = columns.iter().filter(|(column, data_type)| data_type == "text" && column != "time").map(|(column, _)| column).collect();
    let selected: Vec<String> = std::iter::once("\"time\"".to_owned()).chain(std::iter::once(identifier(&field))).chain(tags.iter().map(|tag| identifier(tag))).collect();
    let result = rows(
        session,
        &format!(
            "SELECT {} FROM {} WHERE \"time\" > {} AND \"time\" <= {} AND {} IS NOT NULL ORDER BY \"time\"",
            selected.join(", "),
            table(&table_name),
            timestamp_literal(start_ms.saturating_mul(1000)),
            timestamp_literal(end_ms.saturating_mul(1000)),
            identifier(&field)
        ),
    )?;
    let mut series: BTreeMap<Vec<(String, String)>, Vec<(i64, f64)>> = BTreeMap::new();
    for row in &result.rows {
        let Some(Value::TimestampTz(micros)) = row.first() else {
            continue;
        };
        let Some(value) = row.get(1).and_then(number) else {
            continue;
        };
        let mut labels = vec![("__name__".to_owned(), name.to_owned())];
        for (tag, cell) in tags.iter().zip(row.iter().skip(2)) {
            if let Some(text) = cell.to_text() {
                labels.push(((*tag).clone(), text));
            }
        }
        labels[1..].sort();
        series.entry(labels).or_default().push(((micros + UNIX_EPOCH_FROM_2000_MICROS).div_euclid(1000), value));
    }
    Ok(series.into_iter().map(|(labels, samples)| Series { labels, samples }).collect())
}
