use crate::line_protocol::{FieldValue, Point};
use eggdb_sql::{QueryResult, Session, SqlError, Value};
use std::collections::{BTreeMap, BTreeSet};

type SamplesByLabels = BTreeMap<Vec<(String, String)>, Vec<(i64, f64)>>;

pub(crate) const SCHEMA: &str = "metrics";
const META_SCHEMA: &str = "metrics_meta";
const TAG_KEYS: &str = "metrics_meta.tag_keys";
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
    let text = Value::TimestampTz(unix_micros.saturating_sub(UNIX_EPOCH_FROM_2000_MICROS))
        .to_text()
        .unwrap_or_default();
    format!("{}::timestamptz", literal(&text))
}

pub(crate) fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<QueryResult>, String> {
    let mut results = Vec::new();
    session
        .execute(sql, &mut |result| results.push(result))
        .map_err(|error: SqlError| error.message().to_owned())?;
    Ok(results)
}

fn rows(session: &mut Session<'_>, sql: &str) -> Result<QueryResult, String> {
    run(session, sql)?
        .into_iter()
        .rev()
        .find(|result| result.columns.is_some())
        .ok_or_else(|| "the query returned no rows".to_owned())
}

/// Every column of every metrics table: table, column and `information_schema` data type, with
/// a string field's `text` reported as `string` so only tag columns are `text`.
pub(crate) fn catalog(
    session: &mut Session<'_>,
) -> Result<BTreeMap<String, Vec<(String, String)>>, String> {
    let result = rows(
        session,
        &format!(
            "SELECT table_name, column_name, data_type FROM information_schema.columns WHERE table_schema = {} ORDER BY table_name, ordinal_position",
            literal(SCHEMA)
        ),
    )?;
    let mut tables: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for row in &result.rows {
        let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
        tables.entry(text(0)).or_default().push((text(1), text(2)));
    }
    let tags = tag_keys(session);
    for (measurement, columns) in &mut tables {
        let Some(known) = tags.get(measurement) else {
            continue;
        };
        for (column, kind) in columns.iter_mut() {
            if kind == "text" && !known.contains(column) {
                *kind = "string".to_owned();
            }
        }
    }
    Ok(tables)
}

/// Each measurement's tag keys, as writes recorded them; empty for a measurement written
/// before tag keys were recorded, whose `text` columns all count as tags.
pub(crate) fn tag_keys(session: &mut Session<'_>) -> BTreeMap<String, BTreeSet<String>> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let Ok(result) = rows(session, &format!("SELECT measurement, tag_key FROM {TAG_KEYS}")) else {
        return found;
    };
    for row in &result.rows {
        let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
        found.entry(text(0)).or_default().insert(text(1));
    }
    found
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
        (
            "double precision",
            FieldValue::Float(_) | FieldValue::Integer(_) | FieldValue::Unsigned(_)
        ) | ("bigint", FieldValue::Integer(_))
            | ("numeric", FieldValue::Integer(_) | FieldValue::Unsigned(_))
            | ("text" | "string", FieldValue::Text(_))
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

struct Merged<'p> {
    time_micros: i64,
    tags: BTreeMap<&'p str, &'p str>,
    fields: Vec<(&'p str, &'p FieldValue)>,
}

fn merged<'p>(points: &[&'p Point]) -> Vec<Merged<'p>> {
    let mut found: Vec<Merged<'p>> = Vec::new();
    let mut index: BTreeMap<(i64, Vec<(&'p str, &'p str)>), usize> = BTreeMap::new();
    for point in points {
        let time_micros = point.time_ns.div_euclid(1000);
        let tags: BTreeMap<&str, &str> = point
            .tags
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let key = (time_micros, tags.iter().map(|(key, value)| (*key, *value)).collect::<Vec<_>>());
        let at = *index.entry(key).or_insert_with(|| {
            found.push(Merged {
                time_micros,
                tags,
                fields: Vec::new(),
            });
            found.len() - 1
        });
        for (name, value) in &point.fields {
            let fields = &mut found[at].fields;
            match fields.iter_mut().find(|(held, _)| *held == name.as_str()) {
                Some(slot) => slot.1 = value,
                None => fields.push((name.as_str(), value)),
            }
        }
    }
    found
}

