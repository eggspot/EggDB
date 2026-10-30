use super::sqlserver::BatchStep;
use super::{Client, InfoMessage, NamedParameter, QueryResult, Session, reborrowed_rows};
use crate::ast::{
    AssignmentTarget, BinaryOperator, DynamicSql, Expr, ExprKind, FromClause, GeneratedTable,
    InsertValue, Name, RaiseError, Select, SelectItem, Statement, TableReference, ThrowArguments,
    VariableDeclaration, VariableSource,
};
use crate::bulk_load::BulkRows;
use crate::copy::NoCopyChannel;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::store::KeyValueStore;
use crate::table_variables::{TableVariable, TableVariables};
use crate::triggers::{DELETED_TABLE, INSERTED_TABLE, StatementTriggerCall, TriggerAction};
use crate::tsql::{
    BatchContext, CaughtError, Declaration, DynamicBatch, Instruction, MAX_DYNAMIC_NESTING,
    Program, ProgramEnd, ScalarVariable,
};
use crate::types::{SqlType, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_PRINT_CHARACTERS: usize = 8000;
const NAME_RESOLUTION_ERRORS: [u16; 2] = [207, 208];
const STATEMENT_TERMINATING_ERRORS: [u16; 17] = [
    2627, 2601, 547, 515, 544, 545, 339, 2628, 8152, 3902, 3903, 1088, 8106, 8107, 4917, 1754, 3701,
];

pub(super) struct BatchScope {
    context: Arc<BatchContext>,
    tables: TableVariables,
}

#[derive(Clone, Copy)]
struct OuterStatement {
    batch_resume: Option<(usize, usize)>,
    condition_next: bool,
    condition_holds: Option<bool>,
    pending_dynamic: Option<DynamicBatch>,
    set_error: Option<u32>,
    statement_start: usize,
    statement_walks_expressions: bool,
    extra_results: Vec<QueryResult>,
}

struct Code<'p, 't> {
    text: &'t str,
    tokens: &'p [Token<'t>],
    program: &'p Program,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Top,
    Nested { in_try: bool },
}

enum DynamicEnd {
    Done,
    Stopped,
    Failed(SqlError),
}

fn undeclared(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!("Must declare the scalar variable \"{name}\"."),
    )
    .with_sqlserver_code(137)
}

fn columns_mismatch(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "Column name or number of supplied values does not match table definition.",
    )
    .with_sqlserver_code(213)
    .at(position)
}

fn ends_sqlserver_batch(error: &SqlError) -> bool {
    error.ends_sqlserver_batch().unwrap_or_else(|| {
        !error
            .vendor_code()
            .is_some_and(|code| STATEMENT_TERMINATING_ERRORS.contains(&code))
    })
}

fn assigns_only(result: &QueryResult) -> bool {
    result.columns.is_none()
        && result.info_messages.is_empty()
        && (matches!(result.tag.as_str(), "DECLARE" | "SET 1")
            || result.tag.starts_with("SELECT "))
}

fn resolves_names(error: &SqlError) -> bool {
    error
        .vendor_code()
        .is_some_and(|code| NAME_RESOLUTION_ERRORS.contains(&code))
}

fn constant(value: Value) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    }
}

fn combined(operator: BinaryOperator, left: Value, right: Value) -> SqlResult<Value> {
    let expr = Expr {
        kind: ExprKind::Binary(operator, Box::new(constant(left)), Box::new(constant(right))),
        position: 0,
    };
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    evaluator.eval(&expr, None)
}

