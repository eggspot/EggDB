use super::{Access, Session};
use crate::cli::args::{CommandName, SqlSource};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use eggdb::{Database, Error, ErrorKind, ReadTransaction, WriteTransaction};
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, QueryResult, ScanDirection, SqlError,
    SqlResult, SqlState, StoreFailure, Value,
};
use std::ops::Bound;

pub(crate) struct DatabaseStore<'db>(pub(crate) &'db Database);

fn store_error(error: Error) -> SqlError {
    let failure = match error.kind() {
        ErrorKind::Locked | ErrorKind::WriterActive => StoreFailure::Busy,
        ErrorKind::ReadOnly => StoreFailure::ReadOnly,
        ErrorKind::TooLarge | ErrorKind::CacheFull | ErrorKind::DatabaseFull => {
            StoreFailure::TooLarge
        }
        ErrorKind::Corrupt => StoreFailure::Corrupt,
        ErrorKind::Io | ErrorKind::NeedsReopen => StoreFailure::Io,
        ErrorKind::OutOfMemory => StoreFailure::OutOfMemory,
        _ => StoreFailure::Other,
    };
    SqlError::store(failure, error.to_string())
}

fn scan_through(
    mut scan: eggdb::Scan<'_>,
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    loop {
        let entry = match direction {
            ScanDirection::Forward => scan.next_entry(),
            ScanDirection::Backward => scan.next_back_entry(),
        }
        .map_err(store_error)?;
        let Some((key, value)) = entry else {
            return Ok(());
        };
        if !visit(key, value)? {
            return Ok(());
        }
    }
}

pub(crate) struct SqlSnapshot<'db>(ReadTransaction<'db>);

pub(crate) struct SqlWriter<'db>(WriteTransaction<'db>);

impl KeyValueRead for SqlSnapshot<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }
}

impl KeyValueRead for SqlWriter<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }
}

impl KeyValueWrite for SqlWriter<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.put(key, value).map_err(store_error)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.0.delete(key).map_err(store_error)
    }

    fn commit(self) -> SqlResult<()> {
        self.0.commit().map(|_| ()).map_err(store_error)
    }

    fn rollback(self) -> SqlResult<()> {
        self.0.rollback().map_err(store_error)
    }
}

impl KeyValueStore for DatabaseStore<'_> {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SqlWriter<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        self.0.begin_read().map(SqlSnapshot).map_err(store_error)
    }

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
    }
}

impl Session<'_> {
    pub(super) fn sql(&mut self) -> Result<Exit, Failure> {
        let text = self.sql_text()?;
        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            None,
        )?;
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::new(&store);
        let mut printed = Ok(());
        let outcome = {
            let out = &mut self.out;
            let messages = &mut self.messages;
            session.execute(&text, &mut |result| {
                for notice in &result.notices {
                    messages.text(notice);
                    messages.text("\n");
                }
                print_result(out, &result);
                if printed.is_ok() {
                    printed = out.line_done();
                }
            })
        };
        let exit = match outcome {
            Ok(()) => Exit::Done,
            Err(error) => {
                print_error(&mut self.messages, &error, &text);
                exit_for(&error)
            }
        };
        match session.close() {
            Ok(true) if exit == Exit::Done => self.messages.text(
                "WARNING:  the SQL ended inside a transaction block (BEGIN without COMMIT); it was rolled back\n",
            ),
            Ok(_) => {}
            Err(error) => print_error(&mut self.messages, &error, &text),
        }
        self.close(db)?;
        printed?;
        Ok(exit)
    }

    fn sql_text(&mut self) -> Result<String, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Sql));
        let bytes = match self.request.sql.clone() {
            Some(SqlSource::Command(text)) => return Ok(text),
            Some(SqlSource::Script(path)) => std::fs::read(&path).map_err(|error| {
                usage(format!(
                    "cannot read the script {}: {error}",
                    path.display()
                ))
            })?,
            Some(SqlSource::Stdin) | None => {
                if self.stdin_is_terminal {
                    return Err(usage(
                        "eggdb sql needs SQL: give it with -c, as a script file, or pipe it to stdin"
                            .to_owned(),
                    ));
                }
                let mut bytes = Vec::new();
                self.stdin
                    .read_to_end(&mut bytes)
                    .map_err(|error| usage(format!("cannot read stdin: {error}")))?;
                bytes
            }
        };
        String::from_utf8(bytes).map_err(|error| {
            usage(format!(
                "the SQL is not UTF-8 text (invalid byte at offset {})",
                error.utf8_error().valid_up_to()
            ))
        })
    }
}

