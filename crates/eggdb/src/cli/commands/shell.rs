use super::sql::{error_lines, exit_for, result_text};
use super::{Access, execute_on, session_for, uses_color};
use crate::cli::args::{self, CommandName, Invocation, Request, ResultMode};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::{Output, Style};
use crate::cli::{Console, Terminal, help, report};
use eggdb::Database;
use eggdb::sql::Store as DatabaseStore;
use eggdb_platform::user_files;
use eggdb_sql::{QueryResult, TableSchema, TransactionStatus};
use eggdb_ui::{Completion, InlineTerminal, LineEnd, LineReader, LineTerminal};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const KEY_VALUE_COMMANDS: [&str; 6] = ["put", "get", "del", "scan", "check", "stats"];
const META_COMMANDS: [&str; 7] = [
    ".help", ".tables", ".schema", ".mode", ".timer", ".quit", ".exit",
];
const STATEMENT_KEYWORDS: [&str; 15] = [
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "BEGIN", "COMMIT", "ROLLBACK", "SET",
    "SHOW", "RESET", "START", "END", "ABORT",
];
const SQL_WORDS: [&str; 52] = [
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "AS",
    "JOIN",
    "INNER",
    "LEFT",
    "CROSS",
    "ON",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "ASC",
    "DESC",
    "NULLS",
    "FIRST",
    "LAST",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "RETURNING",
    "CREATE",
    "TABLE",
    "DROP",
    "PRIMARY",
    "KEY",
    "DEFAULT",
    "TRUE",
    "FALSE",
    "CAST",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "IF",
    "EXISTS",
];
const FUNCTIONS: [&str; 13] = [
    "count", "sum", "avg", "min", "max", "coalesce", "nullif", "length", "lower", "upper", "abs",
    "substr", "concat",
];
const TABLE_KEYWORDS: [&str; 5] = ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"];
const KEY_CANDIDATES: usize = 50;
const LONGEST_STEM: usize = 24;
const MOST_HISTORY_BYTES: usize = 1 << 20;
const HISTORY_VARIABLE: &str = "EGGDB_HISTORY";

enum Step {
    Continue(Exit),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    Single,
    Double,
    Other,
}

struct Pending {
    complete_to: usize,
    open: Open,
}

pub(super) fn run(request: Request, console: &mut Console<'_>) -> Result<Exit, Failure> {
    let interactive =
        request.shell.commands.is_empty() && console.terminal.stdin && console.terminal.stdout;
    let access = if request.read_only {
        Access::ReadOnly
    } else {
        Access::Write {
            create: request.create,
        }
    };
    let db = {
        let session = session_for(request.clone(), console, None);
        session
            .open_database(access, None)
            .map_err(|error| session.open_failure(&error, access))?
    };
    let (exit, left_open, color_err, file) = {
        let store = DatabaseStore(&db);
        let mut shell = Shell::new(request, &db, &store, console.terminal, interactive);
        let exit = if interactive {
            shell.interactive(console)
        } else {
            shell.batch(console)
        };
        let color_err = shell.color_err;
        let file = shell.file.clone();
        let committed = super::sql::commit_on_exit(&mut shell.sql);
        (exit, committed.and_then(|()| shell.sql.close()), color_err, file)
    };
    let mut exit = exit?;
    match left_open {
        Ok(true) => print_notice(
            console,
            color_err,
            "WARNING:  the shell ended inside a transaction block (BEGIN without COMMIT); it was rolled back",
        ),
        Ok(false) => {}
        Err(error) => {
            print_sql_error(console, color_err, &error, "");
            exit = exit_for(&error);
        }
    }
    db.close().map_err(|error| {
        Failure::database(
            format!("cannot close {file}: {error}"),
            Exit::for_error(error.kind()),
        )
    })?;
    Ok(exit)
}

struct Shell<'s> {
    request: Request,
    file: String,
    stem: String,
    db: &'s Database,
    sql: eggdb_sql::Session<'s, DatabaseStore<'s>>,
    buffer: String,
    entry: String,
    mode: ResultMode,
    timer: bool,
    width: Option<usize>,
    color_out: bool,
    color_err: bool,
    read_only: bool,
    history_path: Option<PathBuf>,
    history_notice: Option<String>,
}

