mod mysql;
mod oracle;
mod restore;
mod sqlite;
pub(crate) mod sqlserver;
mod tsql;

pub use sqlserver::{BatchStep, NamedParameter, ParameterDeclaration};

pub(crate) use oracle::create_text as oracle_create_text;

pub(crate) use sqlite::create_text as sqlite_create_text;

use crate::ast::{
    CopyDirection, CopySource, CopyStatement, DiscardTarget, MySqlStatement, SqlServerStatement,
    Statement,
};
use crate::bulk_load::BulkRows;
use crate::copy::{CopyChannel, CopyIo, NoCopyChannel};
use crate::destructive::DestructiveStatement;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::{CopyStop, Token, TokenKind, tokenize_from, tokenize_in, tokenize_script};
use crate::mysql_errors::{self, Warning};
use crate::name_resolution::Binding;
use crate::parser::{LiteralRows, Parameters, Parser, parameter_number};
use crate::sequences::{self, SequenceAdvance, SessionSequences};
use crate::settings::{DIALECT_SETTING, Settings};
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use crate::types::{SqlType, Value};
use std::sync::Arc;

fn walks_expressions(tokens: &[Token<'_>]) -> bool {
    let mut selects = 0;
    for token in tokens {
        match &token.kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("select") => selects += 1,
            TokenKind::Word(word) if word.starts_with("current_schema") => return true,
            TokenKind::QuotedIdentifier(name) if name.starts_with("current_schema") => {
                return true;
            }
            _ => {}
        }
    }
    selects > 1
        || selects == 1
            && !tokens.first().is_some_and(|token| {
                matches!(&token.kind, TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("select") || word.eq_ignore_ascii_case("with"))
            })
}

fn mentions_sequence_functions(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| {
        matches!(&token.kind, TokenKind::Word(word)
            if ["nextval", "setval", "currval", "lastval"]
                .iter()
                .any(|name| word.eq_ignore_ascii_case(name)))
    })
}

/// A value bound to a parameter placeholder `$n` of a prepared statement, as PostgreSQL's
/// extended query protocol passes it: the type the client declared, if any, and the value's
/// text, or `None` for NULL. The text is read as a quoted constant would be: of the declared
/// type, or of the type its place in the statement calls for, following PostgreSQL's rules
/// for constants of unknown type. A parameter declared `bytea` is written in PostgreSQL's
/// `bytea` input form (`\x` and hexadecimal digits) in every dialect, so any bytes can be
/// bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// The parameter's type, when the client declared one.
    pub declared_type: Option<SqlType>,
    /// The value in text form, or `None` for NULL.
    pub value: Option<String>,
}

/// Where a session stands toward transactions, as PostgreSQL's `ReadyForQuery` message
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// Outside a transaction block (`I`).
    Idle,
    /// Inside a `BEGIN` block (`T`).
    InBlock,
    /// Inside a block in which a statement failed: every statement fails until `ROLLBACK`
    /// (`E`).
    Failed,
}

/// Why [`Session::run_one_statement`] did not run its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneStatementError {
    /// The text was refused before anything ran.
    Refused(OneStatementRefusal),
    /// The statement failed, or the text did not parse: the error in the session's dialect.
    Failed(SqlError),
}

/// What [`Session::run_one_statement`] refuses before running anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneStatementRefusal {
    /// The text holds no statement, only spaces, comments or semicolons.
    Empty,
    /// Another statement follows the first; `position` is the byte offset where it starts.
    MoreThanOne {
        /// The byte offset in the text where the second statement starts.
        position: usize,
    },
    /// The statement starts, commits or rolls back a transaction.
    TransactionControl,
    /// The statement is a `COPY`.
    Copy,
    /// The statement is destructive and is not the one confirmed
    /// ([`Session::run_one_statement_if_confirmed`]).
    Unconfirmed,
}

#[derive(Debug, Clone, Copy)]
enum DestructiveRule<'c> {
    Allowed,
    OnlyConfirmed(Option<&'c DestructiveStatement>),
}

/// What a prepared statement takes and gives back, before it runs: PostgreSQL's
/// `ParameterDescription` and `RowDescription`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementDescription {
    /// One entry per parameter `$1..$n`: its declared type, or `None` when neither the client
    /// declared it nor this release infers it.
    pub parameter_types: Vec<Option<SqlType>>,
    /// The columns of the rows it returns, or `None` when it returns no rows.
    pub columns: Option<Vec<OutputColumn>>,
}

/// The outline of a statement text checked for a prepared statement: how many parameters it
/// takes and whether it is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedStatement {
    /// The number of parameters: the highest `$n` used, or the number of declared types if
    /// that is higher.
    pub parameter_count: usize,
    /// Whether the text holds no statement at all.
    pub is_empty: bool,
}

fn highest_parameter(tokens: &[Token<'_>]) -> usize {
    let placeholders = tokens
        .iter()
        .filter(|token| token.kind == TokenKind::Placeholder)
        .count();
    tokens
        .iter()
        .filter_map(|token| match token.kind {
            TokenKind::Parameter(digits) => parameter_number(digits),
            TokenKind::NumberedParameter(number) => Some(number),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .max(placeholders)
}

/// A column of a statement's result.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    /// The column's name, as PostgreSQL names it: the column, the alias, the function, or
    /// `?column?`.
    pub name: String,
    /// The column's type when it is known before the rows are read: a table column, a cast,
    /// a constant, an aggregate, or an operator or function whose result type is fixed by its
    /// operands. A client aligns numbers right by it, and a protocol describes the column
    /// with it.
    pub sql_type: Option<SqlType>,
}

/// A message a statement sends its client besides its result, as SQL Server sends the text
/// of `PRINT` and the messages of `RAISERROR` with a severity of 10 or less: a TDS `INFO`
/// token, which a client shows without treating it as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoMessage {
    /// The message number: 0 for `PRINT`, 50000 for a `RAISERROR` message text.
    pub number: u32,
    /// The severity, 0 through 10.
    pub severity: u8,
    /// The state, 0 through 255.
    pub state: u8,
    /// The text.
    pub text: String,
}

/// What one statement returned: its command tag (`SELECT 3`, `INSERT 0 1`, `CREATE TABLE`,
/// ...), its rows when it returns rows, and any notices.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// The command tag, as PostgreSQL reports it.
    pub tag: String,
    /// The result columns, when the statement returns rows (`SELECT`, or `RETURNING`).
    pub columns: Option<Vec<OutputColumn>>,
    /// The rows, each with one value per column.
    pub rows: Vec<Vec<Value>>,
    /// Notices and warnings, such as `there is no transaction in progress`.
    pub notices: Vec<String>,
    /// MySQL's warnings and notes, such as a duplicate key an `INSERT IGNORE` skipped.
    pub warnings: Vec<Warning>,
    /// The first value an `AUTO_INCREMENT` column was given by this statement, which MySQL's
    /// `LAST_INSERT_ID()` returns afterwards.
    pub last_insert_id: Option<u64>,
    /// The row id of the last row this statement inserted into a table with SQLite's row ids:
    /// its `INTEGER PRIMARY KEY` (given or assigned), or the hidden row id of a table without
    /// a primary key. SQLite's `last_insert_rowid()` returns it afterwards.
    pub last_insert_rowid: Option<i64>,
    /// The last value this statement gave a SQL Server `IDENTITY` (auto-increment) column,
    /// given or generated, which SQL Server's `SCOPE_IDENTITY()` returns afterwards.
    pub last_identity_value: Option<i64>,
    /// The informational messages the statement sent, such as Transact-SQL's `PRINT` text; each
    /// is also among [`QueryResult::notices`].
    pub info_messages: Vec<InfoMessage>,
}

impl QueryResult {
    pub(crate) fn command(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            columns: None,
            rows: Vec::new(),
            notices: Vec::new(),
            warnings: Vec::new(),
            last_insert_id: None,
            last_insert_rowid: None,
            last_identity_value: None,
            info_messages: Vec::new(),
        }
    }

    pub(crate) fn rows(tag: String, columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> Self {
        Self {
            tag,
            columns: Some(columns),
            rows,
            notices: Vec::new(),
            warnings: Vec::new(),
            last_insert_id: None,
            last_insert_rowid: None,
            last_identity_value: None,
            info_messages: Vec::new(),
        }
    }

    pub(crate) fn with_notice(mut self, notice: String) -> Self {
        self.notices.push(notice);
        self
    }
}

struct Client<'c> {
    channel: &'c mut dyn CopyChannel,
    script: bool,
    bulk_rows: Option<&'c mut dyn BulkRows>,
}

fn reborrowed_rows<'r>(rows: &'r mut Option<&mut dyn BulkRows>) -> Option<&'r mut dyn BulkRows> {
    match rows {
        Some(rows) => {
            let rows: &'r mut dyn BulkRows = &mut **rows;
            Some(rows)
        }
        None => None,
    }
}

enum Flow {
    Stopped,
    Finished { inline_end: Option<usize> },
    DialectChanged { at: usize },
}

fn is_dialect_setting(name: &str) -> bool {
    name.eq_ignore_ascii_case(DIALECT_SETTING)
}

fn line_end_within(text: &str, from: usize, end: usize) -> usize {
    text.get(from..end)
        .and_then(|part| part.find('\n'))
        .map_or(end, |offset| from + offset + 1)
}

fn inline_data_start(text: &str, ranges: &[std::ops::Range<usize>], stop: CopyStop) -> usize {
    let Some(range) = ranges.get(stop.range) else {
        return text.len();
    };
    let line_end = line_end_within(text, stop.at, range.end);
    if line_end < range.end {
        return line_end;
    }
    ranges
        .get(stop.range + 1)
        .map_or(range.end, |next| next.start)
}

