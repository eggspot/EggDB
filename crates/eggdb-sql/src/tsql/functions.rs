use super::control::{Instruction, MAX_DYNAMIC_NESTING, Program};
use super::errors::nesting_too_deep;
use super::routines::TsqlReturns;
use super::statements::{
    MatchingRow, apply_changes, changed_rows, columns_mismatch, combined, declare, every_row,
    filled_rows, matching_of, matching_select, remove_rows, scalar_select, undeclared,
    value_items, variable_column, variable_columns,
};
use super::variables::{BatchContext, ScalarVariable};
use crate::ast::{
    AssignmentTarget, Expr, Name, Select, SqlServerStatement, Statement, VariableDeclaration,
    VariableSource,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::plpgsql::Returned;
use crate::records::{RECORD_OID, record_value};
use crate::routines::{DEFAULT_ARGUMENT, Prefetch, RoutineDefinition, RoutineKind};
use crate::schemas::is_qualified;
use crate::session::{OutputColumn, QueryResult};
use crate::settings::Settings;
use crate::sqlserver_types::SqlServerType;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::table_variables::{TableVariable, TableVariables};
use crate::types::{SqlType, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

thread_local! {
    static FUNCTION_DEPTH: Cell<u8> = const { Cell::new(0) };
}

struct FunctionDepth;

impl Drop for FunctionDepth {
    fn drop(&mut self) {
        FUNCTION_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

fn enter_function() -> SqlResult<FunctionDepth> {
    FUNCTION_DEPTH.with(|depth| {
        if depth.get() >= MAX_DYNAMIC_NESTING {
            return Err(nesting_too_deep());
        }
        depth.set(depth.get() + 1);
        Ok(FunctionDepth)
    })
}

pub(crate) fn side_effecting(operator: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        format!("Invalid use of a side-effecting operator '{operator}' within a function."),
    )
    .with_sqlserver_code(443)
    .at(position)
}

fn returns_data(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        "Select statements included within a function cannot return data to a client.",
    )
    .with_sqlserver_code(444)
    .at(position)
}

fn executes_procedure(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        "Only functions and some extended stored procedures can be executed from within a function.",
    )
    .with_sqlserver_code(557)
    .at(position)
}

fn shown_name(definition: &RoutineDefinition) -> String {
    if is_qualified(&definition.name) {
        definition.name.clone()
    } else {
        format!("dbo.{}", definition.name)
    }
}

fn too_many_arguments(definition: &RoutineDefinition) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "Procedure or function {} has too many arguments specified.",
            shown_name(definition)
        ),
    )
    .with_sqlserver_code(8144)
}

fn too_few_arguments(definition: &RoutineDefinition) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!(
            "An insufficient number of arguments were supplied for the procedure or function {}.",
            shown_name(definition)
        ),
    )
    .with_sqlserver_code(313)
}

fn not_a_function(definition: &RoutineDefinition) -> SqlError {
    let name = shown_name(definition);
    let schema = name.split('.').next().unwrap_or_default().to_owned();
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "Cannot find either column \"{schema}\" or the user-defined function or aggregate \"{name}\", or the name is ambiguous."
        ),
    )
    .with_sqlserver_code(4121)
}

fn lost_return_table() -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "a table-valued function lost its return table",
    )
}