fn exit_for(error: &SqlError) -> Exit {
    match error.state() {
        SqlState::DATA_CORRUPTED => Exit::Damaged,
        SqlState::LOCK_NOT_AVAILABLE => Exit::Busy,
        _ => Exit::DatabaseError,
    }
}

fn print_error(messages: &mut Output<'_>, error: &SqlError, text: &str) {
    messages.styled(Style::Red, "ERROR:");
    messages.text(&format!("  {}\n", error.message()));
    if let Some(position) = error.position().filter(|&position| position <= text.len()) {
        let (number, line, column) = line_of(text, position);
        let prefix = format!("LINE {number}: ");
        messages.text(&format!("{prefix}{line}\n"));
        messages.text(&format!(
            "{}^\n",
            " ".repeat(prefix.chars().count() + column)
        ));
    }
    if let Some(detail) = error.detail() {
        messages.text(&format!("DETAIL:  {detail}\n"));
    }
    if let Some(hint) = error.hint() {
        messages.text(&format!("HINT:  {hint}\n"));
    }
    messages.text(&format!("SQLSTATE:  {}\n", error.state()));
}

fn line_of(text: &str, position: usize) -> (usize, &str, usize) {
    let mut position = position;
    while !text.is_char_boundary(position) {
        position -= 1;
    }
    let before = &text[..position];
    let number = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |index| index + 1);
    let end = text[position..]
        .find('\n')
        .map_or(text.len(), |index| position + index);
    let line = text[start..end].trim_end_matches('\r');
    let column = text[start..position].chars().count();
    (number, line, column)
}

fn print_result(out: &mut Output<'_>, result: &QueryResult) {
    if let Some(columns) = &result.columns {
        let cells: Vec<Vec<String>> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| value.to_text().unwrap_or_default())
                    .collect()
            })
            .collect();
        let mut widths: Vec<usize> = columns
            .iter()
            .map(|column| column.name.chars().count())
            .collect();
        for row in &cells {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let numeric: Vec<bool> = (0..columns.len())
            .map(|index| {
                columns[index]
                    .sql_type
                    .map(|sql_type| {
                        !matches!(
                            sql_type,
                            eggdb_sql::SqlType::Text
                                | eggdb_sql::SqlType::Varchar(_)
                                | eggdb_sql::SqlType::Boolean
                        )
                    })
                    .or_else(|| {
                        result
                            .rows
                            .iter()
                            .find_map(|row| row.get(index).filter(|value| !value.is_null()))
                            .map(is_number)
                    })
                    .unwrap_or(false)
            })
            .collect();
        let header: Vec<String> = columns
            .iter()
            .zip(&widths)
            .map(|(column, &width)| centered(&column.name, width))
            .collect();
        out.text(&table_line(&header));
        let rule: Vec<String> = widths.iter().map(|&width| "-".repeat(width + 2)).collect();
        out.text(&rule.join("+"));
        out.text("\n");
        for row in &cells {
            let padded: Vec<String> = row
                .iter()
                .zip(&widths)
                .zip(&numeric)
                .map(|((cell, &width), &right)| {
                    let fill = width - cell.chars().count();
                    if right {
                        format!("{}{cell}", " ".repeat(fill))
                    } else {
                        format!("{cell}{}", " ".repeat(fill))
                    }
                })
                .collect();
            out.text(&table_line(&padded));
        }
        let count = result.rows.len();
        out.text(&format!(
            "({count} {})\n\n",
            if count == 1 { "row" } else { "rows" }
        ));
        if !result.tag.starts_with("SELECT") {
            out.text(&format!("{}\n", result.tag));
        }
    } else {
        out.text(&format!("{}\n", result.tag));
    }
}

