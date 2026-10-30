mod exceptions;

#[cfg(test)]
mod tests;

pub(crate) use exceptions::{
    APPLICATION_ERROR_HIGHEST, APPLICATION_ERROR_LOWEST, NO_DATA_FOUND_CODE, PredefinedException,
    USER_EXCEPTION_CODE, application_error_code, predefined_by_code, predefined_by_name, sqlcode_of,
};

use crate::lexer::swap_uniform_case;
use crate::routines::RoutineParameter;
use crate::types::SqlType;

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