pub(crate) fn refused_in_function(
    statement: &Statement,
    first_word: &str,
    position: usize,
) -> Option<SqlError> {
    let operator = match statement {
        Statement::Select(_) | Statement::Query(_) => return Some(returns_data(position)),
        Statement::SqlServer(inner) => match inner {
            SqlServerStatement::Declare(_)
            | SqlServerStatement::DeclareTable { .. }
            | SqlServerStatement::SetVariable { .. }
            | SqlServerStatement::SelectAssign { .. }
            | SqlServerStatement::UpdateVariable { .. }
            | SqlServerStatement::DeleteVariable { .. }
            | SqlServerStatement::Condition(_)
            | SqlServerStatement::Return(_) => return None,
            SqlServerStatement::InsertVariable {
                source: VariableSource::Execute(_),
                ..
            } => "INSERT EXEC",
            SqlServerStatement::InsertVariable { .. } => return None,
            SqlServerStatement::Execute(_) => return Some(executes_procedure(position)),
            SqlServerStatement::Print(_) => "PRINT",
            SqlServerStatement::RaiseError(_) => "RAISERROR",
            SqlServerStatement::Throw(_) => "THROW",
            SqlServerStatement::ExecuteDynamic(_) => "EXECUTE STRING",
            SqlServerStatement::SaveTransaction(_) => "SAVE TRANSACTION",
            SqlServerStatement::RollbackTo(_) => "ROLLBACK TRANSACTION",
            SqlServerStatement::NoCount(_)
            | SqlServerStatement::ImplicitTransactions(_)
            | SqlServerStatement::IdentityInsert { .. }
            | SqlServerStatement::FormatOnly(_)
            | SqlServerStatement::XactAbort(_) => "SET ON/OFF",
            SqlServerStatement::InsertBulk(_) => "INSERT BULK",
            SqlServerStatement::Use(_) => first_word,
        },
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Merge(_) => "MERGE",
        Statement::Truncate(_) => "TRUNCATE TABLE",
        Statement::Begin { .. } => "BEGIN TRANSACTION",
        Statement::Commit { .. } => "COMMIT TRANSACTION",
        Statement::Rollback { .. } => "ROLLBACK TRANSACTION",
        Statement::Savepoint(_) => "SAVE TRANSACTION",
        Statement::Analyze(_) => "UPDATE STATISTICS",
        Statement::Role(role) => role.tag(),
        Statement::Set { .. } | Statement::Reset { .. } => "SET ON/OFF",
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable { .. } => "DROP TABLE",
        Statement::AlterTable(_) => "ALTER TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        _ => first_word,
    };
    Some(side_effecting(operator, position))
}

pub(crate) fn function_arguments(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    definition: &RoutineDefinition,
    names: &[Option<String>],
    values: Vec<Value>,
) -> SqlResult<Vec<Value>> {
    if definition.kind != RoutineKind::Function {
        return Err(not_a_function(definition));
    }
    let parameters = &definition.parameters;
    if values.len() > parameters.len() {
        return Err(too_many_arguments(definition));
    }
    if values.len() < parameters.len() {
        return Err(too_few_arguments(definition));
    }
    let mut arguments = Vec::with_capacity(values.len());
    for (at, (parameter, value)) in parameters.iter().zip(values).enumerate() {
        let defaulted = names.get(at).and_then(Option::as_deref) == Some(DEFAULT_ARGUMENT);
        if !defaulted {
            arguments.push(value);
            continue;
        }
        let Some(text) = &parameter.default else {
            return Err(too_few_arguments(definition));
        };
        arguments.push(crate::routines::default_value(
            reader,
            tables,
            text,
            Dialect::SqlServer,
        )?);
    }
    Ok(arguments)
}

pub(crate) fn invoke_function(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    definition: &Arc<RoutineDefinition>,
    arguments: Vec<Value>,
) -> SqlResult<Returned> {
    let returns = match definition.tsql.as_deref() {
        Some(tsql) if definition.kind == RoutineKind::Function => tsql.returns.clone(),
        _ => return Err(not_a_function(definition)),
    };
    let _depth = enter_function()?;
    crate::cancel::check()?;
    let arguments = converted_arguments(definition, arguments)?;
    if definition.strict
        && matches!(returns, TsqlReturns::Scalar(_))
        && arguments.iter().any(Value::is_null)
    {
        return Ok(Returned::Value(Value::Null));
    }
    let body = definition.body.as_str();
    let tokens = tokenize_in(body, Dialect::SqlServer).map_err(SqlError::without_position)?;
    let mut run = FunctionRun {
        writer,
        tables,
        settings,
        text: body,
        tokens: &tokens,
        context: BatchContext::default(),
        variables: TableVariables::default(),
        returned: None,
    };
    let returned = match &returns {
        TsqlReturns::InlineTable => run.inline_table(definition, arguments),
        TsqlReturns::Scalar(declared) => run.scalar(definition, &returns, arguments, *declared),
        TsqlReturns::Table { variable, columns } => {
            run.table(definition, &returns, arguments, variable, columns)
        }
        TsqlReturns::Status => Err(not_a_function(definition)),
    };
    returned.map_err(SqlError::without_position)
}

