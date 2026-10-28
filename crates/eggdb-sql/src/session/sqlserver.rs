use super::{Client, QueryResult, Session};
use crate::ast::SqlServerStatement;
use crate::copy::NoCopyChannel;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::tokenize_in;
use crate::parser::{Parameters, Parser};
use crate::store::KeyValueStore;
use std::sync::Arc;

const STATEMENT_TERMINATING_ERRORS: [u16; 9] = [2627, 2601, 547, 515, 544, 2628, 8152, 3902, 3903];

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
        loop {
            let mut parser = Parser::new(text, tokens)
                .with_context(Parameters::Absent, &settings)
                .in_dialect(self.dialect)
                .resuming_at(resume);
            parser.skip_empty_statements();
            if parser.at_end() {
                return Ok(());
            }
            if parser.literal_rows().is_none() {
                parser.statement()?;
            }
            resume = parser.resume_point();
        }
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
        }
    }
}
