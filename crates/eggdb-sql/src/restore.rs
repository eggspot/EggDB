mod classify;
mod reader;
mod words;

use crate::copy::{CopyChannel, CopyStart};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::store::KeyValueStore;
use classify::{Action, Effect, Ignored, Skip};
use reader::{DumpReader, DumpStatement, Item, MetaCommand};
use std::collections::HashMap;
use std::io::BufRead;
use words::{Token, Words};

/// The most items each list of a [`RestoreReport`] holds; past it, only a count grows.
pub const MAX_LISTED: usize = 10_000;

const MAX_REMEMBERED_NAMES: usize = 100_000;
const SHOWN_STATEMENT_CHARS: usize = 48;
const PROGRESS_EVERY_LINES: u64 = 4096;

/// How [`restore_dump`] runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Record an error in a supported statement and go on, instead of stopping at it.
    pub keep_going: bool,
}

/// Where a restore has got to, handed to the progress callback of [`restore_dump`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreProgress {
    /// The line of the dump read last.
    pub line: u64,
    /// The tables created so far.
    pub tables: u64,
    /// The rows loaded so far.
    pub rows: u64,
}

/// The rows a restore loaded into one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRows {
    /// The table's name as the dump writes it, such as `public.orders`.
    pub table: String,
    /// The rows loaded into it.
    pub rows: u64,
}

/// An object of the dump that was not created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedObject {
    /// The line of the dump where its statement starts.
    pub line: u64,
    /// Its kind, in `pg_dump`'s lower-case words: `function`, `trigger`, `table`, ...
    pub kind: String,
    /// Its name as the dump writes it.
    pub name: String,
    /// Why it was not created.
    pub reason: String,
    /// More about it: `partitioned`, `inherited`, `partition` or `not supported yet`.
    pub detail: Option<String>,
    /// For a table, the rows of the dump that were not loaded because it was skipped.
    pub rows: Option<u64>,
}

/// A statement that failed only because it uses an object that was skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct Consequence {
    /// The line of the dump where the statement starts.
    pub line: u64,
    /// The start of the statement, cut after 48 characters.
    pub statement: String,
    /// The skipped object it uses, such as `function public.touch()`.
    pub uses: String,
    /// The error the statement failed with.
    pub error: SqlError,
}

/// A psql meta-command of the dump that was not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredMetaCommand {
    /// Its line in the dump.
    pub line: u64,
    /// The command, such as `\connect`.
    pub command: String,
    /// Why it was not run.
    pub reason: String,
}

/// The statements and meta-commands a restore read and deliberately did not run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoredStatements {
    /// `ALTER ... OWNER TO` statements: EggDB has no roles yet.
    pub owner_to: u64,
    /// `GRANT` statements.
    pub grant: u64,
    /// `REVOKE` statements.
    pub revoke: u64,
    /// The run-time parameters of `SET` statements EggDB does not have, each named once.
    pub set: Vec<String>,
    /// How many `SET` statements named a parameter EggDB does not have.
    pub set_statements: u64,
    /// Other kinds of statement with no effect in EggDB, with how many there were, such as
    /// `storage settings` or `ALTER DEFAULT PRIVILEGES`.
    pub other: Vec<(String, u64)>,
    /// Meta-commands that were not run, such as `\connect`.
    pub meta_commands: Vec<IgnoredMetaCommand>,
    /// Meta-commands not run beyond the [`MAX_LISTED`] in `meta_commands`.
    pub meta_commands_not_listed: u64,
}

/// A statement of the dump that failed.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreError {
    /// The line of the dump where it failed: the statement's line, the line its error points
    /// at, or the line of the `COPY` data that broke.
    pub line: u64,
    /// The start of the statement, cut after 48 characters; empty when the dump itself could
    /// not be read.
    pub statement: String,
    /// The error.
    pub error: SqlError,
    /// How many statements of the dump were read before it.
    pub statements_before: u64,
}

