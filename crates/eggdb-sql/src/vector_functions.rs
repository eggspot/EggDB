use crate::ast::BinaryOperator;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

fn vector_error(error: eggdb_vector::VectorError) -> SqlError {
    let state = if error.0.starts_with("different vector dimensions") {
        SqlState::DATA_EXCEPTION
    } else {
        SqlState::INVALID_TEXT_REPRESENTATION
    };
    SqlError::new(state, error.0)
}

fn vector_of(value: &Value) -> SqlResult<Option<Vec<f32>>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(eggdb_vector::parse(text).map_err(vector_error)?),
        Value::Array(array) => {
            let mut values = Vec::with_capacity(array.items.len());
            for element in &array.items {
                match element.as_float() {
                    Some(number) if number.is_finite() => values.push(number as f32),
                    None if matches!(element, Value::Null) => {
                        return Err(SqlError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "array must not contain nulls"));
                    }
                    _ => return Err(SqlError::new(SqlState::DATA_EXCEPTION, "NaN and infinite values are not allowed in vector")),
                }
            }
            if values.is_empty() {
                return Err(SqlError::new(SqlState::DATA_EXCEPTION, "vector must have at least 1 dimension"));
            }
            Some(values)
        }
        other => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("operator does not exist: {} for vectors", other.type_name()),
            ));
        }
    })
}

fn distance(name: &str, left: &[f32], right: &[f32]) -> SqlResult<f64> {
    match name {
        "l2_distance" => eggdb_vector::l2_distance(left, right),
        "cosine_distance" => eggdb_vector::cosine_distance(left, right),
        "inner_product" => eggdb_vector::inner_product(left, right),
        _ => eggdb_vector::l1_distance(left, right),
    }
    .map_err(vector_error)
}

/// pgvector's distance operators: `<->` Euclidean, `<=>` cosine, `<#>` negative inner product,
/// `<+>` taxicab, on vectors written as `[1,2,3]` or as numeric arrays.
pub(crate) fn operator(operator: BinaryOperator, left: &Value, right: &Value) -> SqlResult<Value> {
    let (Some(left), Some(right)) = (vector_of(left)?, vector_of(right)?) else {
        return Ok(Value::Null);
    };
    let value = match operator {
        BinaryOperator::VectorL2Distance => distance("l2_distance", &left, &right)?,
        BinaryOperator::VectorCosineDistance => distance("cosine_distance", &left, &right)?,
        BinaryOperator::VectorNegativeInnerProduct => -distance("inner_product", &left, &right)?,
        _ => distance("l1_distance", &left, &right)?,
    };
    Ok(Value::Double(value))
}

/// pgvector's functions.
pub(crate) fn function(name: &str, arguments: &[Value]) -> SqlResult<Value> {
    let vectors: Vec<Option<Vec<f32>>> = arguments.iter().map(vector_of).collect::<SqlResult<_>>()?;
    let wrong_count = || SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("function {name} takes a different number of arguments"));
    match (name, vectors.as_slice()) {
        (_, [_, None] | [None, _] | [None]) => Ok(Value::Null),
        ("vector_dims", [Some(vector)]) => Ok(Value::Integer(vector.len() as i32)),
        ("vector_norm", [Some(vector)]) => Ok(Value::Double(eggdb_vector::norm(vector))),
        ("l2_normalize", [Some(vector)]) => Ok(Value::Text(eggdb_vector::format(&eggdb_vector::l2_normalize(vector)))),
        ("l2_distance" | "cosine_distance" | "inner_product" | "l1_distance", [Some(left), Some(right)]) => {
            Ok(Value::Double(distance(name, left, right)?))
        }
        _ => Err(wrong_count()),
    }
}
