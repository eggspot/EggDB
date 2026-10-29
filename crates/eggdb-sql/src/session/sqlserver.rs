use super::{Client, OutputColumn, QueryResult, Session};
use crate::ast::{Expr, Name, ProcedureCall, SqlServerStatement};
use crate::bulk_load::{BulkRows, InsertBulk};
use crate::copy::{CopyIo, NoCopyChannel};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::execute::Relation;
use crate::lexer::{Token, TokenKind, tokenize_from, tokenize_in};
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::store::KeyValueStore;
use crate::types::{SqlType, Value};
use std::sync::Arc;

const RPC_PROCEDURES: [&str; 5] = [
    "sp_executesql",
    "sp_prepare",
    "sp_prepexec",
    "sp_execute",
    "sp_unprepare",
];

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
    /// Whether `SET FMTONLY ON` is in effect after the statement: a query's result then holds
    /// its columns and no rows, and a server describes each column by its declared type
    /// rather than by the values it holds.
    pub format_only: bool,
}

/// A value for a named parameter (`@name`) of a parameterized Transact-SQL batch, as SQL
/// Server's `sp_executesql` binds one ([`Session::execute_parameterized_batch`]).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedParameter {
    /// The parameter's name with its `@`, matched without regard to case.
    pub name: String,
    /// The type the parameter was declared with: a non-NULL value converts to it as `CAST`
    /// would, and NULL takes it. `None` keeps the value's own type.
    pub declared_type: Option<SqlType>,
    /// The value.
    pub value: Value,
}

/// One declaration of a parameterized batch's parameter list, such as `@p1 nvarchar(40)
/// OUTPUT` in `sp_executesql`'s `@params` ([`Session::parse_parameter_declarations`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterDeclaration {
    /// The name with its `@`, in lower case.
    pub name: String,
    /// The engine type the declared SQL Server type is kept as.
    pub sql_type: SqlType,
    /// Whether it is declared `OUTPUT` (or `OUT`).
    pub output: bool,
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
    pub(crate) chooses_dialect: bool,
}

fn chooses_dialect(statement: &crate::ast::Statement) -> bool {
    match statement {
        crate::ast::Statement::Set { name, .. } => super::is_dialect_setting(name),
        crate::ast::Statement::Reset { name: Some(name) } => super::is_dialect_setting(name),
        _ => false,
    }
}

