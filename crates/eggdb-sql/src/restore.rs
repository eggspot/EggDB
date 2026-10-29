mod classify;
mod classify_oracle;
mod classify_sqlserver;
mod column_rewrite;
mod reader;
mod script;
mod script_reader;
mod script_text;
mod sqlplus_reader;
mod statement_spans;
mod table_rewrite_oracle;
mod words;

use crate::copy::{CopyChannel, CopyStart};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::{QueryResult, Session};
use crate::store::KeyValueStore;
use classify::{Action, Effect, Ignored, Skip, ToggledConstraint};
use reader::DumpReader;
use script::{ClientCommand, DifferenceNote, ScriptItem, ScriptSource, ScriptStatement};
use sqlplus_reader::SqlPlusReader;
use statement_spans::SqlServerScript;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use words::{Token, WordRules, Words};

/// The most items each list of a [`RestoreReport`] holds; past it, only a count grows.
pub const MAX_LISTED: usize = 10_000;

const MAX_REMEMBERED_NAMES: usize = 100_000;
const SHOWN_STATEMENT_CHARS: usize = 48;
const PROGRESS_EVERY_LINES: u64 = 4096;

/// How [`restore_dump`] runs.
#[derive(Clone, Copy, Default)]
pub struct RestoreOptions<'a> {
    /// Record an error in a supported statement and go on, instead of stopping at it.
    pub keep_going: bool,
    /// Opens the scripts an Oracle script includes with `@`, `@@` or `START`; without it every
    /// include is an error.
    pub includes: Option<&'a dyn IncludeOpener>,
    /// Substitution variables an Oracle script starts with, as `DEFINE name = value` would set
    /// them.
    pub defines: &'a [(String, String)],
}

impl std::fmt::Debug for RestoreOptions<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoreOptions")
            .field("keep_going", &self.keep_going)
            .field("includes", &self.includes.is_some())
            .field("defines", &self.defines.len())
            .finish()
    }
}

/// Where an included script's name is resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeBase {
    /// The folder of the script that includes it (`@@`), an index into
    /// [`RestoreReport::scripts`].
    IncludingScript(u32),
    /// The working directory (`@` and `START`).
    WorkingDirectory,
}

/// A script an Oracle script asks to include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncludeRequest<'n> {
    /// The name as the script writes it, with `.sql` added when it has no extension.
    pub name: &'n str,
    /// What the name is relative to.
    pub relative_to: IncludeBase,
    /// The index the script will have in [`RestoreReport::scripts`] once opened.
    pub script: u32,
    /// The line of the including script.
    pub line: u64,
}

/// Opens the scripts that an Oracle script includes, for [`restore_dump`]: the library itself
/// never opens a file.
pub trait IncludeOpener {
    /// Opens the script `request` names, or says why it cannot.
    fn open<'s>(&'s self, request: &IncludeRequest<'_>) -> SqlResult<Box<dyn BufRead + 's>>;
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
    /// The script the line is in, an index into [`RestoreReport::scripts`].
    pub script: u32,
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
    /// The script the line is in, an index into [`RestoreReport::scripts`].
    pub script: u32,
    /// The start of the statement, cut after 48 characters.
    pub statement: String,
    /// The skipped object it uses, such as `function public.touch()`.
    pub uses: String,
    /// The error the statement failed with.
    pub error: SqlError,
}

/// A client command of the dump that was not run as a statement: a psql meta-command such as
/// `\connect`, or a sqlcmd command such as `:setvar` or `:r`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredMetaCommand {
    /// Its line in the dump.
    pub line: u64,
    /// The script the line is in, an index into [`RestoreReport::scripts`].
    pub script: u32,
    /// The command as written, such as `\connect` or `:setvar`.
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
    /// The script the line is in, an index into [`RestoreReport::scripts`].
    pub script: u32,
    /// The start of the statement, cut after 48 characters; empty when the dump itself could
    /// not be read.
    pub statement: String,
    /// The error.
    pub error: SqlError,
    /// How many statements of the dump were read before it.
    pub statements_before: u64,
}

/// Something restored that is not kept as the source database had it, such as a `datetimeoffset`
/// column whose offsets are lost or an index whose filter is not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownDifference {
    /// The line of the dump where the statement starts.
    pub line: u64,
    /// The script the line is in, an index into [`RestoreReport::scripts`].
    pub script: u32,
    /// The object it concerns, in the dump's own naming, such as `dbo.Orders.PlacedAt`.
    pub object: String,
    /// What differs, in one clause.
    pub difference: String,
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
    /// What was restored but is not kept as the source had it.
    pub differences: Vec<KnownDifference>,
    /// Known differences beyond the [`MAX_LISTED`] in `differences`.
    pub differences_not_listed: u64,
    /// The names of the scripts read, which the caller fills: index 0 is the one restored, and
    /// each listed item's `script` is an index into it.
    pub scripts: Vec<String>,
}

