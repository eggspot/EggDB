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

fn sparse_vector(text: &str) -> SqlResult<Vec<f32>> {
    let bad = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("malformed sparsevec literal: \"{text}\""),
        )
    };
    let (body, dimensions) = text.trim().rsplit_once('/').ok_or_else(bad)?;
    let dimensions: usize = dimensions.trim().parse().map_err(|_| bad())?;
    if dimensions == 0 || dimensions > eggdb_vector::MAX_DIMENSIONS {
        return Err(SqlError::new(
            SqlState::DATA_EXCEPTION,
            format!("sparsevec must have between 1 and {} dimensions", eggdb_vector::MAX_DIMENSIONS),
        ));
    }
    let inner = body
        .trim()
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(bad)?;
    let mut dense = vec![0.0f32; dimensions];
    for pair in inner.split(',').map(str::trim).filter(|pair| !pair.is_empty()) {
        let (index, value) = pair.split_once(':').ok_or_else(bad)?;
        let index: usize = index.trim().parse().map_err(|_| bad())?;
        let value: f32 = value.trim().parse().map_err(|_| bad())?;
        if index == 0 || index > dimensions || !value.is_finite() {
            return Err(bad());
        }
        dense[index - 1] = value;
    }
    Ok(dense)
}

fn vector_of(value: &Value) -> SqlResult<Option<Vec<f32>>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) if text.trim_start().starts_with('{') => {
            Some(sparse_vector(text)?)
        }
        Value::Text(text) | Value::Bpchar(text) => {
            Some(eggdb_vector::parse(text).map_err(vector_error)?)
        }
        Value::Array(array) => {
            let mut values = Vec::with_capacity(array.items.len());
            for element in &array.items {
                match element.as_float() {
                    Some(number) if number.is_finite() => values.push(number as f32),
                    None if matches!(element, Value::Null) => {
                        return Err(SqlError::new(
                            SqlState::NULL_VALUE_NOT_ALLOWED,
                            "array must not contain nulls",
                        ));
                    }
                    _ => {
                        return Err(SqlError::new(
                            SqlState::DATA_EXCEPTION,
                            "NaN and infinite values are not allowed in vector",
                        ));
                    }
                }
            }
            if values.is_empty() {
                return Err(SqlError::new(
                    SqlState::DATA_EXCEPTION,
                    "vector must have at least 1 dimension",
                ));
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

fn bits_of(value: &Value) -> SqlResult<Option<Vec<bool>>> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::Text(text) | Value::Bpchar(text) => text.clone(),
        other => other.to_text().unwrap_or_default(),
    };
    text.trim()
        .chars()
        .map(|digit| match digit {
            '0' => Ok(false),
            '1' => Ok(true),
            _ => Err(SqlError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("\"{digit}\" is not a valid binary digit"),
            )),
        })
        .collect::<SqlResult<Vec<bool>>>()
        .map(Some)
}

fn bit_distance(name: &str, left: &[bool], right: &[bool]) -> SqlResult<Value> {
    if left.len() != right.len() {
        return Err(SqlError::new(
            SqlState::DATA_EXCEPTION,
            format!("different bit lengths {} and {}", left.len(), right.len()),
        ));
    }
    let differing = left.iter().zip(right).filter(|(a, b)| a != b).count();
    if name == "hamming_distance" {
        return Ok(Value::Double(differing as f64));
    }
    let both = left.iter().zip(right).filter(|(a, b)| **a && **b).count();
    let either = left.iter().zip(right).filter(|(a, b)| **a || **b).count();
    Ok(Value::Double(if either == 0 {
        0.0
    } else {
        1.0 - both as f64 / either as f64
    }))
}

/// The names of pgvector's functions that EggDB evaluates.
pub(crate) const FUNCTIONS: [&str; 11] = [
    "l2_distance",
    "cosine_distance",
    "inner_product",
    "l1_distance",
    "vector_dims",
    "vector_norm",
    "l2_normalize",
    "subvector",
    "binary_quantize",
    "hamming_distance",
    "jaccard_distance",
];