fn is_number(value: &Value) -> bool {
    value.sql_type().is_some_and(|sql_type| {
        !matches!(
            sql_type,
            eggdb_sql::SqlType::Text | eggdb_sql::SqlType::Boolean | eggdb_sql::SqlType::Varchar(_)
        )
    })
}

fn centered(text: &str, width: usize) -> String {
    let fill = width - text.chars().count();
    let left = fill / 2;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(fill - left))
}

fn table_line(cells: &[String]) -> String {
    let joined: Vec<String> = cells.iter().map(|cell| format!(" {cell} ")).collect();
    let mut line = joined.join("|");
    line.truncate(line.trim_end().len());
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::{DatabaseStore, line_of};
    use eggdb::Database;
    use eggdb_sql::{QueryResult, Session};
    use std::collections::BTreeMap;

    fn rows_of(session: &mut Session<'_, DatabaseStore<'_>>, sql: &str) -> Vec<String> {
        let mut rows = Vec::new();
        session
            .execute(sql, &mut |result: QueryResult| {
                for row in result.rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|value| value.to_text().unwrap_or_else(|| "NULL".into()))
                        .collect();
                    rows.push(cells.join("|"));
                }
            })
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        rows
    }

    #[test]
    fn random_statements_on_a_real_file_agree_with_a_model_across_reopens() {
        let path = std::env::temp_dir().join(format!("eggdb-sql-model-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut model: BTreeMap<i64, i64> = BTreeMap::new();
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for round in 0..6 {
            let db = Database::open(&path).unwrap();
            let store = DatabaseStore(&db);
            let mut session = Session::new(&store);
            if round == 0 {
                rows_of(
                    &mut session,
                    "CREATE TABLE m (k bigint PRIMARY KEY, n bigint)",
                );
            }
            for _ in 0..80 {
                let k = next(40) as i64;
                let n = next(1000) as i64;
                match next(4) {
                    0 => {
                        let sql = format!("INSERT INTO m VALUES ({k}, {n})");
                        let mut ignored = |_| {};
                        let outcome = session.execute(&sql, &mut ignored);
                        assert_eq!(outcome.is_ok(), !model.contains_key(&k), "{sql}");
                        model.entry(k).or_insert(n);
                    }
                    1 => {
                        rows_of(
                            &mut session,
                            &format!("UPDATE m SET n = n + {n} WHERE k < {k}"),
                        );
                        for (_, value) in model.range_mut(..k) {
                            *value += n;
                        }
                    }
                    2 => {
                        rows_of(
                            &mut session,
                            &format!(
                                "BEGIN; DELETE FROM m WHERE k = {k}; ROLLBACK; DELETE FROM m WHERE n > {}",
                                n * 50
                            ),
                        );
                        model.retain(|_, value| *value <= n * 50);
                    }
                    _ => {
                        let expected: Vec<String> = model
                            .range(k..)
                            .map(|(key, value)| format!("{key}|{value}"))
                            .collect();
                        assert_eq!(
                            rows_of(&mut session, &format!("SELECT k, n FROM m WHERE k >= {k}")),
                            expected
                        );
                    }
                }
            }
            assert_eq!(session.close(), Ok(false));
            assert_eq!(db.check().unwrap().leaked_pages, 0);
            db.close().unwrap();
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_error_position_names_its_line_and_column() {
        let text = "SELECT 1;\nSELECT * FROM nope;";
        assert_eq!(line_of(text, 24), (2, "SELECT * FROM nope;", 14));
        assert_eq!(line_of(text, 0), (1, "SELECT 1;", 0));
        assert_eq!(line_of("é x", 3), (1, "é x", 2));
    }
}
