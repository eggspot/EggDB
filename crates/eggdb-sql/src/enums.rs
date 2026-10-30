use crate::ast::Expr;
use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::types::{DefinedKind, DefinedValue, SqlType, Value};
use crate::user_types::EnumLabel;

/// A label of an enum: the type it belongs to, its place in the type's order and its text.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumValue {
    /// The OID of the enum type.
    pub type_oid: u32,
    /// The label's place in the type's order: labels compare by it, and adding a label never
    /// moves the others.
    pub sort_order: f64,
    /// The label as written.
    pub label: String,
}

pub(crate) const FUNCTIONS: [&str; 3] = ["enum_first", "enum_last", "enum_range"];

pub(crate) fn value(type_oid: u32, label: &EnumLabel) -> Value {
    Value::Defined(Box::new(DefinedValue::Enum(EnumValue {
        type_oid,
        sort_order: label.sort_order,
        label: label.label.clone(),
    })))
}

pub(crate) fn decoded(type_oid: u32, sort_order: f64) -> SqlResult<Value> {
    let label = crate::user_types::enum_label(type_oid, sort_order)?;
    Ok(Value::Defined(Box::new(DefinedValue::Enum(EnumValue {
        type_oid,
        sort_order,
        label,
    }))))
}

fn enum_type_of(evaluator: &Evaluator<'_>, argument: &Expr, name: &str) -> SqlResult<u32> {
    let declared = crate::typing::static_type(argument, evaluator.scope);
    match declared.and_then(SqlType::defined) {
        Some(defined) if defined.kind == DefinedKind::Enum => Ok(defined.oid),
        _ => {
            let value = evaluator.eval(argument, None)?;
            match value.sql_type().and_then(SqlType::defined) {
                Some(defined) if defined.kind == DefinedKind::Enum => Ok(defined.oid),
                _ => Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function {name}({}) does not exist", value.type_name()),
                )
                .with_hint(
                    "No function matches the given name and argument types. You might need to add explicit type casts.",
                )),
            }
        }
    }
}

fn labels_of(type_oid: u32) -> SqlResult<Vec<EnumLabel>> {
    crate::user_types::with_type(type_oid, |found| found.labels().to_vec())
}

fn sort_of(value: &Value) -> Option<f64> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Enum(label) => Some(label.sort_order),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn function(
    evaluator: &Evaluator<'_>,
    name: &str,
    arguments: &[Expr],
) -> SqlResult<Value> {
    let Some(first) = arguments.first() else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name}() does not exist"),
        ));
    };
    let type_oid = enum_type_of(evaluator, first, name)?;
    let labels = labels_of(type_oid)?;
    match (name, arguments.len()) {
        ("enum_first", 1) => Ok(labels
            .first()
            .map_or(Value::Null, |label| value(type_oid, label))),
        ("enum_last", 1) => Ok(labels
            .last()
            .map_or(Value::Null, |label| value(type_oid, label))),
        ("enum_range", 1) => Ok(text_array(labels.iter())),
        ("enum_range", 2) => {
            let hint = Some(crate::types::DefinedType::enumerated(type_oid));
            let low = sort_of(&evaluator.eval(first, hint)?);
            let high = sort_of(&evaluator.eval(&arguments[1], hint)?);
            Ok(text_array(labels.iter().filter(|label| {
                low.is_none_or(|low| label.sort_order >= low)
                    && high.is_none_or(|high| label.sort_order <= high)
            })))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name} does not exist with {} arguments", arguments.len()),
        )),
    }
}

fn text_array<'l>(labels: impl Iterator<Item = &'l EnumLabel>) -> Value {
    Value::Array(Box::new(ArrayValue::new(
        ArrayElement::Text,
        labels.map(|label| Value::Text(label.label.clone())).collect(),
    )))
}

pub(crate) fn result_type(name: &str, argument: Option<SqlType>) -> Option<SqlType> {
    match name {
        "enum_first" | "enum_last" => argument,
        "enum_range" => Some(SqlType::Array(ArrayElement::Text)),
        _ => None,
    }
}
