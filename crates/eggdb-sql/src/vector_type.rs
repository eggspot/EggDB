//! The `vector` type compatible with pgvector's: a list of 1 to 16,000 finite single-precision
//! floats, written `[1,2,3]`, sent in binary as a 16-bit dimension count, 16 unused bits and
//! the floats, all big-endian (D796).

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedValue, Value};
use std::cmp::Ordering;

pub(crate) const VECTOR_OID: u32 = 9004;
pub(crate) const MOST_DIMENSIONS: usize = 16_000;

pub(crate) fn value(values: Vec<f32>) -> Value {
    Value::Defined(Box::new(DefinedValue::Vector(values)))
}

pub(crate) fn vector_of(value: &Value) -> Option<&[f32]> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Vector(values) => Some(values),
            _ => None,
        },
        _ => None,
    }
}

fn invalid_text(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type vector: \"{text}\""),
    )
}

pub(crate) fn checked(values: Vec<f32>) -> SqlResult<Vec<f32>> {
    if values.is_empty() {
        return Err(SqlError::new(
            SqlState::DATA_EXCEPTION,
            "vector must have at least 1 dimension",
        ));
    }
    if values.len() > MOST_DIMENSIONS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("vector cannot have more than {MOST_DIMENSIONS} dimensions"),
        ));
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(SqlError::new(
            SqlState::DATA_EXCEPTION,
            "NaN and infinite values are not allowed in vector",
        ));
    }
    Ok(values)
}

pub(crate) fn parse_text(text: &str) -> SqlResult<Vec<f32>> {
    checked(eggdb_vector::parse(text).map_err(|_| invalid_text(text))?)
}

pub(crate) fn from_array(items: &[Value]) -> SqlResult<Vec<f32>> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        match item.as_float() {
            Some(number) => values.push(number as f32),
            None if item.is_null() => {
                return Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "array must not contain nulls",
                ));
            }
            None => {
                return Err(SqlError::new(
                    SqlState::CANNOT_COERCE,
                    format!("cannot cast type {} to vector", item.type_name()),
                ));
            }
        }
    }
    checked(values)
}

pub(crate) fn to_text(values: &[f32]) -> String {
    eggdb_vector::format(values)
}

pub(crate) fn compare(left: &[f32], right: &[f32]) -> Ordering {
    for (a, b) in left.iter().zip(right) {
        match a.total_cmp(b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    left.len().cmp(&right.len())
}

pub(crate) fn binary(values: &[f32]) -> Option<Vec<u8>> {
    let count = i16::try_from(values.len()).ok()?;
    let mut out = Vec::with_capacity(4 + values.len() * 4);
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
    Some(out)
}

pub(crate) fn from_binary(bytes: &[u8]) -> SqlResult<Vec<f32>> {
    let incorrect = || {
        SqlError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "incorrect binary data format",
        )
    };
    let [high, low, _, _, rest @ ..] = bytes else {
        return Err(incorrect());
    };
    let count = usize::try_from(i16::from_be_bytes([*high, *low])).map_err(|_| incorrect())?;
    if rest.len() != count * 4 {
        return Err(incorrect());
    }
    let values = rest
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| f32::from_be_bytes(*word))
        .collect();
    checked(values)
}

/// The check a `vector(n)` column carries, so a stored vector has exactly `n` dimensions.
pub(crate) fn dimensions_check(column: &crate::ast::Name, dimensions: u32) -> crate::ast::ConstraintSpec {
    use crate::ast::{BinaryOperator, ConstraintKind, ConstraintSpec, Expr, ExprKind, Literal};
    let position = column.position;
    let at = |kind: ExprKind| Expr { kind, position };
    let expr = at(ExprKind::Binary(
        BinaryOperator::Equal,
        Box::new(at(ExprKind::Function {
            name: "vector_dims".to_owned(),
            arguments: vec![at(ExprKind::Column {
                qualifier: None,
                name: column.text.clone(),
            })],
        })),
        Box::new(at(ExprKind::Literal(Literal::Integer(i64::from(dimensions))))),
    ));
    ConstraintSpec {
        name: None,
        kind: ConstraintKind::Check {
            expr,
            text: format!(
                "vector_dims(\"{}\") = {dimensions}",
                column.text.replace('"', "\"\"")
            ),
        },
        position,
        column: Some(column.clone()),
        using_index: None,
        not_valid: false,
    }
}

pub(crate) fn held_bytes(values: &[f32]) -> usize {
    values.len() * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_binary_round_trip_and_bad_input_is_refused() {
        let parsed = parse_text("[1, 2.5,-3]").expect("a vector");
        assert_eq!(parsed, vec![1.0, 2.5, -3.0]);
        assert_eq!(to_text(&parsed), "[1,2.5,-3]");
        let bytes = binary(&parsed).expect("binary");
        assert_eq!(&bytes[..4], &[0, 3, 0, 0]);
        assert_eq!(from_binary(&bytes).expect("decoded"), parsed);
        assert!(from_binary(&bytes[..bytes.len() - 1]).is_err());
        assert!(parse_text("[]").is_err());
        assert!(parse_text("[1,NaN]").is_err() || parse_text("[1,nan]").is_err());
        assert!(parse_text("1,2").is_err());
    }

    #[test]
    fn vectors_order_element_by_element_then_by_length() {
        assert_eq!(compare(&[1.0, 2.0], &[1.0, 3.0]), Ordering::Less);
        assert_eq!(compare(&[1.0, 2.0], &[1.0]), Ordering::Greater);
        assert_eq!(compare(&[1.0], &[1.0]), Ordering::Equal);
    }

    fn run(session: &mut crate::Session<'_>, sql: &str) -> Result<String, crate::SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn a_vector_column_stores_checks_and_orders_by_distance() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        for sql in [
            "CREATE TABLE items (id integer PRIMARY KEY, embedding vector(3))",
            "INSERT INTO items VALUES (1, '[1,2,3]'), (2, '[3,2,1]'), (3, ARRAY[1,1,1])",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT id FROM items ORDER BY embedding <-> '[1,1,1]' LIMIT 1").unwrap(),
            "id\n3\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT embedding FROM items WHERE id = 1").unwrap(),
            "embedding\n[1,2,3]\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT pg_typeof(embedding) AS kind FROM items WHERE id = 2").unwrap(),
            "kind\nvector\nSELECT 1"
        );
        assert!(run(&mut session, "INSERT INTO items VALUES (4, '[1,2]')").is_err());
        assert!(run(&mut session, "INSERT INTO items VALUES (5, '[1,x,3]')").is_err());
    }
}