impl<'s> Shell<'s> {
    fn new(
        request: Request,
        db: &'s Database,
        store: &'s DatabaseStore<'s>,
        terminal: Terminal,
        interactive: bool,
    ) -> Self {
        let file = request.file.display().to_string();
        let stem = prompt_stem(&request.file);
        let (history_path, history_notice) = if interactive {
            history_location()
        } else {
            (None, None)
        };
        let dialect = request.dialect;
        Self {
            mode: request.shell.mode.unwrap_or(ResultMode::Table),
            color_out: interactive && uses_color(request.color, terminal.stdout_color),
            color_err: interactive && uses_color(request.color, terminal.stderr_color),
            read_only: request.read_only,
            request,
            file,
            stem,
            db,
            sql: eggdb_sql::Session::with_dialect(store, dialect),
            buffer: String::new(),
            entry: String::new(),
            timer: false,
            width: None,
            history_path,
            history_notice,
        }
    }

    fn batch(&mut self, console: &mut Console<'_>) -> Result<Exit, Failure> {
        let texts = if self.request.shell.commands.is_empty() {
            let mut bytes = Vec::new();
            console.stdin.read_to_end(&mut bytes).map_err(|error| {
                Failure::usage(
                    format!("cannot read stdin: {error}"),
                    Some(CommandName::Shell),
                )
            })?;
            let text = String::from_utf8(bytes).map_err(|error| {
                Failure::usage(
                    format!(
                        "the input is not UTF-8 text (invalid byte at offset {})",
                        error.utf8_error().valid_up_to()
                    ),
                    Some(CommandName::Shell),
                )
            })?;
            vec![text]
        } else {
            self.request.shell.commands.clone()
        };
        let keep_going = self.request.shell.keep_going;
        let mut first_failure = None;
        for text in texts {
            let lines: Vec<&str> = text.split('\n').collect();
            let count = lines.len();
            for (index, line) in lines.into_iter().enumerate() {
                if index + 1 == count && line.is_empty() {
                    continue;
                }
                let line = line.strip_suffix('\r').unwrap_or(line);
                match self.line(console, line) {
                    Step::Quit => return Ok(first_failure.unwrap_or(Exit::Done)),
                    Step::Continue(Exit::Done) => {}
                    Step::Continue(exit) => {
                        first_failure.get_or_insert(exit);
                        if !keep_going {
                            return Ok(exit);
                        }
                    }
                }
            }
            match self.flush(console) {
                Step::Quit => return Ok(first_failure.unwrap_or(Exit::Done)),
                Step::Continue(Exit::Done) => {}
                Step::Continue(exit) => {
                    first_failure.get_or_insert(exit);
                    if !keep_going {
                        return Ok(exit);
                    }
                }
            }
        }
        Ok(first_failure.unwrap_or(Exit::Done))
    }

    fn interactive(&mut self, console: &mut Console<'_>) -> Result<Exit, Failure> {
        self.banner(console);
        let history = self
            .history_path
            .as_deref()
            .map(read_history)
            .unwrap_or_default();
        let mut reader = LineReader::new(history, self.color_out);
        loop {
            let prompt = self.prompt();
            let mut terminal = match InlineTerminal::open() {
                Ok(terminal) => terminal,
                Err(error) => {
                    return Err(Failure::database(
                        format!("cannot use the terminal: {error}"),
                        Exit::DatabaseError,
                    ));
                }
            };
            self.width = Some(usize::from(terminal.cols()));
            let end = reader.read_line(&mut terminal, &prompt, &mut |text, cursor| {
                self.complete(text, cursor)
            });
            drop(terminal);
            let end = end.map_err(|error| {
                Failure::database(format!("the terminal failed: {error}"), Exit::DatabaseError)
            })?;
            match end {
                LineEnd::EndOfInput => break,
                LineEnd::Cleared => {
                    self.buffer.clear();
                    self.entry.clear();
                }
                LineEnd::Line(line) => {
                    if !self.entry.is_empty() {
                        self.entry.push('\n');
                    }
                    self.entry.push_str(&line);
                    let step = self.line(console, &line);
                    if self.buffer.is_empty() {
                        let entry = std::mem::take(&mut self.entry);
                        let quits = matches!(entry.trim(), ".quit" | ".exit");
                        if !quits && reader.remember(&entry) {
                            let kept = reader.history().last().cloned().unwrap_or_default();
                            self.append_history(console, &kept);
                        }
                    }
                    if matches!(step, Step::Quit) {
                        break;
                    }
                }
            }
        }
        self.trim_history(reader.history());
        Ok(Exit::Done)
    }