pub(crate) fn inline_table_columns(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    definition: &RoutineDefinition,
) -> SqlResult<Vec<OutputColumn>> {
    let body = definition.body.as_str();
    let tokens = tokenize_in(body, Dialect::SqlServer)?;
    let mut run = FunctionRun {
        writer,
        tables,
        settings,
        text: body,
        tokens: &tokens,
        context: BatchContext::default(),
        variables: TableVariables::default(),
        returned: None,
    };
    let nulls = vec![Value::Null; definition.parameters.len()];
    run.bind_parameters(definition, nulls);
    let statement = run.parsed((0, 0), false)?;
    let result = run.run_query(statement, true)?;
    Ok(result.columns.unwrap_or_default())
}

fn converted_arguments(definition: &RoutineDefinition, arguments: Vec<Value>) -> SqlResult<Vec<Value>> {
    if arguments.len() > definition.parameters.len() {
        return Err(too_many_arguments(definition));
    }
    if arguments.len() < definition.parameters.len() {
        return Err(too_few_arguments(definition));
    }
    definition
        .parameters
        .iter()
        .zip(arguments)
        .map(|(parameter, value)| match parameter.declared {
            Some(declared) => ScalarVariable::declared_as(String::new(), declared).assigned(value),
            None => Ok(value),
        })
        .collect()
}

fn records(columns: &[String], rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| record_value(RECORD_OID, row, columns.to_vec()))
        .collect()
}

struct FunctionRun<'r, 't> {
    writer: &'r mut dyn KeyValueWrite,
    tables: &'r mut TableCache,
    settings: &'r Settings,
    text: &'t str,
    tokens: &'r [Token<'t>],
    context: BatchContext,
    variables: TableVariables,
    returned: Option<Value>,
}

impl FunctionRun<'_, '_> {
    fn bind_parameters(&mut self, definition: &RoutineDefinition, arguments: Vec<Value>) {
        for (parameter, value) in definition.parameters.iter().zip(arguments) {
            let (Some(name), Some(declared)) = (&parameter.name, parameter.declared) else {
                continue;
            };
            match self.context.variable_mut(name) {
                Some(variable) => variable.value = value,
                None => {
                    let mut variable = ScalarVariable::declared_as(name.clone(), declared);
                    variable.value = value;
                    self.context.variables.push(variable);
                }
            }
        }
    }

    fn compiled(
        &mut self,
        definition: &Arc<RoutineDefinition>,
        returns: &TsqlReturns,
    ) -> SqlResult<Arc<Program>> {
        let (text, tokens, settings) = (self.text, self.tokens, self.settings);
        crate::routines::compiled_tsql_program(self.tables, definition, || {
            Parser::new(text, tokens)
                .with_context(Parameters::Absent, settings)
                .in_dialect(Dialect::SqlServer)
                .compile_routine_body(&definition.parameters, returns)
        })
    }