/// What a restore did: what it created and loaded, what it skipped, ignored and why, and the
/// errors it met.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RestoreReport {
    /// The statements read from the dump, run or not.
    pub statements: u64,
    /// Schemas created.
    pub schemas: u64,
    /// Tables created.
    pub tables: u64,
    /// Rows loaded, by `COPY` or `INSERT`.
    pub rows: u64,
    /// Views created.
    pub views: u64,
    /// Sequences created, identity columns' sequences included.
    pub sequences: u64,
    /// Indexes created.
    pub indexes: u64,
    /// Constraints created, in `CREATE TABLE` or `ALTER TABLE ... ADD`.
    pub constraints: u64,
    /// Comments set.
    pub comments: u64,
    /// Tables of the dump that were not created.
    pub tables_skipped: u64,
    /// Rows of the dump that were not loaded because their table was not created.
    pub rows_skipped: u64,
    /// The rows loaded into each table, in the order the tables were created or first loaded.
    pub rows_per_table: Vec<TableRows>,
    /// Tables loaded beyond the [`MAX_LISTED`] in `rows_per_table`.
    pub tables_not_listed: u64,
    /// Objects not created.
    pub skipped: Vec<SkippedObject>,
    /// Objects not created beyond the [`MAX_LISTED`] in `skipped`.
    pub skipped_not_listed: u64,
    /// Statements that failed only because they use a skipped object.
    pub consequences: Vec<Consequence>,
    /// Consequences beyond the [`MAX_LISTED`] in `consequences`.
    pub consequences_not_listed: u64,
    /// What was read and deliberately not run.
    pub ignored: IgnoredStatements,
    /// With [`RestoreOptions::keep_going`], the statements that failed and were passed over.
    pub errors: Vec<RestoreError>,
    /// Errors beyond the [`MAX_LISTED`] in `errors`.
    pub errors_not_listed: u64,
    /// The error that stopped the restore, if one did.
    pub error: Option<RestoreError>,
}

impl RestoreReport {
    /// How many objects were skipped, listed or not.
    pub fn skipped_count(&self) -> u64 {
        self.skipped.len() as u64 + self.skipped_not_listed
    }

    /// How many consequences there were, listed or not.
    pub fn consequence_count(&self) -> u64 {
        self.consequences.len() as u64 + self.consequences_not_listed
    }

    /// How many errors were passed over with [`RestoreOptions::keep_going`].
    pub fn error_count(&self) -> u64 {
        self.errors.len() as u64 + self.errors_not_listed
    }

    /// Whether the restore finished with no error, stopping or passed over.
    pub fn succeeded(&self) -> bool {
        self.error.is_none() && self.error_count() == 0
    }
}

/// Restores the dump read from `input` through `session`, statement by statement, each in its
/// own transaction as `psql -f` runs it (a `COPY` and its data in one), and reports what it
/// did. The dump is PostgreSQL's plain SQL form: `pg_dump`'s output, or `pg_restore -f`'s.
///
/// The dump is read one line at a time (lines and statements up to 1 GiB), never whole.
/// psql's meta-commands are handled as psql would for what `pg_dump` writes: `\connect` is
/// noted and ignored (one database per file), `\restrict` and `\unrestrict` must carry the same
/// key, and no other meta-command is ever run. Objects EggDB cannot create yet (functions,
/// triggers, types, partitioned tables, ...) are skipped and listed; `GRANT`, `REVOKE`,
/// `OWNER TO` and unknown `SET` parameters are counted and ignored; a statement that fails only
/// because it uses a skipped object is a consequence, and a schema statement that EggDB answers
/// with `0A000` is skipped as not supported yet. Any other error stops the restore, unless
/// [`RestoreOptions::keep_going`], with the statements before it committed; `progress` is
/// called as the restore moves on. The session must use the PostgreSQL dialect and be outside
/// a transaction block.
pub fn restore_dump<S: KeyValueStore>(
    session: &mut Session<'_, S>,
    input: &mut dyn BufRead,
    options: RestoreOptions,
    progress: &mut dyn FnMut(&RestoreProgress),
) -> RestoreReport {
    let mut report = RestoreReport::default();
    if session.dialect() != Dialect::PostgreSql {
        report.error = Some(RestoreError {
            line: 0,
            statement: String::new(),
            error: SqlError::not_supported("restoring a dump in a dialect other than PostgreSQL"),
            statements_before: 0,
        });
        return report;
    }
    let mut restore = Restore {
        session,
        reader: DumpReader::new(input),
        report,
        options,
        progress,
        restricted: None,
        skipped_names: HashMap::new(),
        skipped_tables: HashMap::new(),
        table_rows: HashMap::new(),
    };
    restore.run();
    restore.report
}