    fn banner(&mut self, console: &mut Console<'_>) {
        let access = if self.read_only {
            "read-only"
        } else {
            "read-write"
        };
        let tables = self.sql.tables().map(|tables| tables.len()).unwrap_or(0);
        let counts = match tables {
            0 => String::new(),
            1 => " (1 table)".to_owned(),
            count => format!(" ({count} tables)"),
        };
        let text = format!(
            "EggDB {}: {}, open {access}{counts}\nType .help for help. SQL ends with ; and .quit or Ctrl-D quits.\n",
            eggdb::VERSION,
            self.file
        );
        self.print(console, &text);
        if let Some(notice) = self.history_notice.take() {
            self.print_err_styled(console, Style::Dim, &format!("{notice}\n"));
        }
    }

    fn prompt(&self) -> String {
        let state = match self.pending().open {
            Open::Single => '\'',
            Open::Double => '"',
            Open::None | Open::Other if self.buffer.trim().is_empty() => '=',
            Open::None | Open::Other => '-',
        };
        let block = match self.sql.transaction_status() {
            TransactionStatus::Idle => "",
            TransactionStatus::InBlock => "*",
            TransactionStatus::Failed => "!",
        };
        format!("{}{state}{block}> ", self.stem)
    }

    fn pending(&self) -> Pending {
        scan_sql(&self.buffer)
    }

    fn line(&mut self, console: &mut Console<'_>, line: &str) -> Step {
        if self.buffer.trim().is_empty() {
            self.buffer.clear();
            let trimmed = line.trim_start();
            if trimmed.starts_with('.') {
                return self.meta(console, trimmed.trim_end());
            }
            let first = trimmed.split_whitespace().next().unwrap_or_default();
            if KEY_VALUE_COMMANDS.contains(&first) {
                return self.key_value(console, trimmed);
            }
            if trimmed.is_empty() {
                return Step::Continue(Exit::Done);
            }
        } else {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);
        let pending = self.pending();
        if pending.complete_to == 0 {
            return Step::Continue(Exit::Done);
        }
        let statements: String = self.buffer.drain(..pending.complete_to).collect();
        if self.buffer.trim().is_empty() {
            self.buffer.clear();
        }
        self.run_sql(console, &statements)
    }

    fn flush(&mut self, console: &mut Console<'_>) -> Step {
        let statements = std::mem::take(&mut self.buffer);
        if statements.trim().is_empty() {
            return Step::Continue(Exit::Done);
        }
        self.run_sql(console, &statements)
    }

    fn run_sql(&mut self, console: &mut Console<'_>, text: &str) -> Step {
        let started = Instant::now();
        let mut printed = Vec::new();
        let outcome = self.sql.execute(text, &mut |result: QueryResult| {
            printed.push(result);
        });
        let width = self.width;
        let mode = self.mode;
        for result in &printed {
            for notice in &result.notices {
                self.print_notice(console, notice);
            }
            let text = result_text(result, mode, width, self.sql.dialect());
            if !self.print(console, &text) {
                return Step::Quit;
            }
        }
        let exit = match outcome {
            Ok(()) => Exit::Done,
            Err(error) => {
                self.print_sql_error(console, &error, text);
                exit_for(&error)
            }
        };
        self.time(console, started.elapsed());
        Step::Continue(exit)
    }

    fn time(&self, console: &mut Console<'_>, elapsed: Duration) {
        if self.timer {
            let millis = elapsed.as_secs_f64() * 1000.0;
            let text = if millis >= 1000.0 {
                format!("Time: {:.3} s\n", millis / 1000.0)
            } else {
                format!("Time: {millis:.3} ms\n")
            };
            self.print_err_styled(console, Style::Dim, &text);
        }
    }

    fn print_sql_error(&self, console: &mut Console<'_>, error: &eggdb_sql::SqlError, text: &str) {
        print_sql_error(console, self.color_err, error, text);
    }

    fn print_notice(&self, console: &mut Console<'_>, notice: &str) {
        print_notice(console, self.color_err, notice);
    }

    fn print(&self, console: &mut Console<'_>, text: &str) -> bool {
        let mut out = Output::new(&mut *console.stdout, false);
        out.text(text);
        out.finish().is_ok()
    }

    fn print_err_styled(&self, console: &mut Console<'_>, style: Style, text: &str) {
        let mut messages = Output::new(&mut *console.stderr, self.color_err);
        messages.styled(style, text.trim_end_matches('\n'));
        messages.text("\n");
        let _ = messages.finish();
    }