fn add_column(session: &mut Session<'_>, measurement: &str, key: &str, definition: &str) -> Result<(), String> {
    run(
        session,
        &format!("ALTER TABLE {} ADD COLUMN {} {definition}", table(measurement), identifier(key)),
    )
    .map(|_| ())
}

fn series_index(measurement: &str) -> String {
    format!("{SCHEMA}.{}", identifier(&format!("{measurement}_series")))
}

fn prepare(
    session: &mut Session<'_>,
    tables: &mut BTreeMap<String, Vec<(String, String)>>,
    known_tags: &mut BTreeMap<String, BTreeSet<String>>,
    measurement: &str,
    points: &[&Point],
) -> Result<Option<Vec<String>>, String> {
    let columns = tables.entry(measurement.to_owned()).or_default();
    let fresh = columns.is_empty();
    if fresh {
        run(
            session,
            &format!("CREATE TABLE IF NOT EXISTS {} (\"time\" timestamptz NOT NULL)", table(measurement)),
        )?;
        columns.push(("time".to_owned(), "timestamp with time zone".to_owned()));
    }
    let tag_set = known_tags.entry(measurement.to_owned()).or_default();
    let mut added_tags: Vec<String> = Vec::new();
    if tag_set.is_empty() && !fresh {
        for (column, _) in columns.iter().filter(|(column, kind)| kind == "text" && column != "time") {
            run(
                session,
                &format!(
                    "UPDATE {} SET {} = '' WHERE {} IS NULL",
                    table(measurement),
                    identifier(column),
                    identifier(column)
                ),
            )?;
            run(
                session,
                &format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT ''",
                    table(measurement),
                    identifier(column)
                ),
            )?;
            added_tags.push(column.clone());
        }
    }
    for point in points {
        for (key, _) in &point.tags {
            let existing = columns.iter().find(|(name, _)| name == key).map(|(_, kind)| kind.clone());
            match existing {
                Some(kind) if kind == "text" => {}
                Some(kind) => {
                    return Err(format!("field type conflict: {key} of {measurement} is {kind}, not a tag"));
                }
                None => {
                    add_column(session, measurement, key, "text NOT NULL DEFAULT ''")?;
                    columns.push((key.clone(), "text".to_owned()));
                }
            }
            if !tag_set.contains(key) && !added_tags.contains(key) {
                added_tags.push(key.clone());
            }
        }
        for (key, value) in &point.fields {
            let existing = columns.iter().find(|(name, _)| name == key).map(|(_, kind)| kind.clone());
            match existing {
                Some(kind) if fits(&kind, value) => {}
                Some(kind) => {
                    return Err(format!(
                        "field type conflict: {key} of {measurement} is {kind}, not {}",
                        sql_type(value)
                    ));
                }
                None => {
                    add_column(session, measurement, key, sql_type(value))?;
                    let kind = if matches!(value, FieldValue::Text(_)) { "string" } else { sql_type(value) };
                    columns.push((key.clone(), kind.to_owned()));
                }
            }
        }
    }
    for tag in &added_tags {
        run(
            session,
            &format!(
                "INSERT INTO {TAG_KEYS} (measurement, tag_key) VALUES ({}, {}) ON CONFLICT DO NOTHING",
                literal(measurement),
                literal(tag)
            ),
        )?;
        tag_set.insert(tag.clone());
    }
    if !added_tags.is_empty() {
        run(session, &format!("DROP INDEX IF EXISTS {}", series_index(measurement)))?;
    }
    let tag_columns: Vec<String> = tag_set.iter().cloned().collect();
    let keys: Vec<String> = std::iter::once("\"time\"".to_owned())
        .chain(tag_columns.iter().map(|tag| identifier(tag)))
        .collect();
    let created = run(
        session,
        &format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {} ON {} ({})",
            identifier(&format!("{measurement}_series")),
            table(measurement),
            keys.join(", ")
        ),
    );
    Ok(created.ok().map(|_| tag_columns))
}