/// pgvector's functions.
pub(crate) fn function(name: &str, arguments: &[Value]) -> SqlResult<Value> {
    match (name, arguments) {
        ("hamming_distance" | "jaccard_distance", [left, right]) => {
            return match (bits_of(left)?, bits_of(right)?) {
                (Some(left), Some(right)) => bit_distance(name, &left, &right),
                _ => Ok(Value::Null),
            };
        }
        ("subvector", [vector, start, count]) => {
            let (Some(vector), Some(start), Some(count)) =
                (vector_of(vector)?, start.as_integer(), count.as_integer())
            else {
                return Ok(Value::Null);
            };
            let end = start.saturating_add(count);
            let from = start.max(1);
            let to = end.min(vector.len() as i64 + 1);
            if count < 1 || from >= to {
                return Err(SqlError::new(
                    SqlState::DATA_EXCEPTION,
                    "vector must have at least 1 dimension",
                ));
            }
            return Ok(Value::Text(eggdb_vector::format(
                &vector[(from - 1) as usize..(to - 1) as usize],
            )));
        }
        ("binary_quantize", [vector]) => {
            return Ok(vector_of(vector)?.map_or(Value::Null, |vector| {
                Value::Text(
                    vector
                        .iter()
                        .map(|value| if *value > 0.0 { '1' } else { '0' })
                        .collect(),
                )
            }));
        }
        _ => {}
    }
    let vectors: Vec<Option<Vec<f32>>> =
        arguments.iter().map(vector_of).collect::<SqlResult<_>>()?;
    let wrong_count = || {
        SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name} takes a different number of arguments"),
        )
    };
    match (name, vectors.as_slice()) {
        (_, [_, None] | [None, _] | [None]) => Ok(Value::Null),
        ("vector_dims", [Some(vector)]) => Ok(Value::Integer(vector.len() as i32)),
        ("vector_norm", [Some(vector)]) => Ok(Value::Double(eggdb_vector::norm(vector))),
        ("l2_normalize", [Some(vector)]) => Ok(Value::Text(eggdb_vector::format(
            &eggdb_vector::l2_normalize(vector),
        ))),
        (
            "l2_distance" | "cosine_distance" | "inner_product" | "l1_distance",
            [Some(left), Some(right)],
        ) => Ok(Value::Double(distance(name, left, right)?)),
        _ => Err(wrong_count()),
    }
}

/// `eggdb_embed(text [, dimensions])`, the local hashed embedding as vector text, and
/// `rrf_score(rank [, k])`, one list's reciprocal rank fusion term (sum them across lists).
pub(crate) fn ai_function(name: &str, arguments: &[Value]) -> SqlResult<Value> {
    let bad = |message: &str| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message.to_owned());
    match (name, arguments) {
        (_, [Value::Null, ..]) => Ok(Value::Null),
        ("eggdb_embed", [text]) | ("eggdb_embed", [text, _]) => {
            let dimensions = match arguments.get(1) {
                None => eggdb_ai::DEFAULT_DIMENSIONS,
                Some(value) => value
                    .as_integer()
                    .and_then(|dimensions| usize::try_from(dimensions).ok())
                    .filter(|dimensions| (1..=eggdb_vector::MAX_DIMENSIONS).contains(dimensions))
                    .ok_or_else(|| bad("dimensions must be between 1 and 16000"))?,
            };
            let text = match text {
                Value::Text(text) | Value::Bpchar(text) => text.clone(),
                other => other.to_text().unwrap_or_default(),
            };
            Ok(Value::Text(eggdb_vector::format(&eggdb_ai::embed(
                &text, dimensions,
            ))))
        }
        ("rrf_score", [rank]) | ("rrf_score", [rank, _]) => {
            let rank = rank
                .as_integer()
                .filter(|rank| *rank >= 1)
                .ok_or_else(|| bad("a rank starts at 1"))?;
            let k = match arguments.get(1) {
                None => eggdb_ai::DEFAULT_RRF_K,
                Some(value) => value
                    .as_float()
                    .filter(|k| *k >= 0.0)
                    .ok_or_else(|| bad("k must not be negative"))?,
            };
            Ok(Value::Double(eggdb_ai::reciprocal_rank_fusion(
                &[Some(rank as u64)],
                k,
            )))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name} takes one or two arguments"),
        )),
    }
}
