use eggdb::sql::{Dialect, SqlType, Store};
use eggdb::{Database, Error, ErrorKind};
use eggdb_sqlite_format::{SqliteFile, SqliteRow, SqliteValue};
use std::io::Read;
use std::path::Path;

const ROWS_PER_INSERT: usize = 500;
const BYTES_PER_INSERT: usize = 1 << 20;

pub(crate) fn is_sqlite_file(path: &Path) -> bool {
    let mut start = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok_and(|()| eggdb_sqlite_format::is_sqlite(&start))
}

fn failed(path: &Path, what: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Incompatible,
        format!("cannot read the SQLite file {}: {what}", path.display()),
    )
}

fn quoted_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(value: &SqliteValue, out: &mut String) {
    match value {
        SqliteValue::Null => out.push_str("NULL"),
        SqliteValue::Integer(number) => out.push_str(&number.to_string()),
        SqliteValue::Real(number) if number.is_finite() => out.push_str(&format!("{number:?}")),
        SqliteValue::Real(number) if *number > 0.0 => out.push_str("9e999"),
        SqliteValue::Real(number) if *number < 0.0 => out.push_str("-9e999"),
        SqliteValue::Real(_) => out.push_str("NULL"),
        SqliteValue::Text(text) => {
            out.push('\'');
            out.push_str(&text.replace('\'', "''"));
            out.push('\'');
        }
        SqliteValue::Blob(bytes) => {
            out.push_str("X'");
            for byte in bytes {
                out.push_str(&format!("{byte:02X}"));
            }
            out.push('\'');
        }
    }
}

/// Reads the SQLite file at `path` into `db`: its tables with their rows, then its indexes,
/// views and triggers. Statements EggDB refuses are skipped and returned as warnings.
pub(crate) fn import(db: &Database, path: &Path) -> Result<Vec<String>, Error> {
    let file = SqliteFile::open(path).map_err(|error| failed(path, error))?;
    let schema = file.schema().map_err(|error| failed(path, error))?;
    let store = Store(db);
    let mut session = store.session(Dialect::Sqlite);
    let mut warnings = Vec::new();
    let mut run = |sql: &str, warnings: &mut Vec<String>| {
        if let Err(error) = session.execute(sql, &mut |_| {}) {
            warnings.push(format!("skipped `{}`: {error}", sql.lines().next().unwrap_or(sql)));
            return false;
        }
        true
    };
    let user_object = |name: &str| !name.starts_with("sqlite_");
    let mut tables = Vec::new();
    for entry in &schema {
        if entry.kind == "table" && user_object(&entry.name)
            && let Some(sql) = &entry.sql
            && run(sql, &mut warnings)
        {
            tables.push(entry.clone());
        }
    }
    let defined = store
        .session(Dialect::Sqlite)
        .tables()
        .map_err(|error| failed(path, error))?;
    for entry in &tables {
        let alias = defined
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(&entry.name))
            .and_then(|table| match table.primary_key.as_slice() {
                [only] => table.columns.iter().position(|column| {
                    &column.name == only
                        && matches!(
                            column.sql_type,
                            SqlType::BigInt | SqlType::Integer | SqlType::SmallInt
                        )
                }),
                _ => None,
            });
        let rows = file.rows(entry.root_page).map_err(|error| failed(path, error))?;
        insert_rows(&mut run, &mut warnings, &entry.name, &rows, alias);
    }
    for kind in ["index", "view", "trigger"] {
        for entry in schema.iter().filter(|entry| entry.kind == kind && user_object(&entry.name)) {
            if let Some(sql) = &entry.sql {
                run(sql, &mut warnings);
            }
        }
    }
    Ok(warnings)
}

fn insert_rows(
    run: &mut impl FnMut(&str, &mut Vec<String>) -> bool,
    warnings: &mut Vec<String>,
    table: &str,
    rows: &[SqliteRow],
    alias: Option<usize>,
) {
    let mut statement = String::new();
    let mut in_statement = 0;
    for row in rows {
        if in_statement == 0 {
            statement.clear();
            statement.push_str("INSERT INTO ");
            statement.push_str(&quoted_name(table));
            statement.push_str(" VALUES ");
        } else {
            statement.push_str(", ");
        }
        statement.push('(');
        for (index, value) in row.values.iter().enumerate() {
            if index > 0 {
                statement.push_str(", ");
            }
            match (alias, value) {
                (Some(at), SqliteValue::Null) if at == index => {
                    statement.push_str(&row.rowid.to_string());
                }
                _ => literal(value, &mut statement),
            }
        }
        statement.push(')');
        in_statement += 1;
        if in_statement >= ROWS_PER_INSERT || statement.len() >= BYTES_PER_INSERT {
            run(&statement, warnings);
            in_statement = 0;
        }
    }
    if in_statement > 0 {
        run(&statement, warnings);
    }
}