fn insert_all(
    session: &mut Session<'_>,
    tables: &BTreeMap<String, Vec<(String, String)>>,
    by_measurement: &BTreeMap<&str, Vec<&Point>>,
    series_keys: &BTreeMap<&str, Vec<String>>,
) -> Result<(), String> {
    for (measurement, points) in by_measurement {
        let rows = merged(points);
        let tag_keys = series_keys.get(measurement);
        let is_tag = |key: &str| {
            tables
                .get(*measurement)
                .and_then(|columns| columns.iter().find(|(name, _)| name == key))
                .is_some_and(|(_, kind)| kind == "text")
        };
        let mut order: Vec<&str> = Vec::new();
        for row in &rows {
            for key in row.tags.keys().copied().chain(row.fields.iter().map(|(key, _)| *key)) {
                if !order.contains(&key) {
                    order.push(key);
                }
            }
        }
        let names: Vec<String> = std::iter::once("\"time\"".to_owned())
            .chain(order.iter().map(|key| identifier(key)))
            .collect();
        let conflict = match tag_keys {
            Some(tags) => {
                let target: Vec<String> = std::iter::once("\"time\"".to_owned())
                    .chain(tags.iter().map(|tag| identifier(tag)))
                    .collect();
                let updates: Vec<String> = order
                    .iter()
                    .filter(|key| !is_tag(key))
                    .map(|key| format!("{0} = COALESCE(EXCLUDED.{0}, {0})", identifier(key)))
                    .collect();
                if updates.is_empty() {
                    format!(" ON CONFLICT ({}) DO NOTHING", target.join(", "))
                } else {
                    format!(" ON CONFLICT ({}) DO UPDATE SET {}", target.join(", "), updates.join(", "))
                }
            }
            None => String::new(),
        };
        for batch in rows.chunks(ROWS_PER_INSERT) {
            let values: Vec<String> = batch
                .iter()
                .map(|row| {
                    let mut cells = vec![timestamp_literal(row.time_micros)];
                    for key in &order {
                        let cell = match row.tags.get(key) {
                            Some(value) => literal(value),
                            None if is_tag(key) => "''".to_owned(),
                            None => row
                                .fields
                                .iter()
                                .find(|(field, _)| field == key)
                                .map_or_else(|| "NULL".to_owned(), |(_, value)| value_sql(value)),
                        };
                        cells.push(cell);
                    }
                    format!("({})", cells.join(", "))
                })
                .collect();
            run(
                session,
                &format!(
                    "INSERT INTO {} ({}) VALUES {}{conflict}",
                    table(measurement),
                    names.join(", "),
                    values.join(", ")
                ),
            )?;
        }
    }
    Ok(())
}

