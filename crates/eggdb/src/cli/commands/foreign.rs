use crate::cli::render::json_string;
use eggdb_parquet::{Cell, Column, ColumnType};

const VARIABLE: &str = "EGGDB_FOREIGN_SOURCES";
const SQL_SERVER_PASSWORD: &str = "SQLCMDPASSWORD";
const MOST_CSV_BYTES: u64 = 1 << 30;
const MICROS_PER_MS: i64 = 1000;
const MS_PER_DAY: i64 = 86_400_000;

/// Lets `eggdb_foreign(source, query)` read the sources the comma-separated entries of
/// `EGGDB_FOREIGN_SOURCES` allow: a `postgresql://user@host[:port][/database]` entry allows that
/// user on that host and port (and only that database when it names one), a `file:` entry allows
/// the files under that directory; with none set, SQL reaches nothing outside the database.
pub(super) fn allow_from_environment() {
    let Ok(text) = std::env::var(VARIABLE) else {
        return;
    };
    let prefixes: Vec<String> = text
        .split(',')
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .map(str::to_owned)
        .collect();
    if prefixes.is_empty() {
        return;
    }
    eggdb_sql::set_foreign_source(Box::new(move |source, query, row| {
        read(&prefixes, source, query, row)
    }));
}

fn object(names: &[String], values: impl Iterator<Item = String>) -> String {
    let fields: Vec<String> = names
        .iter()
        .zip(values)
        .map(|(name, value)| format!("{}:{value}", json_string(name)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn decimal_text(unscaled: i128, scale: u32) -> String {
    let digits = unscaled.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    let sign = if unscaled < 0 { "-" } else { "" };
    if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}

fn cell_json(cell: &Cell, column_type: &ColumnType) -> String {
    match (cell, column_type) {
        (Cell::Null, _) => "null".to_owned(),
        (Cell::Boolean(flag), _) => flag.to_string(),
        (Cell::Int(days), ColumnType::Date) => {
            json_string(&super::archive::rfc3339(days.saturating_mul(MS_PER_DAY))[..10])
        }
        (Cell::Int(micros), ColumnType::Timestamp { .. }) => {
            json_string(&super::archive::rfc3339(micros.div_euclid(MICROS_PER_MS)))
        }
        (Cell::Int(number), _) => number.to_string(),
        (Cell::Float(number), _) if number.is_finite() => format!("{number:?}"),
        (Cell::Float(number), _) => json_string(&number.to_string()),
        (Cell::Decimal(unscaled), ColumnType::Decimal { scale, .. }) => {
            json_string(&decimal_text(*unscaled, *scale))
        }
        (Cell::Decimal(unscaled), _) => json_string(&unscaled.to_string()),
        (Cell::Bytes(bytes), _) => match std::str::from_utf8(bytes) {
            Ok(text) => json_string(text),
            Err(_) => json_string(&format!(
                "\\x{}",
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )),
        },
    }
}

fn read_parquet(
    path: &std::path::Path,
    query: &str,
    row: &mut dyn FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    if !query.trim().is_empty() && query.trim() != "*" {
        return Err("a Parquet source reads every column: give '*' as the query".to_owned());
    }
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let columns: Vec<Column> = eggdb_parquet::read(&bytes).map_err(|error| error.to_string())?;
    let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    let count = columns.first().map_or(0, |column| column.cells.len());
    for index in 0..count {
        row(object(
            &names,
            columns.iter().map(|column| {
                column.cells.get(index).map_or_else(
                    || "null".to_owned(),
                    |cell| cell_json(cell, &column.column_type),
                )
            }),
        ))?;
    }
    Ok(())
}

fn read_postgresql(
    url: &str,
    query: &str,
    row: &mut dyn FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let mut options = eggdb_pgwire::client::parse_postgres_url(url)?;
    if options.password.is_none() {
        options.password = std::env::var("PGPASSWORD").ok();
    }
    let mut client =
        eggdb_pgwire::client::PgClient::connect(&options).map_err(|error| error.to_string())?;
    let results = client
        .simple_query(query)
        .map_err(|error| error.to_string())?;
    let Some(answer) = results
        .into_iter()
        .rev()
        .find(|result| !result.columns.is_empty())
    else {
        return Ok(());
    };
    for values in &answer.rows {
        row(object(
            &answer.columns,
            values.iter().map(|value| {
                value
                    .as_deref()
                    .map_or_else(|| "null".to_owned(), json_string)
            }),
        ))?;
    }
    Ok(())
}

fn read_sql_server(
    url: &str,
    query: &str,
    row: &mut dyn FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let mut options = eggdb_tdswire::client::parse_sql_server_url(url)?;
    if options.password.is_none() {
        options.password = std::env::var(SQL_SERVER_PASSWORD).ok();
    }
    let mut client = eggdb_tdswire::client::TdsClient::connect(&options).map_err(|error| error.to_string())?;
    let statement = format!("{} FOR JSON PATH, INCLUDE_NULL_VALUES", query.trim().trim_end_matches(';'));
    let results = client.batch(&statement).map_err(|error| error.to_string())?;
    let mut joined = String::new();
    for rows in &results {
        for values in rows {
            if let Some(Some(text)) = values.first() {
                joined.push_str(text);
            }
        }
    }
    if joined.trim().is_empty() {
        return Ok(());
    }
    let document = eggdb_sql::json::parse_document(&joined).map_err(|error| format!("SQL Server's FOR JSON answer: {}", error.message))?;
    let eggdb_sql::json::Json::Array(items) = document.value else {
        return Err("SQL Server's FOR JSON answer is not an array".to_owned());
    };
    for item in items {
        let text = joined.get(item.start..item.end).ok_or_else(|| "SQL Server's FOR JSON answer is cut".to_owned())?;
        row(text.to_owned())?;
    }
    Ok(())
}

fn csv_fields(line: &str, fields: &mut Vec<String>, field: &mut String, quoted: &mut bool) -> bool {
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match (character, *quoted) {
            ('"', true) if characters.peek() == Some(&'"') => {
                field.push('"');
                characters.next();
            }
            ('"', true) => *quoted = false,
            ('"', false) if field.is_empty() => *quoted = true,
            (',', false) => fields.push(std::mem::take(field)),
            (other, _) => field.push(other),
        }
    }
    if *quoted {
        field.push('\n');
        return false;
    }
    fields.push(std::mem::take(field));
    true
}

fn read_csv(
    path: &std::path::Path,
    query: &str,
    row: &mut dyn FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    use std::io::{BufRead, Read};
    if !query.trim().is_empty() && query.trim() != "*" {
        return Err("a CSV source reads every column: give '*' as the query".to_owned());
    }
    let file = std::fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let reader = std::io::BufReader::new(file.take(MOST_CSV_BYTES));
    let mut names: Option<Vec<String>> = None;
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if !csv_fields(line, &mut fields, &mut field, &mut quoted) {
            continue;
        }
        let record = std::mem::take(&mut fields);
        match &names {
            None => names = Some(record),
            Some(header) => {
                if record.len() == 1 && record[0].is_empty() {
                    continue;
                }
                row(object(header, header.iter().enumerate().map(|(index, _)| {
                    record.get(index).map_or_else(|| "null".to_owned(), |value| json_string(value))
                })))?;
            }
        }
    }
    Ok(())
}

fn is_sql_server(source: &str) -> bool {
    source.starts_with("sqlserver://")
}

fn read(
    prefixes: &[String],
    source: &str,
    query: &str,
    row: &mut dyn FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    if !prefixes.iter().any(|allowed| allows(allowed, source)) {
        return Err(format!(
            "{} is not among the sources {VARIABLE} allows",
            shown_source(source)
        ));
    }
    if source.starts_with("postgresql://") || source.starts_with("postgres://") {
        return read_postgresql(source, query, row);
    }
    if is_sql_server(source) {
        return read_sql_server(source, query, row);
    }
    if let Some(path) = file_path(source)
        && path.ends_with(".csv")
    {
        let checked = prefixes
            .iter()
            .find_map(|allowed| allowed_file(allowed, source))
            .ok_or_else(|| format!("{} is not among the sources {VARIABLE} allows", shown_source(source)))?;
        return read_csv(&checked, query, row);
    }
    if let Some(path) = source
        .strip_prefix("file://")
        .or_else(|| source.strip_prefix("file:"))
        && path.ends_with(".parquet")
    {
        let checked = prefixes
            .iter()
            .find_map(|allowed| allowed_file(allowed, source))
            .ok_or_else(|| {
                format!(
                    "{} is not among the sources {VARIABLE} allows",
                    shown_source(source)
                )
            })?;
        return read_parquet(&checked, query, row);
    }
    Err(format!(
        "{}: give a postgresql:// URL (EggDB or PostgreSQL), a sqlserver:// URL, or a file:...parquet or file:...csv path",
        shown_source(source)
    ))
}

fn shown_source(source: &str) -> String {
    if !is_postgresql(source) {
        return source.to_owned();
    }
    eggdb_pgwire::client::parse_postgres_url_with_password(source).map_or_else(
        |_| "the PostgreSQL source".to_owned(),
        |options| options.shown(),
    )
}

fn file_path(source: &str) -> Option<&str> {
    source
        .strip_prefix("file://")
        .or_else(|| source.strip_prefix("file:"))
}

fn is_postgresql(source: &str) -> bool {
    source.starts_with("postgresql://") || source.starts_with("postgres://")
}

fn names_database(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let main = rest.split('?').next().unwrap_or(rest);
    main.split_once('/')
        .is_some_and(|(_, database)| !database.is_empty())
}

fn allows(allowed: &str, source: &str) -> bool {
    if is_sql_server(allowed) && is_sql_server(source) {
        let (Ok(entry), Ok(wanted)) = (
            eggdb_tdswire::client::parse_sql_server_url(allowed),
            eggdb_tdswire::client::parse_sql_server_url(source),
        ) else {
            return false;
        };
        return entry.host.eq_ignore_ascii_case(&wanted.host)
            && entry.port == wanted.port
            && entry.user == wanted.user
            && (!names_database(allowed) || entry.database == wanted.database)
            && (entry.trust_server_certificate || !wanted.trust_server_certificate)
            && (wanted.root_certificate.is_none() || wanted.root_certificate == entry.root_certificate);
    }
    if is_postgresql(allowed) && is_postgresql(source) {
        let (Ok(entry), Ok(wanted)) = (
            eggdb_pgwire::client::parse_postgres_url(allowed),
            eggdb_pgwire::client::parse_postgres_url(source),
        ) else {
            return false;
        };
        return entry.host.eq_ignore_ascii_case(&wanted.host)
            && entry.port == wanted.port
            && entry.user == wanted.user
            && (!names_database(allowed) || entry.database == wanted.database)
            && wanted.ssl_mode >= entry.ssl_mode
            && (wanted.ssl_root_cert.is_none() || wanted.ssl_root_cert == entry.ssl_root_cert);
    }
    allowed_file(allowed, source).is_some()
}

fn allowed_file(allowed: &str, source: &str) -> Option<std::path::PathBuf> {
    let directory = std::fs::canonicalize(file_path(allowed)?).ok()?;
    let file = std::fs::canonicalize(file_path(source)?).ok()?;
    (file != directory && file.starts_with(&directory)).then_some(file)
}

#[cfg(test)]
mod tests {
    use super::{allows, csv_fields, decimal_text, read};

    #[test]
    fn database_entries_allow_only_their_own_host_port_and_user() {
        let entry = "postgresql://reader@db.internal";
        assert!(allows(entry, "postgresql://reader@db.internal/sales"));
        assert!(allows(entry, "postgres://reader@DB.internal:5432/x"));
        assert!(!allows(entry, "postgresql://reader@db.internal.evil.com/x"));
        assert!(!allows(entry, "postgresql://reader@db.internal:6000/x"));
        assert!(!allows(entry, "postgresql://admin@db.internal/x"));
        assert!(!allows(
            entry,
            "postgresql://reader@db.internal/x?sslmode=verify-full&sslrootcert=/etc/shadow"
        ));
        let one_database = "postgresql://reader@db.internal/sales";
        assert!(allows(
            one_database,
            "postgresql://reader@db.internal/sales"
        ));
        assert!(!allows(one_database, "postgresql://reader@db.internal/hr"));
        let verified = "postgresql://reader@db.internal?sslmode=verify-full&sslrootcert=/ca.pem";
        assert!(allows(
            verified,
            "postgresql://reader@db.internal/x?sslmode=verify-full&sslrootcert=/ca.pem"
        ));
        assert!(!allows(
            verified,
            "postgresql://reader@db.internal/x?sslmode=require"
        ));
        assert!(!allows(verified, "postgresql://reader@db.internal/x"));
    }

    #[test]
    fn a_refused_source_is_named_without_its_password() {
        let refused = super::read(
            &["postgresql://reader@db.internal".to_owned()],
            "postgresql://admin:hunter2@db.internal/x",
            "SELECT 1",
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(!refused.contains("hunter2"), "{refused}");
        assert!(refused.contains("admin@db.internal"), "{refused}");
    }

    #[test]
    fn file_entries_allow_only_files_under_their_directory() {
        let root = std::env::temp_dir().join(format!("eggdb-foreign-{}", std::process::id()));
        let lake = root.join("lake");
        std::fs::create_dir_all(&lake).unwrap();
        std::fs::write(lake.join("x.parquet"), b"").unwrap();
        std::fs::write(root.join("secret.parquet"), b"").unwrap();
        let entry = format!("file:{}/", lake.display());
        assert!(allows(
            &entry,
            &format!("file:{}/x.parquet", lake.display())
        ));
        assert!(!allows(
            &entry,
            &format!("file:{}/../secret.parquet", lake.display())
        ));
        assert!(!allows(&entry, "file:/etc/passwd"));
        let _ = std::fs::remove_file(lake.join("x.parquet"));
        let _ = std::fs::remove_file(root.join("secret.parquet"));
        let _ = std::fs::remove_dir(&lake);
        let _ = std::fs::remove_dir(&root);
    }

    #[test]
    fn sources_outside_the_allowed_prefixes_are_refused_and_decimals_keep_their_scale() {
        let allowed = vec!["file:/data/lake/".to_owned()];
        let mut rows = Vec::new();
        let refused = read(&allowed, "file:/etc/passwd.parquet", "*", &mut |row| {
            rows.push(row);
            Ok(())
        });
        assert!(refused.unwrap_err().contains("not among the sources"));
        assert!(rows.is_empty());
        assert_eq!(decimal_text(12_345, 2), "123.45");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(7, 0), "7");
    }

    #[test]
    fn sql_server_entries_and_csv_records() {
        let entry = "sqlserver://reader@mssql.internal:1433/sales";
        assert!(allows(entry, "sqlserver://reader@mssql.internal:1433/sales"));
        assert!(!allows(entry, "sqlserver://reader@mssql.internal:1433/hr"));
        assert!(!allows(entry, "sqlserver://sa@mssql.internal:1433/sales"));
        assert!(!allows(entry, "sqlserver://reader@mssql.internal:1433/sales?trustServerCertificate=true"));
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        assert!(!csv_fields("1,\"two, \"\"quoted\"\"", &mut fields, &mut field, &mut quoted));
        assert!(csv_fields("lines\",3", &mut fields, &mut field, &mut quoted));
        assert_eq!(fields, vec!["1".to_owned(), "two, \"quoted\"\nlines".to_owned(), "3".to_owned()]);
    }
}