enum Flow {
    Continue,
    Stop,
}

struct SkippedTable {
    listed: Option<usize>,
}

struct Restore<'a, 's, S: KeyValueStore> {
    session: &'a mut Session<'s, S>,
    reader: DumpReader<'a>,
    report: RestoreReport,
    options: RestoreOptions,
    progress: &'a mut dyn FnMut(&RestoreProgress),
    restricted: Option<(String, u64)>,
    skipped_names: HashMap<String, String>,
    skipped_tables: HashMap<String, SkippedTable>,
    table_rows: HashMap<String, usize>,
}

impl<S: KeyValueStore> Restore<'_, '_, S> {
    fn run(&mut self) {
        loop {
            let item = match self.reader.next_item() {
                Ok(item) => item,
                Err(error) => {
                    self.report.error = Some(RestoreError {
                        line: self.reader.line_number(),
                        statement: String::new(),
                        error,
                        statements_before: self.report.statements,
                    });
                    return;
                }
            };
            let flow = match item {
                Item::End => return,
                Item::Meta(meta) => self.meta_command(meta),
                Item::Statement(statement) => {
                    self.report.statements += 1;
                    self.statement(statement)
                }
            };
            if let Flow::Stop = flow {
                return;
            }
            (self.progress)(&RestoreProgress {
                line: self.reader.line_number(),
                tables: self.report.tables,
                rows: self.report.rows,
            });
        }
    }

    fn meta_command(&mut self, meta: MetaCommand) -> Flow {
        match meta.name.as_str() {
            "restrict" => {
                self.restricted = Some((meta.argument, meta.line));
                Flow::Continue
            }
            "unrestrict" => match self.restricted.take() {
                Some((key, _)) if key == meta.argument => Flow::Continue,
                Some((_, line)) => self.fail_meta(
                    &meta,
                    format!("\\unrestrict key does not match the \\restrict key of line {line}"),
                ),
                None => self.fail_meta(
                    &meta,
                    "\\unrestrict without a \\restrict before it".to_owned(),
                ),
            },
            "." => Flow::Continue,
            "connect" | "c" => {
                self.note_meta(&meta, "one database per file");
                Flow::Continue
            }
            _ => {
                self.note_meta(&meta, "never run");
                Flow::Continue
            }
        }
    }

    fn fail_meta(&mut self, meta: &MetaCommand, message: String) -> Flow {
        let error = SqlError::new(SqlState::SYNTAX_ERROR, message)
            .with_hint("The dump was altered after pg_dump wrote it; take it again.");
        self.genuine_error(RestoreError {
            line: meta.line,
            statement: format!("\\{}", meta.name),
            error,
            statements_before: self.report.statements,
        })
    }

    fn note_meta(&mut self, meta: &MetaCommand, reason: &str) {
        let ignored = &mut self.report.ignored;
        if ignored.meta_commands.len() < MAX_LISTED {
            ignored.meta_commands.push(IgnoredMetaCommand {
                line: meta.line,
                command: format!("\\{}", meta.name),
                reason: reason.to_owned(),
            });
        } else {
            ignored.meta_commands_not_listed += 1;
        }
    }

