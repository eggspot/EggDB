//! PostgreSQL's administration functions that monitoring dashboards and admin tools call
//! (D1301), answered as a server with no per-relation files, no JIT and no lock waits answers
//! them: `pg_size_bytes`, `pg_column_size`, `pg_column_compression`, `pg_relation_filepath`,
//! `pg_relation_filenode`, `pg_blocking_pids`, `pg_safe_snapshot_blocking_pids`,
//! `pg_notification_queue_usage`, `pg_jit_available`, `pg_reload_conf`, `pg_current_logfile`,
//! `pg_my_temp_schema` and `pg_is_other_temp_schema`.

use crate::arrays::{ArrayElement, ArrayValue};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

const FUNCTIONS: [&str; 13] = [
    "pg_size_bytes",
    "pg_column_size",
    "pg_column_compression",
    "pg_relation_filepath",
    "pg_relation_filenode",
    "pg_blocking_pids",
    "pg_safe_snapshot_blocking_pids",
    "pg_notification_queue_usage",
    "pg_jit_available",
    "pg_reload_conf",
    "pg_current_logfile",
    "pg_my_temp_schema",
    "pg_is_other_temp_schema",
];

const UNITS: [(&str, i64); 6] = [
    ("bytes", 1),
    ("b", 1),
    ("kb", 1 << 10),
    ("mb", 1 << 20),
    ("gb", 1 << 30),
    ("tb", 1 << 40),
];

fn invalid_size(text: &str) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid size: \"{text}\""))
}

fn size_bytes(text: &str) -> SqlResult<i64> {
    let trimmed = text.trim();
    let number_end = trimmed
        .char_indices()
        .find(|(at, character)| !(character.is_ascii_digit() || *character == '.' || ((*character == '-' || *character == '+') && *at == 0)))
        .map_or(trimmed.len(), |(at, _)| at);
    let (number, unit) = trimmed.split_at(number_end);
    let value: f64 = number.parse().map_err(|_| invalid_size(text))?;
    let unit = unit.trim().to_ascii_lowercase();
    let factor = if unit.is_empty() {
        1
    } else if unit == "pb" {
        1i64 << 50
    } else {
        UNITS.iter().find(|(name, _)| *name == unit).map(|(_, factor)| *factor).ok_or_else(|| {
            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid size: \"{text}\""))
                .with_detail(format!("Invalid size unit: \"{unit}\"."))
                .with_hint("Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\".")
        })?
    };
    let bytes = value * factor as f64;
    if !bytes.is_finite() || bytes.abs() >= 9.2e18 {
        return Err(SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"));
    }
    Ok(bytes.round() as i64)
}

fn column_size(value: &Value) -> Option<i64> {
    Some(match value {
        Value::Null => return None,
        Value::Boolean(_) => 1,
        Value::SmallInt(_) => 2,
        Value::Integer(_) | Value::Real(_) | Value::Date(_) => 4,
        Value::BigInt(_) | Value::Double(_) | Value::Timestamp(_) | Value::TimestampTz(_) | Value::Time(_) => 8,
        Value::Uuid(_) => 16,
        Value::Bytea(bytes) => varlena(bytes.len()),
        other => varlena(other.to_text().map_or(0, |text| text.len())),
    })
}

fn varlena(length: usize) -> i64 {
    let header = if length < 127 { 1 } else { 4 };
    i64::try_from(length + header).unwrap_or(i64::MAX)
}

fn empty_integers() -> Value {
    Value::Array(Box::new(ArrayValue::new(ArrayElement::Integer, Vec::new())))
}

/// Runs one of these functions in PostgreSQL's dialect; `None` elsewhere or for other names.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::PostgreSql {
        return None;
    }
    Some(match (name, values) {
        ("pg_size_bytes", [Value::Null]) => Ok(Value::Null),
        ("pg_size_bytes", [text]) => size_bytes(&text.to_text().unwrap_or_default()).map(Value::BigInt),
        ("pg_column_size", [value]) => Ok(column_size(value).map_or(Value::Null, |size| Value::Integer(i32::try_from(size).unwrap_or(i32::MAX)))),
        ("pg_column_compression" | "pg_relation_filepath" | "pg_relation_filenode" | "pg_current_logfile", _) => Ok(Value::Null),
        ("pg_blocking_pids" | "pg_safe_snapshot_blocking_pids", [_]) => Ok(empty_integers()),
        ("pg_notification_queue_usage", []) => Ok(Value::Double(0.0)),
        ("pg_jit_available", []) => Ok(Value::Boolean(false)),
        ("pg_reload_conf", []) => Ok(Value::Boolean(true)),
        ("pg_my_temp_schema", []) => Ok(Value::Oid(0)),
        ("pg_is_other_temp_schema", [_]) => Ok(Value::Boolean(false)),
        _ => return None,
    })
}

/// The result type of one of these functions in PostgreSQL's dialect.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::PostgreSql {
        return None;
    }
    Some(match name {
        "pg_size_bytes" => SqlType::BigInt,
        "pg_column_size" => SqlType::Integer,
        "pg_blocking_pids" | "pg_safe_snapshot_blocking_pids" => SqlType::Array(ArrayElement::Integer),
        "pg_notification_queue_usage" => SqlType::Double,
        "pg_jit_available" | "pg_reload_conf" | "pg_is_other_temp_schema" => SqlType::Boolean,
        "pg_my_temp_schema" | "pg_relation_filenode" => SqlType::Catalog(crate::pg_catalog::CatalogType::Oid),
        _ => SqlType::Text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_postgresql_reads_them() {
        assert_eq!(size_bytes("1 kB").unwrap(), 1_024);
        assert_eq!(size_bytes("1.5GB").unwrap(), 1_610_612_736);
        assert_eq!(size_bytes(" 100 bytes ").unwrap(), 100);
        assert_eq!(size_bytes("42").unwrap(), 42);
        assert!(size_bytes("1 XB").is_err());
        assert!(size_bytes("kB").is_err());
        assert_eq!(column_size(&Value::Integer(1)), Some(4));
        assert_eq!(column_size(&Value::Text("abc".to_owned())), Some(4));
    }
}
