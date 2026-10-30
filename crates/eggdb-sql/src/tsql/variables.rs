use super::errors::CaughtError;
use crate::ast::{Expr, ExprKind};
use crate::error::SqlResult;
use crate::session::NamedParameter;
use crate::sqlserver_types::SqlServerType;
use crate::types::{SqlType, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScalarVariable {
    pub(crate) name: String,
    pub(crate) declared: Option<SqlServerType>,
    pub(crate) sql_type: Option<SqlType>,
    pub(crate) value: Value,
}

impl ScalarVariable {
    pub(crate) fn declared_as(name: String, declared: SqlServerType) -> Self {
        Self {
            name,
            declared: Some(declared),
            sql_type: Some(declared.engine_type()),
            value: Value::Null,
        }
    }

    pub(crate) fn parameter(parameter: &NamedParameter) -> Self {
        Self {
            name: parameter.name.to_lowercase(),
            declared: None,
            sql_type: parameter.declared_type,
            value: parameter.value.clone(),
        }
    }

    pub(crate) fn constant(&self, position: usize) -> Expr {
        if self.declared.is_some() {
            return Expr {
                kind: ExprKind::Constant {
                    value: self.value.clone(),
                    sql_type: self.sql_type.or_else(|| self.value.sql_type()),
                },
                position,
            };
        }
        let constant = Expr {
            kind: ExprKind::Constant {
                value: self.value.clone(),
                sql_type: self.value.sql_type().or(self.sql_type),
            },
            position,
        };
        match self.sql_type {
            Some(sql_type) if !self.value.is_null() => Expr {
                kind: ExprKind::Cast {
                    expr: Box::new(constant),
                    sql_type,
                },
                position,
            },
            _ => constant,
        }
    }

    pub(crate) fn assigned(&self, value: Value) -> SqlResult<Value> {
        match (self.declared, self.sql_type) {
            (Some(declared), _) => {
                let value = crate::sqlserver_conversion::implicit(value, declared.engine_type())?;
                crate::sqlserver_conversion::convert(value, declared, None, None)
            }
            (None, Some(sql_type)) => crate::sqlserver_conversion::implicit(value, sql_type),
            (None, None) => Ok(value),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BatchContext {
    pub(crate) variables: Vec<ScalarVariable>,
    pub(crate) caught: Option<CaughtError>,
    pub(crate) last_error: u32,
    pub(crate) transition_tables: Vec<String>,
    pub(crate) updated_columns: Option<Vec<String>>,
}

impl BatchContext {
    pub(crate) fn variable(&self, name: &str) -> Option<&ScalarVariable> {
        self.variables
            .iter()
            .find(|variable| variable.name == name)
    }

    pub(crate) fn variable_mut(&mut self, name: &str) -> Option<&mut ScalarVariable> {
        self.variables
            .iter_mut()
            .find(|variable| variable.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DynamicBatch {
    pub(crate) text: String,
    pub(crate) parameters: Vec<NamedParameter>,
    pub(crate) outputs: Vec<(String, String)>,
}