    fn statement(&mut self, statement: DumpStatement) -> Flow {
        let session = &*self.session;
        let action = classify::classify(&statement.text, &|name| session.parameter(name).is_ok());
        match action {
            Action::Ignore(kind) => {
                self.ignore(kind);
                Flow::Continue
            }
            Action::Skip(skip) => {
                self.skip(&statement, skip);
                Flow::Continue
            }
            Action::Run(effect) => {
                if let Some(table) = effect.data_target()
                    && let Some(key) = self.skipped_table_key(table)
                {
                    self.data_of_skipped_table(&statement, &key);
                    return Flow::Continue;
                }
                self.execute(statement, &effect)
            }
        }
    }

    fn ignore(&mut self, kind: Ignored) {
        let ignored = &mut self.report.ignored;
        match kind {
            Ignored::OwnerTo => ignored.owner_to += 1,
            Ignored::Grant => ignored.grant += 1,
            Ignored::Revoke => ignored.revoke += 1,
            Ignored::Set(name) => {
                ignored.set_statements += 1;
                if !ignored.set.contains(&name) && ignored.set.len() < MAX_LISTED {
                    ignored.set.push(name);
                }
            }
            Ignored::Other(kind) => match ignored.other.iter_mut().find(|(known, _)| known == kind)
            {
                Some((_, count)) => *count += 1,
                None => ignored.other.push((kind.to_owned(), 1)),
            },
        }
    }

    fn skip(&mut self, statement: &DumpStatement, skip: Skip) {
        let rows = if statement.copy_from_stdin {
            self.reader.skip_copy_data().ok()
        } else {
            None
        };
        if skip.names_an_object {
            self.remember_skipped(skip.kind, &skip.name);
        }
        let listed = self.list_skipped(SkippedObject {
            line: statement.line,
            kind: skip.kind.to_owned(),
            name: skip.name.clone(),
            reason: skip.reason.to_owned(),
            detail: skip.detail.map(str::to_owned),
            rows: skip.is_table.then_some(rows.unwrap_or(0)),
        });
        if skip.is_table {
            self.report.tables_skipped += 1;
            self.remember_skipped_table(&skip.name, listed);
        }
    }

    fn list_skipped(&mut self, object: SkippedObject) -> Option<usize> {
        if self.report.skipped.len() < MAX_LISTED {
            self.report.skipped.push(object);
            Some(self.report.skipped.len() - 1)
        } else {
            self.report.skipped_not_listed += 1;
            None
        }
    }

    fn remember_skipped(&mut self, kind: &str, name: &str) {
        let shown = format!("{kind} {name}");
        let base = name.split(['(', ' ']).next().unwrap_or(name);
        for key in [base, base.rsplit('.').next().unwrap_or(base)] {
            if key.is_empty() || self.skipped_names.len() >= MAX_REMEMBERED_NAMES {
                continue;
            }
            self.skipped_names
                .entry(key.to_owned())
                .or_insert_with(|| shown.clone());
        }
    }

    fn remember_skipped_table(&mut self, name: &str, listed: Option<usize>) {
        if self.skipped_tables.len() < MAX_REMEMBERED_NAMES {
            self.skipped_tables
                .insert(table_key(name), SkippedTable { listed });
        }
    }

    fn skipped_table_key(&self, table: &str) -> Option<String> {
        let key = table_key(table);
        self.skipped_tables.contains_key(&key).then_some(key)
    }

    fn data_of_skipped_table(&mut self, statement: &DumpStatement, key: &str) {
        let rows = if statement.copy_from_stdin {
            self.reader.skip_copy_data().unwrap_or_default()
        } else {
            classify::insert_rows(&statement.text)
        };
        self.report.rows_skipped += rows;
        if let Some(SkippedTable {
            listed: Some(index),
        }) = self.skipped_tables.get(key)
            && let Some(object) = self.report.skipped.get_mut(*index)
        {
            *object.rows.get_or_insert(0) += rows;
        }
    }

