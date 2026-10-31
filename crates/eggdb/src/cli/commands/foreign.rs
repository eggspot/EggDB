use crate::cli::render::json_string;
use eggdb_parquet::{Cell, Column, ColumnType};

const VARIABLE: &str = "EGGDB_FOREIGN_SOURCES";
const MICROS_PER_MS: i64 = 1000;
const MS_PER_DAY: i64 = 86_400_000;

/// Lets `eggdb_foreign(source, query)` read the sources whose URL starts with one of the
/// comma-separated prefixes in `EGGDB_FOREIGN_SOURCES`; with none set, SQL reaches nothing outside
/// the database.
pub(super) fn allow_from_environment() {
    let Ok(text) = std::env::var(VARIABLE) else {
        return;
    };
    let prefixes: Vec<String> = text.split(',').map(str::trim).filter(|prefix| !prefix.is_empty()).map(str::to_owned).collect();
    if prefixes.is_empty() {
        return;
    }
    eggdb_sql::set_foreign_source(Box::new(move |source, query, row| read(&prefixes, source, query, row)));
}

fn object(names: &[String], values: impl Iterator<Item = String>) -> String {
    let fields: Vec<String> = names.iter().zip(values).map(|(name, value)| format!("{}:{value}", json_string(name))).collect();
    format!("{{{}}}", fields.join(","))
}

fn decimal_text(unscaled: i128, scale: u32) -> String {
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    let sign = if unscaled < 0 { "-" } else { "" };
    if fraction.is_empty() { format!("{sign}{whole}") } else { format!("{sign}{whole}.{fraction}") }
}

fn cell_json(cell: &Cell, column_type: &ColumnType) -> String {
    match (cell, column_type) {
        (Cell::Null, _) => "null".to_owned(),
        (Cell::Boolean(flag), _) => flag.to_string(),
        (Cell::Int(days), ColumnType::Date) => json_string(&super::archive::rfc3339(days.saturating_mul(MS_PER_DAY))[..10]),
        (Cell::Int(micros), ColumnType::Timestamp { .. }) => json_string(&super::archive::rfc3339(micros.div_euclid(MICROS_PER_MS))),
        (Cell::Int(number), _) => number.to_string(),
        (Cell::Float(number), _) if number.is_finite() => format!("{number:?}"),
        (Cell::Float(number), _) => json_string(&number.to_string()),
        (Cell::Decimal(unscaled), ColumnType::Decimal { scale, .. }) => json_string(&decimal_text(*unscaled, *scale)),
        (Cell::Decimal(unscaled), _) => json_string(&unscaled.to_string()),
        (Cell::Bytes(bytes), _) => match std::str::from_utf8(bytes) {
            Ok(text) => json_string(text),
            Err(_) => json_string(&format!("\\x{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
        },
    }
}

fn read_parquet(path: &str, query: &str, row: &mut dyn FnMut(String) -> Result<(), String>) -> Result<(), String> {
    if !query.trim().is_empty() && query.trim() != "*" {
        return Err("a Parquet source reads every column: give '*' as the query".to_owned());
    }
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read {path}: {error}"))?;
    let columns: Vec<Column> = eggdb_parquet::read(&bytes).map_err(|error| error.to_string())?;
    let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    let count = columns.first().map_or(0, |column| column.cells.len());
    for index in 0..count {
        row(object(&names, columns.iter().map(|column| column.cells.get(index).map_or_else(|| "null".to_owned(), |cell| cell_json(cell, &column.column_type)))))?;
    }
    Ok(())
}

fn read_postgresql(url: &str, query: &str, row: &mut dyn FnMut(String) -> Result<(), String>) -> Result<(), String> {
    let mut options = eggdb_pgwire::client::parse_postgres_url(url)?;
    if options.password.is_none() {
        options.password = std::env::var("PGPASSWORD").ok();
    }
    let mut client = eggdb_pgwire::client::PgClient::connect(&options).map_err(|error| error.to_string())?;
    let results = client.simple_query(query).map_err(|error| error.to_string())?;
    let Some(answer) = results.into_iter().rev().find(|result| !result.columns.is_empty()) else {
        return Ok(());
    };
    for values in &answer.rows {
        row(object(&answer.columns, values.iter().map(|value| value.as_deref().map_or_else(|| "null".to_owned(), json_string))))?;
    }
    Ok(())
}

fn read(prefixes: &[String], source: &str, query: &str, row: &mut dyn FnMut(String) -> Result<(), String>) -> Result<(), String> {
    if !prefixes.iter().any(|prefix| source.starts_with(prefix.as_str())) {
        return Err(format!("{source} is not among the sources {VARIABLE} allows"));
    }
    if source.starts_with("postgresql://") || source.starts_with("postgres://") {
        return read_postgresql(source, query, row);
    }
    if let Some(path) = source.strip_prefix("file://").or_else(|| source.strip_prefix("file:"))
        && path.ends_with(".parquet")
    {
        return read_parquet(path, query, row);
    }
    Err(format!("{source}: give a postgresql:// URL (EggDB or PostgreSQL) or a file:...parquet path"))
}

#[cfg(test)]
mod tests {
    use super::{decimal_text, read};

    #[test]
    fn sources_outside_the_allowed_prefixes_are_refused_and_decimals_keep_their_scale() {
        let allowed = vec!["file:/data/lake/".to_owned()];
        let mut rows = Vec::new();
        let refused = read(&allowed, "file:/etc/passwd.parquet", "*", &mut |row| {
            rows.push(row);
            Ok(())
        });
        assert!(refused.unwrap_err().contains("not among the sources"));
        assert!(read(&allowed, "file:/data/lake/x.csv", "*", &mut |_| Ok(())).unwrap_err().contains("give a postgresql"));
        assert!(rows.is_empty());
        assert_eq!(decimal_text(12_345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
    }
}
