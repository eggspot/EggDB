use super::copy_io::{StreamCopy, script_cache};
use super::restore::FileIncludes;
use super::{Access, Session};
use crate::cli::args::{CommandName, Format, ResultMode, SqlSource};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use crate::cli::render::{JsonObject, json_string};
use eggdb::sql::Store as DatabaseStore;
use eggdb_commands::Category;
use eggdb_sql::{
    CopyChannel, Dialect, IncludeOpener, QueryResult, SqlError, SqlPlusCommand, SqlPlusScript,
    SqlPlusStep, SqlState, TransactionStatus, Value,
};
use std::cell::{Cell, RefCell};
use std::io::Read;

impl Session<'_> {
    pub(super) fn sql(&mut self) -> Result<Exit, Failure> {
        let text = self.sql_text()?;
        let json = self.format == Format::Json;
        let script = !matches!(self.request.sql, Some(SqlSource::Command(_)));
        let access = if self.request.read_only {
            Access::ReadOnly
        } else {
            Access::Write {
                create: self.request.create,
            }
        };
        let db = self.open(access, Some(script_cache(&text)))?;
        let store = DatabaseStore(&db);
        let dialect = self.request.dialect;
        let mut session = eggdb_sql::Session::with_dialect(&store, dialect);
        let printed = RefCell::new(Ok(()));
        let mut failed_batch: Option<String> = None;
        let copied_out = Cell::new(false);
        let includes = FileIncludes::new(match &self.request.sql {
            Some(SqlSource::Script(path)) => Some(path.clone()),
            _ => None,
        });
        let defines = self.request.restore.defines.clone();
        let outcome = {
            let out = RefCell::new(&mut self.out);
            let messages = RefCell::new(&mut self.messages);
            let input: Option<&mut dyn Read> = if script { None } else { Some(&mut *self.stdin) };
            let mut copy = StreamCopy {
                input,
                output: Some(&out),
                copied_out: &copied_out,
            };
            let mut on_result = |result: QueryResult| {
                if !json {
                    let mut messages = messages.borrow_mut();
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
                }
                let mut out = out.borrow_mut();
                if !(copied_out.replace(false) && result.tag.starts_with("COPY ")) {
                    if json {
                        out.text(&statement_json(&result, dialect));
                    } else {
                        print_result(&mut out, &result, dialect);
                    }
                }
                let done = out.line_done();
                let more = done.is_ok();
                *printed.borrow_mut() = done;
                more
            };
            if dialect == Dialect::SqlServer {
                run_sqlserver_batches(
                    &mut session,
                    &text,
                    script,
                    &mut on_result,
                    &mut copy,
                    &mut failed_batch,
                )
            } else if dialect == Dialect::Oracle && script {
                let mut on_event = |event: ScriptEvent| {
                    let done = if json {
                        let mut out = out.borrow_mut();
                        out.text(&event_json(&event));
                        out.line_done()
                    } else {
                        match event {
                            ScriptEvent::Prompt(text) => {
                                let mut out = out.borrow_mut();
                                out.text(&text);
                                out.text("\n");
                                out.line_done()
                            }
                            ScriptEvent::Notice(notice) => {
                                let mut messages = messages.borrow_mut();
                                messages.styled(Style::Bold, "NOTICE:");
                                messages.text("  ");
                                messages.text(&notice);
                                messages.text("\n");
                                Ok(())
                            }
                        }
                    };
                    if done.is_err() {
                        *printed.borrow_mut() = done;
                    }
                };
                let mut input = text.as_bytes();
                let opener: &dyn IncludeOpener = &includes;
                match SqlPlusScript::new(&mut input, Some(opener), &defines) {
                    Ok(mut steps) => run_sqlplus_script(
                        &mut session,
                        &mut steps,
                        &mut on_result,
                        &mut on_event,
                        &mut copy,
                        &mut failed_batch,
                    ),
                    Err(error) => Err(error),
                }
            } else if script {
                session.execute_script(&text, &mut on_result, &mut copy)
            } else {
                session.execute_with_copy(&text, &mut on_result, &mut copy)
            }
        };
        let exit = match outcome {
            Ok(()) => Exit::Done,
            Err(error) => {
                let shown = failed_batch.as_deref().unwrap_or(&text);
                report_error(&mut self.messages, &error, shown, json);
                exit_for(&error)
            }
        };
        if let Err(error) = commit_on_exit(&mut session) {
            report_error(&mut self.messages, &error, &text, json);
        }
        match session.close() {
            Ok(true) if exit == Exit::Done => self.messages.text(
                "WARNING:  the SQL ended inside a transaction block (BEGIN without COMMIT); it was rolled back\n",
            ),
            Ok(_) => {}
            Err(error) => report_error(&mut self.messages, &error, &text, json),
        }
        self.close(db)?;
        printed.into_inner()?;
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

fn run_sqlserver_batches(
    session: &mut eggdb_sql::Session<'_, DatabaseStore<'_>>,
    text: &str,
    script: bool,
    on_result: &mut dyn FnMut(QueryResult) -> bool,
    copy: &mut dyn CopyChannel,
    failed_batch: &mut Option<String>,
) -> Result<(), SqlError> {
    let batches = eggdb_sql::sqlserver_batches(text)?;
    let stopped = Cell::new(false);
    let mut forward = |result: QueryResult| {
        let more = on_result(result);
        if !more {
            stopped.set(true);
        }
        more
    };
    for batch in &batches {
        for _ in 0..batch.repeat {
            let outcome = if script {
                session.execute_script(&batch.text, &mut forward, copy)
            } else {
                session.execute_with_copy(&batch.text, &mut forward, copy)
            };
            if let Err(error) = outcome {
                *failed_batch = Some(batch.text.clone());
                return Err(error);
            }
            if stopped.get() {
                return Ok(());
            }
        }
    }
    Ok(())
}

enum ScriptEvent {
    Prompt(String),
    Notice(String),
}

fn event_json(event: &ScriptEvent) -> String {
    match event {
        ScriptEvent::Prompt(text) => JsonObject::new().string("prompt", text).line(),
        ScriptEvent::Notice(notice) => JsonObject::new()
            .raw("notices", &format!("[{}]", json_string(notice)))
            .line(),
    }
}

const MOST_COUNTED_KINDS_SHOWN: usize = 8;

#[derive(Default)]
struct CommandsWithoutEffect {
    total: u64,
    kinds: Vec<(String, u64)>,
}

impl CommandsWithoutEffect {
    fn add(&mut self, kind: String) {
        self.total += 1;
        match self.kinds.iter_mut().find(|(known, _)| *known == kind) {
            Some((_, count)) => *count += 1,
            None => self.kinds.push((kind, 1)),
        }
    }

    fn notice(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        let mut list: Vec<String> = self
            .kinds
            .iter()
            .take(MOST_COUNTED_KINDS_SHOWN)
            .map(|(kind, count)| format!("{kind} {count}"))
            .collect();
        let more = self.kinds.len().saturating_sub(MOST_COUNTED_KINDS_SHOWN);
        if more > 0 {
            list.push(format!(
                "… and {more} more {}",
                if more == 1 { "kind" } else { "kinds" }
            ));
        }
        let commands = if self.total == 1 {
            "command"
        } else {
            "commands"
        };
        Some(format!(
            "{} SQL*Plus {commands} had no effect: {} (eggdb sql runs the SQL only)",
            self.total,
            list.join(", ")
        ))
    }
}

fn command_notice(command: &SqlPlusCommand) -> Option<String> {
    let place = &command.place;
    match command.name.as_str() {
        "host" => Some(format!(
            "{place}: {} never runs a program; eggdb sql runs the SQL only",
            command.shown
        )),
        "spool" => {
            let file = command.argument.split_whitespace().next()?;
            if file.eq_ignore_ascii_case("off") {
                return None;
            }
            Some(format!(
                "{place}: SPOOL {file} never writes a file; redirect stdout instead (eggdb sql ... > {file})"
            ))
        }
        "whenever" => {
            let mut words = command.argument.split_whitespace();
            let condition = words.next()?;
            let action = words.next()?;
            let continues = condition.eq_ignore_ascii_case("sqlerror")
                && action.eq_ignore_ascii_case("continue");
            continues.then(|| {
                format!(
                    "{place}: WHENEVER SQLERROR CONTINUE has no effect: eggdb sql stops at the first error (eggdb restore --keep-going runs on)"
                )
            })
        }
        "." => Some(format!("{place}: the statement before . was not run")),
        "unterminated" => Some(format!(
            "the text from {place} was not run: no ; or / ends it"
        )),
        "unknown" => Some(format!(
            "{place}: {} is not SQL or a SQL*Plus command EggDB knows; the line was not run",
            command.shown
        )),
        _ => None,
    }
}

fn counted_kind(command: &SqlPlusCommand) -> Option<String> {
    match command.name.as_str() {
        "define" | "undefine" | "set define" | "accept" | "include" | "whenever" | "spool"
        | "host" | "." | "unterminated" | "unknown" => None,
        "/" => Some("/ after ;".to_owned()),
        _ => Some(
            command
                .shown
                .split_whitespace()
                .next()
                .unwrap_or(&command.shown)
                .to_owned(),
        ),
    }
}

fn run_sqlplus_script(
    session: &mut eggdb_sql::Session<'_, DatabaseStore<'_>>,
    script: &mut SqlPlusScript<'_>,
    on_result: &mut dyn FnMut(QueryResult) -> bool,
    on_event: &mut dyn FnMut(ScriptEvent),
    copy: &mut dyn CopyChannel,
    failed_statement: &mut Option<String>,
) -> Result<(), SqlError> {
    let mut without_effect = CommandsWithoutEffect::default();
    let outcome = run_sqlplus_steps(
        session,
        script,
        on_result,
        on_event,
        copy,
        failed_statement,
        &mut without_effect,
    );
    if let Some(notice) = without_effect.notice() {
        on_event(ScriptEvent::Notice(notice));
    }
    outcome
}

fn run_sqlplus_steps(
    session: &mut eggdb_sql::Session<'_, DatabaseStore<'_>>,
    script: &mut SqlPlusScript<'_>,
    on_result: &mut dyn FnMut(QueryResult) -> bool,
    on_event: &mut dyn FnMut(ScriptEvent),
    copy: &mut dyn CopyChannel,
    failed_statement: &mut Option<String>,
    without_effect: &mut CommandsWithoutEffect,
) -> Result<(), SqlError> {
    let stopped = Cell::new(false);
    let mut forward = |result: QueryResult| {
        let more = on_result(result);
        if !more {
            stopped.set(true);
        }
        more
    };
    while let Some(step) = script.next_step()? {
        match step {
            SqlPlusStep::Statement { text, .. } => {
                if let Err(error) = session.execute_with_copy(&text, &mut forward, copy) {
                    *failed_statement = Some(text);
                    return Err(error);
                }
                if stopped.get() {
                    return Ok(());
                }
            }
            SqlPlusStep::Prompt(text) => on_event(ScriptEvent::Prompt(text)),
            SqlPlusStep::Connect { statement } => session.execute(&statement, &mut |_| {})?,
            SqlPlusStep::Exit {
                rollback,
                status,
                place,
            } => {
                if let Some(status) = status {
                    on_event(ScriptEvent::Notice(format!(
                        "{place}: EXIT {status}: the exit code stays 0; eggdb sql's exit codes mean what eggdb help sql says"
                    )));
                }
                if rollback {
                    session.execute("ROLLBACK", &mut |_| {})?;
                }
                return Ok(());
            }
            SqlPlusStep::Command(command) => {
                if let Some(notice) = command_notice(&command) {
                    on_event(ScriptEvent::Notice(notice));
                } else if let Some(kind) = counted_kind(&command) {
                    without_effect.add(kind);
                }
            }
        }
    }
    Ok(())
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

pub(super) fn report_error(messages: &mut Output<'_>, error: &SqlError, text: &str, json: bool) {
    if json {
        messages.text(&error_json(error));
    } else {
        print_error(messages, error, text);
    }
}

pub(crate) fn error_json(error: &SqlError) -> String {
    let code = match exit_for(error) {
        Exit::Damaged => "damaged",
        Exit::Busy => "database_busy",
        _ if error.state() == SqlState::READ_ONLY_SQL_TRANSACTION => "read_only",
        _ => "sql_error",
    };
    let category = eggdb_commands::category_of(code).unwrap_or(Category::Failed);
    let mut object = JsonObject::new()
        .string("code", code)
        .string("category", category.name())
        .string("message", error.message())
        .string("sqlstate", error.state().code());
    if let Some(detail) = error.detail() {
        object = object.string("detail", detail);
    }
    if let Some(hint) = error.hint() {
        object = object.string("hint", hint);
    }
    if let Some(position) = error.position() {
        object = object.number("position", position as u64);
    }
    JsonObject::new().raw("error", &object.object()).line()
}

const LARGEST_EXACT_INTEGER: u64 = (1 << 53) - 1;

pub(crate) fn statement_json(result: &QueryResult, dialect: Dialect) -> String {
    statement_object(result, dialect).line()
}

pub(crate) fn statement_object(result: &QueryResult, dialect: Dialect) -> JsonObject {
    let mut object = JsonObject::new().string("tag", &result.tag);
    if let Some(columns) = &result.columns {
        let columns: Vec<String> = columns
            .iter()
            .map(|column| {
                let kind = column
                    .sql_type
                    .map_or_else(|| "null".to_owned(), |sql_type| json_string(&sql_type.name()));
                JsonObject::new()
                    .string("name", &column.name)
                    .raw("type", &kind)
                    .object()
            })
            .collect();
        let rows: Vec<String> = result
            .rows
            .iter()
            .map(|row| {
                let cells: Vec<String> = row.iter().map(|cell| value_json(cell, dialect)).collect();
                format!("[{}]", cells.join(","))
            })
            .collect();
        object = object
            .raw("columns", &format!("[{}]", columns.join(",")))
            .raw("rows", &format!("[{}]", rows.join(",")))
            .number("row_count", result.rows.len() as u64);
    }
    let notices: Vec<String> = result
        .notices
        .iter()
        .cloned()
        .chain(
            result
                .warnings
                .iter()
                .map(|warning| format!("{} {}: {}", warning.level, warning.code, warning.message)),
        )
        .map(|notice| json_string(&notice))
        .collect();
    if !notices.is_empty() {
        object = object.raw("notices", &format!("[{}]", notices.join(",")));
    }
    object
}

pub(super) fn value_json(value: &Value, dialect: Dialect) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(number) => number.to_string(),
        Value::Integer(number) => number.to_string(),
        Value::BigInt(number) if number.unsigned_abs() <= LARGEST_EXACT_INTEGER => {
            number.to_string()
        }
        other => dialect
            .value_text(other)
            .map_or_else(|| "null".to_owned(), |shown| json_string(&shown)),
    }
}