fn scalar_select(items: Vec<Expr>, from: Option<TableReference>) -> Select {
    Select {
        distinct: false,
        items: items
            .into_iter()
            .map(|expr| SelectItem::Expr { expr, alias: None })
            .collect(),
        from: from.map(|first| FromClause {
            first,
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

fn variable_table(variable: &Name) -> TableReference {
    TableReference {
        name: variable.clone(),
        alias: Some(variable.text.clone()),
        derived: None,
        generated: Some(Box::new(GeneratedTable::Variable(variable.text.clone()))),
    }
}

fn text_value(value: Option<&Value>) -> SqlResult<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(value) => crate::sqlserver_conversion::text_of(value, None, None),
    }
}

fn declare(context: &mut BatchContext, tables: &mut HashMap<String, TableVariable>, program: &Program) {
    for declaration in &program.declarations {
        match declaration {
            Declaration::Scalar(variable) => {
                if context.variable(&variable.name).is_none() {
                    context.variables.push(variable.clone());
                }
            }
            Declaration::Table { name, columns } => {
                tables
                    .entry(name.clone())
                    .or_insert_with(|| TableVariable {
                        columns: columns.clone(),
                        rows: Vec::new(),
                    });
            }
        }
    }
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    /// The values the named parameters of the last parameterized batch
    /// ([`Session::execute_parameterized_batch`]) held when it ended, in the order given: the
    /// value each was bound to unless a statement of the batch assigned it (`SET @p = ...`), as
    /// SQL Server returns `OUTPUT` parameters of `sp_executesql`.
    pub fn parameter_values_after_batch(&self) -> &[NamedParameter] {
        &self.parameter_values
    }

    /// The prepared statement handles that batches asked to release with `EXEC sp_unprepare
    /// handle` since the last call, oldest first, for a server that keeps prepared statements
    /// per connection to forget them.
    pub fn take_unprepare_requests(&mut self) -> Vec<i32> {
        std::mem::take(&mut self.unprepare_requests)
    }

    pub(super) fn compile_batch(
        &mut self,
        text: &str,
        tokens: &[Token<'_>],
        parameters: Parameters<'_>,
        nested: bool,
    ) -> SqlResult<Program> {
        if self.sqlserver() {
            self.sync_transaction_count();
        }
        let settings = Arc::clone(&self.settings);
        let mut parser = Parser::new(text, tokens)
            .with_context(parameters, &settings)
            .in_dialect(self.dialect);
        parser.compile_batch(nested)
    }

    fn enter_batch_scope(&mut self, program: &Program, parameters: Parameters<'_>) -> BatchScope {
        let mut context = BatchContext::default();
        if let Parameters::Named(given) = parameters {
            context
                .variables
                .extend(given.iter().map(ScalarVariable::parameter));
        }
        let mut tables = HashMap::new();
        declare(&mut context, &mut tables, program);
        BatchScope {
            context: std::mem::replace(&mut self.batch_context, Arc::new(context)),
            tables: std::mem::replace(&mut self.table_variables, Arc::new(tables)),
        }
    }

    fn leave_batch_scope(&mut self, scope: BatchScope) -> Arc<BatchContext> {
        self.table_variables = scope.tables;
        std::mem::replace(&mut self.batch_context, scope.context)
    }

    fn declare_program(&mut self, program: &Program) {
        let context = Arc::make_mut(&mut self.batch_context);
        let tables = Arc::make_mut(&mut self.table_variables);
        declare(context, tables, program);
    }

    pub(super) fn batch_step(
        &self,
        outcome: Result<QueryResult, SqlError>,
        statement_start: usize,
    ) -> BatchStep {
        BatchStep {
            outcome,
            statement_start,
            in_transaction: self.in_block(),
            no_count: self.settings.sqlserver.no_count,
            format_only: self.settings.sqlserver.format_only,
        }
    }

    pub(super) fn run_compiled_batch<'t>(
        &mut self,
        text: &'t str,
        tokens: Vec<Token<'t>>,
        parameters: Parameters<'_>,
        mut bulk_rows: Option<&mut dyn BulkRows>,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        let mut tokens = tokens;
        let mut program = self
            .compile_batch(text, &tokens, parameters, false)
            .map_err(|error| self.in_dialect(text, error))?;
        self.batch_resume = Some((0, 0));
        let scope = self.enter_batch_scope(&program, parameters);
        let outcome = loop {
            let code = Code {
                text,
                tokens: &tokens,
                program: &program,
            };
            let end = self.run_program(
                code,
                parameters,
                reborrowed_rows(&mut bulk_rows),
                on_step,
                Level::Top,
            );
            match end {
                Ok(ProgramEnd::DialectChanged { at }) => {
                    match self.rest_of_batch(text, at, parameters) {
                        Ok((rest, rest_program)) => {
                            self.declare_program(&rest_program);
                            tokens = rest;
                            program = rest_program;
                        }
                        Err(error) => {
                            let error = self.in_dialect(text, error);
                            on_step(self.batch_step(Err(error), at));
                            break Ok(());
                        }
                    }
                }
                Ok(_) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let outcome = outcome.and_then(|()| self.end_uncommittable_transaction(text, on_step));
        let finished = self.leave_batch_scope(scope);
        if let Parameters::Named(given) = parameters {
            self.parameter_values = given
                .iter()
                .map(|parameter| {
                    let value = finished
                        .variable(&parameter.name.to_lowercase())
                        .map_or_else(|| parameter.value.clone(), |variable| variable.value.clone());
                    let value = match parameter.declared_type {
                        Some(sql_type) => {
                            crate::sqlserver_conversion::implicit(value.clone(), sql_type)
                                .unwrap_or(value)
                        }
                        None => value,
                    };
                    NamedParameter {
                        name: parameter.name.clone(),
                        declared_type: parameter.declared_type,
                        value,
                    }
                })
                .collect();
        }
        self.batch_resume = None;
        outcome
    }

    fn end_uncommittable_transaction(
        &mut self,
        text: &str,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<()> {
        if !(self.sqlserver() && self.settings.sqlserver.uncommittable && self.in_block()) {
            return Ok(());
        }
        self.abandon_transaction()?;
        let error = super::sqlserver::uncommittable_at_batch_end();
        on_step(self.batch_step(Err(error), text.len()));
        Ok(())
    }

    pub(super) fn run_logic_text<'t>(
        &mut self,
        text: &'t str,
        tokens: Vec<Token<'t>>,
        parameters: Parameters<'_>,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<()> {
        let mut failure: Option<SqlError> = None;
        let mut forward = |step: BatchStep| match step.outcome {
            Ok(result) => on_result(result),
            Err(error) => {
                failure = Some(error);
                false
            }
        };
        self.run_compiled_batch(text, tokens, parameters, None, &mut forward)?;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn set_caught(&mut self, caught: Option<CaughtError>) {
        if self.batch_context.caught != caught {
            Arc::make_mut(&mut self.batch_context).caught = caught;
        }
    }

    fn set_last_error(&mut self, number: u32) {
        if self.batch_context.last_error != number {
            Arc::make_mut(&mut self.batch_context).last_error = number;
        }
    }

    fn run_program(
        &mut self,
        code: Code<'_, '_>,
        parameters: Parameters<'_>,
        mut bulk_rows: Option<&mut dyn BulkRows>,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
        level: Level,
    ) -> SqlResult<ProgramEnd> {
        let mut caught: Vec<Option<CaughtError>> = vec![None; code.program.tries.len()];
        let mut at_instruction = 0usize;
        while let Some(&instruction) = code.program.instructions.get(at_instruction) {
            let (at, test) = match instruction {
                Instruction::Jump { to } => {
                    at_instruction = to;
                    continue;
                }
                Instruction::Return => return Ok(ProgramEnd::Finished),
                Instruction::Run { at } => (at, None),
                Instruction::Test {
                    condition,
                    otherwise,
                    end,
                } => (condition, Some((otherwise, end))),
            };
            let handling = code
                .program
                .handler_around(at_instruction)
                .and_then(|index| caught.get(index).cloned().flatten());
            self.set_caught(handling);
            let statement_start = code
                .tokens
                .get(at.0..)
                .unwrap_or_default()
                .iter()
                .find(|token| token.kind != TokenKind::Semicolon)
                .map_or(code.text.len(), |token| token.start);
            if test.is_some() && self.cancel.is_cancelled() {
                let error = self.in_dialect(
                    code.text,
                    SqlError::new(
                        SqlState::QUERY_CANCELED,
                        "canceling statement due to user request",
                    ),
                );
                on_step(self.batch_step(Err(error), statement_start));
                return Ok(ProgramEnd::Stopped);
            }
            let statement_dialect = self.dialect;
            let was_in_transaction = self.in_block();
            let status = self.sqlserver_status;
            self.batch_resume = Some(at);
            self.condition_next = test.is_some();
            self.condition_holds = None;
            self.pending_dynamic = None;
            self.set_error = None;
            let mut results = Vec::new();
            let mut client = Client {
                channel: &mut NoCopyChannel,
                script: false,
                bulk_rows: reborrowed_rows(&mut bulk_rows),
            };
            let flow = self.run_tokens(
                code.text,
                code.tokens,
                parameters,
                &mut |result| {
                    results.push(result);
                    false
                },
                &mut client,
                None,
            );
            self.condition_next = false;
            let in_transaction = self.in_block();
            if test.is_some() {
                self.sqlserver_status = status;
            }
            let (failure, lower_level) = match flow {
                Ok(_) => {
                    if let Some((otherwise, _)) = test {
                        self.set_last_error(0);
                        at_instruction = if self.condition_holds.take() == Some(true) {
                            at_instruction + 1
                        } else {
                            otherwise
                        };
                        continue;
                    }
                    let raised = self.set_error.take().unwrap_or(0);
                    self.set_last_error(raised);
                    match self.pending_dynamic.take() {
                        None => {
                            for result in results {
                                if assigns_only(&result) {
                                    continue;
                                }
                                if !on_step(self.batch_step(Ok(result), statement_start)) {
                                    return Ok(ProgramEnd::Stopped);
                                }
                            }
                            if self.dialect != statement_dialect {
                                let (next, _) = self.batch_resume.unwrap_or((0, 0));
                                let at = code
                                    .tokens
                                    .get(next)
                                    .map_or(code.text.len(), |token| token.start);
                                return Ok(ProgramEnd::DialectChanged { at });
                            }
                            at_instruction += 1;
                            continue;
                        }
                        Some(dynamic) => {
                            let in_try = code.program.try_around(at_instruction).is_some()
                                || level == Level::Nested { in_try: true };
                            match self.run_dynamic(dynamic, statement_start, in_try, on_step)? {
                                DynamicEnd::Done => {
                                    at_instruction += 1;
                                    continue;
                                }
                                DynamicEnd::Stopped => return Ok(ProgramEnd::Stopped),
                                DynamicEnd::Failed(error) => (error, true),
                            }
                        }
                    }
                }
                Err(error) => (self.in_dialect(code.text, error), false),
            };
            self.set_last_error(failure.sqlserver_number().unwrap_or(50_000));
            let xact_abort = self.xact_abort_applies(&failure);
            if self.sqlserver()
                && (lower_level || !resolves_names(&failure))
                && let Some(index) = code.program.try_around(at_instruction)
                && let Some(block) = code.program.tries.get(index)
            {
                if xact_abort && self.in_block() && !self.settings.sqlserver.uncommittable {
                    Arc::make_mut(&mut self.settings).sqlserver.uncommittable = true;
                }
                let line = crate::tsql::line_of(
                    code.text,
                    failure.position().unwrap_or(statement_start),
                );
                if let Some(slot) = caught.get_mut(index) {
                    *slot = Some(CaughtError::of(&failure, line));
                }
                at_instruction = block.handler.start;
                continue;
            }
            let ends_batch = self.dialect != Dialect::SqlServer
                || ends_sqlserver_batch(&failure)
                || (was_in_transaction && !in_transaction)
                || xact_abort;
            match level {
                Level::Nested { in_try: true } => return Ok(ProgramEnd::Escaped(failure)),
                Level::Nested { in_try: false } if ends_batch => {
                    return Ok(ProgramEnd::Escaped(failure));
                }
                _ => {}
            }
            if xact_abort && self.in_block() {
                self.abandon_transaction()?;
            }
            if !on_step(self.batch_step(Err(failure), statement_start)) || ends_batch {
                return Ok(ProgramEnd::Stopped);
            }
            at_instruction = match test {
                Some((_, end)) => end,
                None => at_instruction + 1,
            };
        }
        Ok(ProgramEnd::Finished)
    }

    fn run_dynamic(
        &mut self,
        dynamic: DynamicBatch,
        statement_start: usize,
        in_try: bool,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<DynamicEnd> {
        if self.dynamic_depth >= MAX_DYNAMIC_NESTING {
            return Ok(DynamicEnd::Failed(crate::tsql::nesting_too_deep()));
        }
        let DynamicBatch {
            text,
            parameters: given,
            outputs,
        } = dynamic;
        let parameters = Parameters::Named(&given);
        let outer_start = self.statement_start;
        self.statement_start = 0;
        let tokens = match tokenize_in(&text, self.dialect) {
            Ok(tokens) => tokens,
            Err(error) => {
                let error = self.in_dialect(&text, error).without_position();
                self.statement_start = outer_start;
                return Ok(DynamicEnd::Failed(error));
            }
        };
        let program = match self.compile_batch(&text, &tokens, parameters, true) {
            Ok(program) => program,
            Err(error) => {
                let error = self.in_dialect(&text, error).without_position();
                self.statement_start = outer_start;
                return Ok(DynamicEnd::Failed(error));
            }
        };
        let scope = self.enter_batch_scope(&program, parameters);
        self.dynamic_depth += 1;
        let mut forward = |mut step: BatchStep| {
            step.statement_start = statement_start;
            step.outcome = match step.outcome {
                Err(error) => Err(error.without_position()),
                ok => ok,
            };
            on_step(step)
        };
        let code = Code {
            text: &text,
            tokens: &tokens,
            program: &program,
        };
        let end = self.run_program(
            code,
            parameters,
            None,
            &mut forward,
            Level::Nested { in_try },
        );
        self.dynamic_depth -= 1;
        let finished = self.leave_batch_scope(scope);
        self.statement_start = outer_start;
        for (parameter, variable) in &outputs {
            if let Some(value) = finished
                .variable(parameter)
                .map(|found| found.value.clone())
                && let Err(error) = self.assign_variable(variable, value)
            {
                return Ok(DynamicEnd::Failed(error));
            }
        }
        Ok(match end? {
            ProgramEnd::Stopped => DynamicEnd::Stopped,
            ProgramEnd::Escaped(error) => DynamicEnd::Failed(error.without_position()),
            ProgramEnd::Finished | ProgramEnd::DialectChanged { .. } => DynamicEnd::Done,
        })
    }

    fn leave_statement_for_trigger(&mut self) -> OuterStatement {
        OuterStatement {
            batch_resume: self.batch_resume.take(),
            condition_next: std::mem::take(&mut self.condition_next),
            condition_holds: self.condition_holds.take(),
            pending_dynamic: self.pending_dynamic.take(),
            set_error: self.set_error.take(),
            statement_start: self.statement_start,
            statement_walks_expressions: self.statement_walks_expressions,
            extra_results: std::mem::take(&mut self.extra_results),
        }
    }

    fn return_to_statement(&mut self, outer: OuterStatement) {
        self.batch_resume = outer.batch_resume;
        self.condition_next = outer.condition_next;
        self.condition_holds = outer.condition_holds;
        self.pending_dynamic = outer.pending_dynamic;
        self.set_error = outer.set_error;
        self.statement_start = outer.statement_start;
        self.statement_walks_expressions = outer.statement_walks_expressions;
        self.extra_results = outer.extra_results;
    }

    pub(super) fn run_tsql_trigger(&mut self, call: StatementTriggerCall) -> SqlResult<()> {
        let TriggerAction::Batch(text) = &call.trigger.action else {
            return Ok(());
        };
        let _active = crate::triggers::enter_active(&call.table, &call.trigger)?;
        let tokens = tokenize_in(text, Dialect::SqlServer)?;
        let outer = self.leave_statement_for_trigger();
        let outcome = self.in_trigger_dialect(Dialect::SqlServer, |session| {
            session.run_trigger_program(text, &tokens, &call)
        });
        self.return_to_statement(outer);
        let results = outcome?;
        self.extra_results.extend(results);
        Ok(())
    }

    fn run_trigger_program(
        &mut self,
        text: &str,
        tokens: &[Token<'_>],
        call: &StatementTriggerCall,
    ) -> SqlResult<Vec<QueryResult>> {
        let program = self.compile_batch(text, tokens, Parameters::Absent, true)?;
        let scope = self.enter_batch_scope(&program, Parameters::Absent);
        {
            let context = Arc::make_mut(&mut self.batch_context);
            context.transition_tables = vec![INSERTED_TABLE.to_owned(), DELETED_TABLE.to_owned()];
            context.updated_columns = Some(call.updated.clone());
        }
        {
            let columns = call.columns.as_ref().clone();
            let tables = Arc::make_mut(&mut self.table_variables);
            tables.insert(
                INSERTED_TABLE.to_owned(),
                TableVariable {
                    columns: columns.clone(),
                    rows: call.new_rows.clone(),
                },
            );
            tables.insert(
                DELETED_TABLE.to_owned(),
                TableVariable {
                    columns,
                    rows: call.old_rows.clone(),
                },
            );
        }
        let mut failure: Option<SqlError> = None;
        let mut results = Vec::new();
        let mut forward = |step: BatchStep| match step.outcome {
            Ok(result) => {
                if result.columns.is_some() || !result.info_messages.is_empty() {
                    results.push(result);
                }
                true
            }
            Err(error) => {
                failure = Some(error);
                false
            }
        };
        let code = Code {
            text,
            tokens,
            program: &program,
        };
        let end = self.run_program(
            code,
            Parameters::Absent,
            None,
            &mut forward,
            Level::Nested { in_try: false },
        );
        self.leave_batch_scope(scope);
        if let ProgramEnd::Escaped(error) = end? {
            return Err(error);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(results),
        }
    }

    fn run_select(&mut self, select: &mut Select) -> SqlResult<QueryResult> {
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: self.statement_walks_expressions,
        };
        self.read(|reader, tables| {
            binding.select(reader, tables, select)?;
            crate::execute::select(reader, tables, select, false)
        })
    }

    fn run_query_statement(&mut self, statement: &mut Statement) -> SqlResult<QueryResult> {
        match statement {
            Statement::Select(select) => self.run_select(select),
            Statement::Query(query) => {
                let settings = Arc::clone(&self.settings);
                let binding = Binding {
                    settings: &settings,
                    walk_expressions: self.statement_walks_expressions,
                };
                self.read(|reader, tables| {
                    binding.query(reader, tables, query)?;
                    crate::execute::query(reader, tables, query, false)
                })
            }
            _ => Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a statement other than a query reached the query runner",
            )),
        }
    }

    fn evaluate_row(&mut self, items: Vec<Expr>) -> SqlResult<Vec<Value>> {
        let width = items.len();
        let mut select = scalar_select(items, None);
        let result = self.run_select(&mut select)?;
        Ok(result
            .rows
            .into_iter()
            .next()
            .unwrap_or_else(|| vec![Value::Null; width]))
    }

    fn evaluate(&mut self, expr: &Expr) -> SqlResult<Value> {
        Ok(self
            .evaluate_row(vec![expr.clone()])?
            .into_iter()
            .next()
            .unwrap_or(Value::Null))
    }

    pub(super) fn assign_variable(&mut self, name: &str, value: Value) -> SqlResult<()> {
        let Some(variable) = self.batch_context.variable(name) else {
            return Err(undeclared(name));
        };
        let assigned = variable.assigned(value)?;
        if let Some(variable) = Arc::make_mut(&mut self.batch_context).variable_mut(name) {
            variable.value = assigned;
        }
        Ok(())
    }

    pub(super) fn run_declare(
        &mut self,
        declarations: &[VariableDeclaration],
    ) -> SqlResult<QueryResult> {
        let initialized: Vec<&VariableDeclaration> = declarations
            .iter()
            .filter(|declaration| declaration.initial.is_some())
            .collect();
        if !initialized.is_empty() {
            let values = self.evaluate_row(
                initialized
                    .iter()
                    .filter_map(|declaration| declaration.initial.clone())
                    .collect(),
            )?;
            for (declaration, value) in initialized.iter().zip(values) {
                self.assign_variable(&declaration.name.text, value)
                    .map_err(|error| error.at_if_unplaced(declaration.name.position))?;
            }
        }
        Ok(QueryResult::command("DECLARE"))
    }

    pub(super) fn run_set_variable(&mut self, variable: &Name, value: &Expr) -> SqlResult<QueryResult> {
        let value = self.evaluate(value)?;
        self.assign_variable(&variable.text, value)
            .map_err(|error| error.at_if_unplaced(variable.position))?;
        Ok(QueryResult::command("SET 1"))
    }

    pub(super) fn run_select_assignment(
        &mut self,
        targets: &[AssignmentTarget],
        query: &Statement,
    ) -> SqlResult<QueryResult> {
        let mut query = query.clone();
        let result = self.run_query_statement(&mut query)?;
        let mut assigned = 0usize;
        for row in result.rows {
            assigned += 1;
            for (target, value) in targets.iter().zip(row) {
                let value = match target.operator {
                    None => value,
                    Some(operator) => {
                        let current = self
                            .batch_context
                            .variable(&target.variable.text)
                            .map_or(Value::Null, |variable| variable.value.clone());
                        combined(operator, current, value)?
                    }
                };
                self.assign_variable(&target.variable.text, value)
                    .map_err(|error| error.at_if_unplaced(target.variable.position))?;
            }
        }
        Ok(QueryResult::command(format!("SELECT {assigned}")))
    }

    pub(super) fn run_condition(&mut self, condition: &Expr) -> SqlResult<QueryResult> {
        match self.evaluate(condition)? {
            Value::Boolean(holds) => self.condition_holds = Some(holds),
            Value::Null => self.condition_holds = Some(false),
            _ => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    "An expression of non-boolean type specified in a context where a condition is expected.",
                )
                .with_sqlserver_code(4145)
                .at(condition.position));
            }
        }
        Ok(QueryResult::command("IF"))
    }

    pub(super) fn run_print(&mut self, message: &Expr) -> SqlResult<QueryResult> {
        let value = self.evaluate(message)?;
        let text: String = text_value(Some(&value))?
            .chars()
            .take(MAX_PRINT_CHARACTERS)
            .collect();
        let mut result = QueryResult::command("PRINT").with_notice(text.clone());
        result.info_messages.push(InfoMessage {
            number: 0,
            severity: 0,
            state: 1,
            text,
        });
        Ok(result)
    }

    pub(super) fn run_raiserror(&mut self, raise: &RaiseError) -> SqlResult<QueryResult> {
        let mut items = vec![
            raise.message.clone(),
            raise.severity.clone(),
            raise.state.clone(),
        ];
        items.extend(raise.arguments.iter().cloned());
        let values = self.evaluate_row(items)?;
        let null = Value::Null;
        let message = crate::tsql::raised_message(
            values.first().unwrap_or(&null),
            values.get(1).unwrap_or(&null),
            values.get(2).unwrap_or(&null),
            values.get(3..).unwrap_or_default(),
        )
        .map_err(|error| error.at(raise.position))?;
        if message.severity > 10 {
            return Err(SqlError::raised_in_sqlserver(
                message.number,
                message.severity,
                message.state,
                message.text,
                false,
            )
            .at(raise.position));
        }
        if raise.set_error {
            self.set_error = Some(message.number);
        }
        let mut result = QueryResult::command("RAISERROR").with_notice(message.text.clone());
        result.info_messages.push(InfoMessage {
            number: message.number,
            severity: message.severity,
            state: message.state,
            text: message.text,
        });
        Ok(result)
    }

    pub(super) fn run_throw(&mut self, arguments: Option<&ThrowArguments>) -> SqlResult<QueryResult> {
        let Some(arguments) = arguments else {
            return Err(match &self.batch_context.caught {
                Some(caught) => caught.rethrown(),
                None => SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "To rethrow an error, a THROW statement must be used inside a CATCH block. Insert the THROW statement inside a CATCH block, or add error parameters to the THROW statement.",
                )
                .with_sqlserver_code(10704),
            });
        };
        let values = self.evaluate_row(vec![
            arguments.number.clone(),
            arguments.message.clone(),
            arguments.state.clone(),
        ])?;
        let null = Value::Null;
        let error = crate::tsql::thrown(
            values.first().unwrap_or(&null),
            values.get(1).unwrap_or(&null),
            values.get(2).unwrap_or(&null),
        )?;
        Err(error)
    }

    pub(super) fn prepare_dynamic(&mut self, dynamic: &DynamicSql) -> SqlResult<QueryResult> {
        let mut items = vec![dynamic.statement.clone()];
        items.extend(dynamic.declarations.iter().cloned());
        let first_argument = items.len();
        items.extend(
            dynamic
                .arguments
                .iter()
                .filter_map(|argument| argument.value.clone()),
        );
        let values = self.evaluate_row(items)?;
        let text = text_value(values.first())?;
        let declarations_text = if dynamic.declarations.is_some() {
            text_value(values.get(1))?
        } else {
            String::new()
        };
        let declarations = crate::parser::sqlserver_parameter_declarations(&declarations_text)
            .map_err(|error| error.at_if_unplaced(dynamic.position))?;
        let mut supplied = values.into_iter().skip(first_argument);
        let mut bound: Vec<Option<Value>> = vec![None; declarations.len()];
        let mut outputs = Vec::new();
        let mut next_position = 0usize;
        for argument in &dynamic.arguments {
            let value = match argument.value {
                Some(_) => supplied.next().unwrap_or(Value::Null),
                None => Value::Null,
            };
            let index = match &argument.name {
                None => {
                    let index = next_position;
                    next_position += 1;
                    if index >= declarations.len() {
                        return Err(SqlError::new(
                            SqlState::PROGRAM_LIMIT_EXCEEDED,
                            "Procedure or function sp_executesql has too many arguments specified.",
                        )
                        .with_sqlserver_code(8144)
                        .at(dynamic.position));
                    }
                    index
                }
                Some(name) => declarations
                    .iter()
                    .position(|declaration| declaration.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::UNDEFINED_PARAMETER,
                            format!("{name} is not a parameter for procedure sp_executesql."),
                        )
                        .with_sqlserver_code(8145)
                        .at(dynamic.position)
                    })?,
            };
            if let (Some(variable), Some(declaration)) =
                (&argument.output_variable, declarations.get(index))
            {
                if !declaration.output {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!(
                            "The formal parameter \"{}\" was not declared as an OUTPUT parameter, but the actual parameter passed in requested output.",
                            declaration.name
                        ),
                    )
                    .with_sqlserver_code(8162)
                    .at(dynamic.position));
                }
                outputs.push((declaration.name.clone(), variable.clone()));
            }
            if let Some(slot) = bound.get_mut(index) {
                *slot = Some(value);
            }
        }
        let parameters = declarations
            .iter()
            .zip(bound)
            .map(|(declaration, value)| match value {
                Some(value) => Ok(NamedParameter {
                    name: declaration.name.clone(),
                    declared_type: Some(declaration.sql_type),
                    value,
                }),
                None => Err(SqlError::new(
                    SqlState::UNDEFINED_PARAMETER,
                    format!(
                        "The parameterized query '({declarations_text}){text}' expects the parameter '{}', which was not supplied.",
                        declaration.name
                    ),
                )
                .with_sqlserver_code(8178)
                .at(dynamic.position)),
            })
            .collect::<SqlResult<Vec<_>>>()?;
        self.pending_dynamic = Some(DynamicBatch {
            text,
            parameters,
            outputs,
        });
        Ok(QueryResult::command("EXECUTE"))
    }

    pub(super) fn declare_table_variable(
        &mut self,
        name: &Name,
        columns: &[(Name, SqlType)],
    ) -> SqlResult<QueryResult> {
        if !self.table_variables.contains_key(&name.text) {
            let variable = TableVariable {
                columns: columns
                    .iter()
                    .map(|(column, sql_type)| super::OutputColumn {
                        name: column.text.clone(),
                        sql_type: Some(*sql_type),
                    })
                    .collect(),
                rows: Vec::new(),
            };
            self.writable_table_variables().insert(name.text.clone(), variable);
        }
        Ok(QueryResult::command("DECLARE"))
    }

    fn writable_table_variables(&mut self) -> &mut HashMap<String, TableVariable> {
        crate::table_variables::release_entered();
        Arc::make_mut(&mut self.table_variables)
    }

    fn variable_columns(
        &self,
        variable: &Name,
        columns: Option<&[Name]>,
    ) -> SqlResult<(Vec<(usize, SqlType)>, usize)> {
        let Some(declared) = self.table_variables.get(&variable.text) else {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        };
        let targets = match columns {
            None => declared
                .columns
                .iter()
                .enumerate()
                .map(|(at, column)| (at, column.sql_type.unwrap_or(SqlType::Text)))
                .collect(),
            Some(names) => names
                .iter()
                .map(|name| self.variable_column(variable, name))
                .collect::<SqlResult<_>>()?,
        };
        Ok((targets, declared.columns.len()))
    }

    fn variable_column(&self, variable: &Name, column: &Name) -> SqlResult<(usize, SqlType)> {
        self.table_variables
            .get(&variable.text)
            .and_then(|declared| {
                declared
                    .columns
                    .iter()
                    .position(|found| found.name == column.text)
                    .map(|at| {
                        let sql_type = declared
                            .columns
                            .get(at)
                            .and_then(|found| found.sql_type)
                            .unwrap_or(SqlType::Text);
                        (at, sql_type)
                    })
            })
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("Invalid column name '{}'.", column.text),
                )
                .with_sqlserver_code(207)
                .at(column.position)
            })
    }

    pub(super) fn insert_into_variable(
        &mut self,
        variable: &Name,
        columns: Option<&[Name]>,
        source: &VariableSource,
    ) -> SqlResult<QueryResult> {
        let (targets, width) = self.variable_columns(variable, columns)?;
        let rows: Vec<Vec<Value>> = match source {
            VariableSource::Execute(call) => {
                let mut rows = Vec::new();
                for result in self.procedure_results(call)? {
                    if result.columns.as_ref().map_or(0, Vec::len) != targets.len() {
                        return Err(columns_mismatch(variable.position));
                    }
                    rows.extend(result.rows);
                }
                rows
            }
            VariableSource::Query(statement) => {
                let mut statement = (**statement).clone();
                let result = self.run_query_statement(&mut statement)?;
                if result.columns.as_ref().map_or(0, Vec::len) != targets.len() {
                    return Err(columns_mismatch(variable.position));
                }
                result.rows
            }
            VariableSource::Values(rows) => {
                let mut evaluated = Vec::with_capacity(rows.len());
                for row in rows {
                    if row.len() != targets.len() {
                        return Err(columns_mismatch(variable.position));
                    }
                    let items = row
                        .iter()
                        .map(|value| match value {
                            InsertValue::Expr(expr) => expr.clone(),
                            InsertValue::Default => constant(Value::Null),
                        })
                        .collect();
                    evaluated.push(self.evaluate_row(items)?);
                }
                evaluated
            }
        };
        let mut appended: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut full = vec![Value::Null; width];
            for ((at, sql_type), value) in targets.iter().zip(row) {
                let converted = crate::sqlserver_conversion::implicit(value, *sql_type)?;
                if let Some(slot) = full.get_mut(*at) {
                    *slot = converted;
                }
            }
            appended.push(full);
        }
        let inserted = appended.len();
        if let Some(target) = self.writable_table_variables().get_mut(&variable.text) {
            target.rows.extend(appended);
        }
        Ok(QueryResult::command(format!("INSERT 0 {inserted}")))
    }

    fn matching_rows(
        &mut self,
        variable: &Name,
        filter: Option<&Expr>,
        values: &[Expr],
    ) -> SqlResult<Vec<(usize, Vec<Value>)>> {
        let mut items: Vec<Expr> = filter.into_iter().cloned().collect();
        items.extend(values.iter().cloned());
        if items.is_empty() {
            let count = self
                .table_variables
                .get(&variable.text)
                .map_or(0, |declared| declared.rows.len());
            return Ok((0..count).map(|index| (index, Vec::new())).collect());
        }
        let mut select = scalar_select(items, Some(variable_table(variable)));
        let result = self.run_select(&mut select)?;
        Ok(result
            .rows
            .into_iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let mut row = row.into_iter();
                if filter.is_some() && !matches!(row.next(), Some(Value::Boolean(true))) {
                    return None;
                }
                Some((index, row.collect()))
            })
            .collect())
    }

    pub(super) fn update_variable(
        &mut self,
        variable: &Name,
        assignments: &[(Name, Expr)],
        filter: Option<&Expr>,
    ) -> SqlResult<QueryResult> {
        if !self.table_variables.contains_key(&variable.text) {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        }
        let columns = assignments
            .iter()
            .map(|(column, _)| self.variable_column(variable, column))
            .collect::<SqlResult<Vec<_>>>()?;
        let values: Vec<Expr> = assignments.iter().map(|(_, value)| value.clone()).collect();
        let matching = self.matching_rows(variable, filter, &values)?;
        let mut changes: Vec<(usize, Vec<(usize, Value)>)> = Vec::with_capacity(matching.len());
        for (index, row) in matching {
            let mut changed = Vec::with_capacity(columns.len());
            for ((at, sql_type), value) in columns.iter().zip(row) {
                changed.push((*at, crate::sqlserver_conversion::implicit(value, *sql_type)?));
            }
            changes.push((index, changed));
        }
        let updated = changes.len();
        if let Some(target) = self.writable_table_variables().get_mut(&variable.text) {
            for (index, changed) in changes {
                if let Some(row) = target.rows.get_mut(index) {
                    for (at, value) in changed {
                        if let Some(slot) = row.get_mut(at) {
                            *slot = value;
                        }
                    }
                }
            }
        }
        Ok(QueryResult::command(format!("UPDATE {updated}")))
    }

    pub(super) fn delete_from_variable(
        &mut self,
        variable: &Name,
        filter: Option<&Expr>,
    ) -> SqlResult<QueryResult> {
        if !self.table_variables.contains_key(&variable.text) {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        }
        let matching = self.matching_rows(variable, filter, &[])?;
        let deleted = matching.len();
        if let Some(target) = self.writable_table_variables().get_mut(&variable.text) {
            let mut doomed = matching.into_iter().map(|(index, _)| index).peekable();
            let mut index = 0usize;
            target.rows.retain(|_| {
                let keep = doomed.peek() != Some(&index);
                if !keep {
                    doomed.next();
                }
                index += 1;
                keep
            });
        }
        Ok(QueryResult::command(format!("DELETE {deleted}")))
    }
}