impl RestoreReport {
    /// How many known differences there were, listed or not.
    pub fn difference_count(&self) -> u64 {
        self.differences.len() as u64 + self.differences_not_listed
    }

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
/// own transaction as `psql -f`, `sqlcmd -i` or SQL*Plus runs it (a `COPY` and its data in one),
/// and reports what it did. The session's dialect says what the dump is:
///
/// - **PostgreSQL**: PostgreSQL's plain SQL form, `pg_dump`'s output or `pg_restore -f`'s.
///   psql's meta-commands are handled as psql would for what `pg_dump` writes: `\connect` is
///   noted and ignored (one database per file), `\restrict` and `\unrestrict` must carry the
///   same key, and no other meta-command is ever run.
/// - **SQL Server**: a Transact-SQL script as SSMS Generate Scripts or `sqlpackage
///   /Action:Script` writes it, in UTF-8 or UTF-16LE with its byte-order mark, in batches ended
///   by `GO`. sqlcmd's `:setvar` defines the `$(name)` variables it replaces once the script has
///   used a sqlcmd command; `:r`, `!!` and the other sqlcmd commands are listed and never run.
///   What EggDB keeps differently from SQL Server is listed as a known difference.
/// - **Oracle**: a SQL*Plus-style script, as SQL Developer, SQLcl, `impdp SQLFILE=` or an
///   installer such as Oracle's sample schemas write it. Statements end at `;` or a `/` line,
///   PL/SQL units at a `/` line; SQL*Plus commands are handled as SQL*Plus would for a restore
///   (`DEFINE` and `&name` substitution, `SET DEFINE`, `CONNECT` choosing the schema, `@`, `@@`
///   and `START` through [`RestoreOptions::includes`]) and never run otherwise. Anonymous blocks
///   of `INSERT`, `UPDATE`, `DELETE` and `COMMIT` run, each as one transaction; other PL/SQL is
///   listed and skipped. Users become schemas, and a constraint disabled and enabled again is
///   dropped and re-added.
///
/// The dump is read one line at a time (lines and statements up to 1 GiB), never whole.
/// Objects EggDB cannot create yet (functions, procedures, triggers, types, partitioned tables,
/// ...) are skipped and listed; `GRANT`, `REVOKE`, `OWNER TO`, unknown `SET` parameters and the
/// like are counted and ignored; a statement that fails only because it uses a skipped object is
/// a consequence, and a schema statement that EggDB answers with `0A000` is skipped as not
/// supported yet. Any other error stops the restore, unless [`RestoreOptions::keep_going`], with
/// the statements before it committed; `progress` is called as the restore moves on. The session
/// must use the PostgreSQL, SQL Server or Oracle dialect and be outside a transaction block.
pub fn restore_dump<S: KeyValueStore>(
    session: &mut Session<'_, S>,
    input: &mut dyn BufRead,
    options: RestoreOptions<'_>,
    progress: &mut dyn FnMut(&RestoreProgress),
) -> RestoreReport {
    match session.dialect() {
        Dialect::PostgreSql => {
            let mut reader = DumpReader::new(input);
            Restore::new(
                session,
                &mut reader,
                RestoreRules::postgresql(),
                options,
                progress,
            )
            .run()
        }
        Dialect::SqlServer => {
            let mut script = SqlServerScript::new(input);
            Restore::new(
                session,
                &mut script,
                RestoreRules::sqlserver(),
                options,
                progress,
            )
            .run()
        }
        Dialect::Oracle => {
            let mut reader = SqlPlusReader::new(input, options.includes);
            for (name, value) in options.defines {
                if let Err(error) = reader.define(name, value) {
                    return failed_before_start(error);
                }
            }
            Restore::new(
                session,
                &mut reader,
                RestoreRules::oracle(),
                options,
                progress,
            )
            .run()
        }
        _ => failed_before_start(SqlError::not_supported(
            "restoring a dump in a dialect other than PostgreSQL, SQL Server and Oracle",
        )),
    }
}

fn failed_before_start(error: SqlError) -> RestoreReport {
    RestoreReport {
        error: Some(RestoreError {
            line: 0,
            script: 0,
            statement: String::new(),
            error,
            statements_before: 0,
        }),
        ..RestoreReport::default()
    }
}

type Classifier = fn(&str, &dyn Fn(&str) -> bool) -> Action;

#[derive(Clone, Copy)]
struct RestoreRules {
    classify: Classifier,
    table_key: fn(&str, &str) -> String,
    name_key: fn(&str) -> String,
    words: WordRules,
    client_command_reason: fn(&str) -> &'static str,
    differences: fn(&str) -> Vec<DifferenceNote>,
    not_supported: fn(&SqlError) -> bool,
    psql_meta_commands: bool,
    index_names_per_table: bool,
    default_schema: &'static str,
    oracle: bool,
}

impl RestoreRules {
    fn postgresql() -> Self {
        Self {
            classify: classify::classify,
            table_key: postgresql_table_key,
            name_key: str::to_owned,
            words: WordRules::POSTGRESQL,
            client_command_reason: psql_command_reason,
            differences: |_| Vec::new(),
            not_supported: |error| error.state() == SqlState::FEATURE_NOT_SUPPORTED,
            psql_meta_commands: true,
            index_names_per_table: false,
            default_schema: "public",
            oracle: false,
        }
    }