    fn execute(&mut self, statement: DumpStatement, effect: &Effect) -> Flow {
        let mut loaded = 0u64;
        let mut copy = DumpCopy {
            reader: &mut self.reader,
            progress: &mut *self.progress,
            tables: self.report.tables,
            rows: self.report.rows,
        };
        let outcome = self.session.execute_with_copy(
            &statement.text,
            &mut |result: QueryResult| {
                loaded += rows_of(&result.tag);
                true
            },
            &mut copy,
        );
        if statement.copy_from_stdin
            && let Err(error) = self.reader.skip_copy_data()
        {
            self.report.error = Some(RestoreError {
                line: self.reader.line_number(),
                statement: String::new(),
                error,
                statements_before: self.report.statements,
            });
            return Flow::Stop;
        }
        match outcome {
            Ok(()) => {
                self.count(effect, loaded);
                Flow::Continue
            }
            Err(error) => self.failed(&statement, effect, error),
        }
    }

    fn count(&mut self, effect: &Effect, loaded: u64) {
        let report = &mut self.report;
        match effect {
            Effect::Schema(_) => report.schemas += 1,
            Effect::Table { name, constraints } => {
                report.tables += 1;
                report.constraints += constraints;
                self.add_rows(name, 0);
            }
            Effect::View(_) => report.views += 1,
            Effect::Sequence(_) => report.sequences += 1,
            Effect::Index(_) => report.indexes += 1,
            Effect::AlterTable {
                constraints,
                identities,
            } => {
                report.constraints += constraints;
                report.sequences += identities;
            }
            Effect::Comment => report.comments += 1,
            Effect::Copy(table) | Effect::Insert(table) => {
                report.rows += loaded;
                self.add_rows(table, loaded);
            }
            Effect::Setting | Effect::Data | Effect::OtherDdl | Effect::Other => {}
        }
    }

    fn add_rows(&mut self, table: &str, rows: u64) {
        let key = table_key(table);
        if let Some(&index) = self.table_rows.get(&key) {
            self.report.rows_per_table[index].rows += rows;
            return;
        }
        if self.report.rows_per_table.len() >= MAX_LISTED {
            self.report.tables_not_listed += 1;
            return;
        }
        self.table_rows
            .insert(key, self.report.rows_per_table.len());
        self.report.rows_per_table.push(TableRows {
            table: table.to_owned(),
            rows,
        });
    }

    fn failed(&mut self, statement: &DumpStatement, effect: &Effect, error: SqlError) -> Flow {
        let state = error.state();
        let code = state.code();
        let missing_object = matches!(code, "42704" | "42883" | "42P01" | "3F000");
        let refused_definition = effect.is_ddl() && matches!(code, "0A000" | "42601" | "42809");
        if (missing_object || refused_definition)
            && let Some(uses) = self.uses_skipped(&statement.text)
        {
            if let Some((kind, name)) = effect.created() {
                self.remember_skipped(kind, name);
                if kind == "table" {
                    self.report.tables_skipped += 1;
                    self.remember_skipped_table(name, None);
                }
            }
            if self.report.consequences.len() < MAX_LISTED {
                self.report.consequences.push(Consequence {
                    line: statement.line,
                    statement: shortened(&statement.text),
                    uses,
                    error,
                });
            } else {
                self.report.consequences_not_listed += 1;
            }
            return Flow::Continue;
        }
        if error.state() == SqlState::FEATURE_NOT_SUPPORTED && effect.is_ddl() {
            let (kind, name) = effect
                .created()
                .map_or(("statement", shortened(&statement.text)), |(kind, name)| {
                    (kind, name.to_owned())
                });
            self.remember_skipped(kind, &name);
            let is_table = kind == "table";
            let listed = self.list_skipped(SkippedObject {
                line: statement.line,
                kind: kind.to_owned(),
                name: name.clone(),
                reason: error.message().to_owned(),
                detail: Some("not supported yet".to_owned()),
                rows: is_table.then_some(0),
            });
            if is_table {
                self.report.tables_skipped += 1;
                self.remember_skipped_table(&name, listed);
            }
            return Flow::Continue;
        }
        let line = error_line(statement, &error);
        self.genuine_error(RestoreError {
            line,
            statement: shortened(&statement.text),
            error,
            statements_before: self.report.statements.saturating_sub(1),
        })
    }