    fn fail(&self, console: &mut Console<'_>, message: &str, exit: Exit) -> Step {
        let mut messages = Output::new(&mut *console.stderr, self.color_err);
        crate::cli::error_line(&mut messages, message);
        let _ = messages.finish();
        Step::Continue(exit)
    }

    fn meta(&mut self, console: &mut Console<'_>, line: &str) -> Step {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        let argument = words.next();
        if words.next().is_some() {
            return self.fail(
                console,
                &format!("too many arguments for {command}. Type .help for the list."),
                Exit::Usage,
            );
        }
        let started = Instant::now();
        match (command, argument) {
            (".quit" | ".exit", None) => Step::Quit,
            (".help", None) => {
                let text = self.help_text();
                self.print(console, &text);
                Step::Continue(Exit::Done)
            }
            (".tables", None) => match self.sql.tables() {
                Ok(tables) if tables.is_empty() => {
                    let mut out = Output::new(&mut *console.stdout, self.color_out);
                    out.styled(Style::Dim, "no tables");
                    out.text("\n");
                    let _ = out.finish();
                    Step::Continue(Exit::Done)
                }
                Ok(tables) => {
                    let names: String = tables
                        .iter()
                        .map(|table| format!("{}\n", eggdb_sql::quoted(&table.name)))
                        .collect();
                    self.print(console, &names);
                    Step::Continue(Exit::Done)
                }
                Err(error) => {
                    self.print_sql_error(console, &error, "");
                    Step::Continue(exit_for(&error))
                }
            },
            (".schema", table) => {
                let step = self.schema(console, table);
                self.time(console, started.elapsed());
                step
            }
            (".mode", None) => {
                let text = format!("mode: {}\n", self.mode.name());
                self.print(console, &text);
                Step::Continue(Exit::Done)
            }
            (".mode", Some(name)) => match ResultMode::parse(name) {
                Some(mode) => {
                    self.mode = mode;
                    self.print(console, &format!("mode: {}\n", mode.name()));
                    Step::Continue(Exit::Done)
                }
                None => self.fail(
                    console,
                    &format!(".mode takes table, expanded or csv, not '{name}'"),
                    Exit::Usage,
                ),
            },
            (".timer", None) => {
                let text = format!("timer: {}\n", if self.timer { "on" } else { "off" });
                self.print(console, &text);
                Step::Continue(Exit::Done)
            }
            (".timer", Some(state @ ("on" | "off"))) => {
                self.timer = state == "on";
                self.print(console, &format!("timer: {state}\n"));
                Step::Continue(Exit::Done)
            }
            (".timer", Some(other)) => self.fail(
                console,
                &format!(".timer takes on or off, not '{other}'"),
                Exit::Usage,
            ),
            (known, Some(_)) if META_COMMANDS.contains(&known) => self.fail(
                console,
                &format!("{known} takes no argument. Type .help for the list."),
                Exit::Usage,
            ),
            (unknown, _) => self.fail(
                console,
                &format!("unknown command '{unknown}'. Type .help for the list."),
                Exit::Usage,
            ),
        }
    }

    fn schema(&mut self, console: &mut Console<'_>, table: Option<&str>) -> Step {
        let tables = match self.sql.tables() {
            Ok(tables) => tables,
            Err(error) => {
                self.print_sql_error(console, &error, "");
                return Step::Continue(exit_for(&error));
            }
        };
        let chosen: Vec<&TableSchema> = match table {
            None => tables.iter().collect(),
            Some(name) => {
                let unquoted = name
                    .strip_prefix('"')
                    .and_then(|name| name.strip_suffix('"'))
                    .map_or_else(|| name.to_lowercase(), |name| name.replace("\"\"", "\""));
                let found: Vec<&TableSchema> = tables
                    .iter()
                    .filter(|table| table.name == unquoted)
                    .collect();
                if found.is_empty() {
                    return self.fail(console, &format!("no table named {name}"), Exit::NotFound);
                }
                found
            }
        };
        let text: String = chosen
            .iter()
            .map(|table| format!("{}\n", table.create_statement_in(self.sql.dialect())))
            .collect();
        self.print(console, &text);
        Step::Continue(Exit::Done)
    }