const NO_TRANSACTION_WARNING: &str = "WARNING:  there is no transaction in progress";

fn aborted_block() -> SqlError {
    SqlError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

enum Block<'s, S: KeyValueStore + 's> {
    None,
    Writing(S::Write<'s>),
    Reading(S::Read<'s>),
    Failed,
}

/// A SQL session over a [`KeyValueStore`]: it runs statements of its [`Dialect`] (PostgreSQL's
/// unless [`Session::with_dialect`] chose MySQL's) one after another, each in its own
/// transaction unless a `BEGIN` opened a block.
///
/// A block holds the store's one write transaction from `BEGIN` to `COMMIT` or `ROLLBACK`.
/// In PostgreSQL's dialect a failed statement inside a block rolls the block back at once and
/// leaves the session in the aborted state: every statement fails with SQLSTATE `25P02` until
/// `ROLLBACK` (or `COMMIT`, which then reports `ROLLBACK`). In MySQL's, a statement that fails
/// before writing leaves the block open, one that fails after writing rolls it back and ends
/// it, and errors carry MySQL's numbers and wording.
pub struct Session<'s, S: KeyValueStore + 's> {
    store: &'s S,
    block: Block<'s, S>,
    tables: TableCache,
    settings: Arc<Settings>,
    dialect: Dialect,
    connection_dialect: Dialect,
    warnings: Vec<Warning>,
    sequence_values: SessionSequences,
    pending_sequences: Vec<SequenceAdvance>,
    statement_uses_sequences: bool,
    statement_walks_expressions: bool,
    search_path_cache: crate::schemas::PathCache,
    search_path_at_begin: Option<String>,
    max_value_bytes: usize,
    max_row_bytes: usize,
    batch_resume: Option<(usize, usize)>,
    sqlserver_status: crate::sqlserver_functions::SessionStatus,
    oracle_nls: Arc<crate::oracle_nls::NlsParameters>,
    identity_insert_table: Option<String>,
    statement_start: usize,
    table_variables: crate::table_variables::TableVariables,
    extra_results: Vec<QueryResult>,
    cancel: crate::cancel::CancelSignal,
    implicit_transaction: bool,
    block_is_implicit: bool,
    connection_settings: Option<Arc<Settings>>,
    pending_bulk_load: Option<String>,
    batch_context: Arc<crate::tsql::BatchContext>,
    condition_next: bool,
    condition_holds: Option<bool>,
    pending_dynamic: Option<crate::tsql::DynamicBatch>,
    dynamic_depth: u8,
    unprepare_requests: Vec<i32>,
    parameter_values: Vec<NamedParameter>,
    set_error: Option<u32>,
    save_points: Vec<String>,
    advisory_locks: crate::advisory_locks::AdvisoryLocks,
}

impl<S: KeyValueStore> std::fmt::Debug for Session<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("in_block", &self.in_block())
            .finish()
    }
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    /// A session over `store`, outside any block.
    pub fn new(store: &'s S) -> Self {
        Self::with_dialect(store, Dialect::PostgreSql)
    }

    /// A session over `store` that reads `dialect`, outside any block.
    pub fn with_dialect(store: &'s S, dialect: Dialect) -> Self {
        Self {
            store,
            block: Block::None,
            tables: TableCache::default(),
            settings: Arc::new(Settings::default()),
            dialect,
            connection_dialect: dialect,
            warnings: Vec::new(),
            sequence_values: SessionSequences::default(),
            pending_sequences: Vec::new(),
            statement_uses_sequences: false,
            statement_walks_expressions: true,
            search_path_cache: crate::schemas::PathCache::default(),
            search_path_at_begin: None,
            max_value_bytes: usize::MAX,
            max_row_bytes: usize::MAX,
            batch_resume: None,
            sqlserver_status: crate::sqlserver_functions::SessionStatus::default(),
            oracle_nls: Arc::new(crate::oracle_nls::NlsParameters::default()),
            identity_insert_table: None,
            statement_start: 0,
            table_variables: crate::table_variables::TableVariables::default(),
            extra_results: Vec::new(),
            cancel: crate::cancel::CancelSignal::default(),
            implicit_transaction: false,
            block_is_implicit: false,
            connection_settings: None,
            pending_bulk_load: None,
            batch_context: Arc::new(crate::tsql::BatchContext::default()),
            condition_next: false,
            condition_holds: None,
            pending_dynamic: None,
            dynamic_depth: 0,
            unprepare_requests: Vec::new(),
            parameter_values: Vec::new(),
            set_error: None,
            save_points: Vec::new(),
            advisory_locks: crate::advisory_locks::AdvisoryLocks::new(
                std::ptr::from_ref(store).addr(),
            ),
        }
    }

    /// Sets the largest text or binary value a statement of this session may build, such as
    /// the result of `||`, `CONCAT`, `LPAD` or `REPLACE`, checked before the value is built;
    /// a larger one fails with SQLSTATE `54000`. Unlimited by default, so only the engine's own
    /// limits apply; a server that runs untrusted statements sets it low.
    pub fn set_max_value_bytes(&mut self, bytes: usize) {
        self.max_value_bytes = bytes;
    }

    /// Sets the largest row a statement of this session may build from its select list and
    /// sort keys, counted value by value as the row is built, so a select list that copies one
    /// large value into many columns stops before it allocates them all; a larger row fails
    /// with SQLSTATE `54000`. Unlimited by default; a server that runs untrusted statements
    /// sets it low.
    pub fn set_max_row_bytes(&mut self, bytes: usize) {
        self.max_row_bytes = bytes;
    }

    /// The dialect this session reads: the one it was opened in, or the one a client chose
    /// with `eggdb.dialect`.
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The dialect the session was opened in, such as PostgreSQL's for a PostgreSQL protocol
    /// connection: `RESET eggdb.dialect` returns to it, and its rules for transactions stay
    /// in force whichever dialect `eggdb.dialect` chooses.
    pub fn connection_dialect(&self) -> Dialect {
        self.connection_dialect
    }

    /// Runs `body` with the session reading its connection's dialect, then reads the dialect
    /// chosen with `eggdb.dialect` again, so a server can run the statements its own protocol
    /// stands for (such as a TDS transaction-manager request) whichever dialect the client
    /// chose.
    pub fn in_connection_dialect<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        let chosen = self.dialect;
        self.dialect = self.connection_dialect;
        let result = body(self);
        if self.dialect == self.connection_dialect {
            self.dialect = chosen;
        }
        result
    }

    fn choose_dialect(&mut self, value: Option<&str>) -> SqlResult<()> {
        let chosen = match value {
            None => self.connection_dialect,
            Some(name) => Dialect::named(name.trim()).ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for parameter \"{DIALECT_SETTING}\": \"{name}\""),
                )
                .with_hint("Choose oracle, or the connection's own dialect.")
            })?,
        };
        if chosen != self.connection_dialect && chosen != Dialect::Oracle {
            return Err(SqlError::not_supported(format!(
                "{DIALECT_SETTING} = {} on a {} connection; it can be {} or oracle",
                chosen.name(),
                self.connection_dialect.name(),
                self.connection_dialect.name()
            )));
        }
        if chosen == self.dialect {
            return Ok(());
        }
        if self.in_block() {
            return Err(SqlError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{DIALECT_SETTING} cannot change inside a transaction"),
            )
            .with_hint("Commit or roll back the transaction first."));
        }
        self.dialect = chosen;
        Arc::make_mut(&mut self.settings).set(DIALECT_SETTING, Some(chosen.name()))?;
        if chosen == Dialect::Oracle {
            self.refresh_oracle_user_schema()?;
        }
        Ok(())
    }

    fn refresh_oracle_user_schema(&mut self) -> SqlResult<()> {
        let user = self
            .settings
            .get("session_authorization")
            .unwrap_or_default();
        let internal = crate::lexer::swap_uniform_case(&user.to_uppercase());
        let exists = self.read(|reader, _| crate::schemas::schema_exists(reader, &internal))?;
        let schema = crate::oracle_dictionary::user_schema_of(&user, exists);
        if self.settings.oracle_user_schema != schema {
            Arc::make_mut(&mut self.settings).oracle_user_schema = schema;
        }
        Ok(())
    }

    /// Sets the number MySQL's `CONNECTION_ID()` and SQL Server's `@@SPID` return.
    pub fn set_connection_id(&mut self, id: u32) {
        let settings = Arc::make_mut(&mut self.settings);
        settings.mysql.connection_id = id;
        settings.sqlserver.connection_id = id;
    }

    /// The warnings and notes of the last statement, as MySQL's `SHOW WARNINGS` lists them.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The database this session reaches, as `current_database()` and MySQL's `DATABASE()`
    /// report it.
    pub fn database(&self) -> &str {
        &self.settings.database
    }

    /// Whether statements outside a `BEGIN` block commit on their own: always in PostgreSQL and
    /// SQLite, in MySQL unless `SET autocommit = 0`, in SQL Server unless `SET
    /// IMPLICIT_TRANSACTIONS ON` (then the first change, DDL included, starts a transaction
    /// that lasts until `COMMIT` or `ROLLBACK`), and never in Oracle, where the first change
    /// starts a transaction that lasts until `COMMIT`, `ROLLBACK` or DDL. The rule is the
    /// connection's dialect's: a PostgreSQL or TDS connection that chose Oracle's dialect with
    /// `eggdb.dialect` keeps its protocol's rule, as the drivers on it expect.
    pub fn autocommit(&self) -> bool {
        match self.connection_dialect {
            Dialect::MySql => self.settings.mysql.autocommit(),
            Dialect::SqlServer => !self.settings.sqlserver.implicit_transactions,
            Dialect::Oracle => false,
            Dialect::PostgreSql | Dialect::Sqlite => true,
        }
    }

    fn mysql(&self) -> bool {
        self.dialect == Dialect::MySql
    }

    fn sqlite(&self) -> bool {
        self.dialect == Dialect::Sqlite
    }

    fn oracle(&self) -> bool {
        self.dialect == Dialect::Oracle
    }

    fn sqlserver(&self) -> bool {
        self.dialect == Dialect::SqlServer
    }

    fn in_dialect(&mut self, text: &str, error: SqlError) -> SqlError {
        let error = error.with_names_displayed();
        if self.sqlserver() {
            let statement = text.get(self.statement_start..).unwrap_or(text);
            return crate::sqlserver_errors::translate(
                error,
                text,
                statement,
                &self.settings.database,
            );
        }
        if self.sqlite() {
            return crate::sqlite_errors::translate(error, text);
        }
        if self.oracle() {
            return crate::oracle_errors::translate(error, text, &self.oracle_user());
        }
        if !self.mysql() {
            return error;
        }
        let error = mysql_errors::translate(error, text, &self.settings.database);
        if let Some(code) = error.vendor_code() {
            self.warnings = vec![Warning {
                level: "Error",
                code,
                message: error.message().to_owned(),
            }];
        }
        error
    }

    /// Names the user this session runs as and the database it reaches, which
    /// `current_user`, `current_database()` and `session_authorization` report.
    pub fn identify(&mut self, user: &str, database: &str) {
        let settings = Arc::make_mut(&mut self.settings);
        settings.set_session_user(user);
        settings.database = database.to_owned();
        if self.dialect == Dialect::Oracle {
            let _ = self.refresh_oracle_user_schema();
        }
    }

    /// Sets the run-time parameter `name` (case-insensitive) as `SET name = value` would, for
    /// the parameters a client sends when it connects. `eggdb.dialect` chooses the dialect
    /// the session reads from its next statement: the connection's own or `oracle`.
    pub fn set_parameter(&mut self, name: &str, value: &str) -> SqlResult<()> {
        if is_dialect_setting(name) {
            return self.choose_dialect(Some(value));
        }
        Arc::make_mut(&mut self.settings).set(name, Some(value))
    }

    /// How far east of UTC the session's `TimeZone` is, in seconds: what a `timestamp with
    /// time zone` is shown in. A zone EggDB cannot resolve yet (a named region) counts as UTC.
    pub fn time_zone_offset_seconds(&self) -> i32 {
        self.settings.time_zone_offset
    }

    /// The value of the run-time parameter `name`, as `SHOW name` gives it.
    pub fn parameter(&self, name: &str) -> SqlResult<String> {
        self.settings.get(name)
    }

    /// The run-time parameters a PostgreSQL server reports to its clients when they connect
    /// and whenever they change (`server_version`, `client_encoding`, `DateStyle`, ...), with
    /// their values.
    pub fn reported_parameters(&self) -> Vec<(&'static str, String)> {
        self.settings.reported()
    }

    /// Where the session stands toward transactions. An implicit transaction
    /// ([`Session::start_implicit_transaction`]) is not a block: the session stays idle in it.
    pub fn transaction_status(&self) -> TransactionStatus {
        if self.block_is_implicit {
            return TransactionStatus::Idle;
        }
        match self.block {
            Block::None => TransactionStatus::Idle,
            Block::Writing(_) | Block::Reading(_) => TransactionStatus::InBlock,
            Block::Failed => TransactionStatus::Failed,
        }
    }

    /// The signal that cancels this session's running statement from another thread, as a
    /// PostgreSQL `CancelRequest` does: the statement stops at its next row with SQLSTATE
    /// `57014`, and so does every statement started before [`CancelSignal::clear`] lowers it.
    ///
    /// [`CancelSignal::clear`]: crate::CancelSignal::clear
    pub fn cancel_signal(&self) -> crate::cancel::CancelSignal {
        self.cancel.clone()
    }

    /// Starts an implicit transaction, as PostgreSQL runs the statements of one simple `Query`
    /// message, or of an extended-query pipeline up to its `Sync`: until
    /// [`Session::commit_implicit_transaction`] or [`Session::roll_back_implicit_transaction`],
    /// the statements outside a `BEGIN` block share one transaction, opened by the first one
    /// that writes, instead of committing one by one. A `BEGIN` among them turns it into a
    /// block that includes what they wrote; a `COMMIT` or `ROLLBACK` ends it with a warning, and
    /// the statements after it start another. Calling it again while one is started changes
    /// nothing.
    pub fn start_implicit_transaction(&mut self) {
        self.implicit_transaction = true;
    }

    /// Ends the implicit transaction [`Session::start_implicit_transaction`] started, committing
    /// what its statements wrote. A block a `BEGIN` opened stays open. Fails when the store
    /// cannot commit, and then nothing of it is kept.
    pub fn commit_implicit_transaction(&mut self) -> SqlResult<()> {
        self.end_implicit_transaction(true)
    }

    /// Ends the implicit transaction [`Session::start_implicit_transaction`] started, rolling
    /// back what its statements wrote, as PostgreSQL does when one of them failed. A block a
    /// `BEGIN` opened stays as it is.
    pub fn roll_back_implicit_transaction(&mut self) -> SqlResult<()> {
        self.end_implicit_transaction(false)
    }

    fn end_implicit_transaction(&mut self, commit: bool) -> SqlResult<()> {
        self.implicit_transaction = false;
        if !self.in_block() {
            self.advisory_locks.release_transaction_locks();
        }
        if !std::mem::take(&mut self.block_is_implicit) {
            return Ok(());
        }
        let ended = match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) if commit => writer.commit().map(|()| {
                self.pending_sequences.clear();
                sequences::mark_changes_committed();
            }),
            Block::Writing(writer) => writer.rollback(),
            Block::Reading(_) | Block::Failed | Block::None => Ok(()),
        };
        let settled = self.settle_sequences();
        ended.and(settled)
    }

    /// Makes the run-time parameters now in force the ones `RESET` and `DISCARD ALL` return
    /// to, as PostgreSQL returns to the values a client sent when it connected rather than to
    /// the server's defaults.
    pub fn remember_connection_settings(&mut self) {
        self.connection_settings = Some(Arc::clone(&self.settings));
    }

    fn reset_all_settings(&mut self) {
        match &self.connection_settings {
            Some(remembered) => {
                let mut restored = Settings::clone(remembered);
                if restored.get(DIALECT_SETTING).is_ok() {
                    let _ = restored.set(DIALECT_SETTING, Some(self.dialect.name()));
                }
                self.settings = Arc::new(restored);
            }
            None => Arc::make_mut(&mut self.settings).reset_all(),
        }
    }

    fn reset_setting(&mut self, name: &str) -> SqlResult<()> {
        let remembered = self
            .connection_settings
            .as_ref()
            .and_then(|settings| settings.get(name).ok());
        Arc::make_mut(&mut self.settings).set(name, remembered.as_deref())
    }

    fn discard(&mut self, target: DiscardTarget) -> SqlResult<QueryResult> {
        match target {
            DiscardTarget::All => {
                if self.in_block() {
                    return Err(SqlError::new(
                        SqlState::ACTIVE_SQL_TRANSACTION,
                        "DISCARD ALL cannot run inside a transaction block",
                    ));
                }
                self.reset_all_settings();
                sequences::discard_session_values();
                Ok(QueryResult::command("DISCARD ALL"))
            }
            DiscardTarget::Plans => Ok(QueryResult::command("DISCARD PLANS")),
            DiscardTarget::Sequences => {
                sequences::discard_session_values();
                Ok(QueryResult::command("DISCARD SEQUENCES"))
            }
            DiscardTarget::Temporary => Ok(QueryResult::command("DISCARD TEMP")),
        }
    }

    /// Checks `text` as the text of a prepared statement: at most one statement, valid
    /// syntax, and `$n` placeholders numbered 1 to 65535. Nothing runs and the catalog is not
    /// read. `declared_types` are the parameter types the client gave.
    pub fn prepare(
        &mut self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<PreparedStatement> {
        self.prepare_in_dialect(text, declared_types)
            .map_err(|error| self.in_dialect(text, error))
    }

    fn prepare_in_dialect(
        &self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<PreparedStatement> {
        let tokens = tokenize_in(text, self.dialect)?;
        let mut parser = Parser::new(text, &tokens)
            .with_context(Parameters::Types(declared_types), &self.settings)
            .in_dialect(self.dialect);
        parser.skip_empty_statements();
        if parser.at_end() {
            return Ok(PreparedStatement {
                parameter_count: declared_types.len(),
                is_empty: true,
            });
        }
        if parser.literal_rows().is_none() {
            parser.statement()?;
        }
        parser.skip_empty_statements();
        if !parser.at_end() {
            return Err(SqlError::syntax(
                "cannot insert multiple commands into a prepared statement",
                parser.position(),
            ));
        }
        Ok(PreparedStatement {
            parameter_count: highest_parameter(&tokens).max(declared_types.len()),
            is_empty: false,
        })
    }

    /// Describes the one statement in `text` without running it: its parameters' types and
    /// the columns of the rows it returns, reading the catalog as the statement would. Inside
    /// a failed block it fails as every statement does.
    pub fn describe(
        &mut self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<StatementDescription> {
        self.describe_in_dialect(text, declared_types)
            .map_err(|error| self.in_dialect(text, error))
    }

    fn describe_in_dialect(
        &mut self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<StatementDescription> {
        let tokens = tokenize_in(text, self.dialect)?;
        let mut parameter_types = declared_types.to_vec();
        parameter_types.resize(highest_parameter(&tokens).max(declared_types.len()), None);
        let settings = Arc::clone(&self.settings);
        let _path = self.enter_search_path(&settings);
        let _catalog = crate::pg_catalog::enter_statement(false, &settings);
        let _constraints = self.enter_constraints();
        let _nls = self.enter_oracle_nls();
        let mut parser = Parser::new(text, &tokens)
            .with_context(Parameters::Types(declared_types), &settings)
            .in_dialect(self.dialect);
        parser.skip_empty_statements();
        if parser.at_end() {
            return Ok(StatementDescription {
                parameter_types,
                columns: None,
            });
        }
        let statement = match parser.literal_rows() {
            Some(_) => None,
            None => Some(parser.statement()?),
        };
        let Some(statement) = statement else {
            return Ok(StatementDescription {
                parameter_types,
                columns: None,
            });
        };
        if matches!(
            statement,
            Statement::Begin { .. } | Statement::Commit | Statement::Rollback
        ) {
            return Ok(StatementDescription {
                parameter_types,
                columns: None,
            });
        }
        if matches!(self.block, Block::Failed) {
            return Err(aborted_block());
        }
        let mut statement = statement;
        let binding = Binding {
            settings: &settings,
            walk_expressions: true,
        };
        let binding = &binding;
        let columns = match &mut statement {
            Statement::Select(select)
                if self.sqlite()
                    && select
                        .from
                        .as_ref()
                        .is_some_and(|from| crate::parser::is_schema_table(&from.first)) =>
            {
                self.select_schema(select, true)
                    .map(|result| result.columns)
            }
            Statement::Select(select) => self
                .read(|reader, tables| {
                    binding.select(reader, tables, select)?;
                    execute::select(reader, tables, select, true)
                })
                .map(|result| result.columns),
            Statement::Query(query) => self
                .read(|reader, tables| {
                    binding.query(reader, tables, query)?;
                    execute::query(reader, tables, query, true)
                })
                .map(|result| result.columns),
            Statement::Insert(insert) => self.read(|reader, tables| {
                binding.insert(reader, tables, insert)?;
                execute::returning_columns(reader, tables, &insert.table, &insert.returning)
            }),
            Statement::Update(update) => self.read(|reader, tables| {
                binding.update(reader, tables, update)?;
                execute::returning_columns(reader, tables, &update.table, &update.returning)
            }),
            Statement::Delete(delete) => self.read(|reader, tables| {
                binding.delete(reader, tables, delete)?;
                execute::returning_columns(reader, tables, &delete.table, &delete.returning)
            }),
            Statement::Show { name } => Ok(Some(self.show_columns(name.as_deref()))),
            Statement::MySql(statement) => self.describe_mysql(statement),
            Statement::Pragma(pragma) => Ok(self.pragma_columns(pragma)),
            Statement::Explain(explain) => {
                Ok(Some(crate::explain::columns(explain.style, self.dialect)))
            }
            _ => Ok(None),
        };
        let columns = columns.map(|columns| {
            if self.oracle() {
                columns.map(oracle::shown_columns)
            } else {
                columns
            }
        });
        match columns {
            Ok(columns) => Ok(StatementDescription {
                parameter_types,
                columns,
            }),
            Err(error) => {
                self.fail_block_in_postgresql();
                Err(error)
            }
        }
    }

    /// Runs the one statement of a prepared statement with values for its parameters, as
    /// [`Session::execute_while`] runs statements. `parameters` holds one value per `$n`.
    pub fn execute_prepared(
        &mut self,
        text: &str,
        parameters: &[Parameter],
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        self.execute_prepared_with_copy(text, parameters, on_result, &mut NoCopyChannel)
    }

    /// Runs a prepared statement as [`Session::execute_prepared`] does, with `copy` as the
    /// client end of a `COPY ... FROM STDIN` or `COPY ... TO STDOUT`.
    pub fn execute_prepared_with_copy(
        &mut self,
        text: &str,
        parameters: &[Parameter],
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        copy: &mut dyn CopyChannel,
    ) -> SqlResult<()> {
        let mut client = Client {
            channel: copy,
            script: false,
            bulk_rows: None,
        };
        self.run_text(text, Parameters::Values(parameters), on_result, &mut client)
            .map_err(|error| self.in_dialect(text, error))
    }

    fn show_columns(&self, name: Option<&str>) -> Vec<OutputColumn> {
        let text_column = |name: &str| OutputColumn {
            name: name.to_owned(),
            sql_type: Some(SqlType::Text),
        };
        match name {
            Some(name) => vec![text_column(&Settings::name_of(name))],
            None => vec![
                text_column("name"),
                text_column("setting"),
                text_column("description"),
            ],
        }
    }

    /// Every table in the catalog, sorted by name, as this session sees them (inside a
    /// block, with the block's own changes).
    pub fn tables(&mut self) -> SqlResult<Vec<crate::schema::TableSchema>> {
        self.read(|reader, _| crate::schema::read_tables(reader))
    }

    /// Every view in the catalog, sorted by name, as this session sees them.
    pub fn views(&mut self) -> SqlResult<Vec<crate::schema::ViewSchema>> {
        self.read(|reader, _| crate::schema::read_views(reader))
    }

    /// Checks every table's secondary indexes against its rows, as this session sees them:
    /// each row has exactly its entries, no entry is left without a row, every entry decodes,
    /// and a unique index holds no value twice. A disagreement fails with SQLSTATE `XX001`
    /// naming the index; rebuilding it (`DROP INDEX` then `CREATE INDEX`) repairs it.
    pub fn check_indexes(&mut self) -> SqlResult<crate::indexes::IndexCheck> {
        self.read(|reader, _| crate::indexes::check_indexes(reader))
    }

    /// Whether a `BEGIN` block is open, failed or not. An implicit transaction
    /// ([`Session::start_implicit_transaction`]) is not one.
    pub fn in_block(&self) -> bool {
        !self.block_is_implicit && !matches!(self.block, Block::None)
    }

    /// Runs every statement in `text`, separated by semicolons, in order, and hands each
    /// result to `on_result`. It stops at the first statement that fails and returns its
    /// error, whose [`position`](SqlError::position) is a byte offset into `text`. Statements
    /// before it keep their effects: committed outside a block, pending inside one.
    pub fn execute(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult)) -> SqlResult<()> {
        self.execute_while(text, &mut |result| {
            on_result(result);
            true
        })
    }

    /// Runs the statements in `text` as [`Session::execute`] does, but stops before the next
    /// statement once `on_result` returns `false`, for example when the reader of the results
    /// has gone. The statements not run have no effect; a block they would have closed stays
    /// open. A `COPY ... FROM STDIN` or `TO STDOUT` fails, since there is no client to take
    /// part: [`Session::execute_with_copy`] gives it one.
    pub fn execute_while(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        self.execute_with_copy(text, on_result, &mut NoCopyChannel)
    }

    /// Runs the statements in `text` as [`Session::execute_while`] does, with `copy` as the
    /// client end of every `COPY ... FROM STDIN` (whose data `copy` reads) and `COPY ... TO
    /// STDOUT` (whose rows it receives), as a PostgreSQL connection does. A `COPY` loads all
    /// its rows or none: any error rolls the whole statement back.
    pub fn execute_with_copy(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        copy: &mut dyn CopyChannel,
    ) -> SqlResult<()> {
        let mut client = Client {
            channel: copy,
            script: false,
            bulk_rows: None,
        };
        self.run_text(text, Parameters::Absent, on_result, &mut client)
            .map_err(|error| self.in_dialect(text, error))
    }

    /// Runs `text` as a script, as `psql -f` runs a file: like
    /// [`Session::execute_with_copy`], except that the data of a `COPY ... FROM STDIN` is the
    /// script's own lines after the statement's line, up to a line `\.` or the script's end,
    /// as `pg_dump` writes it. Statements on the rest of the `COPY`'s line run after its data.
    /// `copy` receives the rows of every `COPY ... TO STDOUT`.
    pub fn execute_script(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        copy: &mut dyn CopyChannel,
    ) -> SqlResult<()> {
        let mut client = Client {
            channel: copy,
            script: true,
            bulk_rows: None,
        };
        self.run_text(text, Parameters::Absent, on_result, &mut client)
            .map_err(|error| self.in_dialect(text, error))
    }

    /// Runs `text` when it holds exactly one statement, as one call of an AI agent does. The
    /// text is parsed once, and refused before anything runs when it holds no statement, when
    /// another statement follows the first (empty statements aside), when the statement
    /// controls a transaction (`BEGIN`, `START TRANSACTION`, `COMMIT`, `END`, `ROLLBACK`), or
    /// when it is a `COPY`, which needs a client to stream through. Otherwise the statement
    /// runs as [`Session::execute_while`] runs it, its result handed to `on_result`.
    pub fn run_one_statement(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> Result<(), OneStatementError> {
        self.run_checked_statement(text, DestructiveRule::Allowed, on_result)
    }

    /// Runs `text` as [`Session::run_one_statement`] does, but refuses it, before anything
    /// runs, with [`OneStatementRefusal::Unconfirmed`] when its statement is destructive
    /// ([`crate::destructive_statement`]) and is not exactly `confirmed`: an AI agent's write
    /// runs only the destructive statement its user confirmed.
    pub fn run_one_statement_if_confirmed(
        &mut self,
        text: &str,
        confirmed: Option<&DestructiveStatement>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> Result<(), OneStatementError> {
        self.run_checked_statement(text, DestructiveRule::OnlyConfirmed(confirmed), on_result)
    }

    /// Parses `text` as [`Session::run_one_statement`] does and says whether its one
    /// statement is destructive, without running it.
    pub fn destructive_statement(
        &mut self,
        text: &str,
    ) -> Result<Option<DestructiveStatement>, OneStatementError> {
        let tokens = match tokenize_in(text, self.dialect) {
            Ok(tokens) => tokens,
            Err(error) => return Err(OneStatementError::Failed(self.in_dialect(text, error))),
        };
        match self.one_statement_in(text, &tokens) {
            Ok(Ok(destructive)) => Ok(destructive),
            Ok(Err(refusal)) => Err(OneStatementError::Refused(refusal)),
            Err(error) => Err(OneStatementError::Failed(self.in_dialect(text, error))),
        }
    }

    fn run_checked_statement(
        &mut self,
        text: &str,
        rule: DestructiveRule<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> Result<(), OneStatementError> {
        let tokens = match tokenize_in(text, self.dialect) {
            Ok(tokens) => tokens,
            Err(error) => return Err(OneStatementError::Failed(self.in_dialect(text, error))),
        };
        match self.one_statement_in(text, &tokens) {
            Ok(Ok(destructive)) => {
                if let DestructiveRule::OnlyConfirmed(confirmed) = rule
                    && destructive.is_some()
                    && destructive.as_ref() != confirmed
                {
                    return Err(OneStatementError::Refused(OneStatementRefusal::Unconfirmed));
                }
            }
            Ok(Err(refusal)) => return Err(OneStatementError::Refused(refusal)),
            Err(error) => return Err(OneStatementError::Failed(self.in_dialect(text, error))),
        }
        let mut client = Client {
            channel: &mut NoCopyChannel,
            script: false,
            bulk_rows: None,
        };
        match self.run_tokens(
            text,
            &tokens,
            Parameters::Absent,
            on_result,
            &mut client,
            None,
        ) {
            Ok(_) => Ok(()),
            Err(error) => Err(OneStatementError::Failed(self.in_dialect(text, error))),
        }
    }

    fn one_statement_in(
        &self,
        text: &str,
        tokens: &[Token<'_>],
    ) -> SqlResult<Result<Option<DestructiveStatement>, OneStatementRefusal>> {
        let settings = Arc::clone(&self.settings);
        let _clock = crate::datetime::enter_statement(settings.time_zone_offset);
        let _bytes = crate::bytea::enter_dialect(self.dialect);
        let _path = self.enter_search_path(&settings);
        let mut parser = Parser::new(text, tokens)
            .with_context(Parameters::Absent, &settings)
            .in_dialect(self.dialect);
        parser.skip_empty_statements();
        if parser.at_end() {
            return Ok(Err(OneStatementRefusal::Empty));
        }
        let _constraints = self.enter_constraints();
        let destructive = if parser.literal_rows().is_none() {
            match parser.statement()? {
                Statement::Begin { .. }
                | Statement::Commit
                | Statement::Rollback
                | Statement::SqlServer(
                    SqlServerStatement::SaveTransaction(_) | SqlServerStatement::RollbackTo(_),
                ) => {
                    return Ok(Err(OneStatementRefusal::TransactionControl));
                }
                Statement::Copy(_) => return Ok(Err(OneStatementRefusal::Copy)),
                statement => crate::destructive::classify(&statement),
            }
        } else {
            None
        };
        parser.skip_empty_statements();
        if parser.at_end() {
            Ok(Ok(destructive))
        } else {
            Ok(Err(OneStatementRefusal::MoreThanOne {
                position: parser.position(),
            }))
        }
    }

    fn run_text(
        &mut self,
        text: &str,
        parameters: Parameters<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        client: &mut Client<'_>,
    ) -> SqlResult<()> {
        self.statement_start = 0;
        if self.sqlserver() && client.bulk_rows.is_none() {
            let tokens = tokenize_in(text, self.dialect)?;
            if crate::tsql::uses_batch_logic(&tokens) {
                return self.run_logic_text(text, tokens, parameters, on_result);
            }
        }
        if !client.script {
            let mut start = 0;
            loop {
                let tokens = match tokenize_from(text, self.dialect, start) {
                    Ok(tokens) => tokens,
                    Err(error) => {
                        self.fail_block_in_postgresql();
                        return Err(error);
                    }
                };
                match self.run_tokens(text, &tokens, parameters, on_result, client, None)? {
                    Flow::DialectChanged { at } => start = at,
                    Flow::Stopped | Flow::Finished { .. } => return Ok(()),
                }
            }
        }
        let mut ranges: Vec<std::ops::Range<usize>> = std::iter::once(0..text.len()).collect();
        loop {
            let script = match tokenize_script(text, self.dialect, &ranges) {
                Ok(script) => script,
                Err(error) => {
                    self.fail_block_in_postgresql();
                    return Err(error);
                }
            };
            let data_start = script
                .copy_stop
                .map(|stop| inline_data_start(text, &ranges, stop));
            let flow = self.run_tokens(
                text,
                &script.tokens,
                parameters,
                on_result,
                client,
                data_start,
            )?;
            if let Flow::DialectChanged { at } = flow {
                ranges = ranges
                    .iter()
                    .filter(|range| range.end > at)
                    .map(|range| range.start.max(at)..range.end)
                    .collect();
                if ranges.is_empty() {
                    return Ok(());
                }
                continue;
            }
            let (Some(stop), Some(data_start), Flow::Finished { inline_end }) =
                (script.copy_stop, data_start, flow)
            else {
                return Ok(());
            };
            let line_end = ranges.get(stop.range).map_or(text.len(), |range| {
                line_end_within(text, stop.at, range.end)
            });
            let data_end = inline_end
                .unwrap_or(data_start)
                .clamp(data_start, text.len());
            ranges = vec![stop.at..line_end, data_end..text.len()];
        }
    }

    fn run_tokens(
        &mut self,
        text: &str,
        tokens: &[Token<'_>],
        parameters: Parameters<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        client: &mut Client<'_>,
        data_start: Option<usize>,
    ) -> SqlResult<Flow> {
        let inline = data_start.and_then(|start| text.as_bytes().get(start..));
        let mut inline_end = None;
        let mut resume = self.batch_resume.unwrap_or((0, 0));
        loop {
            let statement_dialect = self.dialect;
            if self.sqlserver() {
                self.sync_transaction_count();
            }
            let settings = Arc::clone(&self.settings);
            let context = Arc::clone(&self.batch_context);
            let _cancel = crate::cancel::enter(&self.cancel);
            let _advisory = crate::advisory_locks::enter(
                &self.advisory_locks,
                !matches!(self.block, Block::None) || self.implicit_transaction,
            );
            let _clock = crate::datetime::enter_statement(settings.time_zone_offset);
            let _regex = crate::regex::enter_statement();
            let _bytes = crate::bytea::enter_dialect(self.dialect);
            let _values = crate::memory::enter_value_limit(self.max_value_bytes);
            let _rows = crate::memory::enter_row_limit(self.max_row_bytes);
            let _path = self.enter_search_path(&settings);
            let mut parser = Parser::new(text, tokens)
                .with_context(parameters, &settings)
                .with_batch_context(&context)
                .in_dialect(self.dialect)
                .resuming_at(resume);
            parser.skip_empty_statements();
            if parser.at_end() {
                self.condition_next = false;
                return Ok(Flow::Finished { inline_end });
            }
            self.statement_start = parser.position();
            let _status = self.enter_sqlserver_status();
            let _variables = self
                .sqlserver()
                .then(|| crate::table_variables::enter(&self.table_variables));
            let _constraints = self.enter_constraints();
            let _nls = self.enter_oracle_nls();
            if let Some(mut rows) = parser.literal_rows() {
                let _catalog = crate::pg_catalog::enter_statement(false, &settings);
                resume = parser.resume_point();
                if self.batch_resume.is_some() {
                    self.batch_resume = Some(resume);
                }
                self.enter_sequences(false);
                let result = self.insert_literal_rows(&mut rows);
                self.leave_sequences();
                let settled = self.settle_sequences();
                let result = self.after_statement(result?, false);
                settled?;
                if !on_result(result) {
                    return Ok(Flow::Stopped);
                }
                continue;
            }
            let first_token = parser.resume_point().0;
            let parsed = if std::mem::take(&mut self.condition_next) {
                parser.sqlserver_condition()
            } else {
                parser.statement()
            };
            let setting_changes = parser.take_setting_changes();
            resume = parser.resume_point();
            drop(parser);
            drop(context);
            if self.batch_resume.is_some() {
                self.batch_resume = Some(resume);
            }
            let statement_tokens = tokens.get(first_token..resume.0.min(tokens.len()));
            let uses_sequences = statement_tokens.is_some_and(mentions_sequence_functions);
            self.statement_walks_expressions = statement_tokens.is_none_or(walks_expressions);
            let mut statement = match parsed {
                Ok(statement) => statement,
                Err(error) => {
                    self.fail_block_in_postgresql();
                    return Err(error);
                }
            };
            let diagnostic = matches!(
                statement,
                Statement::MySql(
                    MySqlStatement::ShowWarnings { .. } | MySqlStatement::ShowCount { .. }
                )
            );
            let loads_bulk_rows = client.bulk_rows.is_some();
            let mut io = CopyIo {
                channel: &mut *client.channel,
                inline,
                consumed: None,
                bulk_rows: reborrowed_rows(&mut client.bulk_rows),
            };
            let wants_catalog = match self.dialect {
                Dialect::PostgreSql => {
                    statement_tokens.is_some_and(crate::pg_catalog::mentions_catalog)
                }
                Dialect::SqlServer => {
                    statement_tokens.is_some_and(crate::sqlserver_catalog::mentions_metadata)
                }
                _ => false,
            };
            let _catalog = crate::pg_catalog::enter_statement(wants_catalog, &settings);
            self.enter_sequences(uses_sequences);
            self.extra_results.clear();
            let result = self.run(&mut statement, &mut io);
            if let (Some(start), Some(consumed)) = (data_start, io.consumed) {
                inline_end = Some(start + consumed);
            }
            if !loads_bulk_rows
                && result.is_ok()
                && matches!(
                    statement,
                    Statement::SqlServer(SqlServerStatement::InsertBulk(_))
                )
            {
                let end = tokens.get(resume.0).map_or(text.len(), |token| token.start);
                self.pending_bulk_load = text.get(self.statement_start..end).map(str::to_owned);
            }
            _path.remember(&mut self.search_path_cache);
            self.leave_sequences();
            let settled = self.settle_sequences();
            let result =
                result.and_then(|result| self.apply_setting_changes(result, &setting_changes));
            let result = self.after_statement(result?, diagnostic);
            settled?;
            let mut keep_going = on_result(result);
            for extra in std::mem::take(&mut self.extra_results) {
                keep_going = on_result(extra) && keep_going;
            }
            if !keep_going {
                return Ok(Flow::Stopped);
            }
            if self.dialect != statement_dialect {
                let at = tokens.get(resume.0).map_or(text.len(), |token| token.start);
                return Ok(Flow::DialectChanged { at });
            }
        }
    }

    fn enter_constraints(&self) -> crate::constraints::ContextGuard {
        crate::constraints::enter(
            self.dialect,
            !self.sqlite() || self.settings.sqlite.foreign_keys,
            if self.mysql() {
                self.settings.database.clone()
            } else {
                String::new()
            },
            if self.oracle() {
                self.oracle_user()
            } else {
                String::new()
            },
            self.identity_insert_table.clone(),
        )
    }

    fn enter_oracle_nls(&self) -> Option<crate::oracle_nls::NlsGuard> {
        self.oracle()
            .then(|| crate::oracle_nls::enter(Arc::clone(&self.oracle_nls)))
    }

    fn alter_session(&mut self, settings: &[(String, String)]) -> SqlResult<QueryResult> {
        for (name, value) in settings {
            if name == "current_schema" {
                if value.starts_with('\'') {
                    return Err(
                        SqlError::new(SqlState::SYNTAX_ERROR, "missing or invalid option")
                            .with_oracle_code(922),
                    );
                }
                let schema = value.clone();
                let exists =
                    self.read(|reader, _| crate::schemas::schema_exists(reader, &schema))?;
                if !exists {
                    return Err(SqlError::new(
                        SqlState::INVALID_SCHEMA_NAME,
                        "user does not exist",
                    )
                    .with_oracle_code(1435));
                }
                let entry = if schema.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
                }) {
                    schema
                } else {
                    format!("\"{}\"", schema.replace('"', "\"\""))
                };
                Arc::make_mut(&mut self.settings).set("search_path", Some(&entry))?;
                continue;
            }
            self.oracle_nls = Arc::new(self.oracle_nls.with(name, value)?);
        }
        Ok(QueryResult::command("ALTER SESSION"))
    }

    fn enter_search_path(&self, settings: &Arc<Settings>) -> crate::schemas::EnteredPath {
        crate::schemas::enter(settings, self.dialect, &self.search_path_cache)
    }

    fn apply_setting_changes(
        &mut self,
        result: QueryResult,
        changes: &[(String, String)],
    ) -> SqlResult<QueryResult> {
        for (name, value) in changes {
            if is_dialect_setting(name) {
                self.choose_dialect(Some(value))?;
                continue;
            }
            Arc::make_mut(&mut self.settings).set(name, Some(value))?;
        }
        Ok(result)
    }

    fn enter_sequences(&mut self, uses_sequences: bool) {
        self.statement_uses_sequences = uses_sequences;
        sequences::enter(
            std::mem::take(&mut self.sequence_values),
            self.dialect,
            uses_sequences,
        );
    }

    fn leave_sequences(&mut self) {
        let (values, advances) = sequences::leave();
        self.sequence_values = values;
        self.statement_uses_sequences = false;
        for mut advance in advances {
            if let Some(earlier) = self
                .pending_sequences
                .iter()
                .position(|pending| pending.name == advance.name)
            {
                advance.before = self.pending_sequences.remove(earlier).before;
            }
            self.pending_sequences.push(advance);
        }
    }

    fn settle_sequences(&mut self) -> SqlResult<()> {
        if self.pending_sequences.is_empty() || matches!(self.block, Block::Writing(_)) {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_sequences);
        let mut writer = self.store.begin_write().map_err(|error| {
            SqlError::new(
                error.state(),
                format!(
                    "the sequence values a rolled-back transaction used could not be saved again: {}",
                    error.message()
                ),
            )
        })?;
        match sequences::reapply(&mut writer, &pending) {
            Ok(()) => writer.commit(),
            Err(error) => {
                let _ = writer.rollback();
                Err(error)
            }
        }
    }

    fn after_statement(&mut self, mut result: QueryResult, diagnostic: bool) -> QueryResult {
        for notice in &mut result.notices {
            if notice.contains(crate::schemas::QUALIFIER) {
                *notice = crate::schemas::display(notice);
            }
        }
        if self.sqlite() {
            self.sqlite_after_statement(&mut result);
            return result;
        }
        if self.oracle() {
            oracle::after_statement(&mut result);
            return result;
        }
        if self.sqlserver() {
            self.sqlserver_after_statement(&result);
            return result;
        }
        if !self.mysql() {
            return result;
        }
        let database = self.settings.database.clone();
        for notice in std::mem::take(&mut result.notices) {
            if let Some(note) = mysql_errors::note_for(&notice, &database) {
                result.warnings.push(note);
            }
        }
        if let Some(id) = result.last_insert_id {
            Arc::make_mut(&mut self.settings).mysql.last_insert_id = id;
        }
        if !diagnostic {
            self.warnings.clone_from(&result.warnings);
        }
        result
    }

    fn fail_block_in_postgresql(&mut self) {
        if self.dialect == Dialect::PostgreSql {
            self.fail_block();
        }
    }

    /// Ends the session: an open block is rolled back. Whether one was open.
    pub fn close(mut self) -> SqlResult<bool> {
        let was_open = self.in_block();
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => writer.rollback()?,
            Block::Reading(_) | Block::Failed | Block::None => {}
        }
        self.settle_sequences()?;
        Ok(was_open)
    }

    fn fail_block(&mut self) {
        if let Block::Writing(writer) = std::mem::replace(&mut self.block, Block::None) {
            let _ = writer.rollback();
            self.block = Block::Failed;
        } else if let Block::Reading(_) = self.block {
            self.block = Block::Failed;
        }
    }

    fn run_copy(
        &mut self,
        copy: &mut CopyStatement,
        io: &mut CopyIo<'_, '_>,
        binding: &Binding<'_>,
    ) -> SqlResult<QueryResult> {
        if copy.direction == CopyDirection::ToClient
            && let CopySource::Table { name, columns } = &copy.source
            && let Some(relation) =
                self.read(|reader, _| crate::pg_catalog::catalog_reference(reader, &name.text))?
        {
            copy.source =
                CopySource::Query(Box::new(crate::pg_catalog::copy_query(relation, columns)?));
        }
        match copy.direction {
            CopyDirection::FromClient => self.write("COPY FROM", |writer, tables| {
                if let CopySource::Table { name, .. } = &mut copy.source {
                    if let Some(relation) =
                        crate::pg_catalog::catalog_reference(writer, &name.text)?
                    {
                        return Err(crate::pg_catalog::read_only(relation.name()).at(name.position));
                    }
                    binding.names(writer, tables, std::slice::from_mut(name))?;
                }
                crate::copy::copy_from_client(writer, tables, copy, io)
            }),
            CopyDirection::ToClient => {
                let channel = &mut *io.channel;
                if matches!(copy.source, CopySource::Table { .. }) {
                    return self.read(|reader, tables| {
                        if let CopySource::Table { name, .. } = &mut copy.source {
                            binding.names(reader, tables, std::slice::from_mut(name))?;
                        }
                        crate::copy::copy_table_to_client(reader, tables, copy, channel)
                    });
                }
                let run_query = |reader: &dyn KeyValueRead,
                                 tables: &mut TableCache,
                                 copy: &mut CopyStatement| {
                    let CopySource::Query(query) = &mut copy.source else {
                        return Err(SqlError::new(
                            SqlState::INTERNAL_ERROR,
                            "COPY of a table reached the query path",
                        ));
                    };
                    binding.query(reader, tables, query)?;
                    execute::query(reader, tables, query, false)
                };
                let result = if self.statement_uses_sequences {
                    self.write("nextval()", |writer, tables| {
                        run_query(writer, tables, copy)
                    })
                } else {
                    self.read(|reader, tables| run_query(reader, tables, copy))
                }?;
                crate::copy::copy_rows_to_client(result, copy, channel)
            }
        }
    }

    fn run(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        match statement {
            Statement::Set { name, value } if is_dialect_setting(name) => {
                return self
                    .choose_dialect(value.as_deref())
                    .map(|()| QueryResult::command("SET"));
            }
            Statement::Reset { name: Some(name) } if is_dialect_setting(name) => {
                return self
                    .choose_dialect(None)
                    .map(|()| QueryResult::command("RESET"));
            }
            _ => {}
        }
        if self.sqlite() {
            match statement {
                Statement::Begin { .. } if self.in_block() => {
                    return Err(sqlite::transaction_error(
                        SqlState::ACTIVE_SQL_TRANSACTION,
                        "cannot start a transaction within a transaction",
                    ));
                }
                Statement::Commit if !self.in_block() => {
                    return Err(sqlite::transaction_error(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "cannot commit - no transaction is active",
                    ));
                }
                Statement::Rollback if !self.in_block() => {
                    return Err(sqlite::transaction_error(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "cannot rollback - no transaction is active",
                    ));
                }
                _ => {}
            }
        }
        if self.sqlserver() {
            match statement {
                Statement::Begin { .. } => return self.sqlserver_begin(),
                Statement::Commit => return self.sqlserver_commit(),
                Statement::Rollback => return self.sqlserver_rollback(),
                Statement::SqlServer(SqlServerStatement::InsertBulk(bulk)) => {
                    return self.run_insert_bulk(bulk, io);
                }
                Statement::SqlServer(statement) => return self.run_sqlserver(statement),
                _ => {}
            }
        }
        match statement {
            Statement::Begin { read_only } => return self.begin(*read_only),
            Statement::Commit => return self.commit(),
            Statement::Rollback => return self.rollback(),
            _ => {}
        }
        if matches!(self.block, Block::Failed) {
            return Err(aborted_block());
        }
        if self.commits_implicitly(statement)
            && let Block::Writing(_) = self.block
        {
            self.commit()?;
        }
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: self.statement_walks_expressions,
        };
        let binding = &binding;
        let dialect = self.dialect;
        let format_only = self.sqlserver() && self.settings.sqlserver.format_only;
        if format_only
            && !matches!(
                statement,
                Statement::Select(_)
                    | Statement::Query(_)
                    | Statement::Set { .. }
                    | Statement::Ignored { .. }
            )
        {
            return Ok(QueryResult::command("FMTONLY"));
        }
        let result = match statement {
            Statement::Select(select) if format_only => self.read(|reader, tables| {
                binding.select(reader, tables, select)?;
                execute::select(reader, tables, select, true)
            }),
            Statement::Query(query) if format_only => self.read(|reader, tables| {
                binding.query(reader, tables, query)?;
                execute::query(reader, tables, query, true)
            }),
            Statement::MySql(statement) => self.run_mysql(statement),
            Statement::Pragma(pragma) => self.run_pragma(pragma),
            Statement::Select(select)
                if self.sqlite()
                    && select
                        .from
                        .as_ref()
                        .is_some_and(|from| crate::parser::is_schema_table(&from.first)) =>
            {
                self.select_schema(select, false)
            }
            Statement::Truncate(table) => self.write("TRUNCATE TABLE", |writer, tables| {
                binding.target(writer, tables, table)?;
                execute::truncate(writer, tables, table)
            }),
            Statement::Select(select) if self.statement_uses_sequences => {
                self.write("nextval()", |writer, tables| {
                    binding.select(writer, tables, select)?;
                    execute::select(writer, tables, select, false)
                })
            }
            Statement::Query(query) if self.statement_uses_sequences => {
                self.write("nextval()", |writer, tables| {
                    binding.query(writer, tables, query)?;
                    execute::query(writer, tables, query, false)
                })
            }
            Statement::Select(select) => self.read(|reader, tables| {
                binding.select(reader, tables, select)?;
                execute::select(reader, tables, select, false)
            }),
            Statement::Query(query) => self.read(|reader, tables| {
                binding.query(reader, tables, query)?;
                execute::query(reader, tables, query, false)
            }),
            Statement::CreateSequence(create) => self.write("CREATE SEQUENCE", |writer, tables| {
                create.name.text = crate::schemas::creation_name(writer, &create.name)?;
                sequences::create_sequence(writer, tables, create, dialect)
            }),
            Statement::AlterSequence(alter) => self.write("ALTER SEQUENCE", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut alter.name))?;
                if let Some(to) = &mut alter.rename {
                    crate::constraints::check_identifier(to)?;
                    to.text = crate::schemas::qualified(
                        crate::schemas::schema_of(&alter.name.text),
                        &to.text,
                    );
                }
                sequences::alter_sequence(writer, tables, alter, dialect)
            }),
            Statement::DropSequence {
                names,
                if_exists,
                cascade,
            } => self.write("DROP SEQUENCE", |writer, tables| {
                binding.names(writer, tables, names)?;
                sequences::drop_sequences(writer, tables, names, *if_exists, *cascade, dialect)
            }),
            Statement::AlterTable(alter) => self.write("ALTER TABLE", |writer, tables| {
                binding.alter_table(writer, tables, alter)?;
                crate::alter::alter_table(writer, tables, alter, dialect)
            }),
            Statement::CreateSchema {
                name,
                if_not_exists,
            } => self.write("CREATE SCHEMA", |writer, _| {
                crate::schemas::create_schema(writer, name, *if_not_exists, dialect)
            }),
            Statement::DropSchema {
                names,
                if_exists,
                cascade,
            } => self.write("DROP SCHEMA", |writer, tables| {
                crate::schemas::drop_schemas(writer, tables, names, *if_exists, *cascade, dialect)
            }),
            Statement::RenameSchema { from, to } => self.write("ALTER SCHEMA", |writer, tables| {
                crate::schemas::rename_schema(writer, tables, from, to, dialect)
            }),
            Statement::CreateView(create) => self.write("CREATE VIEW", |writer, tables| {
                let context = crate::views::ViewContext {
                    settings: Some(&settings),
                    dialect,
                };
                crate::views::create_view(writer, tables, create, &context)
            }),
            Statement::DropView {
                names,
                if_exists,
                cascade,
            } => self.write("DROP VIEW", |writer, tables| {
                binding.names(writer, tables, names)?;
                crate::views::drop_views(writer, tables, names, *if_exists, *cascade, dialect)
            }),
            Statement::AlterView(alter) => self.write("ALTER VIEW", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut alter.name))?;
                crate::views::alter_view(writer, tables, alter, dialect)
            }),
            Statement::Comment(comment) => self.write("COMMENT", |writer, tables| {
                binding.comment(writer, tables, comment)?;
                crate::comments::comment_on(writer, tables, comment, dialect)
            }),
            Statement::Copy(copy) => self.run_copy(copy, io, binding),
            Statement::Ignored { tag, notice } => {
                let result = QueryResult::command(*tag);
                Ok(match notice {
                    Some(notice) => result.with_notice(notice.clone()),
                    None => result,
                })
            }
            Statement::Set { name, value } => Arc::make_mut(&mut self.settings)
                .set(name, value.as_deref())
                .map(|()| QueryResult::command("SET")),
            Statement::Reset { name: Some(name) } => self
                .reset_setting(name)
                .map(|()| QueryResult::command("RESET")),
            Statement::Reset { name: None } => {
                self.reset_all_settings();
                Ok(QueryResult::command("RESET"))
            }
            Statement::Discard(target) => self.discard(*target),
            Statement::Show { name } => self.show(name.as_deref()),
            Statement::CreateTable(create) => self.write("CREATE TABLE", |writer, tables| {
                binding.create_table(writer, tables, create)?;
                execute::create_table(writer, tables, create, dialect)
            }),
            Statement::CreateIndex(create) => self.write("CREATE INDEX", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut create.table))?;
                crate::indexes::create_index(writer, tables, create, dialect)
            }),
            Statement::DropIndex(drop) => self.write("DROP INDEX", |writer, tables| {
                if let Some(table) = &mut drop.table {
                    binding.names(writer, tables, std::slice::from_mut(table))?;
                }
                crate::indexes::drop_index(writer, tables, drop, dialect)
            }),
            Statement::Explain(explain) => self
                .read(|reader, tables| {
                    binding.statement(reader, tables, &mut explain.statement)?;
                    execute::explain(reader, tables, &explain.statement)
                })
                .map(|query| crate::explain::render(&query, explain.style, dialect)),
            Statement::DropTable {
                names,
                if_exists,
                cascade,
            } => self.write("DROP TABLE", |writer, tables| {
                binding.names(writer, tables, names)?;
                execute::drop_tables(writer, tables, names, *if_exists, *cascade, dialect)
            }),
            Statement::Insert(insert) => self.write("INSERT", |writer, tables| {
                binding.insert(writer, tables, insert)?;
                execute::insert(writer, tables, insert)
            }),
            Statement::Update(update) => self.write("UPDATE", |writer, tables| {
                binding.update(writer, tables, update)?;
                execute::update(writer, tables, update)
            }),
            Statement::Delete(delete) => self.write("DELETE", |writer, tables| {
                binding.delete(writer, tables, delete)?;
                execute::delete(writer, tables, delete)
            }),
            Statement::Merge(merge) => self.write("MERGE", |writer, tables| {
                binding.merge(writer, tables, merge)?;
                crate::merge::merge(writer, tables, merge)
            }),
            Statement::AlterSession(settings) => self.alter_session(settings),
            Statement::Begin { .. }
            | Statement::Commit
            | Statement::Rollback
            | Statement::SqlServer(_) => {
                return Err(SqlError::new(
                    SqlState::INTERNAL_ERROR,
                    "unreachable statement",
                ));
            }
        };
        if result.is_err() {
            self.fail_block_in_postgresql();
            if let Statement::Insert(insert) = statement
                && insert.rollback_on_conflict
                && let Block::Writing(writer) = std::mem::replace(&mut self.block, Block::None)
            {
                writer.rollback()?;
            }
        }
        if self.commits_implicitly(statement)
            && let Block::Writing(_) = self.block
        {
            self.commit()?;
        }
        result
    }

    fn commits_implicitly(&self, statement: &Statement) -> bool {
        (self.mysql() || self.oracle())
            && matches!(
                statement,
                Statement::CreateTable(_)
                    | Statement::DropTable { .. }
                    | Statement::Truncate(_)
                    | Statement::CreateIndex(_)
                    | Statement::DropIndex(_)
                    | Statement::CreateSequence(_)
                    | Statement::AlterSequence(_)
                    | Statement::DropSequence { .. }
                    | Statement::AlterTable(_)
                    | Statement::CreateSchema { .. }
                    | Statement::DropSchema { .. }
                    | Statement::RenameSchema { .. }
                    | Statement::CreateView(_)
                    | Statement::DropView { .. }
                    | Statement::AlterView(_)
                    | Statement::Comment(_)
            )
    }

    fn show(&self, name: Option<&str>) -> SqlResult<QueryResult> {
        let rows = match name {
            Some(name) if is_dialect_setting(name) => {
                vec![vec![Value::Text(self.dialect.name().to_owned())]]
            }
            Some(name) => vec![vec![Value::Text(self.settings.get(name)?)]],
            None => self
                .settings
                .all()
                .into_iter()
                .map(|(name, setting, description)| {
                    vec![
                        Value::Text(name),
                        Value::Text(setting),
                        Value::Text(description),
                    ]
                })
                .collect(),
        };
        Ok(QueryResult::rows(
            "SHOW".to_owned(),
            self.show_columns(name),
            rows,
        ))
    }

    fn insert_literal_rows(&mut self, rows: &mut LiteralRows<'_>) -> SqlResult<QueryResult> {
        if matches!(self.block, Block::Failed) {
            return Err(aborted_block());
        }
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: false,
        };
        let result = self.write("INSERT", |writer, tables| {
            binding.target(writer, tables, &mut rows.table)?;
            execute::insert_literals(writer, tables, rows)
        });
        if result.is_err() {
            self.fail_block_in_postgresql();
        }
        result
    }

    pub(crate) fn read<T>(
        &mut self,
        body: impl FnOnce(&dyn crate::store::KeyValueRead, &mut TableCache) -> SqlResult<T>,
    ) -> SqlResult<T> {
        let tables = &mut self.tables;
        match &self.block {
            Block::Writing(writer) => body(writer, tables),
            Block::Reading(reader) => body(reader, tables),
            Block::None | Block::Failed => {
                let reader = self.store.begin_read()?;
                body(&reader, tables)
            }
        }
    }

    fn write(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn KeyValueWrite, &mut TableCache) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        let body = |writer: &mut dyn KeyValueWrite, tables: &mut TableCache| {
            sequences::load_for_sequence_calls(writer)?;
            let result =
                body(writer, tables).map_err(|error| sequences::explain_missing(writer, error))?;
            sequences::save_changed(writer)?;
            Ok(result)
        };
        let keep_open = !self.autocommit();
        let keeps_ddl_open = self.sqlserver();
        let implicit = self.implicit_transaction;
        let tables = &mut self.tables;
        match &mut self.block {
            Block::Writing(writer) if self.dialect != Dialect::PostgreSql => {
                let mut tracked = TrackedWrite {
                    inner: writer,
                    wrote: false,
                };
                let result = body(&mut tracked, tables);
                let wrote = tracked.wrote;
                if result.is_err()
                    && wrote
                    && let Block::Writing(writer) = std::mem::replace(&mut self.block, Block::None)
                {
                    let _ = writer.rollback();
                }
                result
            }
            Block::Writing(writer) => body(writer, tables),
            Block::Reading(_) => Err(SqlError::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {command} in a read-only transaction"),
            )),
            Block::None | Block::Failed => {
                let mut writer = self
                    .store
                    .begin_write()
                    .map_err(|error| read_only_refusal(error, command))?;
                match body(&mut writer, tables) {
                    Ok(result)
                        if keep_open
                            && (keeps_ddl_open
                                || !matches!(
                                    command,
                                    "CREATE TABLE"
                                        | "DROP TABLE"
                                        | "TRUNCATE TABLE"
                                        | "CREATE INDEX"
                                        | "DROP INDEX"
                                        | "CREATE SEQUENCE"
                                        | "ALTER SEQUENCE"
                                        | "DROP SEQUENCE"
                                        | "ALTER TABLE"
                                        | "CREATE SCHEMA"
                                        | "DROP SCHEMA"
                                        | "ALTER SCHEMA"
                                        | "CREATE VIEW"
                                        | "DROP VIEW"
                                        | "ALTER VIEW"
                                        | "COMMENT"
                                )) =>
                    {
                        self.block = Block::Writing(writer);
                        self.block_is_implicit = false;
                        Ok(result)
                    }
                    Ok(result) if implicit => {
                        self.block = Block::Writing(writer);
                        self.block_is_implicit = true;
                        Ok(result)
                    }
                    Ok(result) => {
                        writer.commit()?;
                        sequences::mark_changes_committed();
                        Ok(result)
                    }
                    Err(error) => {
                        let _ = writer.rollback();
                        Err(error)
                    }
                }
            }
        }
    }

    fn begin(&mut self, read_only: bool) -> SqlResult<QueryResult> {
        if self.mysql() && self.in_block() {
            self.commit()?;
        }
        if self.block_is_implicit {
            match self.block {
                Block::Writing(_) => {
                    self.block_is_implicit = false;
                    if self.dialect == Dialect::PostgreSql {
                        self.search_path_at_begin = self.settings.get("search_path").ok();
                    }
                    return Ok(QueryResult::command("BEGIN"));
                }
                Block::Failed => return Err(aborted_block()),
                Block::Reading(_) | Block::None => {
                    self.block_is_implicit = false;
                    self.block = Block::None;
                }
            }
        }
        match self.block {
            Block::Failed => Err(aborted_block()),
            Block::Writing(_) | Block::Reading(_) => Ok(QueryResult::command("BEGIN")
                .with_notice("WARNING:  there is already a transaction in progress".into())),
            Block::None => {
                self.block = if read_only {
                    Block::Reading(self.store.begin_read()?)
                } else {
                    Block::Writing(self.store.begin_write()?)
                };
                if self.dialect == Dialect::PostgreSql {
                    self.search_path_at_begin = self.settings.get("search_path").ok();
                }
                Ok(QueryResult::command("BEGIN"))
            }
        }
    }

    fn restore_search_path(&mut self) {
        if let Some(path) = self.search_path_at_begin.take()
            && self.settings.get("search_path").ok().as_deref() != Some(path.as_str())
        {
            let _ = Arc::make_mut(&mut self.settings).set("search_path", Some(&path));
        }
    }

    fn commit(&mut self) -> SqlResult<QueryResult> {
        self.save_points.clear();
        self.advisory_locks.release_transaction_locks();
        let implicit = std::mem::take(&mut self.block_is_implicit);
        if matches!(self.block, Block::Failed) {
            self.restore_search_path();
        }
        self.search_path_at_begin = None;
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                writer.commit()?;
                self.pending_sequences.clear();
                sequences::mark_changes_committed();
                let committed = QueryResult::command("COMMIT");
                Ok(if implicit && self.dialect == Dialect::PostgreSql {
                    committed.with_notice(NO_TRANSACTION_WARNING.into())
                } else {
                    committed
                })
            }
            Block::Reading(_) => Ok(QueryResult::command("COMMIT")),
            Block::Failed => Ok(QueryResult::command("ROLLBACK")),
            Block::None if self.mysql() || self.oracle() => Ok(QueryResult::command("COMMIT")),
            Block::None => {
                Ok(QueryResult::command("COMMIT").with_notice(NO_TRANSACTION_WARNING.into()))
            }
        }
    }

    fn rollback(&mut self) -> SqlResult<QueryResult> {
        self.save_points.clear();
        self.advisory_locks.release_transaction_locks();
        let implicit = std::mem::take(&mut self.block_is_implicit);
        self.restore_search_path();
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                writer.rollback()?;
                let rolled_back = QueryResult::command("ROLLBACK");
                Ok(if implicit && self.dialect == Dialect::PostgreSql {
                    rolled_back.with_notice(NO_TRANSACTION_WARNING.into())
                } else {
                    rolled_back
                })
            }
            Block::Reading(_) | Block::Failed => Ok(QueryResult::command("ROLLBACK")),
            Block::None if self.mysql() || self.oracle() => Ok(QueryResult::command("ROLLBACK")),
            Block::None => {
                Ok(QueryResult::command("ROLLBACK").with_notice(NO_TRANSACTION_WARNING.into()))
            }
        }
    }
}

struct TrackedWrite<'w> {
    inner: &'w mut dyn KeyValueWrite,
    wrote: bool,
}

impl KeyValueRead for TrackedWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.inner.scan(start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

impl KeyValueWrite for TrackedWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.wrote = true;
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        let inserted = self.inner.put_if_absent(key, value)?;
        self.wrote |= inserted;
        Ok(inserted)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        let deleted = self.inner.delete(key)?;
        self.wrote |= deleted;
        Ok(deleted)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

fn read_only_refusal(error: SqlError, command: &str) -> SqlError {
    if error.state() == SqlState::READ_ONLY_SQL_TRANSACTION {
        SqlError::new(
            SqlState::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {command} in a read-only transaction"),
        )
    } else {
        error
    }
}
