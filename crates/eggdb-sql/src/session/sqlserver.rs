use super::{Client, QueryResult, Session};
use crate::ast::{Name, SqlServerStatement};
use crate::copy::NoCopyChannel;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::Relation;
use crate::lexer::tokenize_in;
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::store::KeyValueStore;
use std::sync::Arc;

const STATEMENT_TERMINATING_ERRORS: [u16; 17] = [
    2627, 2601, 547, 515, 544, 545, 339, 2628, 8152, 3902, 3903, 1088, 8106, 8107, 4917, 1754, 3701,
];

/// What one statement of a Transact-SQL batch did, as [`Session::execute_batch`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStep {
    /// The statement's result, or the error it failed with, in SQL Server's numbering.
    pub outcome: Result<QueryResult, SqlError>,
    /// The byte offset in the batch where the statement starts, from which a server counts the
    /// line SQL Server reports with an error.
    pub statement_start: usize,
    /// Whether a transaction is open after the statement.
    pub in_transaction: bool,
    /// Whether `SET NOCOUNT ON` is in effect after the statement, so a server leaves the
    /// count of rows out of its completion.
    pub no_count: bool,
}

fn rows_counted(result: &QueryResult) -> i64 {
    let mut words = result.tag.split_whitespace();
    let counted = match words.next() {
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE") => true,
        Some("SELECT") => return result.rows.len() as i64,
        _ => false,
    };
    if !counted {
        return 0;
    }
    words
        .last()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

pub(crate) struct ParsedStatement {
    pub(crate) first_token: usize,
    pub(crate) outcome: SqlResult<(usize, usize)>,
}

pub(crate) fn parse_next_statement(
    text: &str,
    tokens: &[crate::lexer::Token<'_>],
    settings: &crate::settings::Settings,
    dialect: Dialect,
    resume: (usize, usize),
) -> Option<ParsedStatement> {
    let mut parser = Parser::new(text, tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(dialect)
        .resuming_at(resume);
    parser.skip_empty_statements();
    if parser.at_end() {
        return None;
    }
    let first_token = parser.resume_point().0;
    let outcome = if parser.literal_rows().is_none() {
        parser.statement().map(|_| parser.resume_point())
    } else {
        Ok(parser.resume_point())
    };
    Some(ParsedStatement {
        first_token,
        outcome,
    })
}

fn no_transaction(code: u16, message: &str) -> SqlError {
    SqlError::new(SqlState::NO_ACTIVE_SQL_TRANSACTION, message).with_sqlserver_code(code)
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    /// Runs `text` as a Transact-SQL batch, the way SQL Server runs a batch a client sends: the
    /// whole batch is parsed first, so a syntax error runs nothing and is returned as the
    /// error; then each statement runs in turn and `on_step` receives what it did. A
    /// constraint violation and the other statement-terminating errors end only their
    /// statement, unless they rolled back an open transaction; any other error ends the batch
    /// after its step. The batch also stops before the next statement once `on_step` returns
    /// `false`. Errors carry SQL Server's numbers ([`SqlError::vendor_code`]) and severities
    /// ([`SqlError::severity`]). In the other dialects every statement runs as
    /// [`Session::execute_while`] would run it, and the first error ends the batch.
    pub fn execute_batch(
        &mut self,
        text: &str,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        self.statement_start = 0;
        let tokens =
            tokenize_in(text, self.dialect).map_err(|error| self.in_dialect(text, error))?;
        self.check_batch_syntax(text, &tokens)
            .map_err(|error| self.in_dialect(text, error))?;
        self.batch_resume = Some((0, 0));
        let outcome = self.run_batch_statements(text, &tokens, on_step);
        self.batch_resume = None;
        outcome
    }

    fn check_batch_syntax(
        &mut self,
        text: &str,
        tokens: &[crate::lexer::Token<'_>],
    ) -> SqlResult<()> {
        if self.sqlserver() {
            self.sync_transaction_count();
        }
        let settings = Arc::clone(&self.settings);
        let mut resume = (0, 0);
        while let Some(parsed) = parse_next_statement(text, tokens, &settings, self.dialect, resume)
        {
            resume = parsed.outcome?;
        }
        Ok(())
    }

    fn run_batch_statements(
        &mut self,
        text: &str,
        tokens: &[crate::lexer::Token<'_>],
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        loop {
            let (first_token, _) = self.batch_resume.unwrap_or((0, 0));
            let statement_start = tokens
                .iter()
                .skip(first_token)
                .find(|token| token.kind != crate::lexer::TokenKind::Semicolon)
                .map_or(text.len(), |token| token.start);
            let was_in_transaction = self.in_block();
            let mut results = Vec::new();
            let mut client = Client {
                channel: &mut NoCopyChannel,
                script: false,
            };
            let flow = self.run_tokens(
                text,
                tokens,
                Parameters::Absent,
                &mut |result| {
                    results.push(result);
                    false
                },
                &mut client,
                None,
            );
            let in_transaction = self.in_block();
            match flow {
                Ok(super::Flow::Finished { .. }) => return Ok(()),
                Ok(super::Flow::Stopped) => {
                    for result in results {
                        let step = BatchStep {
                            outcome: Ok(result),
                            statement_start,
                            in_transaction,
                            no_count: self.settings.sqlserver.no_count,
                        };
                        if !on_step(step) {
                            return Ok(());
                        }
                    }
                }
                Err(error) => {
                    let error = self.in_dialect(text, error);
                    let ends_batch = self.dialect != Dialect::SqlServer
                        || !error
                            .vendor_code()
                            .is_some_and(|code| STATEMENT_TERMINATING_ERRORS.contains(&code))
                        || (was_in_transaction && !in_transaction);
                    let step = BatchStep {
                        outcome: Err(error),
                        statement_start,
                        in_transaction,
                        no_count: self.settings.sqlserver.no_count,
                    };
                    if !on_step(step) || ends_batch {
                        return Ok(());
                    }
                }
            }
        }
    }

    pub(super) fn enter_sqlserver_status(
        &mut self,
    ) -> Option<crate::sqlserver_functions::StatusGuard> {
        if !self.sqlserver() {
            return None;
        }
        let guard = crate::sqlserver_functions::enter_status(self.sqlserver_status);
        self.sqlserver_status.row_count = 0;
        Some(guard)
    }

    pub(super) fn sqlserver_after_statement(&mut self, result: &QueryResult) {
        self.sqlserver_status.row_count = rows_counted(result);
        if let Some(identity) = result.last_identity_value {
            self.sqlserver_status.identity = Some(identity);
        }
    }

    pub(super) fn sync_transaction_count(&mut self) {
        let open = self.in_block();
        let count = self.settings.sqlserver.transaction_count;
        let wanted = match (open, count) {
            (false, _) => 0,
            (true, 0) => 1,
            (true, count) => count,
        };
        if wanted != count {
            Arc::make_mut(&mut self.settings)
                .sqlserver
                .transaction_count = wanted;
        }
    }

    fn set_transaction_count(&mut self, count: u32) {
        Arc::make_mut(&mut self.settings)
            .sqlserver
            .transaction_count = count;
    }

    pub(super) fn sqlserver_begin(&mut self) -> SqlResult<QueryResult> {
        self.sync_transaction_count();
        let count = self.settings.sqlserver.transaction_count;
        if count == 0 {
            self.begin(false)?;
        }
        self.set_transaction_count(count.saturating_add(1));
        Ok(QueryResult::command("BEGIN TRANSACTION"))
    }

    pub(super) fn sqlserver_commit(&mut self) -> SqlResult<QueryResult> {
        self.sync_transaction_count();
        match self.settings.sqlserver.transaction_count {
            0 => Err(no_transaction(
                3902,
                "The COMMIT TRANSACTION request has no corresponding BEGIN TRANSACTION.",
            )),
            1 => {
                self.set_transaction_count(0);
                self.commit()?;
                Ok(QueryResult::command("COMMIT"))
            }
            count => {
                self.set_transaction_count(count - 1);
                Ok(QueryResult::command("COMMIT"))
            }
        }
    }

    pub(super) fn sqlserver_rollback(&mut self) -> SqlResult<QueryResult> {
        self.sync_transaction_count();
        if self.settings.sqlserver.transaction_count == 0 {
            return Err(no_transaction(
                3903,
                "The ROLLBACK TRANSACTION request has no corresponding BEGIN TRANSACTION.",
            ));
        }
        self.set_transaction_count(0);
        self.rollback()?;
        Ok(QueryResult::command("ROLLBACK"))
    }

    pub(super) fn run_sqlserver(
        &mut self,
        statement: &SqlServerStatement,
    ) -> SqlResult<QueryResult> {
        match statement {
            SqlServerStatement::Use(name) => {
                if !name.text.eq_ignore_ascii_case(&self.settings.database) {
                    return Err(SqlError::new(
                        SqlState::INVALID_CATALOG_NAME,
                        format!(
                            "Database '{}' does not exist. Make sure that the name is entered correctly.",
                            name.text
                        ),
                    )
                    .with_sqlserver_code(911)
                    .at(name.position));
                }
                Ok(QueryResult::command(format!(
                    "USE {}",
                    self.settings.database
                )))
            }
            SqlServerStatement::NoCount(on) => {
                Arc::make_mut(&mut self.settings).sqlserver.no_count = *on;
                Ok(QueryResult::command("SET"))
            }
            SqlServerStatement::IdentityInsert { table, on } => {
                self.set_identity_insert(table, *on)
            }
        }
    }

    fn set_identity_insert(&mut self, table: &Name, on: bool) -> SqlResult<QueryResult> {
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: false,
        };
        let mut resolved = table.clone();
        let relation = self.read(|reader, tables| {
            binding.names(reader, tables, std::slice::from_mut(&mut resolved))?;
            tables.relation(reader, &resolved.text)
        })?;
        let shown = crate::schemas::display(&resolved.text);
        let has_identity = match relation {
            None => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!(
                        "Cannot find the object \"{shown}\" because it does not exist or you do not have permissions."
                    ),
                )
                .with_sqlserver_code(1088)
                .at(table.position));
            }
            Some(Relation::Table(definition)) => definition.auto_increment_column().is_some(),
            Some(Relation::View(_)) => false,
        };
        if !has_identity {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "Table '{shown}' does not have the identity property. Cannot perform SET operation."
                ),
            )
            .with_sqlserver_code(8106)
            .at(table.position));
        }
        let already_on = self.identity_insert_table.as_deref() == Some(resolved.text.as_str());
        if on
            && !already_on
            && let Some(current) = self.identity_insert_table.clone()
            && self
                .read(|reader, tables| tables.relation(reader, &current))?
                .is_none()
        {
            self.identity_insert_table = None;
        }
        if on
            && !already_on
            && let Some(current) = &self.identity_insert_table
        {
            let schema = match crate::schemas::schema_of(current) {
                crate::schemas::DEFAULT_SCHEMA => "dbo",
                other => other,
            };
            return Err(SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "IDENTITY_INSERT is already ON for table '{}.{schema}.{}'. Cannot perform SET operation for table '{shown}'.",
                    self.settings.database,
                    crate::schemas::base_name(current)
                ),
            )
            .with_sqlserver_code(8107)
            .at(table.position));
        }
        if on {
            self.identity_insert_table = Some(resolved.text);
        } else if already_on {
            self.identity_insert_table = None;
        }
        Ok(QueryResult::command("SET"))
    }
}