    fn start(
        &mut self,
        definition: &Arc<RoutineDefinition>,
        returns: &TsqlReturns,
        arguments: Vec<Value>,
    ) -> SqlResult<Arc<Program>> {
        let program = self.compiled(definition, returns)?;
        let mut tables: HashMap<String, TableVariable> = HashMap::new();
        declare(&mut self.context, &mut tables, &program);
        if let TsqlReturns::Table { variable, columns } = returns {
            tables.insert(
                variable.clone(),
                TableVariable {
                    columns: columns
                        .iter()
                        .map(|(name, sql_type)| OutputColumn {
                            name: name.clone(),
                            sql_type: Some(*sql_type),
                        })
                        .collect(),
                    rows: Vec::new(),
                },
            );
        }
        self.variables = Arc::new(tables);
        self.bind_parameters(definition, arguments);
        Ok(program)
    }

    fn scalar(
        &mut self,
        definition: &Arc<RoutineDefinition>,
        returns: &TsqlReturns,
        arguments: Vec<Value>,
        declared: SqlServerType,
    ) -> SqlResult<Returned> {
        let program = self.start(definition, returns, arguments)?;
        self.run(&program)?;
        let value = self.returned.take().unwrap_or(Value::Null);
        let value = ScalarVariable::declared_as(String::new(), declared).assigned(value)?;
        Ok(Returned::Value(value))
    }

    fn table(
        &mut self,
        definition: &Arc<RoutineDefinition>,
        returns: &TsqlReturns,
        arguments: Vec<Value>,
        variable: &str,
        columns: &[(String, SqlType)],
    ) -> SqlResult<Returned> {
        let program = self.start(definition, returns, arguments)?;
        self.run(&program)?;
        let names: Vec<String> = columns.iter().map(|(name, _)| name.clone()).collect();
        let mut variables = std::mem::take(&mut self.variables);
        let rows = Arc::make_mut(&mut variables)
            .remove(variable)
            .ok_or_else(lost_return_table)?
            .rows;
        Ok(Returned::Set(records(&names, rows)))
    }

    fn inline_table(
        &mut self,
        definition: &RoutineDefinition,
        arguments: Vec<Value>,
    ) -> SqlResult<Returned> {
        self.bind_parameters(definition, arguments);
        let statement = self.parsed((0, 0), false)?;
        let result = self.run_query(statement, false)?;
        let names: Vec<String> = result
            .columns
            .as_ref()
            .map(|columns| columns.iter().map(|column| column.name.clone()).collect())
            .unwrap_or_default();
        Ok(Returned::Set(records(&names, result.rows)))
    }

    fn run(&mut self, program: &Program) -> SqlResult<()> {
        let mut at_instruction = 0usize;
        while let Some(&instruction) = program.instructions.get(at_instruction) {
            match instruction {
                Instruction::Jump { to } => at_instruction = to,
                Instruction::Return => return Ok(()),
                Instruction::Run { at } => {
                    self.step(at)?;
                    at_instruction += 1;
                }
                Instruction::Test {
                    condition,
                    otherwise,
                    ..
                } => {
                    crate::cancel::check()?;
                    at_instruction = if self.holds(condition)? {
                        at_instruction + 1
                    } else {
                        otherwise
                    };
                }
            }
        }
        Ok(())
    }

    fn statement_start(&self, at: (usize, usize)) -> (usize, String) {
        let token = self
            .tokens
            .get(at.0..)
            .unwrap_or_default()
            .iter()
            .find(|token| token.kind != TokenKind::Semicolon);
        let position = token.map_or(self.text.len(), |token| token.start);
        let first_word = match token.map(|token| &token.kind) {
            Some(TokenKind::Word(word)) => word.to_uppercase(),
            _ => String::new(),
        };
        (position, first_word)
    }

    fn parsed(&self, at: (usize, usize), condition: bool) -> SqlResult<Statement> {
        let mut parser = Parser::new(self.text, self.tokens)
            .with_context(Parameters::Absent, self.settings)
            .with_batch_context(&self.context)
            .in_dialect(Dialect::SqlServer)
            .resuming_at(at);
        parser.skip_empty_statements();
        if condition {
            parser.sqlserver_condition()
        } else {
            parser.statement()
        }
    }

