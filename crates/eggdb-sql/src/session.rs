mod classifier_catalog;
mod cursors;
pub(crate) mod database_switch;
mod event_triggers;
mod system_versioning;
mod rowversion;
mod mysql;
pub(crate) mod mysql_events;
mod mysql_load;
mod nonatomic;
mod oracle;
mod oracle_over_tds;
mod prepared;
mod read_only;
mod restore;
mod savepoints;
mod sqlite;
mod sqlite_copy;
pub(crate) mod sqlserver;
pub(crate) mod sqlserver_agent;
mod sqlserver_security;
mod transaction_settings;
mod triggers;
mod tsql;

pub use oracle_over_tds::{PlsqlParameter, PlsqlProcedure};
pub use sqlserver::{BatchStep, NamedParameter, ParameterDeclaration, StoredProcedure};

pub(crate) use oracle::create_text as oracle_create_text;
pub(crate) use oracle::identifier as oracle_identifier;

pub(crate) use sqlite::create_text as sqlite_create_text;
pub(crate) use sqlite::{index_text as sqlite_index_text, view_text as sqlite_view_text};

use crate::ast::{
    CopyDirection, CopySource, CopyStatement, DiscardTarget, Explain, MySqlStatement, Name,
    SqlServerStatement, Statement,
};
use crate::bulk_load::BulkRows;
use crate::conflict_resolution::conflict_resolution_applies;
use crate::copy::{CopyChannel, CopyIo, NoCopyChannel};
use crate::destructive::DestructiveStatement;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::{CopyStop, Token, TokenKind, tokenize_from, tokenize_in, tokenize_script};
use crate::mysql_errors::{self, Warning};
use crate::name_resolution::Binding;
use crate::parser::{LiteralRows, Parameters, Parser, parameter_number};
use crate::plan_cache::CachedStatement;
use crate::plpgsql::TransactionEnd;
use crate::sequences::{self, SequenceAdvance, SessionSequences};
use crate::settings::{DIALECT_SETTING, SettingScope, Settings};
use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};
use crate::types::{SqlType, Value};
use std::sync::Arc;

fn check_copied_columns(
    reader: &dyn KeyValueRead,
    settings: &Settings,
    table: &str,
    listed: &[crate::ast::Name],
    privilege: crate::roles::Privilege,
) -> SqlResult<()> {
    let columns: Vec<String> = if listed.is_empty() {
        crate::roles::relation_columns(reader, table)?
            .unwrap_or_default()
            .into_iter()
            .filter(|column| !crate::temp_tables::is_hidden_column(column))
            .collect()
    } else {
        listed.iter().map(|name| name.text.clone()).collect()
    };
    crate::roles::check_columns(
        reader,
        settings,
        table,
        privilege,
        columns.iter().map(String::as_str),
    )
}