    fn help_text(&self) -> String {
        let history = match &self.history_path {
            Some(path) => format!(
                "History: {} ({HISTORY_VARIABLE}=<path> moves it; set empty, turns it off)",
                path.display()
            ),
            None => format!("History: off ({HISTORY_VARIABLE}=<path> keeps it in a file)"),
        };
        format!(
            "Key-value commands, as on the command line without 'eggdb <command> {file}':
  put <key> <value>            store a value (text, 0x.. hex, @file, =text)
  get <key>                    print a value
  del <key>                    remove a key
  scan [--prefix p] [--from a] [--to b] [-r] [-n count] [-k]
  check                        read every page and report the file's health
  stats                        size, pages, last commit and log
SQL: the PostgreSQL dialect, or MySQL's or SQLite's with --dialect, statements ended by ;
  (docs/sql.md); BEGIN spans prompts:
  {stem}=*> is inside a transaction block, {stem}=!> a failed one (ROLLBACK ends it).
  'delete' starts a DELETE statement here; the key-value command is 'del'.
Shell commands:
  .tables                      list the tables
  .schema [table]              CREATE TABLE statements, all or one
  .mode [table|expanded|csv]   how results print; no argument shows the current mode
  .timer [on|off]              print each command's time
  .help                        this text
  .quit  .exit                 leave (so does Ctrl-D on an empty line)
Keys: Tab completes, Up/Down and Ctrl-R recall, Ctrl-C clears the line, Ctrl-L the screen.
{history}
",
            file = self.file,
            stem = self.stem,
        )
    }

    fn key_value(&mut self, console: &mut Console<'_>, line: &str) -> Step {
        let started = Instant::now();
        let words = match split_words(line) {
            Ok(words) => words,
            Err(message) => return self.fail(console, &message, Exit::Usage),
        };
        if words.iter().any(|word| word == "@-") {
            return self.fail(
                console,
                "@- reads stdin, which is the shell's own; use @file",
                Exit::Usage,
            );
        }
        let positional = words.iter().skip(1).filter(|word| !word.starts_with('-'));
        if words[0] == "put" && positional.count() < 2 {
            return self.fail(
                console,
                "put needs a key and a value here; stdin is the shell's own, so give the value or @file",
                Exit::Usage,
            );
        }
        let mut arguments: Vec<OsString> = vec![OsString::from(&words[0])];
        arguments.push(self.request.file.clone().into_os_string());
        arguments.extend(words[1..].iter().map(OsString::from));
        let terminal = Terminal {
            stdin: true,
            stdout: true,
            stderr: console.terminal.stderr,
            stdout_color: self.color_out,
            stderr_color: self.color_err,
        };
        let mut empty = std::io::empty();
        let mut inner = Console {
            stdin: &mut empty,
            stdout: &mut *console.stdout,
            stderr: &mut *console.stderr,
            terminal,
        };
        let exit = match args::parse(&arguments) {
            Ok(Invocation::Run(request)) => {
                let color = request.color;
                match execute_on(*request, &mut inner, Some(self.db)) {
                    Ok(exit) => exit,
                    Err(Failure::OutputClosed) => return Step::Quit,
                    Err(failure) => report(&failure, &mut inner, color),
                }
            }
            Ok(Invocation::Help(Some(command))) => {
                let mut out = Output::new(&mut *inner.stdout, false);
                out.text(&help::command_help(command));
                let _ = out.finish();
                Exit::Done
            }
            Ok(_) => Exit::Done,
            Err(failure) => report(&failure, &mut inner, self.request.color),
        };
        self.time(console, started.elapsed());
        Step::Continue(exit)
    }

