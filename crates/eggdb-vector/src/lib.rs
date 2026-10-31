//! Vectors for EggDB (D203): the text form pgvector documents (`[1,2,3]`), the distances its
//! operators compute, and a hierarchical navigable small world graph (HNSW, from Malkov and
//! Yashunin's paper) for approximate nearest neighbour search.

mod hnsw;

pub use hnsw::{Hnsw, HnswOptions};

/// The most dimensions a vector may have, as pgvector allows.
pub const MAX_DIMENSIONS: usize = 16_000;

/// Why text is not a vector, or two vectors cannot be combined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorError(pub String);

impl std::fmt::Display for VectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VectorError {}

/// Reads a vector written as pgvector writes it: `[1, 2.5, -3e2]`.
pub fn parse(text: &str) -> Result<Vec<f32>, VectorError> {
    let malformed = || VectorError(format!("invalid input syntax for type vector: \"{text}\""));
    let inner = text.trim().strip_prefix('[').and_then(|rest| rest.strip_suffix(']')).ok_or_else(malformed)?;
    if inner.trim().is_empty() {
        return Err(VectorError("vector must have at least 1 dimension".to_owned()));
    }
    let mut values = Vec::new();
    for part in inner.split(',') {
        if values.len() >= MAX_DIMENSIONS {
            return Err(VectorError(format!("vector cannot have more than {MAX_DIMENSIONS} dimensions")));
        }
        let value: f32 = part.trim().parse().map_err(|_| malformed())?;
        if !value.is_finite() {
            return Err(VectorError("NaN and infinite values are not allowed in vector".to_owned()));
        }
        values.push(value);
    }
    Ok(values)
}

/// Writes a vector as pgvector does.
pub fn format(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(|value| value.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn same_dimensions(left: &[f32], right: &[f32]) -> Result<(), VectorError> {
    if left.len() == right.len() {
        Ok(())
    } else {
        Err(VectorError(format!("different vector dimensions {} and {}", left.len(), right.len())))
    }
}

/// The Euclidean distance (`<->`).
pub fn l2_distance(left: &[f32], right: &[f32]) -> Result<f64, VectorError> {
    same_dimensions(left, right)?;
    Ok(left.iter().zip(right).map(|(a, b)| f64::from(a - b).powi(2)).sum::<f64>().sqrt())
}

/// The inner product.
pub fn inner_product(left: &[f32], right: &[f32]) -> Result<f64, VectorError> {
    same_dimensions(left, right)?;
    Ok(left.iter().zip(right).map(|(a, b)| f64::from(*a) * f64::from(*b)).sum())
}

/// The cosine distance (`<=>`): 1 minus the cosine of the angle; NaN when a vector is zero, as
/// in pgvector.
pub fn cosine_distance(left: &[f32], right: &[f32]) -> Result<f64, VectorError> {
    let dot = inner_product(left, right)?;
    let norms = norm(left) * norm(right);
    if norms == 0.0 {
        return Ok(f64::NAN);
    }
    Ok(1.0 - (dot / norms).clamp(-1.0, 1.0))
}

/// The taxicab distance (`<+>`).
pub fn l1_distance(left: &[f32], right: &[f32]) -> Result<f64, VectorError> {
    same_dimensions(left, right)?;
    Ok(left.iter().zip(right).map(|(a, b)| f64::from(a - b).abs()).sum())
}

/// The Euclidean norm.
pub fn norm(values: &[f32]) -> f64 {
    values.iter().map(|value| f64::from(*value).powi(2)).sum::<f64>().sqrt()
}

/// The vector scaled to norm 1 (a zero vector stays zero).
pub fn l2_normalize(values: &[f32]) -> Vec<f32> {
    let length = norm(values);
    if length == 0.0 {
        return values.to_vec();
    }
    values.iter().map(|value| (f64::from(*value) / length) as f32).collect()
}

/// Adds (`+`) or subtracts (`-`) element by element.
pub fn combine(left: &[f32], right: &[f32], subtract: bool) -> Result<Vec<f32>, VectorError> {
    same_dimensions(left, right)?;
    let combined: Vec<f32> = left.iter().zip(right).map(|(a, b)| if subtract { a - b } else { a + b }).collect();
    if combined.iter().any(|value| !value.is_finite()) {
        return Err(VectorError("value out of range: overflow".to_owned()));
    }
    Ok(combined)
}

#[cfg(test)]
mod tests;
