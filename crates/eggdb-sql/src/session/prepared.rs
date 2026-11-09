use super::{OutputColumn, Parameter, QueryResult, Session, highest_parameter};
use crate::ast::{ExecutePrepared, Expr, PrepareStatement, PreparedCommand, Statement};
use crate::copy::CopyIo;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::tokenize_in;
use crate::name_resolution::Binding;
use crate::parser::{Parameters, Parser};
use crate::types::{SqlType, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_PREPARED_STATEMENTS: usize = 4096;
const MAX_PREPARED_TEXT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct SqlPrepared {
    text: String,
    parameter_types: Vec<SqlType>,
    parameter_count: usize,
    dialect: Dialect,
}

#[derive(Debug, Default)]
pub(super) struct SqlPreparedStatements {
    statements: HashMap<String, SqlPrepared>,
    text_bytes: usize,
}

impl SqlPreparedStatements {
    pub(super) fn forget(&mut self, name: Option<&str>) {
        match name {
            Some(name) => {
                if let Some(removed) = self.statements.remove(name) {
                    self.text_bytes = self.text_bytes.saturating_sub(removed.text.len());
                }
            }
            None => {
                self.statements.clear();
                self.text_bytes = 0;
            }
        }
    }
}

fn too_many_prepared(position: usize) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "cannot keep more than {MAX_PREPARED_STATEMENTS} prepared statements or {MAX_PREPARED_TEXT_BYTES} bytes of their text in one session; deallocate some first"
        ),
    )
    .at(position)
}

impl Session<'_> {
    pub(super) fn run_prepared_command(
        &mut self,
        command: &PreparedCommand,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        match command {
            PreparedCommand::Prepare(prepare) => self.remember_prepared(prepare),
            PreparedCommand::Execute(execute) => {
                let mut statement = self.prepared_statement(execute)?;
                self.run_statement(&mut statement, io)
            }
        }
    }

    pub(super) fn prepared_query(&mut self, execute: &ExecutePrepared) -> SqlResult<Statement> {
        let statement = self.prepared_statement(execute)?;
        if !matches!(statement, Statement::Select(_) | Statement::Query(_)) {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                "prepared statement is not a SELECT",
            )
            .at(execute.name.position));
        }
        Ok(statement)
    }

    pub(super) fn prepared_columns(
        &mut self,
        execute: &ExecutePrepared,
        binding: &Binding<'_>,
    ) -> SqlResult<Option<Vec<OutputColumn>>> {
        match self.prepared_statement(execute)? {
            Statement::Select(mut select) => self
                .read(|reader, tables| {
                    binding.select(reader, tables, &mut select)?;
                    crate::execute::select(reader, tables, &select, true)
                })
                .map(|result| result.columns),
            Statement::Query(mut query) => self
                .read(|reader, tables| {
                    binding.query(reader, tables, &mut query)?;
                    crate::execute::query(reader, tables, &query, true)
                })
                .map(|result| result.columns),
            _ => Ok(None),
        }
    }

    fn remember_prepared(&mut self, prepare: &PrepareStatement) -> SqlResult<QueryResult> {
        let name = &prepare.name.text;
        if self.sql_prepared.statements.contains_key(name) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_PREPARED_STATEMENT,
                format!("prepared statement \"{name}\" already exists"),
            )
            .at(prepare.name.position));
        }
        let text_bytes = self
            .sql_prepared
            .text_bytes
            .saturating_add(prepare.text.len());
        if self.sql_prepared.statements.len() >= MAX_PREPARED_STATEMENTS
            || text_bytes > MAX_PREPARED_TEXT_BYTES
        {
            return Err(too_many_prepared(prepare.name.position));
        }
        let tokens = tokenize_in(&prepare.text, self.dialect)?;
        let parameter_count = highest_parameter(&tokens).max(prepare.parameter_types.len());
        self.sql_prepared.text_bytes = text_bytes;
        self.sql_prepared.statements.insert(
            name.clone(),
            SqlPrepared {
                text: prepare.text.clone(),
                parameter_types: prepare.parameter_types.clone(),
                parameter_count,
                dialect: self.dialect,
            },
        );
        Ok(QueryResult::command("PREPARE"))
    }

    fn prepared_statement(&mut self, execute: &ExecutePrepared) -> SqlResult<Statement> {
        let name = &execute.name.text;
        let Some(prepared) = self.sql_prepared.statements.get(name).cloned() else {
            return Err(SqlError::new(
                SqlState::INVALID_SQL_STATEMENT_NAME,
                format!("prepared statement \"{name}\" does not exist"),
            )
            .at(execute.name.position));
        };
        if execute.arguments.len() != prepared.parameter_count {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("wrong number of parameters for prepared statement \"{name}\""),
            )
            .with_detail(format!(
                "Expected {} parameters but got {}.",
                prepared.parameter_count,
                execute.arguments.len()
            ))
            .at(execute.name.position));
        }
        let values = self.argument_values(&execute.arguments)?;
        let parameters: Vec<Parameter> = values
            .iter()
            .enumerate()
            .map(|(index, value)| Parameter {
                declared_type: prepared
                    .parameter_types
                    .get(index)
                    .copied()
                    .or_else(|| value.sql_type()),
                value: prepared.dialect.value_text(value),
            })
            .collect();
        let tokens = tokenize_in(&prepared.text, prepared.dialect)?;
        let settings = Arc::clone(&self.settings);
        let mut parser = Parser::new(&prepared.text, &tokens)
            .with_context(Parameters::Values(&parameters), &settings)
            .in_dialect(prepared.dialect);
        parser
            .statement()
            .map_err(|error| error.at(execute.name.position))
    }

    fn argument_values(&mut self, arguments: &[Expr]) -> SqlResult<Vec<Value>> {
        if arguments.is_empty() {
            return Ok(Vec::new());
        }
        let width = arguments.len();
        let mut select = crate::tsql::scalar_select(arguments.to_vec(), None);
        let settings = Arc::clone(&self.settings);
        let binding = Binding {
            settings: &settings,
            walk_expressions: self.statement_walks_expressions,
            target_privilege: None,
        };
        let result = self.read(|reader, tables| {
            binding.select(reader, tables, &mut select)?;
            crate::execute::select(reader, tables, &select, false)
        })?;
        Ok(result
            .rows
            .into_iter()
            .next()
            .unwrap_or_else(|| vec![Value::Null; width]))
    }
}