    fn complete(&mut self, text: &str, cursor: usize) -> Completion {
        let before = &text[..cursor.min(text.len())];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace() || "(),;=<>+*/".contains(*c))
            .map_or(0, |(index, c)| index + c.len_utf8());
        let word = &before[start..];
        let previous: Vec<&str> = before[..start].split_whitespace().collect();
        let continuing = !self.buffer.trim().is_empty();
        let first = previous.first().copied();
        let mut completion = Completion {
            start,
            candidates: Vec::new(),
            add_space: true,
        };
        if !continuing && previous.is_empty() {
            let mut words: Vec<String> = KEY_VALUE_COMMANDS
                .iter()
                .chain(META_COMMANDS.iter())
                .map(|word| (*word).to_owned())
                .filter(|candidate| candidate.starts_with(word))
                .collect();
            words.extend(keywords_matching(&STATEMENT_KEYWORDS, word));
            completion.candidates = words;
            return completion;
        }
        if !continuing && let Some(first) = first {
            if KEY_VALUE_COMMANDS.contains(&first) {
                let last = previous.last().copied().unwrap_or_default();
                let takes_key = (matches!(first, "get" | "del") && previous.len() == 1)
                    || (first == "scan" && matches!(last, "--prefix" | "-p" | "--from" | "--to"));
                if takes_key {
                    completion.candidates = self.keys_starting_with(word);
                    completion.add_space = false;
                }
                return completion;
            }
            if first.starts_with('.') {
                completion.candidates = match (first, previous.len()) {
                    (".schema", 1) => self.table_names(word),
                    (".mode", 1) => ["table", "expanded", "csv"]
                        .iter()
                        .filter(|mode| mode.starts_with(word))
                        .map(|mode| (*mode).to_owned())
                        .collect(),
                    (".timer", 1) => ["on", "off"]
                        .iter()
                        .filter(|state| state.starts_with(word))
                        .map(|state| (*state).to_owned())
                        .collect(),
                    _ => Vec::new(),
                };
                return completion;
            }
        }
        let last = previous.last().map(|word| word.to_ascii_uppercase());
        if last
            .as_deref()
            .is_some_and(|last| TABLE_KEYWORDS.contains(&last))
        {
            completion.candidates = self.table_names(word);
            return completion;
        }
        let statement = format!("{}\n{before}", self.buffer);
        let mut candidates = self.columns_named_in(&statement, word);
        candidates.extend(keywords_matching(&SQL_WORDS, word));
        candidates.extend(
            FUNCTIONS
                .iter()
                .filter(|name| name.starts_with(&word.to_ascii_lowercase()))
                .map(|name| (*name).to_owned()),
        );
        candidates.dedup();
        completion.candidates = candidates;
        completion
    }

    fn table_names(&mut self, word: &str) -> Vec<String> {
        let tables = self.sql.tables().unwrap_or_default();
        tables
            .iter()
            .map(|table| eggdb_sql::quoted(&table.name))
            .filter(|name| name.starts_with(word))
            .collect()
    }

    fn columns_named_in(&mut self, statement: &str, word: &str) -> Vec<String> {
        let mentioned: Vec<String> = statement
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '"'))
            .map(|name| name.trim_matches('"').to_lowercase())
            .collect();
        let mut columns = Vec::new();
        for table in self.sql.tables().unwrap_or_default() {
            if mentioned.contains(&table.name.to_lowercase()) {
                for column in &table.columns {
                    let name = eggdb_sql::quoted(&column.name);
                    if name.starts_with(word) && !columns.contains(&name) {
                        columns.push(name);
                    }
                }
            }
        }
        columns
    }

    fn keys_starting_with(&self, word: &str) -> Vec<String> {
        let Ok(transaction) = self.db.begin_read() else {
            return Vec::new();
        };
        let mut scan = transaction.scan_prefix(word.as_bytes());
        let mut keys = Vec::new();
        while keys.len() < KEY_CANDIDATES {
            match scan.next_entry() {
                Ok(Some((key, _))) => {
                    if key.starts_with(eggdb_sql::RESERVED_KEY_PREFIX) {
                        continue;
                    }
                    if let Ok(text) = std::str::from_utf8(key)
                        && !text.is_empty()
                        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
                    {
                        keys.push(text.to_owned());
                    }
                }
                _ => break,
            }
        }
        keys
    }

    fn append_history(&mut self, console: &mut Console<'_>, entry: &str) {
        let Some(path) = &self.history_path else {
            return;
        };
        let written = user_files::open_private_append(path)
            .and_then(|mut file| file.write_all(format!("{}\n", escape_entry(entry)).as_bytes()));
        if let Err(error) = written {
            let notice = format!("history off: cannot write {}: {error}", path.display());
            self.history_path = None;
            self.print_err_styled(console, Style::Dim, &notice);
        }
    }

    fn trim_history(&self, history: &[String]) {
        let Some(path) = &self.history_path else {
            return;
        };
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        if BufReader::new(file).lines().count() <= LineReader::MOST_HISTORY_ENTRIES {
            return;
        }
        let skip = history
            .len()
            .saturating_sub(LineReader::MOST_HISTORY_ENTRIES);
        let text: String = history[skip..]
            .iter()
            .map(|entry| format!("{}\n", escape_entry(entry)))
            .collect();
        if let Ok(mut file) = user_files::open_private_rewrite(path) {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

fn print_sql_error(
    console: &mut Console<'_>,
    color: bool,
    error: &eggdb_sql::SqlError,
    text: &str,
) {
    let mut messages = Output::new(&mut *console.stderr, color);
    for (index, line) in error_lines(error, text).iter().enumerate() {
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
    let _ = messages.finish();
}

fn print_notice(console: &mut Console<'_>, color: bool, notice: &str) {
    let mut messages = Output::new(&mut *console.stderr, color);
    match notice.strip_prefix("WARNING:") {
        Some(rest) => {
            messages.styled(Style::Yellow, "WARNING:");
            messages.text(rest);
        }
        None => messages.text(notice),
    }
    messages.text("\n");
    let _ = messages.finish();
}

fn keywords_matching(words: &[&str], typed: &str) -> Vec<String> {
    let upper = typed.to_ascii_uppercase();
    let lower = typed.chars().any(|c| c.is_ascii_lowercase())
        && !typed.chars().any(|c| c.is_ascii_uppercase());
    words
        .iter()
        .filter(|word| word.starts_with(&upper))
        .map(|word| {
            if lower {
                word.to_ascii_lowercase()
            } else {
                (*word).to_owned()
            }
        })
        .collect()
}

fn prompt_stem(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "eggdb".to_owned());
    if stem.chars().count() > LONGEST_STEM {
        let cut: String = stem.chars().take(LONGEST_STEM - 1).collect();
        format!("{cut}\u{2026}")
    } else {
        stem
    }
}

fn history_location() -> (Option<PathBuf>, Option<String>) {
    let path = match std::env::var_os(HISTORY_VARIABLE) {
        Some(value) if value.is_empty() => return (None, None),
        Some(value) => PathBuf::from(value),
        None => match user_files::state_directory() {
            Some(directory) => directory.join("history"),
            None => return (None, None),
        },
    };
    let prepared = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            user_files::create_private_directory(parent)
        }
        _ => Ok(()),
    }
    .and_then(|()| user_files::open_private_append(&path).map(drop));
    match prepared {
        Ok(()) => (Some(path), None),
        Err(error) => {
            let notice = format!("history off: cannot create {}: {error}", path.display());
            (None, Some(notice))
        }
    }
}