    fn sqlserver() -> Self {
        Self {
            classify: classify_sqlserver::classify_sqlserver,
            table_key: classify_sqlserver::sqlserver_table_key,
            name_key: classify_sqlserver::sqlserver_name_key,
            words: WordRules::SQLSERVER,
            client_command_reason: classify_sqlserver::sqlcmd_command_reason,
            differences: classify_sqlserver::sqlserver_differences,
            not_supported: classify_sqlserver::refused_as_not_supported,
            psql_meta_commands: false,
            index_names_per_table: true,
            default_schema: "public",
            oracle: false,
        }
    }

    fn oracle() -> Self {
        Self {
            classify: classify_oracle::classify_oracle,
            table_key: classify_oracle::oracle_table_key,
            name_key: classify_oracle::oracle_name_key,
            words: words::WordRules::ORACLE,
            client_command_reason: classify_oracle::sqlplus_command_reason,
            differences: classify_oracle::oracle_differences,
            not_supported: classify_oracle::refused_as_not_supported,
            psql_meta_commands: false,
            index_names_per_table: false,
            default_schema: "public",
            oracle: true,
        }
    }
}

fn psql_command_reason(name: &str) -> &'static str {
    match name {
        "connect" | "c" => "one database per file",
        _ => "never run",
    }
}

enum Flow {
    Continue,
    Stop,
}

struct SkippedTable {
    listed: Option<usize>,
}

struct DisabledConstraint {
    table: String,
    statement: String,
    default_schema: String,
}

struct Restore<'a, 's, S: KeyValueStore> {
    session: &'a mut Session<'s, S>,
    reader: &'a mut dyn ScriptSource,
    rules: RestoreRules,
    report: RestoreReport,
    options: RestoreOptions<'a>,
    progress: &'a mut dyn FnMut(&RestoreProgress),
    restricted: Option<(String, u64)>,
    skipped_names: HashMap<String, String>,
    skipped_tables: HashMap<String, SkippedTable>,
    table_rows: HashMap<String, usize>,
    set_names: HashSet<String>,
    skipped_sequences: HashSet<String>,
    default_schema: String,
    ensured_schemas: HashSet<String>,
    disabled: BTreeMap<(String, String), DisabledConstraint>,
}

impl<'a, 's, S: KeyValueStore> Restore<'a, 's, S> {
    fn new(
        session: &'a mut Session<'s, S>,
        reader: &'a mut dyn ScriptSource,
        rules: RestoreRules,
        options: RestoreOptions<'a>,
        progress: &'a mut dyn FnMut(&RestoreProgress),
    ) -> Self {
        Self {
            session,
            reader,
            rules,
            report: RestoreReport::default(),
            options,
            progress,
            restricted: None,
            skipped_names: HashMap::new(),
            skipped_tables: HashMap::new(),
            table_rows: HashMap::new(),
            set_names: HashSet::new(),
            skipped_sequences: HashSet::new(),
            default_schema: rules.default_schema.to_owned(),
            ensured_schemas: HashSet::new(),
            disabled: BTreeMap::new(),
        }
    }

    fn run(mut self) -> RestoreReport {
        self.run_items();
        if self.report.error.is_none() {
            self.note_still_disabled();
        }
        let names = self.reader.script_names();
        if !names.is_empty() {
            self.report.scripts = names;
        }
        self.report
    }

    fn table_key(&self, name: &str) -> String {
        (self.rules.table_key)(name, &self.default_schema)
    }

