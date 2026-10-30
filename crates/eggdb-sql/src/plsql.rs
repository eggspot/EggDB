mod exceptions;
mod packages;
mod session_state;

#[cfg(test)]
mod call_tests;
#[cfg(test)]
mod collection_tests;
#[cfg(test)]
mod compile_tests;
#[cfg(test)]
mod ddl_tests;
#[cfg(test)]
mod output_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;

pub(crate) use exceptions::{
    APPLICATION_ERROR_HIGHEST, APPLICATION_ERROR_LOWEST, NORMAL_COMPLETION, NO_DATA_FOUND_CODE,
    PredefinedException, USER_EXCEPTION_CODE, USER_EXCEPTION_MESSAGE, application_error_code,
    predefined_by_code, predefined_by_name, sqlcode_of, sqlerrm_of,
};
pub(crate) use packages::{
    StoredPackage, alter_package, create_package, drop_package, parse_package, resolve_package,
    stored_package,
};
pub(crate) use session_state::{
    DEFAULT_OUTPUT_BUFFER_BYTES, ServerOutput, SharedPlsqlSession, enter_session, with_session,
};

use crate::ast::Name;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{swap_uniform_case, tokenize_in};
use crate::plpgsql::{Block, Program, is_trigger_function};
use crate::routines::{
    RoutineDefinition, RoutineKind, RoutineLanguage, RoutineParameter, RoutineReturns, Volatility,
};
use crate::schemas::{base_name, is_qualified, schema_of};
use crate::types::SqlType;

pub(crate) const COMPILATION_ERROR_CODE: u16 = 6550;
pub(crate) const ANONYMOUS_BLOCK_NAME: &str = "anonymous block";
pub(crate) const REF_CURSOR_TYPE: &str = "sys_refcursor";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeclaredType {
    Sql { sql_type: SqlType, written: String },
    ColumnType(Vec<String>),
    RowType(Vec<String>),
    Named(Vec<String>),
}

fn shown_parts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| swap_uniform_case(part))
        .collect::<Vec<_>>()
        .join(".")
}

impl DeclaredType {
    pub(crate) fn sql(sql_type: SqlType, written: &str) -> Self {
        Self::Sql {
            sql_type,
            written: written.to_ascii_uppercase(),
        }
    }

    pub(crate) fn engine_type(&self) -> SqlType {
        match self {
            Self::Sql { sql_type, .. } => *sql_type,
            Self::ColumnType(_) | Self::RowType(_) | Self::Named(_) => SqlType::Dynamic,
        }
    }