fn read_history(path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    history_entries(&bytes)
}

fn history_entries(bytes: &[u8]) -> Vec<String> {
    let start = bytes.len().saturating_sub(MOST_HISTORY_BYTES);
    let mut kept = &bytes[start..];
    if start > 0 {
        let first_break = kept.iter().position(|&byte| byte == b'\n');
        kept = first_break.map_or(&[][..], |at| &kept[at + 1..]);
    }
    kept.split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| unescape_entry(&String::from_utf8_lossy(line)))
        .collect()
}

fn escape_entry(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_entry(line: &str) -> String {
    let mut entry = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(c) = characters.next() {
        if c != '\\' {
            entry.push(c);
            continue;
        }
        match characters.next() {
            Some('n') => entry.push('\n'),
            Some(other) => entry.push(other),
            None => entry.push('\\'),
        }
    }
    entry
}

fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut characters = line.chars();
    while let Some(c) = characters.next() {
        match c {
            '"' => {
                in_word = true;
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some('\\') => match characters.next() {
                            Some(escaped @ ('"' | '\\')) => word.push(escaped),
                            Some(other) => {
                                word.push('\\');
                                word.push(other);
                            }
                            None => return Err("unfinished \"...\" in the line".to_owned()),
                        },
                        Some(other) => word.push(other),
                        None => return Err("unfinished \"...\" in the line".to_owned()),
                    }
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match characters.next() {
                        Some('\'') => break,
                        Some(other) => word.push(other),
                        None => return Err("unfinished '...' in the line".to_owned()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn scan_sql(text: &str) -> Pending {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut complete_to = 0;
    let mut open = Open::None;
    while index < bytes.len() {
        match bytes[index] {
            b';' => {
                complete_to = index + 1;
                index += 1;
            }
            b'\'' | b'"' => {
                let quote = bytes[index];
                let escapes = quote == b'\''
                    && index > 0
                    && matches!(bytes[index - 1], b'E' | b'e')
                    && (index < 2 || !is_identifier_byte(bytes[index - 2]));
                index += 1;
                open = if quote == b'\'' {
                    Open::Single
                } else {
                    Open::Double
                };
                while index < bytes.len() {
                    if escapes && bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    if bytes[index] == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            index += 2;
                            continue;
                        }
                        open = Open::None;
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let mut depth = 1;
                index += 2;
                open = Open::Other;
                while index < bytes.len() {
                    if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                        depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        depth -= 1;
                        index += 2;
                        if depth == 0 {
                            open = Open::None;
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'$' => match dollar_tag(&bytes[index..]) {
                Some(tag) => {
                    index += tag.len();
                    open = Open::Other;
                    match find(&bytes[index..], tag) {
                        Some(at) => {
                            index += at + tag.len();
                            open = Open::None;
                        }
                        None => index = bytes.len(),
                    }
                }
                None => index += 1,
            },
            _ => index += 1,
        }
    }
    Pending { complete_to, open }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn dollar_tag(bytes: &[u8]) -> Option<&[u8]> {
    let rest = bytes.get(1..)?;
    let end = rest.iter().position(|&b| b == b'$')?;
    let name = &rest[..end];
    let valid = name.first().is_none_or(|b| !b.is_ascii_digit())
        && name
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80);
    valid.then(|| &bytes[..end + 2])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{
        MOST_HISTORY_BYTES, Open, escape_entry, history_entries, prompt_stem, scan_sql,
        split_words, unescape_entry,
    };
    use std::path::Path;

    #[test]
    fn a_statement_ends_at_a_semicolon_outside_quotes_comments_and_dollar_bodies() {
        assert_eq!(scan_sql("select 1").complete_to, 0);
        assert_eq!(scan_sql("select 1; select").complete_to, 9);
        assert_eq!(scan_sql("select ';'").complete_to, 0);
        assert_eq!(scan_sql("select 'it''s;';").complete_to, 16);
        assert_eq!(scan_sql("select 1 -- ;\n").complete_to, 0);
        assert_eq!(scan_sql("select /* ; /* ; */ ; */ 1;").complete_to, 27);
        assert_eq!(scan_sql("select $$;$$;").complete_to, 13);
        assert_eq!(scan_sql("select $a$ ; $a$").complete_to, 0);
        assert_eq!(scan_sql("select \"a;\";").complete_to, 12);
        assert_eq!(scan_sql("select E'it\\'s;';").complete_to, 17);
        assert_eq!(scan_sql("select e'\\\\';").complete_to, 13);
        assert_eq!(scan_sql("select 'a\\';").complete_to, 12);
        assert_eq!(scan_sql("select 'open").open, Open::Single);
        assert_eq!(scan_sql("select \"open").open, Open::Double);
        assert_eq!(scan_sql("select 'closed'").open, Open::None);
    }

    #[test]
    fn words_split_like_a_shell() {
        assert_eq!(
            split_words(r#"put "a key" 'it is' x\y "q\"\\""#).ok(),
            Some(vec![
                "put".to_owned(),
                "a key".to_owned(),
                "it is".to_owned(),
                "x\\y".to_owned(),
                "q\"\\".to_owned()
            ])
        );
        assert_eq!(
            split_words("get ''").ok(),
            Some(vec!["get".into(), String::new()])
        );
        assert!(split_words("get \"open").is_err());
    }

    #[test]
    fn history_entries_keep_line_breaks_and_backslashes() {
        let entry = "select 'a\\b'\nfrom t;";
        assert_eq!(escape_entry(entry), "select 'a\\\\b'\\nfrom t;");
        assert_eq!(unescape_entry(&escape_entry(entry)), entry);
    }

    #[test]
    fn a_history_file_is_read_from_its_last_mebibyte_and_lossily() {
        assert_eq!(
            history_entries(b"a\nb\xff\r\n\nc\\nd\n"),
            ["a", "b\u{fffd}", "c\nd"]
        );
        let mut big = vec![b'x'; MOST_HISTORY_BYTES];
        big.extend_from_slice(b"\nlast\n");
        assert_eq!(history_entries(&big), ["last"]);
    }

    #[test]
    fn the_prompt_names_the_file_stem_cut_at_24_characters() {
        assert_eq!(prompt_stem(Path::new("dir/app.db")), "app");
        let long = prompt_stem(Path::new("a_very_long_database_name_indeed.db"));
        assert_eq!(long.chars().count(), 24);
        assert!(long.ends_with('\u{2026}'));
    }
}