/// Writes `points` into their measurements' tables, creating tables and columns as tags and
/// fields first appear and recording each measurement's tag keys. A point with the same
/// measurement, tag set and time as a stored one replaces the fields it carries and keeps the
/// others, as in InfluxDB; the rows are written in one transaction.
pub(crate) fn write(session: &mut Session<'_>, points: &[Point]) -> Result<usize, String> {
    if points.is_empty() {
        return Ok(0);
    }
    run(session, &format!("CREATE SCHEMA IF NOT EXISTS {SCHEMA}"))?;
    run(session, &format!("CREATE SCHEMA IF NOT EXISTS {META_SCHEMA}"))?;
    run(
        session,
        &format!(
            "CREATE TABLE IF NOT EXISTS {TAG_KEYS} (measurement text NOT NULL, tag_key text NOT NULL, PRIMARY KEY (measurement, tag_key))"
        ),
    )?;
    let mut tables = catalog(session)?;
    let mut known_tags = tag_keys(session);
    let mut by_measurement: BTreeMap<&str, Vec<&Point>> = BTreeMap::new();
    for point in points {
        by_measurement
            .entry(point.measurement.as_str())
            .or_default()
            .push(point);
    }
    let mut series_keys: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (measurement, measurement_points) in &by_measurement {
        if let Some(tags) = prepare(session, &mut tables, &mut known_tags, measurement, measurement_points)? {
            series_keys.insert(measurement, tags);
        }
    }
    run(session, "BEGIN")?;
    match insert_all(session, &tables, &by_measurement, &series_keys) {
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

/// Every metric the tables hold, with its table's tag columns: a table with a `value` column
/// is a metric of its own name, and each other column that is neither text nor `time` is the
/// metric `<table>_<column>`.
pub(crate) fn metrics(
    tables: &BTreeMap<String, Vec<(String, String)>>,
) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for (table, columns) in tables {
        let tags: Vec<String> = columns
            .iter()
            .filter(|(column, data_type)| data_type == "text" && column != "time")
            .map(|(column, _)| column.clone())
            .collect();
        for (column, data_type) in columns {
            if column == "value" {
                found.push((table.clone(), tags.clone()));
            } else if column != "time" && data_type != "text" && data_type != "string" {
                found.push((format!("{table}_{column}"), tags.clone()));
            }
        }
    }
    found
}

/// The samples of metric `name` from `start_ms` (exclusive) to `end_ms` (inclusive), as series
/// split by their tag values, reading at most `most_rows` rows. A metric is a table with a
/// `value` column, or `<table>_<field>`.
pub(crate) fn read(
    session: &mut Session<'_>,
    tables: &BTreeMap<String, Vec<(String, String)>>,
    name: &str,
    start_ms: i64,
    end_ms: i64,
    most_rows: usize,
) -> Result<Vec<Series>, String> {
    let located = match tables.get(name) {
        Some(columns) if columns.iter().any(|(column, _)| column == "value") => {
            Some((name.to_owned(), "value".to_owned(), columns))
        }
        _ => name
            .char_indices()
            .filter(|(_, character)| *character == '_')
            .find_map(|(at, _)| {
                let (table, field) = (&name[..at], &name[at + 1..]);
                let columns = tables.get(table)?;
                columns
                    .iter()
                    .any(|(column, data_type)| column == field && data_type != "text" && data_type != "string")
                    .then(|| (table.to_owned(), field.to_owned(), columns))
            }),
    };
    let Some((table_name, field, columns)) = located else {
        return Ok(Vec::new());
    };
    let tags: Vec<&String> = columns
        .iter()
        .filter(|(column, data_type)| data_type == "text" && column != "time")
        .map(|(column, _)| column)
        .collect();
    let selected: Vec<String> = std::iter::once("\"time\"".to_owned())
        .chain(std::iter::once(identifier(&field)))
        .chain(tags.iter().map(|tag| identifier(tag)))
        .collect();
    let most_rows = most_rows.min(usize::try_from(i64::MAX).unwrap_or(usize::MAX));
    let result = rows(
        session,
        &format!(
            "SELECT {} FROM {} WHERE \"time\" > {} AND \"time\" <= {} AND {} IS NOT NULL ORDER BY \"time\" LIMIT {most_rows}",
            selected.join(", "),
            table(&table_name),
            timestamp_literal(start_ms.saturating_mul(1000)),
            timestamp_literal(end_ms.saturating_mul(1000)),
            identifier(&field)
        ),
    )?;
    let mut series: SamplesByLabels = BTreeMap::new();
    for row in &result.rows {
        let Some(Value::TimestampTz(micros)) = row.first() else {
            continue;
        };
        let Some(value) = row.get(1).and_then(number) else {
            continue;
        };
        let mut labels = vec![("__name__".to_owned(), name.to_owned())];
        for (tag, cell) in tags.iter().zip(row.iter().skip(2)) {
            if let Some(text) = cell.to_text().filter(|text| !text.is_empty()) {
                labels.push(((*tag).clone(), text));
            }
        }
        labels[1..].sort();
        series.entry(labels).or_default().push((
            (micros + UNIX_EPOCH_FROM_2000_MICROS).div_euclid(1000),
            value,
        ));
    }
    Ok(series
        .into_iter()
        .map(|(labels, samples)| Series { labels, samples })
        .collect())
}