    pub(crate) fn needs_resolution(&self) -> bool {
        !matches!(self, Self::Sql { .. })
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Sql { written, .. } => written.clone(),
            Self::ColumnType(parts) => format!("{}%TYPE", shown_parts(parts)),
            Self::RowType(parts) => format!("{}%ROWTYPE", shown_parts(parts)),
            Self::Named(parts) => shown_parts(parts),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlsqlRoutine {
    pub(crate) declared: Vec<DeclaredType>,
    pub(crate) returns: Option<DeclaredType>,
    pub(crate) package: Option<String>,
    pub(crate) deterministic: bool,
    pub(crate) pipelined: bool,
}

impl PlsqlRoutine {
    pub(crate) fn identity(&self, parameters: &[RoutineParameter]) -> String {
        parameters
            .iter()
            .zip(&self.declared)
            .filter(|(parameter, _)| parameter.mode.is_input())
            .map(|(_, declared)| declared.text())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(crate) fn declared_type(&self, parameter: usize) -> Option<&DeclaredType> {
        self.declared.get(parameter)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlsqlUnit {
    pub(crate) name: Name,
    pub(crate) definition: RoutineDefinition,
    pub(crate) program: Program,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlsqlPackage {
    pub(crate) name: Name,
    pub(crate) body: bool,
    pub(crate) text: String,
    pub(crate) block: Block,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerRecords {
    pub(crate) new: String,
    pub(crate) old: String,
}

impl Default for TriggerRecords {
    fn default() -> Self {
        Self {
            new: "new".to_owned(),
            old: "old".to_owned(),
        }
    }
}

pub(crate) fn shown_object(internal: &str) -> String {
    let name = swap_uniform_case(base_name(internal));
    if is_qualified(internal) {
        format!("{}.{name}", swap_uniform_case(schema_of(internal)))
    } else {
        name
    }
}

pub(crate) fn name_in_use(position: usize) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        "name is already used by an existing object",
    )
    .with_oracle_code(955)
    .at(position)
}

pub(crate) fn object_missing(internal: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("object {} does not exist", shown_object(internal)),
    )
    .with_oracle_code(4043)
    .at(position)
}

pub(crate) fn is_ref_cursor(declared: &DeclaredType) -> bool {
    matches!(declared, DeclaredType::Named(parts) if parts.len() == 1 && parts[0] == REF_CURSOR_TYPE)
}

pub(crate) fn anonymous_block_definition(code: &str) -> RoutineDefinition {
    RoutineDefinition {
        name: ANONYMOUS_BLOCK_NAME.to_owned(),
        kind: RoutineKind::Procedure,
        dialect: Dialect::Oracle,
        language: RoutineLanguage::PlSql,
        parameters: Vec::new(),
        returns: RoutineReturns::Nothing,
        volatility: Volatility::Volatile,
        strict: false,
        security_definer: false,
        settings: Vec::new(),
        body: code.to_owned(),
        text: String::new(),
        tsql: None,
        plsql: Some(Box::new(PlsqlRoutine {
            declared: Vec::new(),
            returns: None,
            package: None,
            deterministic: false,
            pipelined: false,
        })),
    }
}

pub(crate) fn compile(definition: &RoutineDefinition) -> SqlResult<Program> {
    let body = &definition.body;
    let tokens =
        tokenize_in(body, Dialect::Oracle).map_err(|error| compilation_error(body, error))?;
    let trigger = is_trigger_function(definition).then(TriggerRecords::default);
    crate::parser::plsql_program(body, &tokens, definition, trigger.as_ref())
        .map_err(|error| compilation_error(body, error))
}

pub(crate) fn compile_package(text: &str, spec: Option<&Block>) -> SqlResult<PlsqlPackage> {
    let tokens =
        tokenize_in(text, Dialect::Oracle).map_err(|error| compilation_error(text, error))?;
    crate::parser::plsql_package_program(text, &tokens, spec)
        .map_err(|error| compilation_error(text, error))
}

pub(crate) fn is_compilation_error(error: &SqlError) -> bool {
    error.vendor() == Some(Dialect::Oracle) && error.vendor_code() == Some(COMPILATION_ERROR_CODE)
}

pub(crate) fn compilation_error(unit: &str, error: SqlError) -> SqlError {
    if is_compilation_error(&error) {
        return error;
    }
    let position = error.position().unwrap_or(0).min(unit.len());
    let (line, column) = line_and_column(unit, position);
    let reason = compilation_reason(unit, &error);
    SqlError::new(error.state(), format!("line {line}, column {column}:\n{reason}"))
        .with_oracle_code(COMPILATION_ERROR_CODE)
}

pub(crate) fn line_and_column(unit: &str, position: usize) -> (usize, usize) {
    let before = unit.get(..position).unwrap_or(unit);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

fn compilation_reason(unit: &str, error: &SqlError) -> String {
    let message = error.message();
    if message.starts_with("PLS-") {
        return message.to_owned();
    }
    if error.state() == SqlState::SYNTAX_ERROR {
        return format!(
            "PLS-00103: Encountered the symbol \"{}\"",
            encountered_symbol(unit, error.position())
        );
    }
    format!("PL/SQL: {message}")
}

fn is_symbol_part(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$' | '#')
}

fn encountered_symbol(unit: &str, position: Option<usize>) -> String {
    let rest = position
        .and_then(|position| unit.get(position..))
        .unwrap_or("")
        .trim_start();
    match rest.chars().next() {
        None => "end-of-file".to_owned(),
        Some(first) if is_symbol_part(first) => rest
            .chars()
            .take_while(|character| is_symbol_part(*character))
            .collect::<String>()
            .to_uppercase(),
        Some(first) => first.to_string(),
    }
}
