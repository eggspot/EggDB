use crate::ast::Statement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute;
use crate::lexer::tokenize;
use crate::parser::Parser;
use crate::store::{KeyValueStore, KeyValueWrite};
use crate::types::{SqlType, Value};

/// A column of a statement's result.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    /// The column's name, as PostgreSQL names it: the column, the alias, the function, or
    /// `?column?`.
    pub name: String,
    /// The column's type when it is a table column, so a client can align numbers right.
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
        let tokens = tokenize(text)?;
        let mut parser = Parser::new(text, &tokens);
        loop {
            parser.skip_empty_statements();
            if parser.at_end() {
                return Ok(());
            }
            let parsed = parser.statement();
            let statement = match parsed {
                Ok(statement) => statement,
                Err(error) => {
                    self.fail_block();
                    return Err(error);
                }
            };
            {
                let result = self.run(&statement)?;
                on_result(result)
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
            return Err(SqlError::new(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let result = match statement {
            Statement::Select(select) => self.read(|reader| execute::select(reader, select)),
            Statement::CreateTable(create) => self.write("CREATE TABLE", |writer| {
                execute::create_table(writer, create)
            }),
            Statement::DropTable { names, if_exists } => self.write("DROP TABLE", |writer| {
                execute::drop_tables(writer, names, *if_exists)
            }),
            Statement::Insert(insert) => {
                self.write("INSERT", |writer| execute::insert(writer, insert))
            }
            Statement::Update(update) => {
                self.write("UPDATE", |writer| execute::update(writer, update))
            }
            Statement::Delete(delete) => {
                self.write("DELETE", |writer| execute::delete(writer, delete))
            }
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

    fn read(
        &mut self,
        body: impl FnOnce(&dyn crate::store::KeyValueRead) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        match &self.block {
            Block::Writing(writer) => body(writer),
            Block::Reading(reader) => body(reader),
            Block::None | Block::Failed => {
                let reader = self.store.begin_read()?;
                body(&reader)
            }
        }
    }

    fn write(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn KeyValueWrite) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        match &mut self.block {
            Block::Writing(writer) => body(writer),
            Block::Reading(_) => Err(SqlError::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {command} in a read-only transaction"),
            )),
            Block::None | Block::Failed => {
                let mut writer = self.store.begin_write()?;
                match body(&mut writer) {
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
            Block::Failed => Err(SqlError::new(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            )),
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
