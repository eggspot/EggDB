use eggdb::sql::{Dialect, SqlType, Store};
use eggdb::{Database, Error, ErrorKind, Options};
use eggdb_sqlite_format::{SqliteFile, SqliteRow, SqliteValue};
use eggdb_storage::CommitStamp;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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

fn write_back_failed(path: &Path, what: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Io,
        format!("cannot write the changes back to the SQLite file {}: {what}", path.display()),
    )
}

/// An SQLite file opened as a copy in memory (D180): when it was opened for writing, the
/// changes made to the copy go back into the file when the command ends, crash-safely through
/// SQLite's rollback journal (D1309).
pub(crate) struct SqliteCopy {
    db: Database,
    path: PathBuf,
    change_counter: u32,
    imported_at: CommitStamp,
    writable: bool,
    finished: bool,
}

/// Opens the SQLite file at `path` as a copy in memory, after rolling back a journal a crash
/// left beside it; `writable` says whether changes go back into the file.
pub(crate) fn open_copy(path: &Path, options: Options, writable: bool) -> Result<SqliteCopy, Error> {
    let rolled_back = eggdb_sqlite_format::roll_back_hot_journal(path).map_err(|error| failed(path, error))?;
    let change_counter = eggdb_sqlite_format::change_counter(path).map_err(|error| failed(path, error))?;
    let db = Database::in_memory_with(options.read_only(false))?;
    let warnings = import(&db, path)?;
    let imported_at = db.stats()?.last_commit;
    let mut stderr = std::io::stderr().lock();
    if rolled_back {
        let _ = writeln!(
            stderr,
            "eggdb: {} had a journal left by an interrupted write; it was rolled back first",
            path.display()
        );
    }
    let _ = if writable {
        writeln!(
            stderr,
            "eggdb: {} is an SQLite file; EggDB reads it into memory and writes the changes back to it when the command ends",
            path.display()
        )
    } else {
        writeln!(
            stderr,
            "eggdb: {} is an SQLite file; EggDB read it into memory, read-only",
            path.display()
        )
    };
    for warning in warnings {
        let _ = writeln!(stderr, "eggdb: {warning}");
    }
    Ok(SqliteCopy {
        db,
        path: path.to_owned(),
        change_counter,
        imported_at,
        writable,
        finished: false,
    })
}

impl SqliteCopy {
    pub(crate) fn database(&self) -> &Database {
        &self.db
    }

    /// Writes the copy back into the file once, if it changed since it was read; answers
    /// whether it wrote.
    pub(crate) fn write_back(&mut self) -> Result<bool, Error> {
        if self.finished || !self.writable {
            return Ok(false);
        }
        self.finished = true;
        if self.db.stats()?.last_commit == self.imported_at {
            return Ok(false);
        }
        let bytes = Store(&self.db)
            .session(Dialect::Sqlite)
            .sqlite_file_bytes()
            .map_err(|error| write_back_failed(&self.path, error))?;
        eggdb_sqlite_format::replace_database(&self.path, &bytes, self.change_counter)
            .map_err(|error| write_back_failed(&self.path, error))?;
        Ok(true)
    }
}

impl Drop for SqliteCopy {
    fn drop(&mut self) {
        if let Err(error) = self.write_back() {
            let _ = writeln!(std::io::stderr().lock(), "eggdb: {error}");
        }
    }
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
            warnings.push(format!(
                "skipped `{}`: {error}",
                sql.lines().next().unwrap_or(sql)
            ));
            return false;
        }
        true
    };
    let user_object = |name: &str| !name.starts_with("sqlite_");
    let mut tables = Vec::new();
    for entry in &schema {
        if entry.kind == "table"
            && user_object(&entry.name)
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
        let found = defined
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(&entry.name));
        if entry
            .sql
            .as_deref()
            .is_some_and(eggdb_sqlite_format::creates_without_rowid)
        {
            let Some(table) = found else {
                continue;
            };
            let key: Vec<usize> = table
                .primary_key
                .iter()
                .filter_map(|name| table.columns.iter().position(|column| &column.name == name))
                .collect();
            let rows = file
                .without_rowid_rows(entry.root_page, table.columns.len(), &key)
                .map_err(|error| failed(path, error))?;
            insert_rows(&mut run, &mut warnings, &entry.name, &rows, None);
            continue;
        }
        let alias = found
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
        let rows = file
            .rows(entry.root_page)
            .map_err(|error| failed(path, error))?;
        insert_rows(&mut run, &mut warnings, &entry.name, &rows, alias);
    }
    for kind in ["index", "view", "trigger"] {
        for entry in schema
            .iter()
            .filter(|entry| entry.kind == kind && user_object(&entry.name))
        {
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
