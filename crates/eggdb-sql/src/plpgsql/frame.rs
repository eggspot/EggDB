use super::CursorDeclaration;
use super::collections::{CollectionShape, PlsqlFrame};
use crate::ast::{Expr, ExprKind};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::parser::parameter_number;
use crate::records::{self, RECORD_OID, RecordValue};
use crate::types::{DefinedKind, DefinedValue, SqlType, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Slot {
    Typed(SqlType),
    Untyped,
    Record,
    Row(Vec<(String, SqlType)>),
    Cursor(Option<Box<CursorDeclaration>>),
    Collection(Box<CollectionShape>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FrameVariable {
    pub(crate) name: String,
    pub(crate) label: Option<String>,
    pub(crate) slot: Slot,
    pub(crate) value: Value,
    pub(crate) constant: bool,
    pub(crate) not_null: bool,
    pub(crate) alias: Option<usize>,
    pub(crate) qualified_only: bool,
}

impl FrameVariable {
    pub(crate) fn new(name: impl Into<String>, slot: Slot, value: Value) -> Self {
        Self {
            name: name.into(),
            label: None,
            slot,
            value,
            constant: false,
            not_null: false,
            alias: None,
            qualified_only: false,
        }
    }

    pub(crate) fn is_composite(&self) -> bool {
        match &self.slot {
            Slot::Record | Slot::Row(_) => true,
            Slot::Typed(sql_type) => is_composite_type(*sql_type),
            Slot::Untyped => record_of(&self.value).is_some(),
            Slot::Cursor(_) | Slot::Collection(_) => false,
        }
    }

    pub(crate) fn sql_type(&self) -> Option<SqlType> {
        match &self.slot {
            Slot::Typed(sql_type) => Some(*sql_type),
            Slot::Cursor(_) => Some(SqlType::Text),
            Slot::Untyped | Slot::Record | Slot::Row(_) | Slot::Collection(_) => {
                self.value.sql_type()
            }
        }
    }

    fn field(&self, field: &str) -> SqlResult<(Value, Option<SqlType>)> {
        match (&self.slot, &self.value) {
            (Slot::Record, Value::Null) => Err(unassigned_record(&self.name)),
            (Slot::Row(columns), Value::Null) => {
                let Some((_, sql_type)) = columns.iter().find(|(name, _)| name == field) else {
                    return Err(missing_field(&self.name, field));
                };
                Ok((Value::Null, Some(*sql_type)))
            }
            (slot, value) => {
                let found = records::field(value, field)
                    .map_err(|_| missing_field(&self.name, field))?;
                let declared = match slot {
                    Slot::Row(columns) => columns
                        .iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, sql_type)| *sql_type),
                    _ => None,
                };
                let sql_type = declared
                    .or_else(|| records::field_type(value.sql_type(), field))
                    .or_else(|| found.sql_type());
                Ok((found, sql_type))
            }
        }
    }
}

pub(crate) fn unassigned_record(name: &str) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("record \"{name}\" is not assigned yet"),
    )
    .with_detail("The tuple structure of a not-yet-assigned record is indeterminate.")
}

pub(crate) fn missing_field(record: &str, field: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("record \"{record}\" has no field \"{field}\""),
    )
}

pub(crate) fn row_record(columns: &[(String, SqlType)], fields: Vec<Value>) -> Value {
    records::record_value(
        RECORD_OID,
        fields,
        columns.iter().map(|(name, _)| name.clone()).collect(),
    )
}

pub(crate) fn record_of(value: &Value) -> Option<&RecordValue> {
    let Value::Defined(defined) = value else {
        return None;
    };
    match defined.as_ref() {
        DefinedValue::Record(record) => Some(record),
        _ => None,
    }
}

pub(crate) fn is_composite_type(sql_type: SqlType) -> bool {
    sql_type
        .defined()
        .is_some_and(|defined| defined.kind == DefinedKind::Composite)
}

fn constant(value: Value, sql_type: Option<SqlType>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: sql_type.or_else(|| value.sql_type()),
            value,
        },
        position,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RoutineFrame {
    pub(crate) variables: Vec<FrameVariable>,
    pub(crate) positional: Vec<usize>,
    pub(crate) plsql: Option<Box<PlsqlFrame>>,
}

impl RoutineFrame {
    fn resolved(&self, index: usize) -> usize {
        self.variables
            .get(index)
            .and_then(|variable| variable.alias)
            .unwrap_or(index)
    }

    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        if let Some(index) = self
            .variables
            .iter()
            .rposition(|variable| variable.name == name && !variable.qualified_only)
        {
            return Some(self.resolved(index));
        }
        let (qualifier, rest) = name.rsplit_once('.')?;
        self.labelled_index(qualifier, rest)
    }

    pub(crate) fn labelled_index(&self, label: &str, name: &str) -> Option<usize> {
        self.variables
            .iter()
            .rposition(|variable| variable.name == name && variable.label.as_deref() == Some(label))
            .map(|index| self.resolved(index))
    }

    pub(crate) fn positional_index(&self, number: usize) -> Option<usize> {
        number
            .checked_sub(1)
            .and_then(|at| self.positional.get(at))
            .copied()
    }

    pub(crate) fn target_index(&self, name: &str) -> Option<usize> {
        match name.strip_prefix('$').and_then(parameter_number) {
            Some(number) => self.positional_index(number),
            None => self.index_of(name),
        }
    }

    pub(crate) fn variable_expr(&self, name: &str, position: usize) -> Option<Expr> {
        let variable = &self.variables[self.index_of(name)?];
        Some(constant(variable.value.clone(), variable.sql_type(), position))
    }

    pub(crate) fn positional_expr(&self, digits: &str, position: usize) -> Option<Expr> {
        self.numbered_expr(parameter_number(digits)?, position)
    }

    pub(crate) fn numbered_expr(&self, number: usize, position: usize) -> Option<Expr> {
        let index = self.positional_index(number)?;
        let variable = &self.variables[index];
        Some(constant(variable.value.clone(), variable.sql_type(), position))
    }

    pub(crate) fn qualified_expr(
        &self,
        qualifier: &str,
        name: &str,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        if let Some(index) = self.index_of(qualifier)
            && self.variables[index].is_composite()
        {
            let (value, sql_type) = self.variables[index].field(name).map_err(|error| error.at(position))?;
            return Ok(Some(constant(value, sql_type, position)));
        }
        let Some(index) = self.labelled_index(qualifier, name) else {
            return Ok(None);
        };
        let variable = &self.variables[index];
        Ok(Some(constant(
            variable.value.clone(),
            variable.sql_type(),
            position,
        )))
    }
}