    fn run_items(&mut self) {
        loop {
            let item = match self.reader.next_item() {
                Ok(item) => item,
                Err(error) => {
                    self.report.error = Some(RestoreError {
                        line: self.reader.line_number(),
                        script: self.reader.script_index(),
                        statement: String::new(),
                        error,
                        statements_before: self.report.statements,
                    });
                    return;
                }
            };
            let flow = match item {
                ScriptItem::End => {
                    if let Some((_, line)) = self.restricted.take() {
                        let error = SqlError::new(
                            SqlState::SYNTAX_ERROR,
                            format!(
                                "the dump ends without the \\unrestrict that closes the \\restrict of line {line}"
                            ),
                        )
                        .with_hint("The dump is cut short; take it again, or copy all of it.");
                        self.genuine_error(RestoreError {
                            line: self.reader.line_number(),
                            script: 0,
                            statement: String::new(),
                            error,
                            statements_before: self.report.statements,
                        });
                    }
                    return;
                }
                ScriptItem::ClientCommand(command) => self.client_command(command),
                ScriptItem::Failed(failure) => self.genuine_error(RestoreError {
                    line: failure.line,
                    script: failure.script,
                    statement: failure.shown,
                    error: failure.error,
                    statements_before: self.report.statements,
                }),
                ScriptItem::Statement(statement) => {
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

    fn client_command(&mut self, meta: ClientCommand) -> Flow {
        if self.rules.oracle && meta.name == "connect" {
            if let Err(error) = self.use_schema(&meta.argument) {
                return self.genuine_error(RestoreError {
                    line: meta.line,
                    script: meta.script,
                    statement: meta.shown.clone(),
                    error,
                    statements_before: self.report.statements,
                });
            }
        }
        if !self.rules.psql_meta_commands {
            let reason = (self.rules.client_command_reason)(&meta.name);
            self.note_meta(&meta, reason);
            return Flow::Continue;
        }
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
            name => {
                let reason = (self.rules.client_command_reason)(name);
                self.note_meta(&meta, reason);
                Flow::Continue
            }
        }
    }

    fn fail_meta(&mut self, meta: &ClientCommand, message: String) -> Flow {
        let error = SqlError::new(SqlState::SYNTAX_ERROR, message)
            .with_hint("The dump was altered after pg_dump wrote it; take it again.");
        self.genuine_error(RestoreError {
            line: meta.line,
            script: meta.script,
            statement: meta.shown.clone(),
            error,
            statements_before: self.report.statements,
        })
    }

    fn note_meta(&mut self, meta: &ClientCommand, reason: &str) {
        let ignored = &mut self.report.ignored;
        if ignored.meta_commands.len() < MAX_LISTED {
            ignored.meta_commands.push(IgnoredMetaCommand {
                line: meta.line,
                script: meta.script,
                command: meta.shown.clone(),
                reason: reason.to_owned(),
            });
        } else {
            ignored.meta_commands_not_listed += 1;
        }
    }

    fn statement(&mut self, statement: ScriptStatement) -> Flow {
        if let Some(error) = &statement.refused
            && !(self.rules.not_supported)(error)
        {
            let error = error.clone();
            return self.failed(&statement, &Effect::Other, error);
        }
        let session = &*self.session;
        let action =
            (self.rules.classify)(&statement.text, &|name| session.parameter(name).is_ok());
        match action {
            Action::Ignore(kind) => {
                self.ignore(kind);
                self.note_differences(&statement);
                Flow::Continue
            }
            Action::Skip(skip) => self.skip(&statement, skip),
            Action::Run(Effect::User(name)) => self.create_user(&statement, &name),
            Action::Run(effect) => {
                if let Some(error) = statement.unresolved.clone() {
                    return self.genuine_error(RestoreError {
                        line: statement.line,
                        script: statement.script,
                        statement: shortened(&statement.text),
                        error,
                        statements_before: self.report.statements.saturating_sub(1),
                    });
                }
                if let Some(table) = effect.data_target()
                    && let Some(key) = self.skipped_table_key(table)
                {
                    return self.data_of_skipped_table(&statement, &key);
                }
                if let Some(error) = statement.refused.clone() {
                    return self.failed(&statement, &effect, error);
                }
                match effect {
                    Effect::AnonymousBlock { statements } => self.run_block(&statement, &statements),
                    Effect::ToggleConstraint {
                        table,
                        target,
                        enable,
                        novalidate,
                    } => self.toggle_constraint(&statement, &table, target, enable, novalidate),
                    Effect::DisabledConstraint { table, name } => {
                        self.remember_disabled_text(&statement, &table, &name)
                    }
                    effect => self.run_statement(statement, &effect),
                }
            }
        }
    }

    fn run_statement(&mut self, mut statement: ScriptStatement, effect: &Effect) -> Flow {
        if !self.rules.oracle {
            return self.execute(statement, effect, true);
        }
        let adjusted;
        let mut effect = effect;
        let schema = match effect {
            Effect::Table { name, .. } | Effect::View(name) | Effect::Sequence(name) => {
                name.split_once('.').map(|(schema, _)| schema.to_owned())
            }
            Effect::SessionSetting {
                current_schema: Some(schema),
            } => Some(schema.clone()),
            _ => None,
        };
        if let Some(schema) = schema
            && let Err(error) = self.ensure_schema(&schema)
        {
            return self.failed(&statement, effect, error);
        }
        if let Effect::Table { name, .. } = effect
            && let Some(rewrite) = table_rewrite_oracle::rewritten_table(&statement.text)
        {
            statement.text = rewrite.text;
            for note in rewrite.notes {
                statement.differences.push(note);
            }
            let disabled = rewrite.disabled.len() as u64;
            for element in rewrite.disabled {
                self.remember_disabled(
                    name,
                    &element.name,
                    format!("ALTER TABLE {} ADD {}", element.table, element.clause),
                );
            }
            if let Effect::Table { name, constraints } = effect
                && disabled > 0
            {
                adjusted = Effect::Table {
                    name: name.clone(),
                    constraints: constraints.saturating_sub(disabled),
                };
                effect = &adjusted;
            }
        }
        let owner = match effect {
            Effect::View(name) => name
                .split_once('.')
                .map(|(schema, _)| schema.to_owned())
                .filter(|schema| *schema != self.default_schema),
            _ => None,
        };
        let current = self.default_schema.clone();
        if let Some(owner) = &owner
            && let Err(error) = self.use_schema(owner)
        {
            return self.failed(&statement, effect, error);
        }
        let flow = self.execute(statement, effect, true);
        if owner.is_some() && let Err(error) = self.use_schema(&current) {
            self.report.error = Some(RestoreError {
                line: self.reader.line_number(),
                script: self.reader.script_index(),
                statement: String::new(),
                error,
                statements_before: self.report.statements,
            });
            return Flow::Stop;
        }
        if let Effect::SessionSetting {
            current_schema: Some(schema),
        } = effect
            && self.session.parameter("search_path").is_ok_and(|path| {
                crate::schemas::path_entries(&path).first() == Some(schema)
            })
        {
            self.default_schema = schema.clone();
        }
        flow
    }

    fn end_transaction(&mut self, commit: bool) -> SqlResult<()> {
        if !self.rules.oracle {
            return Ok(());
        }
        let text = if commit { "COMMIT" } else { "ROLLBACK" };
        self.session.execute(text, &mut |_| {})
    }

    fn session_user(&self) -> String {
        self.session
            .parameter("session_authorization")
            .unwrap_or_default()
            .to_lowercase()
    }

    fn ensure_schema(&mut self, schema: &str) -> SqlResult<()> {
        if schema == "public" || self.ensured_schemas.contains(schema) {
            return Ok(());
        }
        if schema == self.session_user() {
            return Ok(());
        }
        if self.session.ensure_schema(schema)? {
            self.report.schemas += 1;
        }
        if self.ensured_schemas.len() < MAX_REMEMBERED_NAMES {
            self.ensured_schemas.insert(schema.to_owned());
        }
        Ok(())
    }

    fn use_schema(&mut self, schema: &str) -> SqlResult<()> {
        self.ensure_schema(schema)?;
        let text = format!(
            "ALTER SESSION SET CURRENT_SCHEMA = {}",
            oracle_quoted(schema)
        );
        self.session.execute(&text, &mut |_| {})?;
        self.default_schema = schema.to_owned();
        Ok(())
    }

    fn create_user(&mut self, statement: &ScriptStatement, name: &str) -> Flow {
        if name.is_empty() || name.contains('&') {
            let error = statement.unresolved.clone().unwrap_or_else(|| {
                SqlError::new(SqlState::SYNTAX_ERROR, "CREATE USER needs a user name")
            });
            return self.genuine_error(RestoreError {
                line: statement.line,
                script: statement.script,
                statement: shortened(&statement.text),
                error,
                statements_before: self.report.statements.saturating_sub(1),
            });
        }
        if let Err(error) = self.ensure_schema(name) {
            return self.failed(statement, &Effect::Other, error);
        }
        self.ignore(Ignored::Other(classify_oracle::USERS));
        Flow::Continue
    }

    fn run_block(&mut self, statement: &ScriptStatement, ranges: &[std::ops::Range<usize>]) -> Flow {
        let mut pending: Vec<(String, u64)> = Vec::new();
        let mut loaded_total = 0u64;
        for range in ranges {
            let Some(piece) = statement.text.get(range.clone()) else {
                continue;
            };
            let line = statement.line
                + statement.text[..range.start].matches('\n').count() as u64;
            let mut words = words::Words::oracle(piece);
            if words.next().is_some_and(|token| token.is_word("commit")) {
                if let Err(error) = self.end_transaction(true) {
                    return self.block_failed(statement, piece, line, error);
                }
                self.add_pending_rows(&mut pending, &mut loaded_total);
                continue;
            }
            let effect = match classify_oracle::classify_oracle(piece, &|_| false) {
                Action::Run(effect) => effect,
                Action::Skip(_) | Action::Ignore(_) => Effect::Data,
            };
            if let Some(table) = effect.data_target()
                && let Some(key) = self.skipped_table_key(table)
            {
                let rows = classify::insert_rows(piece, self.rules.words);
                self.report.rows_skipped += rows;
                if let Some(SkippedTable {
                    listed: Some(index),
                }) = self.skipped_tables.get(&key)
                    && let Some(object) = self.report.skipped.get_mut(*index)
                {
                    *object.rows.get_or_insert(0) += rows;
                }
                continue;
            }
            let mut loaded = 0u64;
            let outcome = self.session.execute(piece, &mut |result: QueryResult| {
                loaded += rows_of(&result.tag);
            });
            if let Err(error) = outcome {
                let _ = self.end_transaction(false);
                return self.block_failed(statement, piece, line, error);
            }
            if let Some(table) = effect.data_target() {
                match pending.iter_mut().find(|(known, _)| known == table) {
                    Some((_, rows)) => *rows += loaded,
                    None => pending.push((table.to_owned(), loaded)),
                }
            }
        }
        if let Err(error) = self.end_transaction(true) {
            return self.block_failed(statement, "END;", statement.end_line, error);
        }
        self.add_pending_rows(&mut pending, &mut loaded_total);
        Flow::Continue
    }

    fn add_pending_rows(&mut self, pending: &mut Vec<(String, u64)>, total: &mut u64) {
        for (table, rows) in pending.drain(..) {
            self.report.rows += rows;
            *total += rows;
            self.add_rows(&table, rows);
        }
    }

    fn block_failed(
        &mut self,
        statement: &ScriptStatement,
        piece: &str,
        line: u64,
        error: SqlError,
    ) -> Flow {
        let error = error.with_hint(format!(
            "The anonymous block at line {} loads all its rows or none, as Oracle runs it; none of its rows since its last COMMIT were kept.",
            statement.line
        ));
        self.genuine_error(RestoreError {
            line,
            script: statement.script,
            statement: shortened(piece),
            error,
            statements_before: self.report.statements.saturating_sub(1),
        })
    }

    fn remember_disabled(&mut self, table: &str, name: &str, statement: String) {
        if self.disabled.len() >= MAX_REMEMBERED_NAMES {
            return;
        }
        let key = (self.table_key(table), name.to_owned());
        self.disabled.insert(
            key,
            DisabledConstraint {
                table: table.to_owned(),
                statement,
                default_schema: self.default_schema.clone(),
            },
        );
    }

    fn remember_disabled_text(
        &mut self,
        statement: &ScriptStatement,
        table: &str,
        name: &str,
    ) -> Flow {
        let Some(text) = table_rewrite_oracle::without_disable(&statement.text) else {
            return self.skip(
                statement,
                Skip {
                    kind: "constraint",
                    name: name.to_owned(),
                    reason: "not supported yet",
                    detail: None,
                    is_table: false,
                    names_an_object: false,
                },
            );
        };
        self.remember_disabled(table, name, text);
        Flow::Continue
    }

    fn toggle_constraint(
        &mut self,
        statement: &ScriptStatement,
        table: &str,
        target: ToggledConstraint,
        enable: bool,
        novalidate: bool,
    ) -> Flow {
        let name = match target {
            ToggledConstraint::Named(name) => name,
            ToggledConstraint::PrimaryKey | ToggledConstraint::Unique => {
                if !enable {
                    self.note_difference(
                        statement,
                        table.to_owned(),
                        "kept enforced: EggDB doesn't disable keys".to_owned(),
                    );
                }
                return Flow::Continue;
            }
        };
        let key = (self.table_key(table), name.clone());
        if enable {
            let Some(disabled) = self.disabled.remove(&key) else {
                return Flow::Continue;
            };
            if novalidate {
                self.note_difference(
                    statement,
                    format!("{table}.{name}"),
                    "existing rows are checked here; Oracle's NOVALIDATE doesn't check them"
                        .to_owned(),
                );
            }
            return self.re_add(statement, &disabled, &name);
        }
        if self.disabled.contains_key(&key) {
            return Flow::Continue;
        }
        let clause = match self.session.oracle_constraint_clause(table, &name) {
            Ok(clause) => clause,
            Err(error) => return self.failed(statement, &Effect::OtherDdl, error),
        };
        let Some(clause) = clause else {
            self.note_difference(
                statement,
                format!("{table}.{name}"),
                "not found, so nothing was disabled (EggDB keeps no names for NOT NULL constraints)"
                    .to_owned(),
            );
            return Flow::Continue;
        };
        let Some(text) = clause.clause else {
            let difference = match clause.kind {
                crate::constraints::ConstraintClauseKind::Key => {
                    "kept enforced: EggDB doesn't disable keys"
                }
                _ => "kept enforced: its CHECK was written in another dialect",
            };
            self.note_difference(statement, format!("{table}.{name}"), difference.to_owned());
            return Flow::Continue;
        };
        let drop = format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            clause.table,
            oracle_quoted(&name)
        );
        let dropped = self
            .session
            .execute(&drop, &mut |_| {})
            .and_then(|()| self.end_transaction(true));
        if let Err(error) = dropped {
            let _ = self.end_transaction(false);
            return self.failed(statement, &Effect::OtherDdl, error);
        }
        self.remember_disabled(
            table,
            &name,
            format!("ALTER TABLE {} ADD {text}", clause.table),
        );
        Flow::Continue
    }

    fn re_add(
        &mut self,
        statement: &ScriptStatement,
        disabled: &DisabledConstraint,
        name: &str,
    ) -> Flow {
        let switch = disabled.default_schema != self.default_schema;
        let current = self.default_schema.clone();
        if switch && let Err(error) = self.use_schema(&disabled.default_schema) {
            return self.failed(statement, &Effect::Other, error);
        }
        let mut adding = statement.clone();
        adding.text = disabled.statement.clone();
        let effect = Effect::AddConstraint {
            table: disabled.table.clone(),
            name: Some(name.to_owned()),
        };
        let flow = self.execute(adding, &effect, false);
        if switch && let Err(error) = self.use_schema(&current) {
            return self.failed(statement, &Effect::Other, error);
        }
        flow
    }

    fn note_still_disabled(&mut self) {
        let disabled = std::mem::take(&mut self.disabled);
        for ((_, name), constraint) in disabled {
            if self.report.differences.len() < MAX_LISTED {
                self.report.differences.push(KnownDifference {
                    line: 0,
                    script: 0,
                    object: format!("{}.{name}", constraint.table),
                    difference: "disabled in the source: not created here, so not enforced"
                        .to_owned(),
                });
            } else {
                self.report.differences_not_listed += 1;
            }
        }
    }

    fn note_differences(&mut self, statement: &ScriptStatement) {
        for note in &statement.differences {
            self.note_difference(statement, note.object.clone(), note.difference.clone());
        }
        for note in (self.rules.differences)(&statement.text) {
            self.note_difference(statement, note.object, note.difference);
        }
    }

    fn note_difference(&mut self, statement: &ScriptStatement, object: String, difference: String) {
        if self.report.differences.len() < MAX_LISTED {
            self.report.differences.push(KnownDifference {
                line: statement.line,
                script: statement.script,
                object,
                difference,
            });
        } else {
            self.report.differences_not_listed += 1;
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
                if ignored.set.len() < MAX_LISTED && self.set_names.insert(name.clone()) {
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

    fn skip(&mut self, statement: &ScriptStatement, skip: Skip) -> Flow {
        let rows = if statement.copy_from_stdin {
            match self.reader.skip_data() {
                Ok(rows) => Some(rows),
                Err(error) => return self.unreadable(error),
            }
        } else {
            None
        };
        if skip.names_an_object {
            self.remember_skipped(skip.kind, &skip.name);
        }
        let name = if self.rules.oracle
            && skip.names_an_object
            && !skip.name.contains('.')
            && self.default_schema != "public"
        {
            format!("{}.{}", self.default_schema, skip.name)
        } else {
            skip.name.clone()
        };
        let listed = self.list_skipped(SkippedObject {
            line: statement.line,
            script: statement.script,
            kind: skip.kind.to_owned(),
            name,
            reason: skip.reason.to_owned(),
            detail: skip.detail.map(str::to_owned),
            rows: skip.is_table.then_some(rows.unwrap_or(0)),
        });
        if skip.is_table {
            self.report.tables_skipped += 1;
            self.remember_skipped_table(&skip.name, listed);
        }
        Flow::Continue
    }

    fn unreadable(&mut self, error: SqlError) -> Flow {
        self.report.error = Some(RestoreError {
            line: self.reader.line_number(),
            script: 0,
            statement: String::new(),
            error,
            statements_before: self.report.statements,
        });
        Flow::Stop
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
        let key = name.split(['(', ' ']).next().unwrap_or(name);
        if key.is_empty() || self.skipped_names.len() >= MAX_REMEMBERED_NAMES {
            return;
        }
        self.skipped_names
            .entry((self.rules.name_key)(key))
            .or_insert_with(|| format!("{kind} {name}"));
    }

    fn remember_skipped_sequence(&mut self, name: &str) {
        self.remember_skipped("sequence", name);
        if self.skipped_sequences.len() < MAX_REMEMBERED_NAMES {
            let key = self.table_key(name);
            self.skipped_sequences.insert(key);
        }
    }

    fn remember_identity_sequences(&mut self, effect: &Effect) {
        if let Effect::AlterTable {
            identity_sequences, ..
        } = effect
        {
            for sequence in identity_sequences {
                self.remember_skipped_sequence(sequence);
            }
        }
    }

    fn remember_skipped_table(&mut self, name: &str, listed: Option<usize>) {
        if self.skipped_tables.len() < MAX_REMEMBERED_NAMES {
            let key = self.table_key(name);
            self.skipped_tables.insert(key, SkippedTable { listed });
        }
    }

    fn skipped_table_key(&self, table: &str) -> Option<String> {
        let key = self.table_key(table);
        self.skipped_tables.contains_key(&key).then_some(key)
    }

    fn data_of_skipped_table(&mut self, statement: &ScriptStatement, key: &str) -> Flow {
        let rows = if statement.copy_from_stdin {
            match self.reader.skip_data() {
                Ok(rows) => rows,
                Err(error) => return self.unreadable(error),
            }
        } else {
            classify::insert_rows(&statement.text, self.rules.words)
        };
        self.report.rows_skipped += rows;
        if let Some(SkippedTable {
            listed: Some(index),
        }) = self.skipped_tables.get(key)
            && let Some(object) = self.report.skipped.get_mut(*index)
        {
            *object.rows.get_or_insert(0) += rows;
        }
        Flow::Continue
    }

    fn execute(&mut self, statement: ScriptStatement, effect: &Effect, counted: bool) -> Flow {
        let mut loaded = 0u64;
        let mut copy = DumpCopy {
            reader: &mut *self.reader,
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
            && let Err(error) = self.reader.skip_data()
        {
            return self.unreadable(error);
        }
        let outcome = match outcome {
            Ok(()) => self.end_transaction(true),
            Err(error) => {
                let _ = self.end_transaction(false);
                Err(error)
            }
        };
        match outcome {
            Ok(()) => {
                if counted {
                    self.count(effect, loaded);
                }
                self.note_differences(&statement);
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
                identity_sequences,
            } => {
                report.constraints += constraints;
                report.sequences += identity_sequences.len() as u64;
            }
            Effect::Comment => report.comments += 1,
            Effect::AddConstraint { .. } => report.constraints += 1,
            Effect::Copy(table) | Effect::Insert(table) => {
                report.rows += loaded;
                self.add_rows(table, loaded);
            }
            Effect::SequenceCall(_)
            | Effect::IdentityInsert(_)
            | Effect::SetDefault { .. }
            | Effect::Setting
            | Effect::SessionSetting { .. }
            | Effect::User(_)
            | Effect::AnonymousBlock { .. }
            | Effect::ToggleConstraint { .. }
            | Effect::DisabledConstraint { .. }
            | Effect::Drop
            | Effect::Data
            | Effect::OtherDdl
            | Effect::Other => {}
        }
    }

    fn add_rows(&mut self, table: &str, rows: u64) {
        let key = self.table_key(table);
        let table = if self.rules.oracle {
            key.strip_prefix("public.").unwrap_or(&key).to_owned()
        } else {
            table.to_owned()
        };
        let table = table.as_str();
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

    fn failed(&mut self, statement: &ScriptStatement, effect: &Effect, error: SqlError) -> Flow {
        let state = error.state();
        let code = state.code();
        let missing_object = matches!(code, "42704" | "42883" | "42P01" | "3F000");
        let refused_definition = matches!(code, "0A000" | "42601" | "42809");
        let not_supported = (self.rules.not_supported)(&error);
        if let Effect::Drop = effect
            && matches!(code, "42P01" | "42704")
        {
            self.ignore(Ignored::Other(classify_oracle::NOTHING_TO_DROP));
            return Flow::Continue;
        }
        if let Effect::SessionSetting { .. } = effect
            && matches!(error.vendor_code(), Some(3001 | 2248))
        {
            self.ignore(Ignored::Set(session_parameter(&statement.text)));
            return Flow::Continue;
        }
        if let Effect::AddConstraint { table, name } = effect
            && matches!(code, "23505" | "23503" | "23514")
        {
            let object = name.as_deref().unwrap_or(table).to_owned();
            if let Some(name) = name {
                self.remember_skipped("constraint", name);
            }
            self.note_difference(
                statement,
                object,
                "not created: existing rows violate it (rows the source kept under a disabled constraint, or a collation, padding or LIKE difference)".to_owned(),
            );
            return Flow::Continue;
        }
        if self.rules.index_names_per_table
            && let Effect::Index(name) = effect
            && code == "42P07"
        {
            self.note_difference(
                statement,
                format!("index {name}"),
                "not created: another table's index or a table has its name, and EggDB index names are unique per schema".to_owned(),
            );
            return Flow::Continue;
        }
        if let Effect::SetDefault { object, default } = effect
            && not_supported
        {
            self.note_difference(
                statement,
                object.clone(),
                format!("default not kept: {default}"),
            );
            return Flow::Continue;
        }
        let uses = match effect {
            Effect::SequenceCall(sequence)
                if missing_object && self.skipped_sequences.contains(&self.table_key(sequence)) =>
            {
                Some(format!("sequence {sequence}"))
            }
            effect if effect.is_ddl() && (missing_object || refused_definition) => {
                self.uses_skipped(&statement.text)
            }
            _ => None,
        };
        if let Some(uses) = uses {
            self.remember_identity_sequences(effect);
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
                    script: statement.script,
                    statement: shortened(&statement.text),
                    uses,
                    error,
                });
            } else {
                self.report.consequences_not_listed += 1;
            }
            return Flow::Continue;
        }
        if not_supported && effect.is_ddl() {
            self.remember_identity_sequences(effect);
            let (kind, name) = effect
                .created()
                .map_or(("statement", shortened(&statement.text)), |(kind, name)| {
                    (kind, name.to_owned())
                });
            self.remember_skipped(kind, &name);
            let is_table = kind == "table";
            let listed = self.list_skipped(SkippedObject {
                line: statement.line,
                script: statement.script,
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
            script: statement.script,
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
        let mut chain = String::new();
        let mut after_dot = false;
        let words = self.rules.words;
        for token in Words::with_rules(text, words) {
            let part = if words.bracketed_names {
                token.plain_name_part().map(str::to_owned)
            } else {
                token.name_part()
            };
            match part {
                Some(part) => {
                    if !after_dot {
                        chain.clear();
                    } else {
                        chain.push('.');
                    }
                    chain.push_str(&part);
                    after_dot = false;
                    if let Some(shown) = self.skipped_names.get(&(self.rules.name_key)(&chain)) {
                        return Some(shown.clone());
                    }
                }
                None => {
                    after_dot = token == Token::Symbol(b'.') && !chain.is_empty() && !after_dot;
                    if !after_dot {
                        chain.clear();
                    }
                }
            }
        }
        None
    }
}

struct DumpCopy<'r> {
    reader: &'r mut dyn ScriptSource,
    progress: &'r mut dyn FnMut(&RestoreProgress),
    tables: u64,
    rows: u64,
}

impl CopyChannel for DumpCopy<'_> {
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

fn oracle_quoted(name: &str) -> String {
    format!(
        "\"{}\"",
        crate::lexer::swap_uniform_case(name).replace('"', "\"\"")
    )
}

fn session_parameter(text: &str) -> String {
    let mut words = words::Words::oracle(text);
    let found = words.find(|token| token.is_word("set")).is_some();
    let name = if found {
        words.next().and_then(|token| token.plain_name_part().map(str::to_owned))
    } else {
        None
    };
    name.unwrap_or_default().to_ascii_uppercase()
}

fn postgresql_table_key(name: &str, default_schema: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{default_schema}.{name}")
    }
}

fn before_password(text: &str) -> &str {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower.get(from..).and_then(|rest| rest.find("identified")) {
        let at = from + offset;
        let before = at.checked_sub(1).map(|index| bytes[index]);
        let after = bytes.get(at + "identified".len()).copied();
        let boundary =
            |byte: Option<u8>| byte.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'));
        if boundary(before) && boundary(after) {
            return text.get(..at).unwrap_or(text);
        }
        from = at + "identified".len();
    }
    text
}

fn shortened(text: &str) -> String {
    let text = before_password(text);
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

fn error_line(statement: &ScriptStatement, error: &SqlError) -> u64 {
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
#[cfg(test)]
mod tests_oracle;
#[cfg(test)]
mod tests_sqlserver;
