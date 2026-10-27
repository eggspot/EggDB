use crate::ast::Statement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::{Token, TokenKind, tokenize};
use crate::parser::{LiteralRows, Parameters, Parser, parameter_number};
use crate::settings::Settings;
use crate::store::{KeyValueStore, KeyValueWrite};
use crate::types::{SqlType, Value};
use std::sync::Arc;

/// A value bound to a parameter placeholder `$n` of a prepared statement, as PostgreSQL's
/// extended query protocol passes it: the type the client declared, if any, and the value's
/// text, or `None` for NULL. The text is read as a quoted constant would be: of the declared
/// type, or of the type its place in the statement calls for, following PostgreSQL's rules
/// for constants of unknown type.
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
    tokens
        .iter()
        .filter_map(|token| match token.kind {
            TokenKind::Parameter(digits) => parameter_number(digits),
            _ => None,
        })
        .max()
        .unwrap_or(0)
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
}

impl QueryResult {
    pub(crate) fn command(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            columns: None,
            rows: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub(crate) fn rows(tag: String, columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> Self {
        Self {
            tag,
            columns: Some(columns),
            rows,
            notices: Vec::new(),
        }
    }

    pub(crate) fn with_notice(mut self, notice: String) -> Self {
        self.notices.push(notice);
        self
    }
}

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

/// A SQL session over a [`KeyValueStore`]: it runs statements of the PostgreSQL dialect one
/// after another, each in its own transaction unless a `BEGIN` opened a block.
///
/// A block holds the store's one write transaction from `BEGIN` to `COMMIT` or `ROLLBACK`.
/// A failed statement inside a block rolls the block back at once and leaves the session in
/// PostgreSQL's aborted state: every statement fails with SQLSTATE `25P02` until `ROLLBACK`
/// (or `COMMIT`, which then reports `ROLLBACK`).
pub struct Session<'s, S: KeyValueStore + 's> {
    store: &'s S,
    block: Block<'s, S>,
    tables: TableCache,
    settings: Arc<Settings>,
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
        Self {
            store,
            block: Block::None,
            tables: TableCache::default(),
            settings: Arc::new(Settings::default()),
        }
    }

    /// Names the user this session runs as and the database it reaches, which
    /// `current_user`, `current_database()` and `session_authorization` report.
    pub fn identify(&mut self, user: &str, database: &str) {
        let settings = Arc::make_mut(&mut self.settings);
        settings.set_session_user(user);
        settings.database = database.to_owned();
    }

    /// Sets the run-time parameter `name` (case-insensitive) as `SET name = value` would, for
    /// the parameters a client sends when it connects.
    pub fn set_parameter(&mut self, name: &str, value: &str) -> SqlResult<()> {
        Arc::make_mut(&mut self.settings).set(name, Some(value))
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

    /// Where the session stands toward transactions.
    pub fn transaction_status(&self) -> TransactionStatus {
        match self.block {
            Block::None => TransactionStatus::Idle,
            Block::Writing(_) | Block::Reading(_) => TransactionStatus::InBlock,
            Block::Failed => TransactionStatus::Failed,
        }
    }

    /// Checks `text` as the text of a prepared statement: at most one statement, valid
    /// syntax, and `$n` placeholders numbered 1 to 65535. Nothing runs and the catalog is not
    /// read. `declared_types` are the parameter types the client gave.
    pub fn prepare(
        &self,
        text: &str,
        declared_types: &[Option<SqlType>],
    ) -> SqlResult<PreparedStatement> {
        let tokens = tokenize(text)?;
        let mut parser = Parser::new(text, &tokens)
            .with_context(Parameters::Types(declared_types), &self.settings);
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
        let tokens = tokenize(text)?;
        let mut parameter_types = declared_types.to_vec();
        parameter_types.resize(highest_parameter(&tokens).max(declared_types.len()), None);
        let mut parser = Parser::new(text, &tokens)
            .with_context(Parameters::Types(declared_types), &self.settings);
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
        let columns = match &statement {
            Statement::Select(select) => self
                .read(|reader, tables| execute::select(reader, tables, select, true))
                .map(|result| result.columns),
            Statement::Insert(insert) => self.read(|reader, tables| {
                execute::returning_columns(reader, tables, &insert.table, &insert.returning)
            }),
            Statement::Update(update) => self.read(|reader, tables| {
                execute::returning_columns(reader, tables, &update.table, &update.returning)
            }),
            Statement::Delete(delete) => self.read(|reader, tables| {
                execute::returning_columns(reader, tables, &delete.table, &delete.returning)
            }),
            Statement::Show { name } => Ok(Some(self.show_columns(name.as_deref()))),
            _ => Ok(None),
        };
        match columns {
            Ok(columns) => Ok(StatementDescription {
                parameter_types,
                columns,
            }),
            Err(error) => {
                self.fail_block();
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
        self.run_text(text, Parameters::Values(parameters), on_result)
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

    /// Whether a `BEGIN` block is open, failed or not.
    pub fn in_block(&self) -> bool {
        !matches!(self.block, Block::None)
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
    /// open.
    pub fn execute_while(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        self.run_text(text, Parameters::Absent, on_result)
    }

    fn run_text(
        &mut self,
        text: &str,
        parameters: Parameters<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        let tokens = match tokenize(text) {
            Ok(tokens) => tokens,
            Err(error) => {
                self.fail_block();
                return Err(error);
            }
        };
        let settings = Arc::clone(&self.settings);
        let mut parser = Parser::new(text, &tokens).with_context(parameters, &settings);
        loop {
            parser.skip_empty_statements();
            if parser.at_end() {
                return Ok(());
            }
            if let Some(rows) = parser.literal_rows() {
                let result = self.insert_literal_rows(&rows)?;
                if !on_result(result) {
                    return Ok(());
                }
                continue;
            }
            let parsed = parser.statement();
            let statement = match parsed {
                Ok(statement) => statement,
                Err(error) => {
                    self.fail_block();
                    return Err(error);
                }
            };
            let result = self.run(&statement)?;
            if !on_result(result) {
                return Ok(());
            }
        }
    }

    /// Ends the session: an open block is rolled back. Whether one was open.
    pub fn close(mut self) -> SqlResult<bool> {
        let was_open = self.in_block();
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => writer.rollback()?,
            Block::Reading(_) | Block::Failed | Block::None => {}
        }
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

    fn run(&mut self, statement: &Statement) -> SqlResult<QueryResult> {
        match statement {
            Statement::Begin { read_only } => return self.begin(*read_only),
            Statement::Commit => return self.commit(),
            Statement::Rollback => return self.rollback(),
            _ => {}
        }
        if matches!(self.block, Block::Failed) {
            return Err(aborted_block());
        }
        let result = match statement {
            Statement::Select(select) => {
                self.read(|reader, tables| execute::select(reader, tables, select, false))
            }
            Statement::Set { name, value } => Arc::make_mut(&mut self.settings)
                .set(name, value.as_deref())
                .map(|()| QueryResult::command("SET")),
            Statement::Reset { name: Some(name) } => Arc::make_mut(&mut self.settings)
                .set(name, None)
                .map(|()| QueryResult::command("RESET")),
            Statement::Reset { name: None } => {
                Arc::make_mut(&mut self.settings).reset_all();
                Ok(QueryResult::command("RESET"))
            }
            Statement::Show { name } => self.show(name.as_deref()),
            Statement::CreateTable(create) => self.write("CREATE TABLE", |writer, tables| {
                execute::create_table(writer, tables, create)
            }),
            Statement::DropTable { names, if_exists } => self
                .write("DROP TABLE", |writer, tables| {
                    execute::drop_tables(writer, tables, names, *if_exists)
                }),
            Statement::Insert(insert) => self.write("INSERT", |writer, tables| {
                execute::insert(writer, tables, insert)
            }),
            Statement::Update(update) => self.write("UPDATE", |writer, tables| {
                execute::update(writer, tables, update)
            }),
            Statement::Delete(delete) => self.write("DELETE", |writer, tables| {
                execute::delete(writer, tables, delete)
            }),
            Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {
                return Err(SqlError::new(
                    SqlState::INTERNAL_ERROR,
                    "unreachable statement",
                ));
            }
        };
        if result.is_err() {
            self.fail_block();
        }
        result
    }

    fn show(&self, name: Option<&str>) -> SqlResult<QueryResult> {
        let rows = match name {
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

    fn insert_literal_rows(&mut self, rows: &LiteralRows<'_>) -> SqlResult<QueryResult> {
        if matches!(self.block, Block::Failed) {
            return Err(aborted_block());
        }
        let result = self.write("INSERT", |writer, tables| {
            execute::insert_literals(writer, tables, rows)
        });
        if result.is_err() {
            self.fail_block();
        }
        result
    }

    fn read<T>(
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
        let tables = &mut self.tables;
        match &mut self.block {
            Block::Writing(writer) => body(writer, tables),
            Block::Reading(_) => Err(SqlError::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {command} in a read-only transaction"),
            )),
            Block::None | Block::Failed => {
                let mut writer = self.store.begin_write()?;
                match body(&mut writer, tables) {
                    Ok(result) => {
                        writer.commit()?;
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
                Ok(QueryResult::command("BEGIN"))
            }
        }
    }

    fn commit(&mut self) -> SqlResult<QueryResult> {
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                writer.commit()?;
                Ok(QueryResult::command("COMMIT"))
            }
            Block::Reading(_) => Ok(QueryResult::command("COMMIT")),
            Block::Failed => Ok(QueryResult::command("ROLLBACK")),
            Block::None => Ok(QueryResult::command("COMMIT")
                .with_notice("WARNING:  there is no transaction in progress".into())),
        }
    }

    fn rollback(&mut self) -> SqlResult<QueryResult> {
        match std::mem::replace(&mut self.block, Block::None) {
            Block::Writing(writer) => {
                writer.rollback()?;
                Ok(QueryResult::command("ROLLBACK"))
            }
            Block::Reading(_) | Block::Failed => Ok(QueryResult::command("ROLLBACK")),
            Block::None => Ok(QueryResult::command("ROLLBACK")
                .with_notice("WARNING:  there is no transaction in progress".into())),
        }
    }
}