fn starts_query(tokens: &[Token<'_>], at: usize) -> bool {
    match tokens.get(at).map(|token| &token.kind) {
        Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("select") => true,
        Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("table") => matches!(
            tokens.get(at + 1).map(|token| &token.kind),
            Some(TokenKind::Word(_) | TokenKind::QuotedIdentifier(_))
        ),
        _ => false,
    }
}

fn walks_expressions(tokens: &[Token<'_>]) -> bool {
    let mut queries = 0;
    for (at, token) in tokens.iter().enumerate() {
        match &token.kind {
            TokenKind::Word(word) if word.starts_with("current_schema") => return true,
            TokenKind::QuotedIdentifier(name) if name.starts_with("current_schema") => {
                return true;
            }
            _ if starts_query(tokens, at) => queries += 1,
            _ => {}
        }
    }
    let leads_with_query = starts_query(tokens, 0)
        || tokens.first().is_some_and(
            |token| matches!(&token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("with")),
        );
    queries > 1 || queries == 1 && !leads_with_query
}

/// What a statement's tokens say it needs: expression walks, row locks, sequences, the
/// catalog. They depend only on the tokens that are not constants, so the plan cache keeps them
/// with a statement's shape.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatementFlags {
    walks_expressions: bool,
    row_locking: Option<&'static str>,
    uses_sequences: bool,
    wants_catalog: bool,
}

impl StatementFlags {
    pub(crate) fn from_tokens(tokens: Option<&[Token<'_>]>, dialect: Dialect) -> Self {
        Self {
            walks_expressions: tokens.is_none_or(walks_expressions),
            row_locking: tokens.and_then(read_only::row_locking_command),
            uses_sequences: tokens.is_some_and(mentions_sequence_functions),
            wants_catalog: match dialect {
                Dialect::PostgreSql => tokens.is_some_and(crate::pg_catalog::mentions_catalog),
                Dialect::SqlServer => {
                    tokens.is_some_and(crate::sqlserver_catalog::mentions_metadata)
                }
                _ => false,
            },
        }
    }

    fn combined_with(self, other: Self) -> Self {
        Self {
            walks_expressions: self.walks_expressions || other.walks_expressions,
            row_locking: self.row_locking.or(other.row_locking),
            uses_sequences: self.uses_sequences || other.uses_sequences,
            wants_catalog: self.wants_catalog || other.wants_catalog,
        }
    }
}

fn is_sql_prepare(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::Prepared(crate::ast::PreparedCommand::Prepare(_))
    )
}

fn only_separators_after(tokens: &[Token<'_>], at: usize) -> bool {
    tokens.get(at..).is_none_or(|rest| {
        rest.iter()
            .all(|token| matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
    })
}

fn mentions_sequence_functions(tokens: &[Token<'_>]) -> bool {
    tokens.iter().enumerate().any(|(at, token)| {
        let TokenKind::Word(word) = &token.kind else {
            return false;
        };
        ["nextval", "setval", "currval", "lastval"]
            .iter()
            .any(|name| word.eq_ignore_ascii_case(name))
            || (word.eq_ignore_ascii_case("next")
                && tokens.get(at + 1).is_some_and(|next| {
                    matches!(&next.kind, TokenKind::Word(value) if value.eq_ignore_ascii_case("value"))
                }))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseFor {
    Refusing,
    Classifying,
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

pub(crate) fn highest_parameter(tokens: &[Token<'_>]) -> usize {
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
pub(crate) const PLSQL_BLOCK_TAG: &str = "PL/SQL BLOCK";
const POSTGRESQL_CATALOG: &str = "pg_catalog";

fn is_one_statement_naming_the_postgresql_catalog(text: &str) -> bool {
    let mentions = text
        .as_bytes()
        .windows(POSTGRESQL_CATALOG.len())
        .any(|window| window.eq_ignore_ascii_case(POSTGRESQL_CATALOG.as_bytes()));
    if !mentions {
        return false;
    }
    let Ok(tokens) = tokenize_in(text, Dialect::PostgreSql) else {
        return false;
    };
    let mut ended = false;
    let mut qualifies = false;
    let mut previous: Option<&TokenKind<'_>> = None;
    for token in &tokens {
        match &token.kind {
            TokenKind::End => break,
            TokenKind::Semicolon => ended = true,
            _ if ended => return false,
            TokenKind::Dot => {
                qualifies |= previous.is_some_and(|before| match before {
                    TokenKind::Word(word) => word.eq_ignore_ascii_case(POSTGRESQL_CATALOG),
                    TokenKind::QuotedIdentifier(name) => name == POSTGRESQL_CATALOG,
                    _ => false,
                });
            }
            _ => {}
        }
        previous = Some(&token.kind);
    }
    qualifies
}

fn materialized_view_key(name: &str) -> String {
    crate::dump::sql_string(&materialized_view_key_text(name))
}

/// The name a materialized view is kept under in `eggdb_internal.materialized_views`.
pub(crate) fn materialized_view_key_text(name: &str) -> String {
    let name = crate::schemas::normalized(name);
    match name.split_once(crate::schemas::QUALIFIER) {
        Some((schema, relation)) => format!(
            "{}.{}",
            double_quoted_key_part(schema),
            double_quoted_key_part(relation)
        ),
        None if name.starts_with('"') => double_quoted_key_part(name),
        None => name.to_owned(),
    }
}

fn double_quoted_key_part(part: &str) -> String {
    format!("\"{}\"", part.replace('"', "\"\""))
}

fn rolls_back_transaction_on_conflict(statement: &Statement, error: &SqlError) -> bool {
    let rolls_back = match statement {
        Statement::Insert(insert) => insert.rollback_on_conflict,
        Statement::Update(update) => {
            update.conflict_resolution == crate::ast::ConflictResolution::Rollback
        }
        _ => false,
    };
    rolls_back && conflict_resolution_applies(error)
}

fn aborted_block() -> SqlError {
    SqlError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

enum Block<'s> {
    None,
    Writing(Box<dyn crate::store::BoxedWrite + 's>),
    Reading(Box<dyn KeyValueRead + 's>),
    Failed,
}

/// A SQL session over a [`KeyValueStore`]: it runs statements of its [`Dialect`] (PostgreSQL's
/// unless [`Session::with_dialect`] chose MySQL's) one after another, each in its own
/// transaction unless a `BEGIN` opened a block.
///
/// A block holds the store's one write transaction from `BEGIN` to `COMMIT` or `ROLLBACK`.
/// In PostgreSQL's dialect a failed statement inside a block rolls the block back at once and
/// leaves the session in the aborted state: every statement fails with SQLSTATE `25P02` until
/// `ROLLBACK` (or `COMMIT`, which then reports `ROLLBACK`). In MySQL's, SQLite's, Oracle's and
/// SQL Server's a failed statement undoes only its own changes and the block goes on, except
/// SQLite's `INSERT OR FAIL`, which keeps the rows it wrote before the conflict, and SQL
/// Server's `SET XACT_ABORT ON` and run-time errors that end its batch, which roll the
/// transaction back. Errors carry each dialect's numbers and wording.
pub struct Session<'s> {
    store: &'s dyn crate::store::ErasedStore,
    block: Block<'s>,
    tables: TableCache,
    settings: Arc<Settings>,
    dialect: Dialect,
    connection_dialect: Dialect,
    warnings: Vec<Warning>,
    sequence_values: SessionSequences,
    pending_sequences: Vec<SequenceAdvance>,
    statement_uses_sequences: bool,
    statement_walks_expressions: bool,
    statement_row_locking: Option<&'static str>,
    search_path_cache: crate::schemas::PathCache,
    transaction_settings: crate::settings::TransactionSettings,
    max_value_bytes: usize,
    max_row_bytes: usize,
    deadline: Option<std::time::Instant>,
    max_result_rows: Option<u64>,
    batch_resume: Option<(usize, usize)>,
    sqlserver_status: crate::sqlserver_functions::SessionStatus,
    oracle_nls: Arc<crate::oracle_nls::NlsParameters>,
    plsql: crate::plsql::SharedPlsqlSession,
    identity_insert_table: Option<String>,
    keeps_identity_values: bool,
    statement_start: usize,
    shown_statement: Option<String>,
    explain_plan_displays: bool,
    scan_stats: bool,
    creating_plan_table: bool,
    table_variables: crate::table_variables::TableVariables,
    extra_results: Vec<QueryResult>,
    cancel: crate::cancel::CancelSignal,
    implicit_transaction: bool,
    block_is_implicit: bool,
    statements_in_text: usize,
    statement_stands_alone: bool,
    connection_settings: Option<Arc<Settings>>,
    pending_bulk_load: Option<String>,
    batch_context: Arc<crate::tsql::BatchContext>,
    condition_next: bool,
    condition_holds: Option<bool>,
    pending_dynamic: Option<crate::tsql::DynamicBatch>,
    pending_procedure: Option<crate::tsql::ProcedureRun>,
    dynamic_depth: u8,
    tsql_cursors: Vec<crate::tsql::Cursor>,
    returned_value: Option<Value>,
    unprepare_requests: Vec<i32>,
    parameter_values: Vec<NamedParameter>,
    set_error: Option<u32>,
    save_points: Vec<String>,
    savepoints: savepoints::Savepoints,
    failed_statement_was_undone: bool,
    keeps_failed_statement_changes: bool,
    transaction_mode: read_only::TransactionMode,
    advisory_locks: crate::advisory_locks::AdvisoryLocks,
    notifications: crate::notifications::Notifications,
    deallocated: Vec<Option<String>>,
    sql_prepared: prepared::SqlPreparedStatements,
    _open: crate::activity::OpenSession,
    temporary: crate::temporary::TemporaryObjects,
    portals: crate::portals::Portals,
    deferred: crate::deferred_checks::DeferredState,
    pending_user_types: Option<Arc<crate::user_types::Registry>>,
    plan_cache: crate::plan_cache::PlanCache,
    trigger_transaction: bool,
    served_databases: Option<&'s dyn crate::served_databases::ServedDatabases>,
    new_file_writer: Option<&'s dyn crate::new_files::NewFileWriter>,
    file_reader: Option<&'static dyn crate::new_files::FileReader>,
    database_switch: Option<String>,
    database_listing_version: Option<u64>,
    protocol: Option<&'static str>,
}

impl std::fmt::Debug for Session<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("in_block", &self.in_block())
            .finish()
    }
}

impl<'s> Session<'s> {
    /// A session over `store`, outside any block.
    pub fn new<S: KeyValueStore>(store: &'s S) -> Self {
        Self::with_dialect(store, Dialect::PostgreSql)
    }

    /// A session over `store` that reads `dialect`, outside any block.
    pub fn with_dialect<S: KeyValueStore>(store: &'s S, dialect: Dialect) -> Self {
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
            statement_row_locking: None,
            search_path_cache: crate::schemas::PathCache::default(),
            transaction_settings: crate::settings::TransactionSettings::default(),
            max_value_bytes: crate::memory::MAX_VALUE_BYTES,
            max_row_bytes: crate::memory::MAX_ROW_BYTES,
            deadline: None,
            max_result_rows: None,
            batch_resume: None,
            sqlserver_status: crate::sqlserver_functions::SessionStatus::default(),
            oracle_nls: Arc::new(crate::oracle_nls::NlsParameters::default()),
            plsql: crate::plsql::SharedPlsqlSession::default(),
            identity_insert_table: None,
            keeps_identity_values: false,
            statement_start: 0,
            shown_statement: None,
            explain_plan_displays: false,
            scan_stats: false,
            creating_plan_table: false,
            table_variables: crate::table_variables::TableVariables::default(),
            extra_results: Vec::new(),
            cancel: crate::cancel::CancelSignal::default(),
            implicit_transaction: false,
            statements_in_text: 0,
            statement_stands_alone: false,
            block_is_implicit: false,
            connection_settings: None,
            pending_bulk_load: None,
            batch_context: Arc::new(crate::tsql::BatchContext::default()),
            condition_next: false,
            condition_holds: None,
            pending_dynamic: None,
            pending_procedure: None,
            dynamic_depth: 0,
            tsql_cursors: Vec::new(),
            returned_value: None,
            unprepare_requests: Vec::new(),
            parameter_values: Vec::new(),
            set_error: None,
            save_points: Vec::new(),
            savepoints: savepoints::Savepoints::default(),
            failed_statement_was_undone: false,
            keeps_failed_statement_changes: false,
            transaction_mode: read_only::TransactionMode::default(),
            advisory_locks: crate::advisory_locks::AdvisoryLocks::new(store.store_id()),
            temporary: crate::temporary::TemporaryObjects::default(),
            portals: crate::portals::Portals::default(),
            notifications: crate::notifications::Notifications::default(),
            deallocated: Vec::new(),
            sql_prepared: prepared::SqlPreparedStatements::default(),
            _open: crate::activity::OpenSession::new(),
            deferred: crate::deferred_checks::DeferredState::default(),
            pending_user_types: None,
            plan_cache: crate::plan_cache::PlanCache::default(),
            trigger_transaction: false,
            served_databases: None,
            new_file_writer: None,
            file_reader: None,
            database_switch: None,
            database_listing_version: None,
            protocol: None,
        }
    }

    /// Sets the largest text or binary value a statement of this session may build, such as
    /// the result of `||`, `CONCAT`, `LPAD` or `REPLACE`, checked before the value is built;
    /// a larger one fails with SQLSTATE `54000`. [`crate::MAX_VALUE_BYTES`] (1 GiB) by default
    /// and at most; a server that runs untrusted statements sets it low.
    pub fn set_max_value_bytes(&mut self, bytes: usize) {
        self.max_value_bytes = bytes.min(crate::memory::MAX_VALUE_BYTES);
    }

    /// Sets the largest row a statement of this session may build from its select list and
    /// sort keys, counted value by value as the row is built, so a select list that copies one
    /// large value into many columns stops before it allocates them all; a larger row fails
    /// with SQLSTATE `54000`. [`crate::MAX_ROW_BYTES`] (2 GiB) by default and at most; a
    /// server that runs untrusted statements sets it low.
    pub fn set_max_row_bytes(&mut self, bytes: usize) {
        self.max_row_bytes = bytes.min(crate::memory::MAX_ROW_BYTES);
    }

    /// Stops every statement of this session still running at `deadline` with SQLSTATE
    /// `57014`, as `statement_timeout` does (the earlier of the two applies), checked by the
    /// engine itself as it works through rows, so a statement that reads nothing from the
    /// store stops too. `None` (the default) sets no deadline. A server
    /// that runs untrusted statements under a time limit sets it.
    pub fn set_deadline(&mut self, deadline: Option<std::time::Instant>) {
        self.deadline = deadline;
    }

    /// Makes a `SELECT` of this session return at most `rows` rows: the engine stops reading
    /// once it has them, as a `LIMIT` would (a statement's own lower `LIMIT` stays), so a
    /// query that would produce millions of rows never builds them. Statements that can write
    /// (a `SELECT` of a function that may change data, `nextval`, `INSERT ... RETURNING`) are
    /// never cut. `None` (the default) sets no cap. A server that sends at most `n` rows
    /// sets `n + 1`, so it can tell the client the result was cut.
    pub fn set_max_result_rows(&mut self, rows: Option<u64>) {
        self.max_result_rows = rows;
    }

    /// With `true`, Oracle's `EXPLAIN PLAN` writes no plan table and answers with the
    /// `PLAN_TABLE_OUTPUT` lines `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)` would show right
    /// after it, so a caller that opens the database read-only (the CLI's `eggdb explain`,
    /// MCP's `explain_query`) can explain in Oracle's dialect. It also makes a statement EggDB
    /// cannot plan under SQL Server's `SET SHOWPLAN_TEXT` or `SHOWPLAN_ALL` fail with
    /// `not_supported`, as Oracle's `EXPLAIN PLAN` does, instead of answering with only the
    /// statement's text. `false` is the default.
    pub fn set_explain_plan_displays(&mut self, displays: bool) {
        self.explain_plan_displays = displays;
    }

    fn set_role(&mut self, wanted: Option<&str>) -> SqlResult<()> {
        let (login, login_superuser) = match &self.settings.role_before_set_role {
            Some((_, superuser)) => (
                self.settings.login_role.clone().unwrap_or_else(|| self.settings.get("session_authorization").unwrap_or_default()),
                *superuser,
            ),
            None => (
                self.settings.login_role.clone().unwrap_or_else(|| self.settings.get("session_authorization").unwrap_or_default()),
                self.settings.checked_role.is_none() && self.settings.is_superuser(),
            ),
        };
        let wanted = wanted.filter(|wanted| *wanted != login);
        let Some(wanted) = wanted else {
            let settings = Arc::make_mut(&mut self.settings);
            if let Some((checked_role, superuser)) = settings.role_before_set_role.take() {
                settings.checked_role = checked_role;
                settings.set_superuser(superuser);
            }
            settings.choose_role(None);
            return Ok(());
        };
        let chosen = self.read(|reader, _| crate::roles::choose_role(reader, &login, login_superuser, wanted))?;
        let settings = Arc::make_mut(&mut self.settings);
        if settings.role_before_set_role.is_none() {
            settings.role_before_set_role = Some((settings.checked_role.clone(), login_superuser));
        }
        settings.checked_role = chosen.checked_role;
        settings.set_superuser(chosen.superuser);
        settings.choose_role(Some(wanted));
        Ok(())
    }

    /// SQLite's `.scanstats on`: in the SQLite dialect, each `SELECT` is followed by a second
    /// result, its `QUERY PLAN` with `(loops=1 rows=n)` on each scan the executor measured.
    /// `false` is the default.
    pub fn set_scan_stats(&mut self, on: bool) {
        self.scan_stats = on;
    }

    fn scan_stats_plan(&mut self, statement: &Statement) -> Option<crate::explain::Planned> {
        if !(self.scan_stats && self.sqlite())
            || crate::explain::PlannedStatement::of(statement) != Some(crate::explain::PlannedStatement::Select)
        {
            return None;
        }
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: true,
            target_privilege: Some(crate::roles::target_privilege(statement)),
        };
        let mut planned = statement.clone();
        let dialect = self.dialect;
        self.read(|reader, tables| {
            binding.statement(reader, tables, &mut planned)?;
            crate::explain::plan(reader, tables, &planned, crate::ast::ExplainStyle::QueryPlan, dialect)
        })
        .ok()
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

    fn reads_the_postgresql_catalog(&self, text: &str) -> bool {
        self.connection_dialect == Dialect::PostgreSql
            && self.dialect != Dialect::PostgreSql
            && is_one_statement_naming_the_postgresql_catalog(text)
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

    /// Names the protocol this session's statements arrive on, which the slow-statement log
    /// reports (D286); by default it is the connection's dialect, such as `postgresql`,
    /// `mysql` or `sqlserver`.
    pub fn set_protocol(&mut self, protocol: &'static str) {
        self.protocol = Some(protocol);
    }

    fn protocol(&self) -> &'static str {
        self.protocol
            .unwrap_or_else(|| self.connection_dialect.name())
    }

    fn record_statement(
        &self,
        tokens: Option<&[crate::lexer::Token<'_>]>,
        text: &str,
        started: Option<std::time::Instant>,
        result: &SqlResult<QueryResult>,
    ) {
        let (Some(tokens), Some(started), Ok(result)) = (tokens, started, result) else {
            return;
        };
        let user = self
            .settings
            .get("session_authorization")
            .unwrap_or_default();
        crate::statement_statistics::record(
            tokens,
            text,
            &crate::statement_statistics::RecordedStatement {
                database: &self.settings.database,
                user: &user,
                protocol: self.protocol(),
                duration: started.elapsed(),
                rows: crate::statement_statistics::rows_of(result),
            },
        );
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

    /// The channels this session listens on (`LISTEN`).
    pub fn listening_channels(&self) -> Vec<String> {
        self.notifications.listening().iter().cloned().collect()
    }

    /// Takes the notifications this session's committed transactions sent, oldest first, for
    /// the server to deliver to the sessions listening on their channels.
    pub fn take_notifications(&mut self) -> Vec<crate::Notification> {
        self.notifications.take_committed()
    }

    /// Checks that this session's role may read every stored row of `schema.name` directly, as
    /// a tool that pages through stored rows does: it holds `SELECT`, and row security does not
    /// filter the table for it.
    pub fn check_readable(&mut self, schema: &str, name: &str) -> SqlResult<()> {
        let settings = Arc::clone(&self.settings);
        let table = crate::schemas::qualified(schema, name);
        self.read(|reader, _| {
            crate::roles::check_table(reader, &settings, &table, crate::roles::Privilege::Select)?;
            if crate::row_security::visible_rows(
                reader,
                &settings,
                &table,
                crate::row_security::PolicyCommand::Select,
            )?
            .is_some()
            {
                return Err(SqlError::not_supported(
                    "reading the stored rows of a table with row-level security for this role",
                )
                .with_hint("Query the table with SQL, which applies the policies."));
            }
            Ok(())
        })
    }

    /// Names every database the server serves, which `pg_database` lists beside this one.
    pub fn set_database_names(&mut self, names: Vec<String>) {
        Arc::make_mut(&mut self.settings).database_names = names;
    }

    /// Lets the session create files for statements that write one, such as SQLite's
    /// `VACUUM INTO`; without it those statements are refused.
    pub fn set_new_file_writer(&mut self, writer: &'s dyn crate::new_files::NewFileWriter) {
        self.new_file_writer = Some(writer);
    }

    /// Lets this session's SQL read files on this machine, as `read_parquet` does, through
    /// `reader`. Embedded use gives it one; servers never do.
    pub fn set_file_reader(&mut self, reader: &'static dyn crate::new_files::FileReader) {
        self.file_reader = Some(reader);
    }

    /// Gives the session the server's databases: `CREATE DATABASE` and `DROP DATABASE` run
    /// through `served`, and the catalogs list its databases as they are at each statement.
    pub fn set_served_databases(
        &mut self,
        served: &'s dyn crate::served_databases::ServedDatabases,
    ) {
        self.served_databases = Some(served);
        self.database_listing_version = None;
        self.refresh_database_names();
    }

    /// The server's databases [`Session::set_served_databases`] gave this session, if any.
    pub fn served_databases(&self) -> Option<&'s dyn crate::served_databases::ServedDatabases> {
        self.served_databases
    }

    fn refresh_database_names(&mut self) {
        let Some(served) = self.served_databases else {
            return;
        };
        let version = served.listing_version();
        if version.is_some() && version == self.database_listing_version {
            return;
        }
        self.database_listing_version = version;
        let names = served.names();
        if names != self.settings.database_names {
            self.set_database_names(names);
        }
    }

    fn run_database_command(
        &mut self,
        statement: &crate::served_databases::DatabaseStatement,
    ) -> SqlResult<QueryResult> {
        let checked_role = self.settings.checked_role.clone();
        let may_create_databases = match &checked_role {
            None => true,
            Some(role) => self
                .read(|reader, _| crate::roles::stored_role(reader, role))?
                .is_some_and(|role| role.create_database),
        };
        let settings = Arc::clone(&self.settings);
        let result = crate::served_databases::run(
            &crate::served_databases::DatabaseCommandContext {
                served: self.served_databases,
                dialect: self.dialect,
                current_database: &settings.database,
                superuser: checked_role.is_none(),
                login_role: settings.login_role.as_deref(),
                may_create_databases,
                in_block: self.in_block(),
            },
            statement,
        );
        self.refresh_database_names();
        result
    }

    /// Names a stored role this session runs as, like [`Session::identify`], and checks its
    /// privileges from now on unless it is a superuser. Fails with `28000` when the role does not
    /// exist or may not log in.
    pub fn identify_role(&mut self, user: &str, database: &str) -> SqlResult<()> {
        let role = self.read(|reader, _| crate::roles::stored_role(reader, user))?;
        let Some(role) = role.filter(|role| role.login) else {
            return Err(SqlError::new(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{user}\" is not permitted to log in"),
            ));
        };
        self.identify(user, database);
        let settings = Arc::make_mut(&mut self.settings);
        settings.checked_role = (!role.superuser).then(|| role.name.clone());
        settings.set_superuser(role.superuser);
        settings.login_role = Some(role.name);
        Ok(())
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

    /// The session's `TimeZone`: what a `timestamp with time zone` is shown in, a fixed
    /// offset or a region of the time zone database.
    pub fn time_zone(&self) -> crate::time_zones::TimeZone {
        self.settings.time_zone
    }

    /// Turns on the server output that PL/SQL writes with `DBMS_OUTPUT.PUT_LINE`, as SQL*Plus
    /// `SET SERVEROUTPUT ON [SIZE n]` does: lines are kept, up to `buffer_bytes` (`None` for
    /// as much as EggDB allows, 1 MiB) until [`Session::take_server_output`] takes them.
    /// Until then a block's `DBMS_OUTPUT` calls are ignored, as in an Oracle session whose
    /// client never enabled it.
    pub fn enable_server_output(&mut self, buffer_bytes: Option<usize>) {
        if let Ok(mut state) = self.plsql.lock() {
            state.output.enable(buffer_bytes);
        }
    }

    /// Turns the server output off again and drops the lines not yet taken, as SQL*Plus
    /// `SET SERVEROUTPUT OFF` does.
    pub fn disable_server_output(&mut self) {
        if let Ok(mut state) = self.plsql.lock() {
            state.output.disable();
        }
    }

    /// The complete lines PL/SQL wrote with `DBMS_OUTPUT` since the last call, oldest first,
    /// which a client prints after each statement as SQL*Plus does. A line still being built
    /// with `DBMS_OUTPUT.PUT` stays until it ends.
    pub fn take_server_output(&mut self) -> Vec<String> {
        self.plsql
            .lock()
            .map(|mut state| state.output.take_lines())
            .unwrap_or_default()
    }

    /// A reader of this session's server output that a result callback can hold while the
    /// session runs: it takes the lines [`Session::take_server_output`] would.
    #[must_use]
    pub fn server_output_reader(&self) -> crate::plsql::ServerOutputReader {
        crate::plsql::ServerOutputReader::new(&self.plsql)
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
        let ends_settings = !self.in_block();
        if ends_settings {
            self.advisory_locks.release_transaction_locks();
        }
        if !std::mem::take(&mut self.block_is_implicit) {
            if ends_settings {
                self.end_transaction_settings(commit);
                self.sync_transaction_read_only();
            }
            return Ok(());
        }
        let pending = std::mem::take(&mut self.deferred.pending);
        self.deferred.clear();
        let ended = match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) if commit => self.commit_writes(writer, &pending),
            Block::Writing(writer) => {
                self.notifications.rollback();
                crate::activity::rolled_back();
                self.temporary.rollback();
                writer.rollback()
            }
            Block::Reading(_) if commit => {
                self.temporary.commit();
                self.notifications.commit();
                Ok(())
            }
            Block::Reading(_) => {
                self.temporary.rollback();
                self.notifications.rollback();
                Ok(())
            }
            Block::Failed | Block::None => {
                self.notifications.rollback();
                Ok(())
            }
        };
        if ends_settings {
            self.end_transaction_settings(commit && ended.is_ok());
        }
        self.sync_transaction_read_only();
        let settled = self.settle_sequences();
        ended.and(settled)
    }

    fn commit_writes(
        &mut self,
        writer: Box<dyn crate::store::BoxedWrite + 's>,
        pending: &[crate::deferred_checks::PendingCheck],
    ) -> SqlResult<()> {
        if !pending.is_empty() {
            let checked = crate::constraints::check_pending(
                &crate::temporary::TemporaryRead {
                    inner: &writer,
                    objects: &self.temporary,
                },
                &mut self.tables,
                pending,
            );
            if let Err(error) = checked {
                let _ = writer.rollback();
                self.temporary.rollback();
                self.notifications.rollback();
                crate::activity::rolled_back();
                return Err(error);
            }
        }
        if let Err(error) = writer.commit() {
            self.temporary.rollback();
            self.notifications.rollback();
            crate::activity::rolled_back();
            return Err(error);
        }
        self.temporary.commit();
        self.notifications.commit();
        crate::activity::committed();
        self.pending_sequences.clear();
        sequences::mark_changes_committed();
        Ok(())
    }

    /// Makes the run-time parameters now in force the ones `RESET` and `DISCARD ALL` return
    /// to, as PostgreSQL returns to the values a client sent when it connected rather than to
    /// the server's defaults.
    pub fn remember_connection_settings(&mut self) {
        self.connection_settings = Some(Arc::clone(&self.settings));
    }

    fn reset_all_settings(&mut self) {
        if self.settings_follow_transaction() {
            self.transaction_settings.before_reset_all(&self.settings);
        }
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

    fn connection_value(&self, name: &str) -> Option<String> {
        self.connection_settings
            .as_ref()
            .and_then(|settings| settings.get(name).ok())
    }

    fn reset_setting(&mut self, name: &str) -> SqlResult<()> {
        self.change_setting(name, None, SettingScope::Session)
            .map(drop)
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
                crate::portals::take();
                self.temporary.discard();
                self.temporary.commit();
                self.sql_prepared.forget(None);
                Ok(QueryResult::command("DISCARD ALL"))
            }
            DiscardTarget::Plans => Ok(QueryResult::command("DISCARD PLANS")),
            DiscardTarget::Sequences => {
                sequences::discard_session_values();
                Ok(QueryResult::command("DISCARD SEQUENCES"))
            }
            DiscardTarget::Temporary => {
                self.temporary.discard();
                if !self.in_block() {
                    self.temporary.commit();
                }
                Ok(QueryResult::command("DISCARD TEMP"))
            }
            DiscardTarget::PreparedStatement(name) => {
                let tag = if name.is_some() {
                    "DEALLOCATE"
                } else {
                    "DEALLOCATE ALL"
                };
                self.sql_prepared.forget(name.as_deref());
                match &name {
                    Some(named) => self
                        .deallocated
                        .retain(|known| known.as_ref() != Some(named)),
                    None => self.deallocated.clear(),
                }
                self.deallocated.push(name);
                Ok(QueryResult::command(tag))
            }
        }
    }

    /// Takes the prepared statements `DEALLOCATE` named since the last call, oldest first, for
    /// the server to forget: a name, or `None` for `DEALLOCATE ALL`.
    pub fn take_deallocated_statements(&mut self) -> Vec<Option<String>> {
        std::mem::take(&mut self.deallocated)
    }

    /// Checks `text` as the text of a prepared statement: at most one statement, valid
    /// syntax, and `$n` placeholders numbered 1 to 65535. Nothing runs, and the catalog is read
    /// only for the definitions of the user-defined types the text names. `declared_types` are
    /// the parameter types the client gave.
    pub fn prepare(
        &mut self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<PreparedStatement> {
        if self.reads_the_postgresql_catalog(text) {
            return self.in_connection_dialect(|session| session.prepare(text, declared_types));
        }
        self.prepare_in_dialect(text, declared_types)
            .map_err(|error| self.in_dialect(text, error))
    }

    fn prepare_in_dialect(
        &mut self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<PreparedStatement> {
        let tokens = tokenize_in(text, self.dialect)?;
        let settings = Arc::clone(&self.settings);
        let _path = self.enter_search_path(&settings);
        let _types = crate::user_types::enter(None);
        let mut retried = false;
        let (mut parser, prepares_statement) = loop {
            let mut parser = Parser::new(text, &tokens)
                .with_context(Parameters::Types(declared_types), &settings)
                .in_dialect(self.dialect);
            parser.skip_empty_statements();
            if parser.at_end() {
                return Ok(PreparedStatement {
                    parameter_count: declared_types.len(),
                    is_empty: true,
                });
            }
            crate::user_types::take_wanted();
            if parser.literal_rows().is_some() {
                break (parser, false);
            }
            match parser.statement() {
                Ok(statement) => break (parser, is_sql_prepare(&statement)),
                Err(_) if !retried && crate::user_types::take_wanted() => {
                    retried = true;
                    let registry = self.read(|reader, _| crate::user_types::load(reader))?;
                    crate::user_types::install(registry);
                }
                Err(error) => return Err(error),
            }
        };
        parser.skip_empty_statements();
        if !parser.at_end() {
            return Err(SqlError::syntax(
                "cannot insert multiple commands into a prepared statement",
                parser.position(),
            ));
        }
        let parameter_count = if prepares_statement {
            declared_types.len()
        } else {
            highest_parameter(&tokens).max(declared_types.len())
        };
        Ok(PreparedStatement {
            parameter_count,
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
        if self.reads_the_postgresql_catalog(text) {
            return self.in_connection_dialect(|session| session.describe(text, declared_types));
        }
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
        let _types = crate::user_types::enter(None);
        let mut retried = false;
        let statement = loop {
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
            crate::user_types::take_wanted();
            let parsed = match parser.literal_rows() {
                Some(_) => Ok(None),
                None => parser.statement().map(Some),
            };
            drop(parser);
            match parsed {
                Ok(statement) => break statement,
                Err(_) if !retried && crate::user_types::take_wanted() => {
                    retried = true;
                    let registry = self.read(|reader, _| crate::user_types::load(reader))?;
                    crate::user_types::install(registry);
                }
                Err(error) => return Err(error),
            }
        };
        let Some(statement) = statement else {
            return Ok(StatementDescription {
                parameter_types,
                columns: None,
            });
        };
        if is_sql_prepare(&statement) {
            parameter_types.truncate(declared_types.len());
        }
        if matches!(
            statement,
            Statement::Begin { .. }
                | Statement::Commit
                | Statement::Rollback
                | Statement::Savepoint(_)
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
            target_privilege: None,
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
                crate::joined_dml::update_columns(reader, tables, update)
            }),
            Statement::Delete(delete) => self.read(|reader, tables| {
                binding.delete(reader, tables, delete)?;
                crate::joined_dml::delete_columns(reader, tables, delete)
            }),
            Statement::With(with) => self.read(|reader, tables| {
                crate::with_statement::describe(reader, tables, with, binding)
            }),
            Statement::Show { name } => Ok(Some(self.show_columns(name.as_deref()))),
            Statement::Prepared(crate::ast::PreparedCommand::Execute(execute)) => {
                self.prepared_columns(execute, binding)
            }
            Statement::MySql(statement) => self.describe_mysql(statement),
            Statement::Pragma(pragma) => Ok(self.pragma_columns(pragma)),
            Statement::Explain(explain) if explain.plan_table.is_some() => Ok(None),
            Statement::Explain(explain) => {
                Ok(Some(crate::explain::columns(explain.style, self.dialect)))
            }
            Statement::Cursor(crate::ast::CursorCommand::Fetch {
                name,
                only_move: false,
                ..
            }) => Ok(self.portals.columns_of(name)),
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
        if self.reads_the_postgresql_catalog(text) {
            return self.in_connection_dialect(|session| {
                session.execute_prepared_with_copy(text, parameters, on_result, copy)
            });
        }
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

    /// Whether a `SAVEPOINT` is set in the open block, so that a `ROLLBACK TO SAVEPOINT` could
    /// undo part of it: the writes of this session's statements, and the ones recorded with
    /// [`Session::record_savepoint_undo`].
    pub fn has_savepoints(&self) -> bool {
        self.savepoints.is_active()
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
        if self.reads_the_postgresql_catalog(text) {
            return self
                .in_connection_dialect(|session| session.execute_with_copy(text, on_result, copy));
        }
        let mut client = Client {
            channel: copy,
            script: false,
            bulk_rows: None,
        };
        let outcome = self.run_text(text, Parameters::Absent, on_result, &mut client);
        outcome.map_err(|error| self.failed_text(text, error))
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
        let outcome = self.run_text(text, Parameters::Absent, on_result, &mut client);
        outcome.map_err(|error| self.failed_text(text, error))
    }

    fn failed_text(&mut self, text: &str, error: SqlError) -> SqlError {
        let error = self.in_dialect(text, error);
        match self.abort_transaction_after(&error) {
            Ok(()) => error,
            Err(rollback) => rollback,
        }
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
    /// statement is destructive, without running it. For a `DELETE` or `UPDATE` it reads the
    /// keys, foreign keys and triggers of the table it changes; when it cannot read them, the
    /// statement counts as one that can change many rows.
    pub fn destructive_statement(
        &mut self,
        text: &str,
    ) -> Result<Option<DestructiveStatement>, OneStatementError> {
        let tokens = match tokenize_in(text, self.dialect) {
            Ok(tokens) => tokens,
            Err(error) => return Err(OneStatementError::Failed(self.in_dialect(text, error))),
        };
        match self.one_statement_in(text, &tokens, ParseFor::Classifying) {
            Ok(Ok(statement)) => Ok(self.destructive_part_of(statement)),
            Ok(Err(refusal)) => Err(OneStatementError::Refused(refusal)),
            Err(error) => Err(OneStatementError::Failed(self.in_dialect(text, error))),
        }
    }

    fn destructive_part_of(
        &mut self,
        statement: Option<Statement>,
    ) -> Option<DestructiveStatement> {
        let statement = statement?;
        crate::destructive::classify(&statement, self)
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
        let parse_for = match rule {
            DestructiveRule::OnlyConfirmed(_) => ParseFor::Classifying,
            DestructiveRule::Allowed => ParseFor::Refusing,
        };
        match self.one_statement_in(text, &tokens, parse_for) {
            Ok(Ok(statement)) => {
                let destructive = match rule {
                    DestructiveRule::OnlyConfirmed(_) => self.destructive_part_of(statement),
                    DestructiveRule::Allowed => None,
                };
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
        if self.sqlserver() && crate::tsql::uses_batch_logic(&tokens) {
            self.statement_start = 0;
            return self
                .run_logic_text(text, tokens, Parameters::Absent, on_result)
                .map_err(|error| OneStatementError::Failed(self.in_dialect(text, error)));
        }
        let mut client = Client {
            channel: &mut NoCopyChannel,
            script: false,
            bulk_rows: None,
        };
        self.statements_in_text = 0;
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
        parse_for: ParseFor,
    ) -> SqlResult<Result<Option<Statement>, OneStatementRefusal>> {
        let settings = Arc::clone(&self.settings);
        let _clock = crate::datetime::enter_statement(settings.time_zone);
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
        let statement = if parse_for == ParseFor::Classifying || parser.literal_rows().is_none() {
            match parser.statement()? {
                Statement::Begin { .. }
                | Statement::SetTransaction(_)
                | Statement::Commit
                | Statement::Rollback
                | Statement::Savepoint(_)
                | Statement::SqlServer(
                    SqlServerStatement::SaveTransaction(_) | SqlServerStatement::RollbackTo(_),
                ) => {
                    return Ok(Err(OneStatementRefusal::TransactionControl));
                }
                Statement::Copy(_) => return Ok(Err(OneStatementRefusal::Copy)),
                statement => Some(statement),
            }
        } else {
            None
        };
        parser.skip_empty_statements();
        if parser.at_end() {
            Ok(Ok(statement))
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
        let reader = self.file_reader;
        crate::new_files::with_reader(reader, || self.run_text_with_reader(text, parameters, on_result, client))
    }

    fn run_text_with_reader(
        &mut self,
        text: &str,
        parameters: Parameters<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
        client: &mut Client<'_>,
    ) -> SqlResult<()> {
        self.statement_start = 0;
        self.statements_in_text = 0;
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
        let mut types_loaded = false;
        loop {
            let statement_dialect = self.dialect;
            if self.sqlserver() {
                self.sync_transaction_count();
            }
            self.show_requested_isolation();
            let settings = Arc::clone(&self.settings);
            let context = Arc::clone(&self.batch_context);
            let _cancel = crate::cancel::enter(&self.cancel);
            let _deadline = crate::cancel::enter_deadline(self.statement_timeout(), self.deadline);
            let _advisory = crate::advisory_locks::enter(
                &self.advisory_locks,
                !matches!(self.block, Block::None) || self.implicit_transaction,
            );
            let _clock = crate::datetime::enter_statement(settings.time_zone);
            let _regex = crate::regex::enter_statement();
            let _notify = crate::notifications::enter_statement();
            let _bytes = crate::bytea::enter_dialect(self.dialect);
            let _values = crate::memory::enter_value_limit(self.max_value_bytes);
            let _rows = crate::memory::enter_row_limit(self.max_row_bytes);
            let _path = self.enter_search_path(&settings);
            let preloaded_types = self.pending_user_types.take();
            let parses_without_types = preloaded_types.is_none();
            let _types = crate::user_types::enter(preloaded_types);
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
            self.statements_in_text = self.statements_in_text.saturating_add(1);
            let _status = self.enter_sqlserver_status();
            let _variables = self
                .sqlserver()
                .then(|| crate::table_variables::enter(&self.table_variables));
            let _constraints = self.enter_constraints();
            let _nls = self.enter_oracle_nls();
            let _plsql = self
                .oracle()
                .then(|| crate::plsql::enter_session(&self.plsql));
            let literal_start = parser.resume_point().0;
            if self.settings.checked_role.is_none()
                && let Some(mut rows) = parser.literal_rows()
            {
                let _catalog = crate::pg_catalog::enter_statement(false, &settings);
                resume = parser.resume_point();
                if self.batch_resume.is_some() {
                    self.batch_resume = Some(resume);
                }
                crate::mysql_errors::clear_evaluation_warnings();
            crate::table_sample::reset();
                crate::mysql_errors::set_strict_write(self.mysql());
                self.enter_sequences(false);
                crate::deferred_checks::enter(&self.deferred.modes);
                let started = crate::statement_statistics::wanted().then(std::time::Instant::now);
                let result = self.insert_literal_rows(&mut rows);
                let routine_changes = crate::routine_settings::take_for_session(result.is_ok());
                let routine_applied = self.apply_routine_setting_changes(routine_changes);
                let result = with_refused_routine_setting(result, routine_applied);
                let literal_tokens = tokens.get(literal_start..resume.0.min(tokens.len()));
                self.record_statement(literal_tokens, text, started, &result);
                let result = self.keep_deferred(result);
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
            let statement_point = parser.resume_point();
            crate::user_types::take_wanted();
            let condition = std::mem::take(&mut self.condition_next);
            let shape = if !condition
                && parses_without_types
                && self.batch_resume.is_none()
                && matches!(parameters, Parameters::Absent)
            {
                self.plan_cache
                    .shape(text, tokens, first_token, self.dialect)
            } else {
                None
            };
            let cached = match shape {
                Some(shape) => self.plan_cache.find(shape, tokens, self.dialect, &settings),
                None => CachedStatement::NeverCached,
            };
            let parsed_unknown_shape = matches!(cached, CachedStatement::Unknown);
            let (parsed, cached_flags) = match (condition, cached, shape) {
                (true, _, _) => (parser.sqlserver_condition(), None),
                (false, CachedStatement::Parsed { statement, flags }, Some(shape)) => {
                    parser = parser.resuming_at((shape.stop(), statement_point.1));
                    (Ok(statement), Some(flags))
                }
                (false, _, _) => (parser.statement(), None),
            };
            let setting_changes = parser.take_setting_changes();
            resume = parser.resume_point();
            drop(parser);
            drop(context);
            if parsed_unknown_shape
                && let Some(shape) = shape
                && let Ok(statement) = &parsed
                && setting_changes.is_empty()
                && resume.1 == statement_point.1
                && !crate::user_types::take_wanted()
            {
                self.plan_cache.remember(
                    shape,
                    tokens,
                    self.dialect,
                    &settings,
                    statement,
                    resume.0,
                );
            }
            if parsed.is_err() && crate::user_types::take_wanted() && !types_loaded {
                types_loaded = true;
                resume = statement_point;
                match self.read(|reader, _| crate::user_types::load(reader)) {
                    Ok(registry) => {
                        self.pending_user_types = Some(registry);
                        self.statements_in_text = self.statements_in_text.saturating_sub(1);
                        continue;
                    }
                    Err(error) => {
                        self.fail_block_in_postgresql();
                        return Err(error);
                    }
                }
            }
            types_loaded = false;
            if self.batch_resume.is_some() {
                self.batch_resume = Some(resume);
            }
            self.statement_stands_alone =
                self.statements_in_text == 1 && only_separators_after(tokens, resume.0);
            let statement_tokens = tokens.get(first_token..resume.0.min(tokens.len()));
            let mut statement = match parsed {
                Ok(statement) => statement,
                Err(error) => {
                    self.fail_block_in_postgresql();
                    return Err(error);
                }
            };
            let mut flags = cached_flags
                .unwrap_or_else(|| StatementFlags::from_tokens(statement_tokens, self.dialect));
            if let Some(executed) = self.executed_prepared_flags(&statement) {
                flags = flags.combined_with(executed);
            }
            self.statement_walks_expressions =
                flags.walks_expressions || self.settings.checked_role.is_some();
            self.statement_row_locking = flags.row_locking;
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
            let _catalog = crate::pg_catalog::enter_statement(flags.wants_catalog, &settings);
            crate::mysql_errors::clear_evaluation_warnings();
            crate::table_sample::reset();
            crate::mysql_errors::set_strict_write(
                self.mysql() && matches!(statement, Statement::Insert(_) | Statement::Update(_)),
            );
            self.enter_sequences(flags.uses_sequences);
            self.extra_results.clear();
            drop(take_notices_before_error());
            crate::deferred_checks::enter(&self.deferred.modes);
            let leftover_changes = crate::routine_settings::take_for_session(true);
            let leftover_applied = self.apply_routine_setting_changes(leftover_changes);
            crate::portals::install(std::mem::take(&mut self.portals));
            if self.sqlserver()
                && (self.settings.sqlserver.showplan.is_some()
                    || self.settings.sqlserver.statistics_profile)
            {
                let end = tokens.get(resume.0).map_or(text.len(), |token| token.start);
                self.shown_statement = text
                    .get(self.statement_start..end)
                    .map(|shown| shown.trim().trim_end_matches(';').trim_end().to_owned());
            }
            let started = crate::statement_statistics::wanted().then(std::time::Instant::now);
            let audited = crate::audit_events::wanted()
                .then(|| {
                    crate::audit_events::statement_kind(&statement).or_else(|| {
                        statement_tokens
                            .is_some_and(crate::audit_events::names_audited_table)
                            .then_some(crate::audit_events::AuditKind::AuditedRead)
                    })
                })
                .flatten();
            let audited_text = audited.map(|_| {
                let end = tokens.get(resume.0).map_or(text.len(), |token| token.start);
                text.get(self.statement_start..end)
                    .map_or_else(String::new, |shown| shown.trim().trim_end_matches(';').trim_end().to_owned())
            });
            let scan_plan = self.scan_stats_plan(&statement);
            let result = match &scan_plan {
                Some(plan) => {
                    let (result, selects, _) =
                        crate::step_times::record_with_subplans(|| with_raised_notices(self.run(&mut statement, &mut io)));
                    if result.is_ok() {
                        self.extra_results.push(crate::explain::render_scan_stats(&plan.query, &selects));
                    }
                    result
                }
                None => with_raised_notices(self.run(&mut statement, &mut io)),
            };
            let routine_changes = crate::routine_settings::take_for_session(result.is_ok());
            self.record_statement(statement_tokens, text, started, &result);
            if let (Some(kind), Some(shown)) = (audited, &audited_text) {
                let user = self.settings.get("session_authorization").unwrap_or_default();
                crate::audit_events::record(&crate::audit_events::AuditEvent {
                    kind,
                    user: &user,
                    database: &self.settings.database,
                    protocol: self.protocol(),
                    detail: shown,
                    succeeded: result.is_ok(),
                });
            }
            self.portals = crate::portals::take();
            if matches!(self.block, Block::None) {
                self.portals.close_at_transaction_end();
            }
            let result = self.keep_deferred(result);
            crate::activity::statement_finished(result.is_ok());
            let raised = crate::notifications::take_raised();
            if result.is_ok() {
                let in_block = !matches!(self.block, Block::None);
                for notification in raised {
                    self.notifications.queue(notification, in_block)?;
                }
            }
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
            _path.0.remember(&mut self.search_path_cache);
            self.leave_sequences();
            let settled = self.settle_sequences();
            let routine_applied = self.apply_routine_setting_changes(routine_changes);
            let result =
                result.and_then(|result| self.apply_setting_changes(result, &setting_changes));
            let result = with_refused_routine_setting(result, leftover_applied);
            let result = with_refused_routine_setting(result, routine_applied);
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

    fn keep_deferred(&mut self, result: SqlResult<QueryResult>) -> SqlResult<QueryResult> {
        let pending = crate::deferred_checks::leave();
        let result = result?;
        if matches!(self.block, Block::Writing(_)) {
            self.deferred.add(pending)?;
        }
        Ok(result)
    }

    fn set_constraints(
        &mut self,
        names: Option<&[Name]>,
        deferred: bool,
    ) -> SqlResult<QueryResult> {
        let shown: Option<Vec<String>> = names.map(|names| {
            names
                .iter()
                .map(|name| crate::schemas::base_name(&name.text).to_owned())
                .collect()
        });
        if let Some(names) = names {
            for name in names {
                let base = crate::schemas::base_name(&name.text).to_owned();
                let found = self
                    .read(|reader, _| crate::constraints::deferrable_constraint(reader, &base))?;
                match found {
                    None => {
                        return Err(SqlError::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("constraint \"{base}\" does not exist"),
                        )
                        .at(name.position));
                    }
                    Some(false) => {
                        return Err(SqlError::new(
                            SqlState::WRONG_OBJECT_TYPE,
                            format!("constraint \"{base}\" is not deferrable"),
                        )
                        .at(name.position));
                    }
                    Some(true) => {}
                }
            }
        }
        let result = QueryResult::command("SET CONSTRAINTS");
        if !matches!(self.block, Block::Writing(_) | Block::Reading(_)) {
            return Ok(result.with_notice(
                "WARNING:  SET CONSTRAINTS can only be used in transaction blocks".to_owned(),
            ));
        }
        self.deferred.modes.set(shown.as_deref(), deferred);
        if !deferred {
            let pending = self.deferred.take_for(shown.as_deref());
            if !pending.is_empty() {
                let checked = self.read(|reader, tables| {
                    crate::constraints::check_pending(reader, tables, &pending)
                });
                if let Err(error) = checked {
                    self.deferred.add(pending)?;
                    return Err(error);
                }
            }
        }
        Ok(result)
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
            self.keeps_identity_values,
            self.settings.checked_role.clone(),
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
            if name == "time_zone" {
                let zone = crate::oracle_date_functions::session_time_zone(value)?;
                Arc::make_mut(&mut self.settings).set(
                    "TimeZone",
                    Some(&crate::oracle_date_functions::time_zone_setting(zone)),
                )?;
                continue;
            }
            self.oracle_nls = Arc::new(self.oracle_nls.with(name, value)?);
        }
        Ok(QueryResult::command("ALTER SESSION"))
    }

    fn enter_search_path(
        &self,
        settings: &Arc<Settings>,
    ) -> (crate::schemas::EnteredPath, crate::temporary::Entered) {
        (
            crate::schemas::enter(settings, self.dialect, &self.search_path_cache),
            crate::temporary::enter(self.temporary.has_objects()),
        )
    }

    fn apply_setting_changes(
        &mut self,
        result: QueryResult,
        changes: &[crate::settings::SettingChange],
    ) -> SqlResult<QueryResult> {
        for crate::settings::SettingChange { name, value, scope } in changes {
            if is_dialect_setting(name) {
                self.choose_dialect(value.as_deref())?;
                continue;
            }
            self.change_setting(name, value.as_deref(), *scope)?;
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
        let mut writer = self.store.begin_boxed_write().map_err(|error| {
            SqlError::new(
                error.state(),
                format!(
                    "the sequence values a rolled-back transaction used could not be saved again: {}",
                    error.message()
                ),
            )
        })?;
        let reapplied = sequences::reapply(
            &mut crate::temporary::TemporaryWrite {
                inner: &mut writer,
                objects: &mut self.temporary,
            },
            &pending,
        );
        match reapplied {
            Ok(()) => match writer.commit() {
                Ok(()) => {
                    self.temporary.commit();
                    Ok(())
                }
                Err(error) => {
                    self.temporary.rollback();
                    Err(error)
                }
            },
            Err(error) => {
                self.temporary.rollback();
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
        if let Err(error) = self.apply_routine_assignments() {
            result.notices.push(error.message().to_owned());
        }
        let database = self.settings.database.clone();
        for notice in std::mem::take(&mut result.notices) {
            if let Some(note) = mysql_errors::note_for(&notice, &database) {
                result.warnings.push(note);
            }
        }
        result.warnings.extend(mysql_errors::take_evaluation_warnings());
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

    fn failing_block_on_error<T>(&mut self, result: SqlResult<T>) -> SqlResult<T> {
        if result.is_err() {
            self.fail_block_in_postgresql();
        }
        result
    }

    /// Ends the session: an open block is rolled back. Whether one was open.
    fn run_quietly(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let mut results = Vec::new();
        self.execute(text, &mut |result| results.push(result))?;
        Ok(results)
    }

    fn atomically(&mut self, body: impl FnOnce(&mut Self) -> SqlResult<()>) -> SqlResult<()> {
        if !self.autocommit() || !matches!(self.block, Block::None) {
            return body(self);
        }
        self.run_quietly("BEGIN")?;
        match body(self) {
            Ok(()) => self.run_quietly("COMMIT").map(drop),
            Err(error) => {
                let _ = self.run_quietly("ROLLBACK");
                Err(error)
            }
        }
    }

    fn materialized_views_registered(&mut self) -> SqlResult<bool> {
        let internal = crate::schemas::qualified("eggdb_internal", "materialized_views");
        self.read(|reader, _| {
            Ok(reader
                .get(&crate::encoding::table_key(&internal))?
                .is_some())
        })
    }

    fn materialized_view_query(&mut self, name: &str) -> SqlResult<Option<String>> {
        if !self.materialized_views_registered()? {
            return Ok(None);
        }
        let rows = self
            .run_quietly(&format!(
                "SELECT query FROM eggdb_internal.materialized_views WHERE name = {}",
                materialized_view_key(name)
            ))?
            .into_iter()
            .flat_map(|result| result.rows)
            .collect::<Vec<_>>();
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| match row.into_iter().next() {
                Some(Value::Text(query)) => Some(query),
                _ => None,
            }))
    }

    fn materialized_view(
        &mut self,
        command: &crate::ast::MaterializedViewCommand,
    ) -> SqlResult<QueryResult> {
        use crate::ast::MaterializedViewCommand;
        let written = |name: &str| {
            name.split(crate::schemas::QUALIFIER)
                .map(crate::schema::quoted)
                .collect::<Vec<_>>()
                .join(".")
        };
        match command {
            MaterializedViewCommand::Create {
                name,
                query,
                with_data,
                if_not_exists,
            } => {
                if self.materialized_view_query(&name.text)?.is_some() {
                    if *if_not_exists {
                        return Ok(
                            QueryResult::command("CREATE MATERIALIZED VIEW").with_notice(format!(
                                "NOTICE:  relation \"{}\" already exists, skipping",
                                name.text
                            )),
                        );
                    }
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_TABLE,
                        format!("relation \"{}\" already exists", name.text),
                    ));
                }
                let described = self
                    .run_quietly(&format!(
                        "SELECT * FROM ({query}) AS materialized_definition LIMIT 0"
                    ))?
                    .into_iter()
                    .find_map(|result| result.columns)
                    .unwrap_or_default();
                let columns: Vec<String> = described
                    .iter()
                    .map(|column| {
                        format!(
                            "{} {}",
                            crate::schema::quoted(&column.name),
                            column
                                .sql_type
                                .map_or_else(|| "text".to_owned(), |sql_type| sql_type.name())
                        )
                    })
                    .collect();
                let table = written(&name.text);
                let name_text = name.text.clone();
                let query = query.clone();
                let with_data = *with_data;
                self.atomically(|session| {
                    session.run_quietly(
                        "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.materialized_views (name text PRIMARY KEY, query text NOT NULL)",
                    )?;
                    session.run_quietly(&format!("CREATE TABLE {table} ({})", columns.join(", ")))?;
                    session.run_quietly(&format!(
                        "INSERT INTO eggdb_internal.materialized_views VALUES ({}, {})",
                        materialized_view_key(&name_text),
                        crate::dump::sql_string(&query)
                    ))?;
                    if with_data {
                        session.run_quietly(&format!("INSERT INTO {table} {query}"))?;
                    }
                    Ok(())
                })?;
                Ok(QueryResult::command("CREATE MATERIALIZED VIEW"))
            }
            MaterializedViewCommand::Refresh { name, with_data } => {
                let Some(query) = self.materialized_view_query(&name.text)? else {
                    return Err(SqlError::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a materialized view", name.text),
                    ));
                };
                let table = written(&name.text);
                let with_data = *with_data;
                self.atomically(|session| {
                    session.run_quietly(&format!("DELETE FROM {table}"))?;
                    if with_data {
                        session.run_quietly(&format!("INSERT INTO {table} {query}"))?;
                    }
                    Ok(())
                })?;
                Ok(QueryResult::command("REFRESH MATERIALIZED VIEW"))
            }
            MaterializedViewCommand::Drop { names, if_exists } => {
                let mut dropped = Vec::new();
                for name in names {
                    match self.materialized_view_query(&name.text)? {
                        Some(_) => dropped.push(name.text.clone()),
                        None if *if_exists => {}
                        None => {
                            return Err(SqlError::new(
                                SqlState::UNDEFINED_TABLE,
                                format!("materialized view \"{}\" does not exist", name.text),
                            ));
                        }
                    }
                }
                self.atomically(|session| {
                    for name in &dropped {
                        session.run_quietly(&format!("DROP TABLE {}", written(name)))?;
                        session.run_quietly(&format!(
                            "DELETE FROM eggdb_internal.materialized_views WHERE name = {}",
                            materialized_view_key(name)
                        ))?;
                    }
                    Ok(())
                })?;
                Ok(QueryResult::command("DROP MATERIALIZED VIEW"))
            }
        }
    }

    fn install_extension(&mut self, name: &str, if_not_exists: bool) -> SqlResult<QueryResult> {
        let (schema, script) = match name {
            "pg_cron" => ("cron", crate::jobs::PG_CRON_SCRIPT),
            "eggdb_tenancy" => ("eggdb_tenancy", crate::tenancy::TENANCY_SCRIPT),
            "eggdb_ledger" => ("eggdb_ledger", crate::ledger::LEDGER_SCRIPT),
            "eggdb_console" => ("eggdb_console", crate::console::CONSOLE_SCRIPT),
            "eggdb_bi" => ("eggdb_bi", crate::reports::BI_SCRIPT),
            "eggdb_catalog" => ("eggdb_catalog", crate::catalog::CATALOG_SCRIPT),
            "eggdb_privacy" => ("eggdb_privacy", crate::privacy::PRIVACY_SCRIPT),
            "eggdb_retention" => ("eggdb_retention", crate::retention::RETENTION_SCRIPT),
            "eggdb_coordination" => (
                "eggdb_coordination",
                crate::coordination::COORDINATION_SCRIPT,
            ),
            "eggdb_migration" => ("eggdb_migration", crate::migration::MIGRATION_SCRIPT),
            "timescaledb" => ("_timescaledb_catalog", crate::timescale::TIMESCALE_SCRIPT),
            "eggdb_ai" => ("eggdb_ai", crate::ai_memory::AI_SCRIPT),
            "eggdb_features" => ("eggdb_features", crate::features::FEATURES_SCRIPT),
            "eggdb_pipelines" => ("eggdb_pipelines", crate::pipelines::PIPELINES_SCRIPT),
            "eggdb_secrets" => ("eggdb_secrets", crate::secrets::SECRETS_SCRIPT),
            "eggdb_temporal" => ("eggdb_temporal", crate::temporal_tables::TEMPORAL_SCRIPT),
            "eggdb_finance" => ("eggdb_finance", crate::finance::FINANCE_SCRIPT),
            "eggdb_cleanroom" => ("eggdb_cleanroom", crate::cleanroom::CLEANROOM_SCRIPT),
            "eggdb_metering" => ("eggdb_metering", crate::metering::METERING_SCRIPT),
            "eggdb_compliance" => ("eggdb_compliance", crate::compliance::COMPLIANCE_SCRIPT),
            "eggdb_marketplace" => ("eggdb_marketplace", crate::marketplace::MARKETPLACE_SCRIPT),
            "eggdb_sheets" => ("eggdb_sheets", crate::sheets::SHEETS_SCRIPT),
            "eggdb_streams" => ("eggdb_streams", crate::streams::STREAMS_SCRIPT),
            "eggdb_etl" => ("eggdb_etl", crate::etl::ETL_SCRIPT),
            "eggdb_app_auth" => ("eggdb_app_auth", crate::app_auth::APP_AUTH_SCRIPT),
            "eggdb_api_keys" => ("eggdb_api_keys", crate::api_keys::API_KEYS_SCRIPT),
            "eggdb_shredding" => ("eggdb_shredding", crate::shredding::SHREDDING_SCRIPT),
            "eggdb_iot" => ("eggdb_iot", crate::iot::IOT_SCRIPT),
            "eggdb_advisor" => ("eggdb_advisor", crate::advisor::ADVISOR_SCRIPT),
            "pgrouting" => ("pgrouting", crate::pgrouting::PGROUTING_SCRIPT),
            _ => ("eggdb_cdc", crate::cdc::CDC_SCRIPT),
        };
        let mut installed = false;
        self.execute(
            &format!("SELECT 1 FROM information_schema.schemata WHERE schema_name = '{schema}'"),
            &mut |result| installed = !result.rows.is_empty(),
        )?;
        if installed {
            if if_not_exists {
                return Ok(
                    QueryResult::command("CREATE EXTENSION").with_notice(format!(
                        "NOTICE:  extension \"{name}\" already exists, skipping"
                    )),
                );
            }
            return Err(SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("extension \"{name}\" already exists"),
            ));
        }
        self.execute(script, &mut |_| {})?;
        Ok(QueryResult::command("CREATE EXTENSION"))
    }

    /// Ends the session, rolling back a transaction it left open; returns whether one was open.
    pub fn close(mut self) -> SqlResult<bool> {
        let was_open = self.in_block();
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                self.temporary.rollback();
                writer.rollback()?;
            }
            Block::Reading(_) | Block::Failed | Block::None => {}
        }
        self.settle_sequences()?;
        Ok(was_open)
    }

    fn fail_block(&mut self) {
        if self.savepoints.is_active()
            && matches!(self.block, Block::Writing(_) | Block::Reading(_))
        {
            self.savepoints.aborted = true;
            return;
        }
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                let _ = writer.rollback();
                self.temporary.rollback();
                self.block = Block::Failed;
            }
            Block::Reading(_) => {
                self.temporary.rollback();
                self.block = Block::Failed;
            }
            Block::Failed => self.block = Block::Failed,
            Block::None => {}
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
        let refuses_writes = self.transaction_is_read_only();
        match copy.direction {
            CopyDirection::FromClient => self.write("COPY FROM", |writer, tables| {
                if let CopySource::Table { name, columns } = &mut copy.source {
                    if let Some(relation) =
                        crate::pg_catalog::catalog_reference(writer, &name.text)?
                    {
                        return Err(crate::pg_catalog::read_only(relation.name()).at(name.position));
                    }
                    binding.names(writer, tables, std::slice::from_mut(name))?;
                    read_only::refuse_permanent_name(refuses_writes, "COPY FROM", name)?;
                    check_copied_columns(
                        writer,
                        binding.settings,
                        &name.text,
                        columns,
                        crate::roles::Privilege::Insert,
                    )?;
                    if crate::row_security::visible_rows(
                        writer,
                        binding.settings,
                        &name.text,
                        crate::row_security::PolicyCommand::Insert,
                    )?
                    .is_some()
                    {
                        return Err(SqlError::not_supported(
                            "COPY FROM not supported with row-level security",
                        )
                        .with_hint("Use INSERT statements instead."));
                    }
                    if crate::triggers::fire_on(
                        writer,
                        tables,
                        &name.text,
                        crate::triggers::TriggerEvent::Insert,
                    )? {
                        return Err(SqlError::not_supported(
                            "COPY FROM into a table with INSERT triggers",
                        )
                        .with_hint("Use INSERT statements, which fire the triggers, or disable them for the load."));
                    }
                }
                if let Some(condition) = &mut copy.condition {
                    binding.condition(writer, tables, condition)?;
                }
                crate::copy::copy_from_client(writer, tables, copy, io)
            }),
            CopyDirection::ToClient => {
                let channel = &mut *io.channel;
                if matches!(copy.source, CopySource::Table { .. }) {
                    return self.read(|reader, tables| {
                        if let CopySource::Table { name, columns } = &mut copy.source {
                            binding.names(reader, tables, std::slice::from_mut(name))?;
                            check_copied_columns(
                                reader,
                                binding.settings,
                                &name.text,
                                columns,
                                crate::roles::Privilege::Select,
                            )?;
                            if crate::row_security::visible_rows(
                                reader,
                                binding.settings,
                                &name.text,
                                crate::row_security::PolicyCommand::Select,
                            )?
                            .is_some()
                            {
                                return Err(SqlError::not_supported(
                                    "COPY of a table with row-level security to this role",
                                )
                                .with_hint("Use COPY (SELECT * FROM table) TO STDOUT, which applies the policies."));
                            }
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

    fn explain_analyze(
        &mut self,
        explain: &Explain,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: true,
            target_privilege: Some(crate::roles::target_privilege(&explain.statement)),
        };
        let store = self.store;
        let lookups = || {
            explain
                .buffers
                .then(|| store.erased_page_lookups())
                .flatten()
        };
        let planning = std::time::Instant::now();
        let before_planning = lookups();
        let mut planned = (*explain.statement).clone();
        let (style, dialect) = (explain.style, self.dialect);
        let plan = self.read(|reader, tables| {
            binding.statement(reader, tables, &mut planned)?;
            crate::explain::plan(reader, tables, &planned, style, dialect)
        })?;
        let planning = planning.elapsed();
        let before_execution = lookups();
        let started = std::time::Instant::now();
        let mut statement = (*explain.statement).clone();
        let (result, selects, subplans) = if crate::explain::PlannedStatement::of(&statement)
            == Some(crate::explain::PlannedStatement::Select)
        {
            crate::step_times::record_with_subplans(|| self.run(&mut statement, io))
        } else {
            (self.run(&mut statement, io), Vec::new(), Vec::new())
        };
        let result = result?;
        let execution = started.elapsed();
        let after_execution = lookups();
        let rows = crate::statement_statistics::rows_of(&result);
        let buffers = match (before_planning, before_execution, after_execution) {
            (Some(planning_start), Some(execution_start), Some(execution_end)) => {
                Some(crate::explain::Buffers {
                    planning: execution_start.since(planning_start),
                    execution: execution_end.since(execution_start),
                })
            }
            _ => None,
        };
        let analyzed = crate::explain::Analyzed {
            rows,
            planning,
            execution,
            buffers,
            selects,
            subplans,
        };
        Ok(if explain.style == crate::ast::ExplainStyle::Tree {
            crate::explain::render_analyzed_tree(&plan, analyzed)
        } else {
            crate::explain::render_analyzed(&plan.query, analyzed)
        })
    }

    /// The `log_min_duration_statement` threshold: statements that take at least this long are
    /// worth logging, or `None` when it is off (`-1`, the default).
    pub fn slow_statement_threshold(&self) -> Option<std::time::Duration> {
        let millis =
            milliseconds_setting(self.settings.known_value("log_min_duration_statement")?)?;
        Some(std::time::Duration::from_millis(
            u64::try_from(millis).ok()?,
        ))
    }

    fn statement_timeout(&self) -> Option<std::time::Duration> {
        let millis = milliseconds_setting(self.settings.known_value("statement_timeout")?)?;
        (millis > 0).then(|| std::time::Duration::from_millis(millis.unsigned_abs()))
    }

    fn run(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        if matches!(self.block, Block::None) {
            self.transaction_mode.forget_transaction();
        }
        let result = self.run_with_event_triggers(statement, io);
        self.sync_transaction_read_only();
        result
    }

    fn run_statement(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        self.refuse_after_database_switch()?;
        self.failed_statement_was_undone = false;
        self.keeps_failed_statement_changes = match statement {
            Statement::Insert(insert) => insert.keeps_changes_on_conflict,
            Statement::Update(update) => {
                update.conflict_resolution == crate::ast::ConflictResolution::Fail
            }
            _ => false,
        };
        match statement {
            Statement::Set { name, value, .. } if is_dialect_setting(name) => {
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
        if self.sqlserver()
            && let Some(planned) = self.plan_instead_under_showplan(statement)?
        {
            return Ok(planned);
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
                Statement::SqlServer(SqlServerStatement::OutputInto(_)) => {}
                Statement::SqlServer(SqlServerStatement::InsertExecute(insert)) => {
                    let mut statement = self.insert_execute_statement(insert)?;
                    return self.run_statement(&mut statement, io);
                }
                Statement::SqlServer(SqlServerStatement::TopRows(top)) => {
                    let percent = matches!(
                        &top.count.kind,
                        crate::ast::ExprKind::OracleFunction { name, .. }
                            if *name == "sqlserver_top_percent"
                    );
                    let limit = if percent {
                        crate::execute::row_limit(Some(&top.count))?
                            .percent
                            .map(crate::dml_row_limit::DmlRowLimit::Percent)
                    } else {
                        crate::execute::row_count(Some(&top.count), "TOP")?
                            .map(crate::dml_row_limit::DmlRowLimit::Rows)
                    };
                    let _limited = crate::dml_row_limit::limit(limit);
                    return self.run_statement(&mut top.statement, io);
                }
                Statement::SqlServer(statement) => return self.run_sqlserver(statement),
                _ => {}
            }
        }
        match statement {
            Statement::InstallExtension {
                name,
                if_not_exists,
            } => return self.install_extension(name, *if_not_exists),
            Statement::MaterializedView(command) => return self.materialized_view(command),
            Statement::EventTrigger(command) => return self.event_trigger(command),
            Statement::Begin {
                read_only,
                isolation,
            } => {
                let opens = matches!(self.block, Block::None) || self.block_is_implicit;
                let read_only = read_only.unwrap_or_else(|| self.new_transactions_read_only());
                let began = self.begin(read_only)?;
                if opens {
                    self.transaction_mode.isolation = isolation.clone();
                }
                return Ok(began);
            }
            Statement::Commit => return self.commit(),
            Statement::Rollback => return self.rollback(),
            Statement::Savepoint(command @ crate::ast::SavepointCommand::RollbackTo(_)) => {
                return self.savepoint_command(command);
            }
            _ => {}
        }
        if matches!(self.block, Block::Failed) || self.savepoints.aborted {
            return Err(aborted_block());
        }
        if let Statement::Savepoint(command) = statement {
            return self.savepoint_command(command);
        }
        if let Statement::Prepared(command) = statement {
            return self.run_prepared_command(command, io);
        }
        match statement {
            Statement::SetTransaction(characteristics) => {
                let result = self.set_transaction_statement(characteristics);
                return self.failing_block_on_error(result);
            }
            Statement::Set { name, value, .. }
                if self.dialect == Dialect::PostgreSql
                    && (name.eq_ignore_ascii_case("transaction_read_only")
                        || name.eq_ignore_ascii_case("transaction_isolation")) =>
            {
                let result = self.set_transaction_parameter(name, value.as_deref());
                return self.failing_block_on_error(result);
            }
            Statement::Set { .. }
            | Statement::Reset { .. }
            | Statement::Show { .. }
            | Statement::Ignored { .. } => {}
            _ => self.transaction_mode.has_read = true,
        }
        if self.commits_implicitly(statement)
            && (matches!(self.block, Block::Writing(_))
                || (self.oracle() && matches!(self.block, Block::Reading(_))))
        {
            self.commit()?;
        }
        if let Statement::Database(command) = statement {
            return self.run_database_command(command);
        }
        self.refresh_database_names();
        let settings = Arc::clone(&self.settings);
        crate::roles::check_statement(&settings, statement)?;
        if let Some(most_rows) = self.max_result_rows
            && !self.statement_uses_sequences
        {
            crate::result_row_cap::cap_result_rows(statement, most_rows);
        }
        let binding = Binding {
            settings: &settings,
            walk_expressions: self.statement_walks_expressions,
            target_privilege: Some(crate::roles::target_privilege(statement)),
        };
        let binding = &binding;
        let dialect = self.dialect;
        let refuses_writes = self.transaction_is_read_only();
        if refuses_writes
            && let Some(command) = self.statement_row_locking
            && matches!(dialect, Dialect::PostgreSql | Dialect::Oracle)
            && self.locks_permanent_rows(binding, statement)
        {
            let refused = Err(read_only::refused_in_read_only_transaction(command));
            return self.failing_block_on_error(refused);
        }
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
        let profiled = if self.sqlserver() && self.settings.sqlserver.statistics_profile {
            self.statement_plan(statement, binding).ok().flatten()
        } else {
            None
        };
        if self.oracle() {
            self.show_plan_displays(statement)?;
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
            Statement::MySql(MySqlStatement::LoadData(load)) => self.load_data(load, io, binding),
            Statement::MySql(statement) => self.run_mysql(statement),
            Statement::Pragma(pragma) => self.run_pragma(pragma),
            Statement::VacuumInto(vacuum) => self.vacuum_into(vacuum),
            Statement::Select(select)
                if self.sqlite()
                    && select
                        .from
                        .as_ref()
                        .is_some_and(|from| crate::parser::is_schema_table(&from.first)) =>
            {
                self.select_schema(select, false)
            }
            Statement::Listen(channel) => {
                self.notifications.listen(&channel.text);
                Ok(QueryResult::command("LISTEN"))
            }
            Statement::Unlisten(channel) => {
                self.notifications
                    .unlisten(channel.as_ref().map(|channel| channel.text.as_str()));
                Ok(QueryResult::command("UNLISTEN"))
            }
            Statement::Notify { channel, payload } => {
                let notification = crate::notifications::checked(
                    &channel.text,
                    payload.as_deref().unwrap_or_default(),
                )?;
                let in_block = !matches!(self.block, Block::None);
                self.notifications.queue(notification, in_block)?;
                Ok(QueryResult::command("NOTIFY"))
            }
            Statement::RowSecurity(statement) => {
                let mut statement = statement.clone();
                let tag = statement.tag();
                self.write(tag, |writer, tables| {
                    let mut names = [statement.table_mut().clone()];
                    binding.names(writer, tables, &mut names)?;
                    let [resolved] = names;
                    *statement.table_mut() = resolved;
                    crate::row_security::run(writer, binding.settings, &statement)
                })
            }
            Statement::Database(command) => {
                Err(SqlError::not_supported(format!("{} here", command.tag())))
            }
            Statement::Role(crate::roles::RoleStatement::SetRole { role, reset }) => {
                let role = role.clone();
                let tag = if *reset { "RESET" } else { "SET" };
                self.set_role(role.as_deref()).map(|()| QueryResult::command(tag))
            }
            Statement::Role(role) => {
                let mut role = role.clone();
                let tag = role.tag();
                self.write(tag, |writer, tables| {
                    binding.names(writer, tables, role.tables_mut())?;
                    crate::roles::run(writer, binding.settings, &role)
                })
            }
            Statement::Analyze(names) => {
                let mut names = names.clone();
                self.write("ANALYZE", |writer, tables| {
                    binding.names(writer, tables, &mut names)?;
                    crate::statistics::analyze(writer, tables, &names)
                })
            }
            Statement::Truncate(table) => {
                self.write_firing_triggers("TRUNCATE TABLE", |writer, tables| {
                    binding.target(writer, tables, table)?;
                    execute::truncate(writer, tables, table)
                })
            }
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
            Statement::Select(select)
                if dialect == Dialect::PostgreSql
                    && crate::routines::select_may_write(&*select)
                    && self
                        .read(|reader, tables| {
                            crate::routines::select_calls_volatile(reader, tables, &*select)
                        })
                        .unwrap_or(false) =>
            {
                self.write("SELECT", |writer, tables| {
                    binding.select(&*writer, tables, select)?;
                    crate::routines::prepare_select(
                        writer,
                        tables,
                        &settings,
                        select,
                        crate::routines::Prefetch::Everything,
                    )?;
                    execute::select(&*writer, tables, select, false)
                })
            }
            Statement::Select(select) => self.read(|reader, tables| {
                binding.select(reader, tables, select)?;
                if matches!(dialect, Dialect::PostgreSql | Dialect::MySql)
                    || (dialect == Dialect::SqlServer
                        && crate::routines::select_calls_marked_routine(select))
                {
                    crate::routines::prepare_select(
                        &mut crate::plpgsql::ReadOnlyWrite { inner: reader },
                        tables,
                        &settings,
                        select,
                        crate::routines::Prefetch::NonVolatile,
                    )?;
                } else if dialect == Dialect::Oracle {
                    crate::routines::prepare_oracle_table_function(
                        &mut crate::plpgsql::ReadOnlyWrite { inner: reader },
                        tables,
                        &settings,
                        select,
                    )?;
                }
                execute::select(reader, tables, select, false)
            }),
            Statement::Query(query) => self.read(|reader, tables| {
                binding.query(reader, tables, query)?;
                if matches!(dialect, Dialect::PostgreSql | Dialect::MySql)
                    || (dialect == Dialect::SqlServer
                        && crate::routines::query_calls_marked_routine(query))
                {
                    crate::routines::prepare_query(
                        &mut crate::plpgsql::ReadOnlyWrite { inner: reader },
                        tables,
                        &settings,
                        query,
                        crate::routines::Prefetch::NonVolatile,
                    )?;
                }
                execute::query(reader, tables, query, false)
            }),
            Statement::CreateSequence(create) => self.write("CREATE SEQUENCE", |writer, tables| {
                create.name.text = crate::schemas::creation_name(writer, &create.name)?;
                if create.or_replace && sequences::read(writer, &create.name.text)?.is_some() {
                    sequences::drop_sequences(
                        writer,
                        tables,
                        std::slice::from_ref(&create.name),
                        true,
                        false,
                        dialect,
                    )?;
                }
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
            Statement::AlterTable(alter) => {
                let altered = self.write("ALTER TABLE", |writer, tables| {
                    if let Some(mut renamed_view) = view_renamed_by(writer, tables, alter, dialect)? {
                        binding.names(writer, tables, std::slice::from_mut(&mut renamed_view.name))?;
                        return crate::views::alter_view(writer, tables, &renamed_view, dialect)
                            .map(|_| QueryResult::command("ALTER TABLE"));
                    }
                    binding.alter_table(writer, tables, alter)?;
                    crate::alter::alter_table(writer, tables, alter, dialect)
                });
                match altered {
                    Ok(answer)
                        if alter
                            .actions
                            .iter()
                            .any(|action| matches!(action, crate::ast::AlterAction::SystemVersioning(_))) =>
                    {
                        let table = alter.name.text.clone();
                        self.apply_system_versioning_actions(&table, &alter.actions)
                            .map(|()| answer)
                    }
                    other => other,
                }
            }
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
                crate::sqlite_schema_text::create_view(writer, tables, create, &context)
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
            Statement::Set { name, value, scope } => {
                self.set_statement(name, value.as_deref(), *scope)
            }
            Statement::Reset { name: Some(name) } => self
                .reset_setting(name)
                .map(|()| QueryResult::command("RESET")),
            Statement::Reset { name: None } => {
                self.reset_all_settings();
                Ok(QueryResult::command("RESET"))
            }
            Statement::Discard(target) => self.discard(target.clone()),
            Statement::Cursor(command) => self.cursor_command(command, io),
            Statement::Show { name } => self.show(name.as_deref()),
            Statement::CreateTable(create) => {
                let created = self.write("CREATE TABLE", |writer, tables| {
                    binding.create_table(writer, tables, create)?;
                    crate::sqlite_schema_text::create_table(writer, tables, create, dialect)
                });
                let table = create.name.text.clone();
                let created = match (created, create.system_versioning.clone()) {
                    (Ok(answer), Some(versioning)) => self
                        .enable_system_versioning(&table, &versioning)
                        .map(|()| answer),
                    (outcome, _) => outcome,
                };
                match (created, create.rowversion_column.clone()) {
                    (Ok(answer), Some(column)) => {
                        self.enable_rowversion(&table, &column).map(|()| answer)
                    }
                    (outcome, _) => outcome,
                }
            }
            Statement::CreateIndex(create) => self.write("CREATE INDEX", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut create.table))?;
                crate::sqlite_schema_text::create_index(writer, tables, create, dialect)
            }),
            Statement::DropIndex(drop) => self.write("DROP INDEX", |writer, tables| {
                if let Some(table) = &mut drop.table {
                    binding.names(writer, tables, std::slice::from_mut(table))?;
                }
                crate::indexes::drop_index(writer, tables, drop, dialect)
            }),
            Statement::Explain(explain) if explain.plan_table.is_some() => {
                let explain = explain.clone();
                self.explain_into_plan_table(&explain)
            }
            Statement::Explain(explain) if explain.analyze => {
                let explain = explain.clone();
                self.explain_analyze(&explain, io)
            }
            Statement::Explain(explain) => {
                let style = explain.style;
                self.read(|reader, tables| {
                    binding.statement(reader, tables, &mut explain.statement)?;
                    crate::explain::plan(reader, tables, &explain.statement, style, dialect)
                })
                .map(|planned| crate::explain::render(&planned, style, dialect))
            }
            Statement::DropTable {
                names,
                if_exists,
                cascade,
            } => self.write("DROP TABLE", |writer, tables| {
                binding.names(writer, tables, names)?;
                execute::drop_tables(writer, tables, names, *if_exists, *cascade, dialect)
            }),
            Statement::Insert(insert) => self.write_firing_triggers("INSERT", |writer, tables| {
                binding.insert(writer, tables, insert)?;
                read_only::refuse_permanent_target(refuses_writes, "INSERT", &insert.table)?;
                if matches!(dialect, Dialect::PostgreSql | Dialect::MySql)
                    || (dialect == Dialect::SqlServer
                        && crate::routines::insert_calls_marked_routine(insert))
                {
                    crate::routines::prepare_insert(writer, tables, &settings, insert)?;
                }
                execute::insert(writer, tables, insert)
            }),
            Statement::Update(update) => self.write_firing_triggers("UPDATE", |writer, tables| {
                binding.update(writer, tables, update)?;
                read_only::refuse_permanent_target(refuses_writes, "UPDATE", &update.table)?;
                if matches!(dialect, Dialect::PostgreSql | Dialect::MySql)
                    || (dialect == Dialect::SqlServer
                        && crate::routines::exprs_call_marked_routine(
                            crate::execute::update_exprs(update),
                        ))
                {
                    crate::routines::prepare_update(writer, tables, &settings, update)?;
                }
                execute::update(writer, tables, update)
            }),
            Statement::Delete(delete) => self.write_firing_triggers("DELETE", |writer, tables| {
                binding.delete(writer, tables, delete)?;
                read_only::refuse_permanent_target(refuses_writes, "DELETE", &delete.table)?;
                if matches!(dialect, Dialect::PostgreSql | Dialect::MySql)
                    || (dialect == Dialect::SqlServer
                        && crate::routines::exprs_call_marked_routine(
                            crate::execute::delete_exprs(delete),
                        ))
                {
                    crate::routines::prepare_delete(writer, tables, &settings, delete)?;
                }
                execute::delete(writer, tables, delete)
            }),
            Statement::With(with) => self.write_firing_triggers("WITH", |writer, tables| {
                crate::with_statement::run_checking_targets(
                    writer,
                    tables,
                    with,
                    binding,
                    &|statement| {
                        read_only::refuse_permanent_statement_target(refuses_writes, statement)
                    },
                )
            }),
            Statement::Merge(merge) => self.write_firing_triggers("MERGE", |writer, tables| {
                binding.merge(writer, tables, merge)?;
                read_only::refuse_permanent_target(refuses_writes, "MERGE", &merge.target)?;
                crate::merge::merge(writer, tables, merge)
            }),
            Statement::CreateTrigger(create) => self.write("CREATE TRIGGER", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut create.table))?;
                crate::triggers::create_trigger(writer, tables, create)
            }),
            Statement::DropTrigger(drop) => self.write("DROP TRIGGER", |writer, tables| {
                if let Some(table) = &mut drop.table {
                    binding.names(writer, tables, std::slice::from_mut(table))?;
                }
                crate::triggers::drop_triggers(writer, tables, drop, dialect)
            }),
            Statement::EnableTrigger(enable) => {
                let tag = if enable.enable {
                    "ENABLE TRIGGER"
                } else {
                    "DISABLE TRIGGER"
                };
                self.write(tag, |writer, tables| {
                    binding.names(writer, tables, std::slice::from_mut(&mut enable.table))?;
                    crate::triggers::enable_triggers(writer, tables, enable, dialect)
                })
            }
            Statement::AlterTrigger(alter) => self.write("ALTER TRIGGER", |writer, tables| {
                crate::triggers::alter_trigger(writer, tables, alter)
            }),
            Statement::CreateRoutine(create) => {
                let tag = match create.definition.kind {
                    crate::routines::RoutineKind::Function => "CREATE FUNCTION",
                    crate::routines::RoutineKind::Procedure => "CREATE PROCEDURE",
                };
                let body_check = crate::routines::BodyCheck::of_setting(
                    self.settings.get("check_function_bodies").ok().as_deref(),
                );
                self.write(tag, |writer, tables| {
                    crate::routines::create_routine(writer, tables, create, body_check)
                })
            }
            Statement::DropRoutine(drop) => self.write("DROP FUNCTION", |writer, tables| {
                crate::routines::drop_routines(writer, tables, drop, dialect)
            }),
            Statement::AlterRoutine(alter) => self.write("ALTER FUNCTION", |writer, tables| {
                crate::routines::alter_routine(writer, tables, alter, dialect)
            }),
            Statement::Call(call) if dialect == Dialect::MySql => {
                let mut assigned = Vec::new();
                crate::mysql_variables::take_routine_assignments();
                let result = self.write("CALL", |writer, tables| {
                    for argument in &mut call.arguments {
                        binding.condition(&*writer, tables, argument)?;
                    }
                    let (result, sources) =
                        crate::routines::call_procedure_outputs(writer, tables, &settings, call)?;
                    let row = result.rows.first().cloned().unwrap_or_default();
                    for (value, source) in row.into_iter().zip(sources) {
                        if let Some(name) =
                            source.and_then(|at| call.user_variables.get(at).cloned().flatten())
                        {
                            assigned.push((name, value));
                        }
                    }
                    Ok(QueryResult::command("CALL"))
                });
                let applied = self.apply_routine_assignments();
                if result.is_ok() {
                    for (name, value) in assigned {
                        Arc::make_mut(&mut self.settings)
                            .mysql
                            .set_user(&name, value);
                    }
                }
                result.and_then(|result| applied.map(|()| result))
            }
            Statement::Call(call) if self.runs_procedures_nonatomic() => {
                self.write_nonatomic("CALL", |control, tables| {
                    for argument in &mut call.arguments {
                        binding.condition(&*control, tables, argument)?;
                    }
                    crate::routines::call_procedure_nonatomic(control, tables, &settings, call)
                        .map(|(result, _)| result)
                })
            }
            Statement::Do(block) if self.runs_procedures_nonatomic() => {
                self.write_nonatomic("DO", |control, tables| {
                    crate::routines::run_do_nonatomic(control, tables, &settings, &block.code)
                })
            }
            Statement::Call(call) => self.write_plsql("CALL", |writer, tables| {
                for argument in &mut call.arguments {
                    binding.condition(&*writer, tables, argument)?;
                }
                crate::routines::call_procedure(writer, tables, &settings, call)
            }),
            Statement::Do(block) => self.write("DO", |writer, tables| {
                crate::routines::run_do(writer, tables, &settings, &block.code)
            }),
            Statement::Synonym(command) => {
                let tag = match command {
                    crate::ast::SynonymCommand::Create { .. } => "CREATE SYNONYM",
                    crate::ast::SynonymCommand::Drop { .. } => "DROP SYNONYM",
                };
                self.write(tag, |writer, _| crate::synonyms::run(writer, command))
            }
            Statement::PlsqlBlock(block) => self.write_plsql(PLSQL_BLOCK_TAG, |writer, tables| {
                crate::plpgsql::run_anonymous_block(writer, tables, &settings, block)?;
                Ok(QueryResult::command(PLSQL_BLOCK_TAG))
            }),
            Statement::CreatePackage(create) => {
                let tag = if create.package.body {
                    "CREATE PACKAGE BODY"
                } else {
                    "CREATE PACKAGE"
                };
                self.write(tag, |writer, tables| {
                    crate::plsql::create_package(writer, tables, create)
                })
            }
            Statement::DropPackage(drop) => self.write("DROP PACKAGE", |writer, _| {
                crate::plsql::drop_package(writer, drop)
            }),
            Statement::AlterPackage(alter) => self.write("ALTER PACKAGE", |writer, _| {
                crate::plsql::alter_package(&*writer, alter)
            }),
            Statement::AlterSession(settings) => self.alter_session(settings),
            Statement::CreateType(create) => self.write("CREATE TYPE", |writer, _| {
                crate::user_types::create_type(writer, create)
            }),
            Statement::CreateDomain(create) => self.write("CREATE DOMAIN", |writer, _| {
                crate::user_types::create_domain(writer, create)
            }),
            Statement::DropType {
                names,
                if_exists,
                cascade,
                domain,
            } => {
                let tag = if *domain { "DROP DOMAIN" } else { "DROP TYPE" };
                self.write(tag, |writer, tables| {
                    crate::user_types::drop_types(
                        writer, tables, names, *if_exists, *cascade, *domain,
                    )
                })
            }
            Statement::AlterType(alter) => self.write("ALTER TYPE", |writer, _| {
                crate::user_types::alter_type(writer, alter)
            }),
            Statement::AlterDomain(alter) => self.write("ALTER DOMAIN", |writer, _| {
                crate::user_types::alter_domain(writer, alter)
            }),
            Statement::SetConstraints { names, deferred } => {
                self.set_constraints(names.as_deref(), *deferred)
            }
            Statement::CreateTableAs(create) => {
                let execute = match &create.query {
                    Statement::Prepared(crate::ast::PreparedCommand::Execute(execute)) => {
                        Some(execute.clone())
                    }
                    _ => None,
                };
                if let Some(execute) = execute {
                    create.query = self.prepared_query(&execute)?;
                }
                let command = create.command();
                self.write(command, |writer, tables| {
                    crate::create_table_as::run(writer, tables, binding, dialect, create)
                })
            }
            Statement::CreateTableLike(like) => self.write("CREATE TABLE", |writer, tables| {
                crate::create_table_like::run(writer, tables, binding, dialect, like)
            }),
            Statement::MultitableInsert(insert) => self
                .write_firing_triggers("INSERT", |writer, tables| {
                    crate::multitable_insert::run(writer, tables, binding, insert)
                }),
            Statement::SqlServer(SqlServerStatement::OutputInto(output)) => {
                self.run_output_into(output, binding)
            }
            Statement::Begin { .. }
            | Statement::SetTransaction(_)
            | Statement::Commit
            | Statement::Rollback
            | Statement::Savepoint(_)
            | Statement::InstallExtension { .. }
            | Statement::MaterializedView(_)
            | Statement::EventTrigger(_)
            | Statement::Prepared(_)
            | Statement::SqlServer(_) => {
                return Err(SqlError::new(
                    SqlState::INTERNAL_ERROR,
                    "unreachable statement",
                ));
            }
        };
        if let (Some(plan), Ok(answer)) = (&profiled, &result) {
            let text = self.shown_statement.take().unwrap_or_default();
            let rows = crate::statement_statistics::rows_of(answer);
            self.extra_results.push(crate::explain::showplan_all(
                Some((&plan.0, plan.1)),
                &text,
                Some(rows),
            ));
        }
        if let Err(error) = &result {
            self.fail_block_in_postgresql();
            if rolls_back_transaction_on_conflict(statement, error)
                && let Block::Writing(writer) = std::mem::replace(&mut self.block, Block::None)
            {
                self.temporary.rollback();
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
        !self.creating_plan_table
            && (self.mysql() || self.oracle())
            && matches!(
                statement,
                Statement::CreateTable(_)
                    | Statement::CreateTableLike(_)
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
                    | Statement::Database(_)
                    | Statement::CreateView(_)
                    | Statement::DropView { .. }
                    | Statement::AlterView(_)
                    | Statement::Comment(_)
                    | Statement::CreateTrigger(_)
                    | Statement::DropTrigger(_)
                    | Statement::EnableTrigger(_)
                    | Statement::AlterTrigger(_)
                    | Statement::CreateRoutine(_)
                    | Statement::DropRoutine(_)
                    | Statement::AlterRoutine(_)
                    | Statement::CreatePackage(_)
                    | Statement::DropPackage(_)
                    | Statement::AlterPackage(_)
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
        if matches!(self.block, Block::Failed) || self.savepoints.aborted {
            return Err(aborted_block());
        }
        self.failed_statement_was_undone = false;
        self.keeps_failed_statement_changes = false;
        if matches!(self.block, Block::None) {
            self.transaction_mode.forget_transaction();
        }
        self.transaction_mode.has_read = true;
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: false,
            target_privilege: Some(crate::roles::Privilege::Insert),
        };
        let result = self.write_firing_triggers("INSERT", |writer, tables| {
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
        let _types = crate::user_types::enter_if_absent();
        let store_id = self.store.erased_store_id();
        let tables = &mut self.tables;
        let objects = &self.temporary;
        let (result, exported) = match &self.block {
            Block::Writing(writer) => (
                body(
                    &crate::temporary::TemporaryRead {
                        inner: writer,
                        objects,
                    },
                    tables,
                ),
                Vec::new(),
            ),
            Block::Reading(reader) => {
                let scope = crate::exported_snapshots::enter(store_id, reader.snapshot_handle());
                let result = body(
                    &crate::temporary::TemporaryRead {
                        inner: reader,
                        objects,
                    },
                    tables,
                );
                let exported = crate::exported_snapshots::take_exported();
                drop(scope);
                (result, exported)
            }
            Block::None | Block::Failed => {
                let reader = self.store.begin_boxed_read()?;
                (
                    body(
                        &crate::temporary::TemporaryRead {
                            inner: &reader,
                            objects,
                        },
                        tables,
                    ),
                    Vec::new(),
                )
            }
        };
        if !exported.is_empty() {
            self.keep_exported_snapshots(exported);
        }
        result
    }

    fn keep_exported_snapshots(&mut self, ids: Vec<String>) {
        if let Block::Reading(reader) = std::mem::replace(&mut self.block, Block::None) {
            self.block = Block::Reading(Box::new(crate::exported_snapshots::ExportingRead {
                inner: reader,
                ids,
            }));
        }
    }

    fn write_plsql(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn KeyValueWrite, &mut TableCache) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        if !self.oracle() {
            return self.write(command, body);
        }
        let _ = crate::plpgsql::take_transaction_end();
        let mut ended = None;
        let mut rolled_back = None;
        let outcome = self.write(command, |writer, tables| {
            let outcome = body(writer, tables);
            ended = crate::plpgsql::take_transaction_end();
            let result = outcome?;
            if ended == Some(TransactionEnd::Rollback) {
                rolled_back = Some(result);
                return Err(SqlError::new(
                    SqlState::TRANSACTION_ROLLBACK,
                    "the PL/SQL unit rolled back its transaction",
                ));
            }
            Ok(result)
        });
        if let Some(result) = rolled_back {
            if matches!(self.block, Block::Writing(_)) {
                self.rollback()?;
            }
            return Ok(result);
        }
        let result = outcome?;
        if ended == Some(TransactionEnd::Commit) && matches!(self.block, Block::Writing(_)) {
            self.commit()?;
        }
        Ok(result)
    }

    fn write(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn KeyValueWrite, &mut TableCache) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        self.refuse_writes_when_uncommittable()?;
        let refuses_writes = self.transaction_is_read_only();
        if matches!(self.block, Block::None) {
            self.transaction_mode.next_read_only = None;
        }
        if refuses_writes {
            if read_only::changes_definitions(command) {
                return Err(read_only::refused_in_read_only_transaction(command));
            }
            if matches!(self.block, Block::None) {
                return self.write_in_statement_snapshot(command, body);
            }
        }
        let _types = crate::user_types::enter_if_absent();
        let body = |writer: &mut dyn KeyValueWrite, tables: &mut TableCache| {
            let mut refusing;
            let writer: &mut dyn KeyValueWrite = if refuses_writes {
                refusing = read_only::ReadOnlyTransactionWrite {
                    inner: writer,
                    command,
                };
                &mut refusing
            } else {
                writer
            };
            sequences::load_for_sequence_calls(writer)?;
            let result =
                body(writer, tables).map_err(|error| sequences::explain_missing(writer, error))?;
            sequences::save_changed(writer).map_err(|error| {
                if error.state() == SqlState::READ_ONLY_SQL_TRANSACTION {
                    let refused = read_only::refused_in_read_only_transaction(
                        sequences::first_changing_function(),
                    );
                    sequences::forget_changes();
                    refused
                } else {
                    error
                }
            })?;
            Ok(result)
        };
        let keep_open = !self.autocommit();
        let keeps_ddl_open = self.sqlserver();
        let implicit = self.implicit_transaction;
        let rolls_back_statements = self.dialect != Dialect::PostgreSql;
        let keeps_procedure_statements =
            self.dialect == Dialect::MySql && command.eq_ignore_ascii_case("CALL");
        crate::undo_marks::begin_statement();
        let undo_start = |statement_start: usize, length: usize| {
            if keeps_procedure_statements {
                crate::undo_marks::take_procedure_statement_start()
                    .filter(|&kept| kept >= statement_start && kept <= length)
                    .unwrap_or(statement_start)
            } else {
                statement_start
            }
        };
        let keeps_failed_changes = std::mem::take(&mut self.keeps_failed_statement_changes);
        self.failed_statement_was_undone = false;
        let undoes = |result: &SqlResult<QueryResult>| {
            rolls_back_statements
                && result.as_ref().is_err_and(|error| {
                    !(keeps_failed_changes && conflict_resolution_applies(error))
                })
        };
        let tables = &mut self.tables;
        let objects = &mut self.temporary;
        let undo = self
            .savepoints
            .is_active()
            .then_some(&mut self.savepoints.undo);
        match &mut self.block {
            Block::Writing(writer) if undo.is_some() => {
                let mut routed = crate::temporary::TemporaryWrite {
                    inner: writer,
                    objects,
                };
                let Some(log) = undo else {
                    return Err(SqlError::new(SqlState::INTERNAL_ERROR, "no undo log"));
                };
                let statement_start = log.len();
                let result = body(
                    &mut savepoints::UndoWrite::new(
                        &mut routed,
                        &mut *log,
                        savepoints::UndoKeptFor::RollbackToSavepoint,
                    ),
                    tables,
                );
                let undo_to = undo_start(statement_start, log.len());
                if undoes(&result) && log.len() > undo_to {
                    let undone = savepoints::undo_back_to(&mut routed, log, undo_to);
                    return self.end_undone_statement(result, undone);
                }
                result
            }
            Block::Writing(writer) if rolls_back_statements => {
                let mut routed = crate::temporary::TemporaryWrite {
                    inner: writer,
                    objects,
                };
                let mut log = savepoints::Undo::new();
                let result = body(
                    &mut savepoints::UndoWrite::new(
                        &mut routed,
                        &mut log,
                        savepoints::UndoKeptFor::FailedStatement,
                    ),
                    tables,
                );
                let undo_to = undo_start(0, log.len());
                if undoes(&result) && log.len() > undo_to {
                    let undone = savepoints::undo_back_to(&mut routed, &mut log, undo_to);
                    return self.end_undone_statement(result, undone);
                }
                result
            }
            Block::Writing(writer) => body(
                &mut crate::temporary::TemporaryWrite {
                    inner: writer,
                    objects,
                },
                tables,
            ),
            Block::Reading(reader) => {
                let mut unwritable = crate::plpgsql::ReadOnlyWrite { inner: &**reader };
                let mut routed = crate::temporary::TemporaryWrite {
                    inner: &mut unwritable,
                    objects,
                };
                let mut own_log = savepoints::Undo::new();
                let (log, statement_start, kept_for) = match undo {
                    Some(log) => {
                        let start = log.len();
                        (log, start, savepoints::UndoKeptFor::RollbackToSavepoint)
                    }
                    None => (&mut own_log, 0, savepoints::UndoKeptFor::FailedStatement),
                };
                let result = body(
                    &mut savepoints::UndoWrite::new(&mut routed, &mut *log, kept_for),
                    tables,
                );
                if undoes(&result) && log.len() > statement_start {
                    let undone = savepoints::undo_back_to(&mut routed, log, statement_start);
                    return self.end_undone_statement(result, undone);
                }
                result
            }
            Block::None | Block::Failed if keeps_procedure_statements => {
                let mut writer = self
                    .store
                    .begin_boxed_write()
                    .map_err(|error| read_only_refusal(error, command))?;
                let mut log = savepoints::Undo::new();
                let outcome = body(
                    &mut savepoints::UndoWrite::new(
                        &mut crate::temporary::TemporaryWrite {
                            inner: &mut writer,
                            objects,
                        },
                        &mut log,
                        savepoints::UndoKeptFor::FailedStatement,
                    ),
                    tables,
                );
                let kept = undo_start(0, log.len());
                if outcome.is_err() && kept > 0 && !keep_open && !implicit {
                    let undone = savepoints::undo_back_to(
                        &mut crate::temporary::TemporaryWrite {
                            inner: &mut writer,
                            objects,
                        },
                        &mut log,
                        kept,
                    );
                    if undone.is_ok() {
                        self.tables = TableCache::default();
                        return self.commit_statement(writer).and(outcome);
                    }
                }
                self.end_autocommit_statement(
                    writer,
                    outcome,
                    command,
                    keep_open,
                    keeps_ddl_open,
                    implicit,
                )
            }
            Block::None | Block::Failed => {
                let mut writer = self
                    .store
                    .begin_boxed_write()
                    .map_err(|error| read_only_refusal(error, command))?;
                let outcome = body(
                    &mut crate::temporary::TemporaryWrite {
                        inner: &mut writer,
                        objects,
                    },
                    tables,
                );
                if !keep_open
                    && !implicit
                    && outcome.as_ref().is_err_and(|error| {
                        keeps_failed_changes && conflict_resolution_applies(error)
                    })
                {
                    return self.commit_statement(writer).and(outcome);
                }
                self.end_autocommit_statement(
                    writer,
                    outcome,
                    command,
                    keep_open,
                    keeps_ddl_open,
                    implicit,
                )
            }
        }
    }

    fn end_undone_statement(
        &mut self,
        result: SqlResult<QueryResult>,
        undone: SqlResult<()>,
    ) -> SqlResult<QueryResult> {
        self.tables = TableCache::default();
        match undone {
            Ok(()) => self.failed_statement_was_undone = true,
            Err(_) if self.sqlserver() => {
                let _ = self.abandon_transaction();
            }
            Err(_) => {
                let _ = self.rollback();
            }
        }
        result
    }

    fn end_autocommit_statement(
        &mut self,
        writer: Box<dyn crate::store::BoxedWrite + 's>,
        outcome: SqlResult<QueryResult>,
        command: &str,
        keep_open: bool,
        keeps_ddl_open: bool,
        implicit: bool,
    ) -> SqlResult<QueryResult> {
        match outcome {
            Ok(result)
                if !keep_open && !implicit && crate::triggers::statement_triggers_pending() =>
            {
                self.block = Block::Writing(writer);
                self.block_is_implicit = false;
                self.trigger_transaction = true;
                Ok(result)
            }
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
                                | "CREATE TRIGGER"
                                | "DROP TRIGGER"
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
            Ok(result) => self.commit_statement(writer).map(|()| result),
            Err(error) => {
                let _ = writer.rollback();
                self.temporary.rollback();
                crate::activity::rolled_back();
                Err(error)
            }
        }
    }

    fn commit_statement(
        &mut self,
        writer: Box<dyn crate::store::BoxedWrite + 's>,
    ) -> SqlResult<()> {
        let pending = crate::deferred_checks::take_statement_pending();
        if !pending.is_empty() {
            let checked = crate::constraints::check_pending(
                &crate::temporary::TemporaryRead {
                    inner: &writer,
                    objects: &self.temporary,
                },
                &mut self.tables,
                &pending,
            );
            if let Err(error) = checked {
                let _ = writer.rollback();
                self.temporary.rollback();
                return Err(error);
            }
        }
        if let Err(error) = writer.commit() {
            self.temporary.rollback();
            return Err(error);
        }
        self.temporary.commit();
        sequences::mark_changes_committed();
        crate::activity::committed();
        Ok(())
    }

    fn begin(&mut self, read_only: bool) -> SqlResult<QueryResult> {
        if self.mysql() && self.in_block() {
            self.commit()?;
        }
        if self.block_is_implicit {
            match self.block {
                Block::Writing(_) => {
                    self.block_is_implicit = false;
                    self.transaction_mode.writing_block_read_only = read_only;
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
                self.transaction_mode.started();
                self.block = if read_only {
                    Block::Reading(self.store.begin_boxed_read()?)
                } else {
                    Block::Writing(self.store.begin_boxed_write()?)
                };
                Ok(QueryResult::command("BEGIN"))
            }
        }
    }

    fn commit(&mut self) -> SqlResult<QueryResult> {
        self.save_points.clear();
        if self.savepoints.aborted {
            self.savepoints.clear();
            return self.rollback();
        }
        self.savepoints.clear();
        self.advisory_locks.release_transaction_locks();
        let implicit = std::mem::take(&mut self.block_is_implicit);
        let failed = matches!(self.block, Block::Failed);
        let pending = std::mem::take(&mut self.deferred.pending);
        self.deferred.clear();
        let ended = self.end_block_for_commit(implicit, &pending);
        self.end_transaction_settings(!failed && ended.is_ok());
        ended
    }

    fn committed(&self, implicit: bool) -> QueryResult {
        let committed = QueryResult::command("COMMIT");
        if implicit && self.dialect == Dialect::PostgreSql {
            committed.with_notice(NO_TRANSACTION_WARNING.into())
        } else {
            committed
        }
    }

    fn end_block_for_commit(
        &mut self,
        implicit: bool,
        pending: &[crate::deferred_checks::PendingCheck],
    ) -> SqlResult<QueryResult> {
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                self.commit_writes(writer, pending)?;
                Ok(self.committed(implicit))
            }
            Block::Reading(_) => {
                self.temporary.commit();
                self.notifications.commit();
                Ok(self.committed(implicit))
            }
            Block::Failed => {
                self.notifications.rollback();
                Ok(QueryResult::command("ROLLBACK"))
            }
            Block::None if self.mysql() || self.oracle() => Ok(QueryResult::command("COMMIT")),
            Block::None => {
                Ok(QueryResult::command("COMMIT").with_notice(NO_TRANSACTION_WARNING.into()))
            }
        }
    }

    fn rollback(&mut self) -> SqlResult<QueryResult> {
        self.notifications.rollback();
        crate::activity::rolled_back();
        self.deferred.clear();
        self.save_points.clear();
        self.savepoints.clear();
        self.advisory_locks.release_transaction_locks();
        let implicit = std::mem::take(&mut self.block_is_implicit);
        self.end_transaction_settings(false);
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                self.temporary.rollback();
                writer.rollback()?;
                let rolled_back = QueryResult::command("ROLLBACK");
                Ok(if implicit && self.dialect == Dialect::PostgreSql {
                    rolled_back.with_notice(NO_TRANSACTION_WARNING.into())
                } else {
                    rolled_back
                })
            }
            Block::Reading(_) => {
                self.temporary.rollback();
                Ok(QueryResult::command("ROLLBACK"))
            }
            Block::Failed => Ok(QueryResult::command("ROLLBACK")),
            Block::None if self.mysql() || self.oracle() => Ok(QueryResult::command("ROLLBACK")),
            Block::None => {
                Ok(QueryResult::command("ROLLBACK").with_notice(NO_TRANSACTION_WARNING.into()))
            }
        }
    }
}

fn view_renamed_by(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    alter: &crate::ast::AlterTable,
    dialect: Dialect,
) -> SqlResult<Option<crate::ast::AlterView>> {
    let [crate::ast::AlterAction::RenameTable(new_name)] = alter.actions.as_slice() else {
        return Ok(None);
    };
    if dialect != Dialect::PostgreSql {
        return Ok(None);
    }
    let target = crate::ast::TableReference::named(alter.name.clone(), None);
    if !crate::updatable_views::is_view(reader, tables, &target)? {
        return Ok(None);
    }
    Ok(Some(crate::ast::AlterView {
        name: alter.name.clone(),
        if_exists: alter.if_exists,
        action: crate::ast::AlterViewAction::RenameTo(new_name.clone()),
    }))
}

pub(crate) fn milliseconds_setting(value: &str) -> Option<i64> {
    let value = value.trim();
    let (number, unit) = value
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .map_or((value, "ms"), |split| {
            (&value[..split], value[split..].trim())
        });
    let number: i64 = number.parse().ok()?;
    if number < 0 {
        return None;
    }
    match unit {
        "ms" => Some(number),
        "s" => number.checked_mul(1000),
        "min" => number.checked_mul(60_000),
        "h" => number.checked_mul(3_600_000),
        _ => None,
    }
}

fn with_raised_notices(result: SqlResult<QueryResult>) -> SqlResult<QueryResult> {
    let raised = crate::plpgsql::take_raised_notices();
    if raised.is_empty() {
        return result;
    }
    match result {
        Ok(mut result) => {
            let own = std::mem::replace(&mut result.notices, raised);
            result.notices.extend(own);
            Ok(result)
        }
        Err(error) => {
            NOTICES_BEFORE_ERROR.with(|kept| {
                let mut kept = kept.borrow_mut();
                let room = MOST_NOTICES_BEFORE_ERROR.saturating_sub(kept.len());
                kept.extend(raised.into_iter().take(room));
            });
            Err(error)
        }
    }
}

const MOST_NOTICES_BEFORE_ERROR: usize = 10_000;

thread_local! {
    static NOTICES_BEFORE_ERROR: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The notices a statement raised before it failed (PL/pgSQL `RAISE NOTICE`, `COPY`'s
/// `LOG_VERBOSITY verbose` lines before a `REJECT_LIMIT` error), each once: a server sends
/// them before the error, as PostgreSQL does.
pub fn take_notices_before_error() -> Vec<String> {
    NOTICES_BEFORE_ERROR.with(|kept| std::mem::take(&mut *kept.borrow_mut()))
}

fn with_refused_routine_setting(
    result: SqlResult<QueryResult>,
    applied: SqlResult<()>,
) -> SqlResult<QueryResult> {
    match applied {
        Ok(()) => result,
        Err(refused) => {
            result.map(|result| result.with_notice(format!("WARNING:  {}", refused.message())))
        }
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
