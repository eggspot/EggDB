use super::{Code, DynamicEnd, Level};
use crate::ast::{Expr, ProcedureArgument, ProcedureCall};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, tokenize_in};
use crate::parser::{Parameters, Parser};
use crate::routines::{RoutineDefinition, RoutineKind, RoutineLanguage};
use crate::schemas::base_name;
use crate::session::sqlserver::BatchStep;
use crate::session::{QueryResult, Session};
use crate::store::KeyValueStore;
use crate::tsql::{
    MAX_DYNAMIC_NESTING, ProcedureRun, Program, ProgramEnd, ScalarVariable, TsqlReturns,
};
use crate::types::{SqlType, Value};
use std::sync::Arc;

fn function_object(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!(
            "The request for procedure '{name}' failed because '{name}' is a function object."
        ),
    )
    .with_sqlserver_code(2809)
    .at(position)
}

fn too_many_arguments(procedure: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("Procedure or function {procedure} has too many arguments specified."),
    )
    .with_sqlserver_code(8144)
    .at(position)
}

fn not_a_parameter(name: &str, procedure: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!("{name} is not a parameter for procedure {procedure}."),
    )
    .with_sqlserver_code(8145)
    .at(position)
}

fn supplied_twice(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("Parameter '{name}' was supplied multiple times."),
    )
    .with_sqlserver_code(8143)
    .at(position)
}

fn not_declared_output(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "The formal parameter \"{name}\" was not declared as an OUTPUT parameter, but the actual parameter passed in requested output."
        ),
    )
    .with_sqlserver_code(8162)
    .at(position)
}

fn not_supplied(procedure: &str, name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!(
            "Procedure or function '{procedure}' expects parameter '{name}', which was not supplied."
        ),
    )
    .with_sqlserver_code(201)
    .at(position)
}