    fn genuine_error(&mut self, error: RestoreError) -> Flow {
        if !self.options.keep_going {
            self.report.error = Some(error);
            return Flow::Stop;
        }
        if self.report.errors.len() < MAX_LISTED {
            self.report.errors.push(error);
        } else {
            self.report.errors_not_listed += 1;
        }
        Flow::Continue
    }

    fn uses_skipped(&self, text: &str) -> Option<String> {
        if self.skipped_names.is_empty() {
            return None;
        }
        let mut name = String::new();
        let mut after_dot = false;
        for token in Words::new(text) {
            match token.name_part() {
                Some(part) => {
                    if after_dot {
                        name.push('.');
                        name.push_str(&part);
                    } else {
                        name = part.clone();
                    }
                    after_dot = false;
                    for key in [name.as_str(), part.as_str()] {
                        if let Some(shown) = self.skipped_names.get(key) {
                            return Some(shown.clone());
                        }
                    }
                }
                None => {
                    after_dot = token == Token::Symbol(b'.') && !name.is_empty();
                    if !after_dot {
                        name.clear();
                    }
                }
            }
        }
        None
    }
}

struct DumpCopy<'r, 'd> {
    reader: &'r mut DumpReader<'d>,
    progress: &'r mut dyn FnMut(&RestoreProgress),
    tables: u64,
    rows: u64,
}

impl CopyChannel for DumpCopy<'_, '_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        let before = self.reader.copy_lines();
        let more = self.reader.copy_data(data)?;
        if self.reader.copy_lines() / PROGRESS_EVERY_LINES != before / PROGRESS_EVERY_LINES {
            (self.progress)(&RestoreProgress {
                line: self.reader.line_number(),
                tables: self.tables,
                rows: self.rows + self.reader.copy_lines(),
            });
        }
        Ok(more)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

fn rows_of(tag: &str) -> u64 {
    let count = if let Some(rest) = tag.strip_prefix("COPY ") {
        rest
    } else if let Some(rest) = tag.strip_prefix("INSERT ") {
        rest.rsplit(' ').next().unwrap_or("0")
    } else {
        return 0;
    };
    count.parse().unwrap_or(0)
}

fn table_key(name: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("public.{name}")
    }
}

fn shortened(text: &str) -> String {
    let mut shown = String::new();
    let mut space = false;
    for (count, character) in text
        .trim()
        .chars()
        .filter(|character| {
            let blank = character.is_whitespace();
            let keep = !(blank && space);
            space = blank;
            keep
        })
        .enumerate()
    {
        if count == SHOWN_STATEMENT_CHARS {
            shown.push('…');
            return shown;
        }
        shown.push(if character.is_whitespace() {
            ' '
        } else {
            character
        });
    }
    shown
}

fn error_line(statement: &DumpStatement, error: &SqlError) -> u64 {
    if let Some(data_line) = error
        .context()
        .filter(|context| context.starts_with("COPY "))
        .and_then(|context| context.split(", line ").nth(1))
        .and_then(|rest| {
            rest.split(|character: char| !character.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u64>().ok())
        })
    {
        return statement.end_line.saturating_add(data_line);
    }
    match error.position() {
        Some(position) if position <= statement.text.len() => {
            let before = statement.text.get(..position).unwrap_or("");
            statement.line + before.matches('\n').count() as u64
        }
        _ => statement.line,
    }
}

#[cfg(test)]
mod tests;