    fn holds(&mut self, condition: (usize, usize)) -> SqlResult<bool> {
        let Statement::SqlServer(SqlServerStatement::Condition(expr)) =
            self.parsed(condition, true)?
        else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a T-SQL condition did not parse as a condition",
            ));
        };
        match self.evaluate(&expr)? {
            Value::Boolean(holds) => Ok(holds),
            Value::Null => Ok(false),
            _ => Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                "An expression of non-boolean type specified in a context where a condition is expected.",
            )
            .with_sqlserver_code(4145)),
        }
    }

    fn step(&mut self, at: (usize, usize)) -> SqlResult<()> {
        let (position, first_word) = self.statement_start(at);
        let statement = self.parsed(at, false)?;
        if let Some(refused) = refused_in_function(&statement, &first_word, position) {
            return Err(refused);
        }
        let Statement::SqlServer(statement) = statement else {
            return Err(side_effecting(&first_word, position));
        };
        match statement {
            SqlServerStatement::Declare(declarations) => self.declare(&declarations),
            SqlServerStatement::DeclareTable { .. } | SqlServerStatement::Condition(_) => Ok(()),
            SqlServerStatement::SetVariable { variable, value } => {
                let value = self.evaluate(&value)?;
                self.assign(&variable, value)
            }
            SqlServerStatement::SelectAssign { targets, query } => {
                self.select_assign(&targets, *query)
            }
            SqlServerStatement::InsertVariable {
                variable,
                columns,
                source,
            } => self.insert(&variable, columns.as_deref(), source),
            SqlServerStatement::UpdateVariable {
                variable,
                assignments,
                filter,
            } => self.update(&variable, &assignments, filter.as_ref()),
            SqlServerStatement::DeleteVariable { variable, filter } => {
                self.delete(&variable, filter.as_ref())
            }
            SqlServerStatement::Return(value) => {
                self.returned = Some(self.evaluate(&value)?);
                Ok(())
            }
            _ => Err(side_effecting(&first_word, position)),
        }
    }

    fn run_select(&mut self, select: &mut Select, columns_only: bool) -> SqlResult<QueryResult> {
        let binding = Binding {
            settings: self.settings,
            walk_expressions: true,
        };
        let _variables = crate::table_variables::enter(&self.variables);
        binding.select(&*self.writer, self.tables, select)?;
        if crate::routines::select_calls_marked_routine(select) {
            crate::routines::prepare_select(
                &mut *self.writer,
                self.tables,
                self.settings,
                select,
                Prefetch::NonVolatile,
            )?;
        }
        crate::execute::select(&*self.writer, self.tables, select, columns_only)
    }

    fn run_query(&mut self, statement: Statement, columns_only: bool) -> SqlResult<QueryResult> {
        match statement {
            Statement::Select(mut select) => self.run_select(&mut select, columns_only),
            Statement::Query(mut query) => {
                let binding = Binding {
                    settings: self.settings,
                    walk_expressions: true,
                };
                let _variables = crate::table_variables::enter(&self.variables);
                binding.query(&*self.writer, self.tables, &mut query)?;
                if crate::routines::query_calls_marked_routine(&query) {
                    crate::routines::prepare_query(
                        &mut *self.writer,
                        self.tables,
                        self.settings,
                        &mut query,
                        Prefetch::NonVolatile,
                    )?;
                }
                crate::execute::query(&*self.writer, self.tables, &query, columns_only)
            }
            _ => Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a statement other than a query reached the T-SQL function's query runner",
            )),
        }
    }

    fn evaluate_row(&mut self, items: Vec<Expr>) -> SqlResult<Vec<Value>> {
        let width = items.len();
        let mut select = scalar_select(items, None);
        let result = self.run_select(&mut select, false)?;
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

    fn assign(&mut self, variable: &Name, value: Value) -> SqlResult<()> {
        let Some(found) = self.context.variable_mut(&variable.text) else {
            return Err(undeclared(&variable.text).at(variable.position));
        };
        found.value = found.assigned(value)?;
        Ok(())
    }

    fn declare(&mut self, declarations: &[VariableDeclaration]) -> SqlResult<()> {
        let initialized: Vec<&VariableDeclaration> = declarations
            .iter()
            .filter(|declaration| declaration.initial.is_some())
            .collect();
        if initialized.is_empty() {
            return Ok(());
        }
        let values = self.evaluate_row(
            initialized
                .iter()
                .filter_map(|declaration| declaration.initial.clone())
                .collect(),
        )?;
        for (declaration, value) in initialized.iter().zip(values) {
            self.assign(&declaration.name, value)?;
        }
        Ok(())
    }

    fn select_assign(&mut self, targets: &[AssignmentTarget], query: Statement) -> SqlResult<()> {
        let result = self.run_query(query, false)?;
        for row in result.rows {
            for (target, value) in targets.iter().zip(row) {
                let value = match target.operator {
                    None => value,
                    Some(operator) => {
                        let current = self
                            .context
                            .variable(&target.variable.text)
                            .map_or(Value::Null, |variable| variable.value.clone());
                        combined(operator, current, value)?
                    }
                };
                self.assign(&target.variable, value)?;
            }
        }
        Ok(())
    }

    fn writable_rows(&mut self, variable: &Name) -> Option<&mut Vec<Vec<Value>>> {
        Arc::make_mut(&mut self.variables)
            .get_mut(&variable.text)
            .map(|found| &mut found.rows)
    }

    fn insert(
        &mut self,
        variable: &Name,
        columns: Option<&[Name]>,
        source: VariableSource,
    ) -> SqlResult<()> {
        let (targets, width) = variable_columns(&self.variables, variable, columns)?;
        let rows = match source {
            VariableSource::Execute(_) => {
                return Err(side_effecting("INSERT EXEC", variable.position));
            }
            VariableSource::Query(statement) => {
                let result = self.run_query(*statement, false)?;
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
                    evaluated.push(self.evaluate_row(value_items(&row))?);
                }
                evaluated
            }
        };
        let appended = filled_rows(&targets, width, rows)?;
        if let Some(rows) = self.writable_rows(variable) {
            rows.extend(appended);
        }
        Ok(())
    }

    fn matching_rows(
        &mut self,
        variable: &Name,
        filter: Option<&Expr>,
        values: &[Expr],
    ) -> SqlResult<Vec<MatchingRow>> {
        let Some(declared) = self.variables.get(&variable.text) else {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        };
        let Some(mut select) = matching_select(variable, filter, values) else {
            return Ok(every_row(declared.rows.len()));
        };
        let result = self.run_select(&mut select, false)?;
        Ok(matching_of(result.rows, filter.is_some()))
    }

    fn update(
        &mut self,
        variable: &Name,
        assignments: &[(Name, Expr)],
        filter: Option<&Expr>,
    ) -> SqlResult<()> {
        if !self.variables.contains_key(&variable.text) {
            return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
        }
        let columns = assignments
            .iter()
            .map(|(column, _)| variable_column(&self.variables, variable, column))
            .collect::<SqlResult<Vec<_>>>()?;
        let values: Vec<Expr> = assignments.iter().map(|(_, value)| value.clone()).collect();
        let matching = self.matching_rows(variable, filter, &values)?;
        let changes = changed_rows(&columns, matching)?;
        if let Some(rows) = self.writable_rows(variable) {
            apply_changes(rows, changes);
        }
        Ok(())
    }

    fn delete(&mut self, variable: &Name, filter: Option<&Expr>) -> SqlResult<()> {
        let matching = self.matching_rows(variable, filter, &[])?;
        if let Some(rows) = self.writable_rows(variable) {
            remove_rows(rows, matching);
        }
        Ok(())
    }
}
