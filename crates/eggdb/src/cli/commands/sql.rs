use super::{Access, Session};
use crate::cli::args::{CommandName, ResultMode, SqlSource};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{Dialect, QueryResult, SqlError, SqlState, TransactionStatus, Value};

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
        let dialect = self.request.dialect;
        let mut session = eggdb_sql::Session::with_dialect(&store, dialect);
        let mut printed = Ok(());
        let outcome = {
            let out = &mut self.out;
            let messages = &mut self.messages;
            session.execute_while(&text, &mut |result| {
                for notice in &result.notices {
                    messages.text(notice);
                    messages.text("\n");
                }
                for warning in &result.warnings {
                    messages.text(&format!(
                        "{} {}: {}\n",
                        warning.level, warning.code, warning.message
                    ));
                }
                print_result(out, &result, dialect);
                printed = out.line_done();
                printed.is_ok()
            })
        };
        let exit = match outcome {
            Ok(()) => Exit::Done,
            Err(error) => {
                print_error(&mut self.messages, &error, &text);
                exit_for(&error)
            }
        };
        if let Err(error) = commit_on_exit(&mut session) {
            print_error(&mut self.messages, &error, &text);
        }
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

pub(super) fn commit_on_exit(
    session: &mut eggdb_sql::Session<'_, DatabaseStore<'_>>,
) -> Result<(), SqlError> {
    if session.dialect() == Dialect::Oracle
        && session.transaction_status() == TransactionStatus::InBlock
    {
        session.execute("COMMIT", &mut |_| {})?;
    }
    Ok(())
}

pub(super) fn exit_for(error: &SqlError) -> Exit {
    match error.state() {
        SqlState::DATA_CORRUPTED => Exit::Damaged,
        SqlState::LOCK_NOT_AVAILABLE => Exit::Busy,
        _ => Exit::DatabaseError,
    }
}

pub(super) fn print_error(messages: &mut Output<'_>, error: &SqlError, text: &str) {
    let lines = error_lines(error, text);
    for (index, line) in lines.iter().enumerate() {
        let prefix = if line.starts_with("Error:") {
            "Error:"
        } else if line.starts_with("ORA-") {
            line.get(..10).unwrap_or("ORA-")
        } else {
            "ERROR:"
        };
        match line.strip_prefix(prefix) {
            Some(rest) if index == 0 => {
                messages.styled(Style::Red, prefix);
                messages.text(rest);
            }
            _ => messages.text(line),
        }
        messages.text("\n");
    }
}