pub(crate) fn parse_next_statement(
    text: &str,
    tokens: &[crate::lexer::Token<'_>],
    settings: &crate::settings::Settings,
    dialect: Dialect,
    resume: (usize, usize),
) -> Option<ParsedStatement> {
    parse_next_parameterized_statement(text, tokens, settings, dialect, resume, Parameters::Absent)
}

fn parse_next_parameterized_statement(
    text: &str,
    tokens: &[crate::lexer::Token<'_>],
    settings: &crate::settings::Settings,
    dialect: Dialect,
    resume: (usize, usize),
    parameters: Parameters<'_>,
) -> Option<ParsedStatement> {
    let mut parser = Parser::new(text, tokens)
        .with_context(parameters, settings)
        .in_dialect(dialect)
        .resuming_at(resume);
    parser.skip_empty_statements();
    if parser.at_end() {
        return None;
    }
    let first_token = parser.resume_point().0;
    let mut chooses = false;
    let outcome = if parser.literal_rows().is_none() {
        parser.statement().map(|statement| {
            chooses = chooses_dialect(&statement);
            parser.resume_point()
        })
    } else {
        Ok(parser.resume_point())
    };
    Some(ParsedStatement {
        first_token,
        outcome,
        chooses_dialect: chooses,
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
        self.run_batch(text, Parameters::Absent, None, on_step)
    }

    /// Runs `text` as a parameterized Transact-SQL batch, as SQL Server's `sp_executesql`
    /// does: like [`Session::execute_batch`], with each `@name` that is not a declared variable
    /// read as the value of the parameter of that name in `parameters` (converted to its
    /// declared type). A name with no parameter is error 137. Only the SQL Server dialect reads
    /// `@name`s.
    pub fn execute_parameterized_batch(
        &mut self,
        text: &str,
        parameters: &[NamedParameter],
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        self.run_batch(text, Parameters::Named(parameters), None, on_step)
    }

    /// Whether an `INSERT BULK` statement has run and waits for its rows
    /// ([`Session::execute_bulk_load`]), as SQL Server waits for the bulk load data a client
    /// sends after one. The next batch, connection reset or load ends the wait.
    pub fn bulk_load_pending(&self) -> bool {
        self.pending_bulk_load.is_some()
    }

    /// Loads `rows` into the table of the `INSERT BULK` statement this session ran last, as
    /// SQL Server loads the bulk data `bcp in` and `SqlBulkCopy` send after one: all of them
    /// in one statement, which an error ends with nothing loaded, each row through the
    /// `INSERT` path (defaults, identity, constraints, indexes, foreign keys). A NULL takes
    /// its column's default unless the statement said `KEEP_NULLS`; columns the statement
    /// did not name take their defaults; an identity column the statement named keeps the
    /// values given, as `SqlBulkCopyOptions.KeepIdentity` asks, and one it did not name is
    /// numbered. `on_step` receives what the load did, as [`Session::execute_batch`] reports
    /// a statement: `INSERT 0 n`, or the error. With no `INSERT BULK` waiting, it fails.
    pub fn execute_bulk_load(
        &mut self,
        rows: &mut dyn BulkRows,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        let Some(text) = self.pending_bulk_load.take() else {
            return Err(SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "no INSERT BULK statement is waiting for its rows",
            )
            .with_hint("Run INSERT BULK first; the rows must follow it before any other batch."));
        };
        self.run_batch(&text, Parameters::Absent, Some(rows), on_step)
    }

    /// Parses `text` as [`Session::execute_parameterized_batch`] would, running nothing: the
    /// syntax error it would fail with, if any, as `sp_prepare` reports it.
    pub fn check_parameterized_batch(
        &mut self,
        text: &str,
        parameters: &[NamedParameter],
    ) -> SqlResult<()> {
        self.statement_start = 0;
        let tokens =
            tokenize_in(text, self.dialect).map_err(|error| self.in_dialect(text, error))?;
        self.check_batch_syntax(text, &tokens, Parameters::Named(parameters))
            .map_err(|error| self.in_dialect(text, error))
    }

    /// Reads a parameter list as `sp_executesql` and `sp_prepare` take it: `@name type
    /// [OUTPUT]` declarations separated by commas, with SQL Server's types. An empty text
    /// declares nothing. A repeated name is error 134 and an unknown type error 2715.
    pub fn parse_parameter_declarations(
        &mut self,
        text: &str,
    ) -> SqlResult<Vec<ParameterDeclaration>> {
        self.statement_start = 0;
        crate::parser::sqlserver_parameter_declarations(text)
            .map_err(|error| self.in_dialect(text, error))
    }

    /// Resets the session as SQL Server resets a connection a pool hands out again: an open
    /// transaction rolls back unless `keep_transaction`, and the `SET` options (`eggdb.dialect`
    /// among them), `IDENTITY_INSERT`, `@@ROWCOUNT` and `SCOPE_IDENTITY()` return to their
    /// state after login. The user, the database and `@@SPID` stay. An error is the rollback's.
    pub fn reset_connection(&mut self, keep_transaction: bool) -> SqlResult<()> {
        let rolled_back = if keep_transaction || !self.in_block() {
            Ok(())
        } else {
            self.rollback().map(|_| ())
        };
        let settings = Arc::make_mut(&mut self.settings);
        settings.reset_all();
        settings.sqlserver.no_count = false;
        settings.sqlserver.implicit_transactions = false;
        settings.sqlserver.format_only = false;
        self.pending_bulk_load = None;
        self.dialect = self.connection_dialect;
        self.sync_transaction_count();
        self.identity_insert_table = None;
        self.table_variables = crate::table_variables::TableVariables::default();
        self.sqlserver_status = crate::sqlserver_functions::SessionStatus::default();
        self.warnings.clear();
        rolled_back
    }

    fn run_batch(
        &mut self,
        text: &str,
        parameters: Parameters<'_>,
        bulk_rows: Option<&mut dyn BulkRows>,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        self.statement_start = 0;
        self.pending_bulk_load = None;
        let tokens =
            tokenize_in(text, self.dialect).map_err(|error| self.in_dialect(text, error))?;
        self.check_batch_syntax(text, &tokens, parameters)
            .map_err(|error| self.in_dialect(text, error))?;
        self.batch_resume = Some((0, 0));
        self.table_variables = crate::table_variables::TableVariables::default();
        let outcome = self.run_batch_statements(text, &tokens, parameters, bulk_rows, on_step);
        self.table_variables = crate::table_variables::TableVariables::default();
        self.batch_resume = None;
        outcome
    }

    fn check_batch_syntax(
        &mut self,
        text: &str,
        tokens: &[crate::lexer::Token<'_>],
        parameters: Parameters<'_>,
    ) -> SqlResult<()> {
        if self.sqlserver() {
            self.sync_transaction_count();
        }
        let settings = Arc::clone(&self.settings);
        let mut resume = (0, 0);
        while let Some(parsed) = parse_next_parameterized_statement(
            text,
            tokens,
            &settings,
            self.dialect,
            resume,
            parameters,
        ) {
            resume = parsed.outcome?;
            if parsed.chooses_dialect {
                break;
            }
        }
        Ok(())
    }

    fn run_batch_statements<'t>(
        &mut self,
        text: &'t str,
        tokens: &[Token<'t>],
        parameters: Parameters<'_>,
        mut bulk_rows: Option<&mut dyn BulkRows>,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        let mut rest_in_new_dialect: Option<Vec<Token<'t>>> = None;
        loop {
            let tokens: &[Token<'t>] = rest_in_new_dialect.as_deref().unwrap_or(tokens);
            let statement_dialect = self.dialect;
            let (first_token, _) = self.batch_resume.unwrap_or((0, 0));
            let statement_start = tokens
                .iter()
                .skip(first_token)
                .find(|token| token.kind != TokenKind::Semicolon)
                .map_or(text.len(), |token| token.start);
            let was_in_transaction = self.in_block();
            let mut results = Vec::new();
            let mut client = Client {
                channel: &mut NoCopyChannel,
                script: false,
                bulk_rows: super::reborrowed_rows(&mut bulk_rows),
            };
            let flow = self.run_tokens(
                text,
                tokens,
                parameters,
                &mut |result| {
                    results.push(result);
                    false
                },
                &mut client,
                None,
            );
            let in_transaction = self.in_block();
            match flow {
                Ok(super::Flow::Finished { .. } | super::Flow::DialectChanged { .. }) => {
                    return Ok(());
                }
                Ok(super::Flow::Stopped) => {
                    for result in results {
                        let step = BatchStep {
                            outcome: Ok(result),
                            statement_start,
                            in_transaction,
                            no_count: self.settings.sqlserver.no_count,
                            format_only: self.settings.sqlserver.format_only,
                        };
                        if !on_step(step) {
                            return Ok(());
                        }
                    }
                    if self.dialect != statement_dialect {
                        let (next, _) = self.batch_resume.unwrap_or((0, 0));
                        let at = tokens.get(next).map_or(text.len(), |token| token.start);
                        match self.rest_of_batch(text, at, parameters) {
                            Ok(rest) => {
                                rest_in_new_dialect = Some(rest);
                                self.batch_resume = Some((0, 0));
                            }
                            Err(error) => {
                                let step = BatchStep {
                                    outcome: Err(self.in_dialect(text, error)),
                                    statement_start: at,
                                    in_transaction: self.in_block(),
                                    no_count: self.settings.sqlserver.no_count,
                                    format_only: self.settings.sqlserver.format_only,
                                };
                                on_step(step);
                                return Ok(());
                            }
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
                        format_only: self.settings.sqlserver.format_only,
                    };
                    if !on_step(step) || ends_batch {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn rest_of_batch<'t>(
        &mut self,
        text: &'t str,
        at: usize,
        parameters: Parameters<'_>,
    ) -> SqlResult<Vec<Token<'t>>> {
        let tokens = tokenize_from(text, self.dialect, at)?;
        self.check_batch_syntax(text, &tokens, parameters)?;
        Ok(tokens)
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
            SqlServerStatement::ImplicitTransactions(on) => {
                Arc::make_mut(&mut self.settings)
                    .sqlserver
                    .implicit_transactions = *on;
                Ok(QueryResult::command("SET"))
            }
            SqlServerStatement::FormatOnly(on) => {
                Arc::make_mut(&mut self.settings).sqlserver.format_only = *on;
                Ok(QueryResult::command("SET"))
            }
            SqlServerStatement::InsertBulk(_) => Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "an INSERT BULK statement reached the statement runner",
            )),
            SqlServerStatement::IdentityInsert { table, on } => {
                self.set_identity_insert(table, *on)
            }
            SqlServerStatement::If { .. } => Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "an IF statement reached the statement runner",
            )),
            SqlServerStatement::Execute(call) => {
                let mut results = self.procedure_results(call)?.into_iter();
                let first = results
                    .next()
                    .unwrap_or_else(|| QueryResult::command("EXECUTE"));
                self.extra_results.extend(results);
                Ok(first)
            }
            SqlServerStatement::DeclareTable { name, columns } => {
                self.declare_table_variable(name, columns)
            }
            SqlServerStatement::InsertExecute {
                variable,
                columns,
                call,
            } => self.insert_procedure_results(variable, columns.as_deref(), call),
        }
    }

    pub(super) fn run_insert_bulk(
        &mut self,
        bulk: &mut InsertBulk,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: false,
        };
        match &mut io.bulk_rows {
            None => self.read(|reader, tables| {
                binding.names(reader, tables, std::slice::from_mut(&mut bulk.table))?;
                crate::bulk_load::check_target(reader, tables, bulk)
            }),
            Some(rows) => self.write("INSERT BULK", |writer, tables| {
                binding.names(writer, tables, std::slice::from_mut(&mut bulk.table))?;
                crate::bulk_load::load(writer, tables, bulk, &mut **rows)
            }),
        }
    }

    fn procedure_results(&mut self, call: &ProcedureCall) -> SqlResult<Vec<QueryResult>> {
        let name = call.procedure.text.as_str();
        let Some(procedure) = crate::sqlserver_catalog::CatalogProcedure::named(name) else {
            if RPC_PROCEDURES.contains(&name) {
                return Err(crate::parser::sqlserver_not_supported(&format!(
                    "EXEC {name} in a batch"
                ))
                .at(call.procedure.position));
            }
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("Could not find stored procedure '{name}'."),
            )
            .with_sqlserver_code(2812)
            .at(call.procedure.position));
        };
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        let arguments = call
            .arguments
            .iter()
            .map(|argument| {
                let value = match &argument.value {
                    Some(expr) => Some(evaluator.eval(expr, None)?),
                    None => None,
                };
                Ok((argument.name.clone(), value))
            })
            .collect::<SqlResult<Vec<_>>>()?;
        let bound = crate::sqlserver_catalog::bind_arguments(procedure, arguments)
            .map_err(|error| error.at(call.procedure.position))?;
        self.read(|reader, _| {
            let snapshot = crate::pg_catalog::snapshot_for(reader)?;
            crate::sqlserver_catalog::run_procedure(&snapshot, procedure, bound)
        })
    }

    fn declare_table_variable(
        &mut self,
        name: &Name,
        columns: &[(Name, SqlType)],
    ) -> SqlResult<QueryResult> {
        if self.table_variables.contains_key(&name.text) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                format!(
                    "The variable name '{}' has already been declared. Variable names must be unique within a query batch or stored procedure.",
                    name.text
                ),
            )
            .with_sqlserver_code(134)
            .at(name.position));
        }
        let variable = crate::table_variables::TableVariable {
            columns: columns
                .iter()
                .map(|(column, sql_type)| OutputColumn {
                    name: column.text.clone(),
                    sql_type: Some(*sql_type),
                })
                .collect(),
            rows: Vec::new(),
        };
        Arc::make_mut(&mut self.table_variables).insert(name.text.clone(), variable);
        Ok(QueryResult::command("DECLARE"))
    }

    fn insert_procedure_results(
        &mut self,
        variable: &Name,
        columns: Option<&[Name]>,
        call: &ProcedureCall,
    ) -> SqlResult<QueryResult> {
        let Some(declared) = self.table_variables.get(&variable.text) else {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        };
        let targets: Vec<(usize, SqlType)> = match columns {
            None => declared
                .columns
                .iter()
                .enumerate()
                .map(|(at, column)| (at, column.sql_type.unwrap_or(SqlType::Text)))
                .collect(),
            Some(names) => names
                .iter()
                .map(|name| {
                    declared
                        .columns
                        .iter()
                        .position(|column| column.name == name.text)
                        .map(|at| {
                            let sql_type = declared
                                .columns
                                .get(at)
                                .and_then(|column| column.sql_type)
                                .unwrap_or(SqlType::Text);
                            (at, sql_type)
                        })
                        .ok_or_else(|| {
                            SqlError::new(
                                SqlState::UNDEFINED_COLUMN,
                                format!("Invalid column name '{}'.", name.text),
                            )
                            .with_sqlserver_code(207)
                            .at(name.position)
                        })
                })
                .collect::<SqlResult<_>>()?,
        };
        let width = declared.columns.len();
        let results = self.procedure_results(call)?;
        let mut appended: Vec<Vec<Value>> = Vec::new();
        for result in results {
            let count = result.columns.as_ref().map_or(0, Vec::len);
            if count != targets.len() {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "Column name or number of supplied values does not match table definition.",
                )
                .with_sqlserver_code(213)
                .at(variable.position));
            }
            for row in result.rows {
                let mut full = vec![Value::Null; width];
                for ((at, sql_type), value) in targets.iter().zip(row) {
                    let converted = crate::sqlserver_conversion::implicit(value, *sql_type)?;
                    if let Some(slot) = full.get_mut(*at) {
                        *slot = converted;
                    }
                }
                appended.push(full);
            }
        }
        let inserted = appended.len();
        if let Some(target) = Arc::make_mut(&mut self.table_variables).get_mut(&variable.text) {
            target.rows.extend(appended);
        }
        Ok(QueryResult::command(format!("INSERT 0 {inserted}")))
    }

    pub(super) fn sqlserver_condition_holds(&self, condition: &Expr) -> SqlResult<bool> {
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        Ok(matches!(
            evaluator.eval(condition, None)?,
            Value::Boolean(true)
        ))
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