fn mismatching_transaction_count(before: u32, after: u32) -> SqlError {
    SqlError::raised_in_sqlserver(
        266,
        16,
        2,
        format!(
            "Transaction count after EXECUTE indicates a mismatching number of BEGIN and COMMIT statements. Previous count = {before}, current count = {after}."
        ),
        false,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SetOptions {
    no_count: bool,
    implicit_transactions: bool,
    format_only: bool,
    xact_abort: bool,
}

fn return_status(returned: Option<Value>) -> Value {
    match returned.map(|value| crate::sqlserver_conversion::implicit(value, SqlType::Integer)) {
        Some(Ok(Value::Integer(status))) => Value::Integer(status),
        _ => Value::Integer(0),
    }
}

fn parameter_index(
    definition: &RoutineDefinition,
    ordinal: usize,
    argument: &ProcedureArgument,
    position: usize,
) -> SqlResult<usize> {
    let shown = base_name(&definition.name);
    match &argument.name {
        None if ordinal < definition.parameters.len() => Ok(ordinal),
        None => Err(too_many_arguments(shown, position)),
        Some(name) => definition
            .parameters
            .iter()
            .position(|parameter| {
                parameter
                    .name
                    .as_deref()
                    .is_some_and(|known| known.eq_ignore_ascii_case(name))
            })
            .ok_or_else(|| not_a_parameter(name, shown, position)),
    }
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(in crate::session) fn tsql_routine(
        &mut self,
        routine: &str,
    ) -> SqlResult<Option<Arc<RoutineDefinition>>> {
        let found = self.read(|reader, tables| crate::routines::resolve(reader, tables, routine))?;
        Ok(found.and_then(|named| {
            named
                .routines
                .iter()
                .find(|definition| definition.language == RoutineLanguage::Tsql)
                .cloned()
        }))
    }

    pub(in crate::session) fn run_execute(&mut self, call: &ProcedureCall) -> SqlResult<QueryResult> {
        let system = crate::session::sqlserver::is_system_procedure(&call.procedure.text);
        if !system
            && self.batch_resume.is_some()
            && let Some(procedure) = self.user_procedure_run(call)?
        {
            self.pending_procedure = Some(procedure);
            return Ok(QueryResult::command("EXECUTE"));
        }
        let mut results = self.procedure_results(call)?.into_iter();
        if system && let Some(variable) = &call.status_variable {
            self.assign_variable(&variable.text, Value::Integer(0))
                .map_err(|error| error.at_if_unplaced(variable.position))?;
        }
        let first = results
            .next()
            .unwrap_or_else(|| QueryResult::command("EXECUTE"));
        self.extra_results.extend(results);
        Ok(first)
    }

    pub(in crate::session) fn user_procedure_run(
        &mut self,
        call: &ProcedureCall,
    ) -> SqlResult<Option<ProcedureRun>> {
        let Some(definition) = self.tsql_routine(&call.routine)? else {
            return Ok(None);
        };
        let position = call.procedure.position;
        if definition.kind != RoutineKind::Procedure {
            return Err(function_object(base_name(&definition.name), position));
        }
        let mut chosen: Vec<Option<&ProcedureArgument>> = vec![None; definition.parameters.len()];
        let mut outputs = Vec::new();
        for (ordinal, argument) in call.arguments.iter().enumerate() {
            let index = parameter_index(&definition, ordinal, argument, position)?;
            let (Some(slot), Some(parameter)) =
                (chosen.get_mut(index), definition.parameters.get(index))
            else {
                return Err(too_many_arguments(base_name(&definition.name), position));
            };
            let name = parameter.name.clone().unwrap_or_default();
            if slot.is_some() {
                return Err(supplied_twice(&name, position));
            }
            *slot = Some(argument);
            if let Some(variable) = &argument.output_variable {
                if !parameter.mode.is_output() {
                    return Err(not_declared_output(&name, position));
                }
                outputs.push((name, variable.clone()));
            }
        }
        let expressions: Vec<Expr> = chosen
            .iter()
            .flatten()
            .filter_map(|argument| argument.value.clone())
            .collect();
        let mut values = if expressions.is_empty() {
            Vec::new()
        } else {
            self.evaluate_row(expressions)?
        }
        .into_iter();
        let mut arguments = Vec::with_capacity(definition.parameters.len());
        for (parameter, argument) in definition.parameters.iter().zip(chosen.iter().copied()) {
            let Some(name) = parameter.name.clone() else {
                continue;
            };
            let value = match argument.and_then(|argument| argument.value.as_ref()) {
                Some(_) => values.next().unwrap_or(Value::Null),
                None => match &parameter.default {
                    Some(text) => self.tsql_default(text)?,
                    None => {
                        return Err(not_supplied(base_name(&definition.name), &name, position));
                    }
                },
            };
            let variable = ScalarVariable {
                name: name.clone(),
                declared: parameter.declared,
                sql_type: Some(parameter.sql_type),
                value: Value::Null,
            };
            let value = variable
                .assigned(value)
                .map_err(|error| error.at_if_unplaced(position))?;
            arguments.push((name, value));
        }
        Ok(Some(ProcedureRun {
            definition,
            arguments,
            outputs,
            status_variable: call
                .status_variable
                .as_ref()
                .map(|variable| variable.text.clone()),
        }))
    }

    fn tsql_default(&mut self, text: &str) -> SqlResult<Value> {
        let query = format!("SELECT {text}");
        let tokens = tokenize_in(&query, Dialect::SqlServer)?;
        let settings = Arc::clone(&self.settings);
        let mut statement = Parser::new(&query, &tokens)
            .with_context(Parameters::Absent, &settings)
            .in_dialect(Dialect::SqlServer)
            .statement()?;
        let result = self.run_query_statement(&mut statement)?;
        Ok(result
            .rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next())
            .unwrap_or(Value::Null))
    }

    fn compile_procedure(
        &self,
        body: &str,
        tokens: &[Token<'_>],
        definition: &RoutineDefinition,
    ) -> SqlResult<Program> {
        let settings = Arc::clone(&self.settings);
        let returns = definition
            .tsql
            .as_ref()
            .map_or(TsqlReturns::Status, |tsql| tsql.returns.clone());
        let mut parser = Parser::new(body, tokens)
            .with_context(Parameters::Absent, &settings)
            .in_dialect(Dialect::SqlServer);
        parser.compile_routine_body(&definition.parameters, &returns)
    }

    pub(in crate::session) fn procedure_result_sets(
        &mut self,
        procedure: ProcedureRun,
    ) -> SqlResult<Vec<QueryResult>> {
        let outer = self.leave_statement_for_trigger();
        let statement_start = outer.statement_start;
        let mut results = Vec::new();
        let mut failure: Option<SqlError> = None;
        let mut collect = |step: BatchStep| match step.outcome {
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
        let end = self.run_procedure(procedure, statement_start, false, &mut collect);
        self.return_to_statement(outer);
        if let DynamicEnd::Failed(error) = end? {
            return Err(error);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(results),
        }
    }

    fn set_options(&self) -> SetOptions {
        let sqlserver = &self.settings.sqlserver;
        SetOptions {
            no_count: sqlserver.no_count,
            implicit_transactions: sqlserver.implicit_transactions,
            format_only: sqlserver.format_only,
            xact_abort: sqlserver.xact_abort,
        }
    }

    fn restore_set_options(&mut self, options: SetOptions) {
        if self.set_options() == options {
            return;
        }
        let sqlserver = &mut Arc::make_mut(&mut self.settings).sqlserver;
        sqlserver.no_count = options.no_count;
        sqlserver.implicit_transactions = options.implicit_transactions;
        sqlserver.format_only = options.format_only;
        sqlserver.xact_abort = options.xact_abort;
    }

    pub(super) fn run_procedure(
        &mut self,
        procedure: ProcedureRun,
        statement_start: usize,
        in_try: bool,
        on_step: &mut dyn FnMut(BatchStep) -> bool,
    ) -> SqlResult<DynamicEnd> {
        if self.dynamic_depth >= MAX_DYNAMIC_NESTING {
            return Ok(DynamicEnd::Failed(crate::tsql::nesting_too_deep()));
        }
        let ProcedureRun {
            definition,
            arguments,
            outputs,
            status_variable,
        } = procedure;
        let body = definition.body.as_str();
        let tokens = match tokenize_in(body, Dialect::SqlServer) {
            Ok(tokens) => tokens,
            Err(error) => return Ok(DynamicEnd::Failed(error.without_position())),
        };
        let program = match self.compile_procedure(body, &tokens, &definition) {
            Ok(program) => program,
            Err(error) => return Ok(DynamicEnd::Failed(error.without_position())),
        };
        self.sync_transaction_count();
        let count_before = self.settings.sqlserver.transaction_count;
        let outer_start = std::mem::replace(&mut self.statement_start, 0);
        let outer_returned = self.returned_value.take();
        let outer_options = self.set_options();
        let scope = self.enter_batch_scope(&program, Parameters::Absent);
        self.dynamic_depth += 1;
        {
            let context = Arc::make_mut(&mut self.batch_context);
            context.nest_level = self.dynamic_depth;
            for (name, value) in arguments {
                if let Some(variable) = context.variable_mut(&name) {
                    variable.value = value;
                }
            }
        }
        let mut forward = |mut step: BatchStep| {
            step.statement_start = statement_start;
            step.outcome = match step.outcome {
                Err(error) => Err(error.without_position()),
                ok => ok,
            };
            on_step(step)
        };
        let code = Code {
            text: body,
            tokens: &tokens,
            program: &program,
        };
        let end = self.run_program(
            code,
            Parameters::Absent,
            None,
            &mut forward,
            Level::Nested { in_try },
        );
        self.dynamic_depth -= 1;
        let finished = self.leave_batch_scope(scope);
        self.statement_start = outer_start;
        self.restore_set_options(outer_options);
        let returned = std::mem::replace(&mut self.returned_value, outer_returned);
        match end? {
            ProgramEnd::Stopped => return Ok(DynamicEnd::Stopped),
            ProgramEnd::Escaped(error) => return Ok(DynamicEnd::Failed(error.without_position())),
            ProgramEnd::Finished | ProgramEnd::DialectChanged { .. } => {}
        }
        for (parameter, variable) in &outputs {
            let value = finished
                .variable(parameter)
                .map_or(Value::Null, |found| found.value.clone());
            if let Err(error) = self.assign_variable(variable, value) {
                return Ok(DynamicEnd::Failed(error));
            }
        }
        if let Some(variable) = &status_variable
            && let Err(error) = self.assign_variable(variable, return_status(returned))
        {
            return Ok(DynamicEnd::Failed(error));
        }
        self.sync_transaction_count();
        let count_after = self.settings.sqlserver.transaction_count;
        if count_after != count_before {
            return Ok(DynamicEnd::Failed(mismatching_transaction_count(
                count_before,
                count_after,
            )));
        }
        Ok(DynamicEnd::Done)
    }
}