pub(crate) fn error_lines(error: &SqlError, text: &str) -> Vec<String> {
    if error.vendor() == Some(Dialect::Sqlite) {
        let code = error.vendor_code().unwrap_or(1);
        let name = eggdb_sql::sqlite_result_code_name(code).unwrap_or("SQLITE_ERROR");
        return vec![
            format!("Error: {}", error.message()),
            format!("Result code: {name} ({code})"),
        ];
    }
    if error.vendor() == Some(Dialect::Oracle) {
        return vec![error.to_string()];
    }
    if let Some(code) = error.vendor_code() {
        return vec![format!(
            "ERROR {code} ({}): {}",
            error.state(),
            error.message()
        )];
    }
    let mut lines = vec![format!("ERROR:  {}", error.message())];
    if let Some(position) = error.position().filter(|&position| position <= text.len()) {
        let (number, line, column) = line_of(text, position);
        let prefix = format!("LINE {number}: ");
        lines.push(format!("{prefix}{line}"));
        lines.push(format!("{}^", " ".repeat(prefix.chars().count() + column)));
    }
    if let Some(detail) = error.detail() {
        lines.push(format!("DETAIL:  {detail}"));
    }
    if let Some(hint) = error.hint() {
        lines.push(format!("HINT:  {hint}"));
    }
    lines.push(format!("SQLSTATE:  {}", error.state()));
    lines
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

fn print_result(out: &mut Output<'_>, result: &QueryResult, dialect: Dialect) {
    out.text(&result_text(result, ResultMode::Table, None, dialect));
}

pub(crate) struct ResultCells {
    pub(crate) columns: Vec<String>,
    pub(crate) numeric: Vec<bool>,
    pub(crate) rows: Vec<Vec<Option<String>>>,
}

pub(crate) fn result_cells(result: &QueryResult, dialect: Dialect) -> Option<ResultCells> {
    let columns = result.columns.as_ref()?;
    let numeric = (0..columns.len())
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
    Some(ResultCells {
        columns: columns.iter().map(|column| column.name.clone()).collect(),
        numeric,
        rows: result
            .rows
            .iter()
            .map(|row| row.iter().map(|value| dialect.value_text(value)).collect())
            .collect(),
    })
}

pub(super) fn result_text(
    result: &QueryResult,
    mode: ResultMode,
    width: Option<usize>,
    dialect: Dialect,
) -> String {
    let Some(cells) = result_cells(result, dialect) else {
        return format!("{}\n", result.tag);
    };
    let mut text = match mode {
        ResultMode::Csv => csv_text(&cells),
        ResultMode::Expanded => expanded_text(&cells),
        ResultMode::Table => {
            let table = table_text(&cells);
            let widest = table.lines().map(|line| line.chars().count()).max();
            match (width, widest) {
                (Some(width), Some(widest)) if widest > width => expanded_text(&cells),
                _ => table,
            }
        }
    };
    if !result.tag.starts_with("SELECT") {
        text.push_str(&format!("{}\n", result.tag));
    }
    text
}

fn rows_line(count: usize) -> String {
    format!("({count} {})\n\n", if count == 1 { "row" } else { "rows" })
}

fn table_text(cells: &ResultCells) -> String {
    let mut widths: Vec<usize> = cells
        .columns
        .iter()
        .map(|name| name.chars().count())
        .collect();
    for row in &cells.rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.as_deref().map_or(0, |cell| cell.chars().count()));
        }
    }
    let mut text = String::new();
    let header: Vec<String> = cells
        .columns
        .iter()
        .zip(&widths)
        .map(|(name, &width)| centered(name, width))
        .collect();
    text.push_str(&table_line(&header));
    let rule: Vec<String> = widths.iter().map(|&width| "-".repeat(width + 2)).collect();
    text.push_str(&rule.join("+"));
    text.push('\n');
    for row in &cells.rows {
        let padded: Vec<String> = row
            .iter()
            .zip(&widths)
            .zip(&cells.numeric)
            .map(|((cell, &width), &right)| {
                let cell = cell.as_deref().unwrap_or_default();
                let fill = " ".repeat(width - cell.chars().count());
                if right {
                    format!("{fill}{cell}")
                } else {
                    format!("{cell}{fill}")
                }
            })
            .collect();
        text.push_str(&table_line(&padded));
    }
    text.push_str(&rows_line(cells.rows.len()));
    text
}

fn expanded_text(cells: &ResultCells) -> String {
    if cells.rows.is_empty() {
        return rows_line(0);
    }
    let name_width = cells
        .columns
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    let value_width = cells
        .rows
        .iter()
        .flatten()
        .map(|cell| cell.as_deref().map_or(0, |cell| cell.chars().count()))
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (number, row) in cells.rows.iter().enumerate() {
        let label = format!("-[ RECORD {} ]", number + 1);
        let left = name_width + 1;
        let label_width = label.chars().count();
        let rule = if label_width < left {
            format!(
                "{label}{}+{}",
                "-".repeat(left - label_width),
                "-".repeat(value_width + 1)
            )
        } else {
            format!(
                "{label}{}",
                "-".repeat((left + 2 + value_width).saturating_sub(label_width).max(1))
            )
        };
        text.push_str(&rule);
        text.push('\n');
        for (name, cell) in cells.columns.iter().zip(row) {
            let fill = " ".repeat(name_width - name.chars().count());
            let line = format!("{name}{fill} | {}", cell.as_deref().unwrap_or_default());
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    text.push('\n');
    text
}

fn csv_text(cells: &ResultCells) -> String {
    let field = |text: &str| {
        if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    };
    let mut text = String::new();
    let header: Vec<String> = cells.columns.iter().map(|name| field(name)).collect();
    text.push_str(&header.join(","));
    text.push('\n');
    for row in &cells.rows {
        let fields: Vec<String> = row
            .iter()
            .map(|cell| cell.as_deref().map_or_else(String::new, field))
            .collect();
        text.push_str(&fields.join(","));
        text.push('\n');
    }
    text
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
    use super::line_of;
    use eggdb::Database;
    use eggdb::sql::Store as DatabaseStore;
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
