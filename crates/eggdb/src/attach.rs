use crate::sql::{Dialect, SqlType, Store, Value};
use crate::{Database, Options};
use eggdb_sqlite_format::{SqliteFile, SqliteValue};
use std::io::Read;
use std::path::Path;

const ROWS_PER_INSERT: usize = 500;

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn qualified(alias: &str, table: &str) -> String {
    format!("{}.{}", quoted(alias), quoted(table))
}

fn sqlite_literal(value: &SqliteValue) -> String {
    match value {
        SqliteValue::Null => "NULL".to_owned(),
        SqliteValue::Integer(number) => number.to_string(),
        SqliteValue::Real(number) if number.is_finite() => format!("{number:?}"),
        SqliteValue::Real(number) if *number > 0.0 => "9e999".to_owned(),
        SqliteValue::Real(number) if *number < 0.0 => "-9e999".to_owned(),
        SqliteValue::Real(_) => "NULL".to_owned(),
        SqliteValue::Text(text) => format!("'{}'", text.replace('\'', "''")),
        SqliteValue::Blob(bytes) => blob(bytes),
    }
}

fn blob(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2 + 3);
    text.push_str("X'");
    for byte in bytes {
        text.push_str(&format!("{byte:02X}"));
    }
    text.push('\'');
    text
}

fn value_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(flag) => if *flag { "1" } else { "0" }.to_owned(),
        Value::SmallInt(number) => number.to_string(),
        Value::Integer(number) => number.to_string(),
        Value::BigInt(number) => number.to_string(),
        Value::Real(number) if number.is_finite() => format!("{number:?}"),
        Value::Double(number) if number.is_finite() => format!("{number:?}"),
        Value::Bytea(bytes) => blob(bytes),
        other => format!("'{}'", other.to_text().replace('\'', "''")),
    }
}

fn affinity(sql_type: SqlType) -> &'static str {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt | SqlType::Boolean => "INTEGER",
        SqlType::Real | SqlType::Double => "REAL",
        SqlType::Numeric(_) => "NUMERIC",
        SqlType::Bytea => "BLOB",
        _ => "TEXT",
    }
}

fn top_level_parts(definitions: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (at, character) in definitions.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&definitions[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&definitions[start..]);
    parts
}

fn rowid_alias(definitions: &str) -> Option<usize> {
    top_level_parts(definitions).iter().position(|part| {
        let words: Vec<String> = part
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        words.get(1).is_some_and(|kind| kind == "integer")
            && words.windows(2).any(|pair| pair[0] == "primary" && pair[1] == "key")
    })
}

fn sqlite_statements(path: &Path, alias: &str) -> Result<Vec<String>, String> {
    let file = SqliteFile::open(path).map_err(|error| error.to_string())?;
    let schema = file.schema().map_err(|error| error.to_string())?;
    let mut statements = Vec::new();
    for entry in schema
        .iter()
        .filter(|entry| entry.kind == "table" && !entry.name.starts_with("sqlite_"))
    {
        let Some(sql) = &entry.sql else {
            continue;
        };
        if eggdb_sqlite_format::creates_without_rowid(sql) {
            continue;
        }
        let (Some(open), Some(close)) = (sql.find('('), sql.rfind(')')) else {
            continue;
        };
        let definitions = &sql[open + 1..close];
        statements.push(format!(
            "CREATE TABLE {} ({definitions})",
            qualified(alias, &entry.name)
        ));
        let alias_column = rowid_alias(definitions);
        let rows = file.rows(entry.root_page).map_err(|error| error.to_string())?;
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            let tuples: Vec<String> = chunk
                .iter()
                .map(|row| {
                    let values: Vec<String> = row
                        .values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| match (alias_column, value) {
                            (Some(at), SqliteValue::Null) if at == index => row.rowid.to_string(),
                            _ => sqlite_literal(value),
                        })
                        .collect();
                    format!("({})", values.join(", "))
                })
                .collect();
            statements.push(format!(
                "INSERT INTO {} VALUES {}",
                qualified(alias, &entry.name),
                tuples.join(", ")
            ));
        }
    }
    Ok(statements)
}

fn eggdb_statements(path: &Path, alias: &str) -> Result<Vec<String>, String> {
    let database = Database::open_with(path, Options::new().read_only(true)).map_err(|error| error.to_string())?;
    let store = Store(&database);
    let mut session = store.session(Dialect::Sqlite);
    let tables = session.tables().map_err(|error| error.to_string())?;
    let mut statements = Vec::new();
    for table in tables.iter().filter(|table| table.schema == "public") {
        let mut definitions: Vec<String> = table
            .columns
            .iter()
            .map(|column| {
                let mut definition = format!("{} {}", quoted(&column.name), affinity(column.sql_type));
                if column.not_null {
                    definition.push_str(" NOT NULL");
                }
                definition
            })
            .collect();
        if !table.primary_key.is_empty() {
            let key: Vec<String> = table.primary_key.iter().map(|name| quoted(name)).collect();
            definitions.push(format!("PRIMARY KEY ({})", key.join(", ")));
        }
        statements.push(format!(
            "CREATE TABLE {} ({})",
            qualified(alias, &table.name),
            definitions.join(", ")
        ));
        let mut rows = Vec::new();
        session
            .execute(&format!("SELECT * FROM {}", quoted(&table.name)), &mut |result| {
                rows.extend(result.rows)
            })
            .map_err(|error| error.to_string())?;
        for chunk in rows.chunks(ROWS_PER_INSERT) {
            let tuples: Vec<String> = chunk
                .iter()
                .map(|row| format!("({})", row.iter().map(value_literal).collect::<Vec<_>>().join(", ")))
                .collect();
            statements.push(format!(
                "INSERT INTO {} VALUES {}",
                qualified(alias, &table.name),
                tuples.join(", ")
            ));
        }
    }
    Ok(statements)
}

fn is_sqlite(path: &Path) -> bool {
    let mut start = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok_and(|()| eggdb_sqlite_format::is_sqlite(&start))
}

fn read_attached(path: &str, alias: &str) -> Result<Vec<String>, String> {
    let path = Path::new(path.strip_prefix("file:").unwrap_or(path));
    if !path.exists() {
        return Ok(Vec::new());
    }
    if is_sqlite(path) {
        sqlite_statements(path, alias)
    } else {
        eggdb_statements(path, alias)
    }
}

pub(crate) fn install() {
    eggdb_sql::set_attach_reader(Box::new(read_attached));
}