pub(super) fn print_error(messages: &mut Output<'_>, error: &SqlError, text: &str) {
    let lines = error_lines(error, text);
    for (index, line) in lines.iter().enumerate() {
        let prefix = if line.starts_with("Error:") {
            "Error:"
        } else if line.starts_with("ORA-") {
            line.get(..10).unwrap_or("ORA-")
        } else if line.starts_with("Msg ") {
            line.split_once(':').map_or("Msg", |(prefix, _)| prefix)
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
    if error.vendor() == Some(Dialect::Oracle) || error.vendor() == Some(Dialect::SqlServer) {
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
    if let Some(context) = error.context() {
        lines.push(format!("CONTEXT:  {context}"));
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

pub(super) fn print_result(out: &mut Output<'_>, result: &QueryResult, dialect: Dialect) {
    out.text(&result_text(result, ResultMode::Table, None, dialect));
}

pub(super) fn rows_text(result: &QueryResult, dialect: Dialect) -> String {
    result_cells(result, dialect).map_or_else(String::new, |cells| table_text(&cells))
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
                .map(right_aligned)
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
            .map(|row| {
                row.iter()
                    .zip(columns)
                    .map(|(value, column)| dialect.value_text_typed(value, column.sql_type))
                    .collect()
            })
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

fn right_aligned(sql_type: eggdb_sql::SqlType) -> bool {
    !matches!(
        sql_type,
        eggdb_sql::SqlType::Text
            | eggdb_sql::SqlType::Varchar(_)
            | eggdb_sql::SqlType::Boolean
            | eggdb_sql::SqlType::Bpchar(_)
            | eggdb_sql::SqlType::Bytea
            | eggdb_sql::SqlType::Uuid
            | eggdb_sql::SqlType::Time(_)
            | eggdb_sql::SqlType::TimeTz(_)
            | eggdb_sql::SqlType::Interval { .. }
            | eggdb_sql::SqlType::Array(_)
    )
}

fn is_number(value: &Value) -> bool {
    value.sql_type().is_some_and(right_aligned)
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
