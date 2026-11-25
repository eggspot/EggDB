//! Dynamic data masking (D1348, D1360): SQL Server's `MASKED WITH (FUNCTION = ...)` masks kept
//! in a catalog space of their own, `UNMASK` grants, and the masking of the values a statement
//! returns to a role that holds no `UNMASK`.

use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::collections::{HashMap, HashSet};

pub(crate) const COLUMN_MASK_SPACE: u8 = 0x24;
const MASK_RECORDS: u8 = 1;
const UNMASK_RECORDS: u8 = 2;
const RECORD_VERSION: u8 = 1;
const DEFAULT_TEXT: &str = "xxxx";
const EMAIL_TAIL: &str = "XXX@XXXX.com";
const DEFAULT_DATE_DAYS: i32 = -36_524;
const DEFAULT_TIMESTAMP_MICROS: i64 = DEFAULT_DATE_DAYS as i64 * 86_400_000_000;

/// A mask function, as SQL Server's `MASKED WITH (FUNCTION = '...')` names it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MaskFunction {
    /// `default()`: `xxxx` (shorter for shorter text), 0, 1900-01-01, one zero byte.
    Default,
    /// `email()`: the first character, then `XXX@XXXX.com`.
    Email,
    /// `random(low, high)`: a number drawn between the bounds, for numbers only.
    Random { low: f64, high: f64 },
    /// `partial(prefix, "padding", suffix)`: the first `prefix` characters, the padding, and
    /// the last `suffix` characters.
    Partial { prefix: usize, padding: String, suffix: usize },
}

impl MaskFunction {
    /// The function in `text`, such as `partial(1, "XXX-XX-", 4)`.
    pub(crate) fn parse(text: &str) -> SqlResult<Self> {
        let invalid = || {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("'{text}' is not a mask function: use default(), email(), random(low, high) or partial(prefix, \"padding\", suffix)"),
            )
            .with_sqlserver_code(16003)
        };
        let trimmed = text.trim();
        let open = trimmed.find('(').ok_or_else(invalid)?;
        let name = trimmed[..open].trim().to_ascii_lowercase();
        let arguments = trimmed[open + 1..].strip_suffix(')').ok_or_else(invalid)?.trim();
        let parts = split_arguments(arguments).ok_or_else(invalid)?;
        match (name.as_str(), parts.as_slice()) {
            ("default", []) => Ok(Self::Default),
            ("email", []) => Ok(Self::Email),
            ("random", [low, high]) => {
                let low: f64 = low.parse().map_err(|_| invalid())?;
                let high: f64 = high.parse().map_err(|_| invalid())?;
                if !(low.is_finite() && high.is_finite()) || low > high {
                    return Err(invalid());
                }
                Ok(Self::Random { low, high })
            }
            ("partial", [prefix, padding, suffix]) => {
                let padding = padding
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .ok_or_else(invalid)?;
                Ok(Self::Partial {
                    prefix: prefix.parse().map_err(|_| invalid())?,
                    padding: padding.to_owned(),
                    suffix: suffix.parse().map_err(|_| invalid())?,
                })
            }
            _ => Err(invalid()),
        }
    }

    /// The function as SQL Server's catalogs show it.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Default => "default()".to_owned(),
            Self::Email => "email()".to_owned(),
            Self::Random { low, high } => format!("random({low}, {high})"),
            Self::Partial { prefix, padding, suffix } => format!("partial({prefix}, \"{padding}\", {suffix})"),
        }
    }

    /// `value` as a user without `UNMASK` sees it; NULL stays NULL.
    pub(crate) fn apply(&self, value: &Value) -> Value {
        match (self, value) {
            (_, Value::Null) => Value::Null,
            (Self::Email, Value::Text(text)) => Value::Text(email_mask(text)),
            (Self::Email, Value::Bpchar(text)) => Value::Bpchar(email_mask(text)),
            (Self::Partial { prefix, padding, suffix }, Value::Text(text)) => {
                Value::Text(partial_mask(text, *prefix, padding, *suffix))
            }
            (Self::Partial { prefix, padding, suffix }, Value::Bpchar(text)) => {
                Value::Bpchar(partial_mask(text, *prefix, padding, *suffix))
            }
            (Self::Random { low, high }, number) if is_number(number) => random_number(number, *low, *high),
            (_, other) => default_mask(other),
        }
    }
}

fn split_arguments(text: &str) -> Option<Vec<String>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => {
                parts.push(current.trim().to_owned());
                current.clear();
            }
            other => current.push(other),
        }
    }
    if quoted {
        return None;
    }
    parts.push(current.trim().to_owned());
    Some(parts)
}

fn email_mask(text: &str) -> String {
    let first: String = text.chars().take(1).collect();
    format!("{first}{EMAIL_TAIL}")
}

fn partial_mask(text: &str, prefix: usize, padding: &str, suffix: usize) -> String {
    let characters: Vec<char> = text.chars().collect();
    if prefix + suffix >= characters.len() {
        return padding.to_owned();
    }
    let head: String = characters[..prefix].iter().collect();
    let tail: String = characters[characters.len() - suffix..].iter().collect();
    format!("{head}{padding}{tail}")
}

fn is_number(value: &Value) -> bool {
    matches!(
        value,
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Real(_) | Value::Double(_) | Value::Numeric(_)
    )
}

fn random_number(like: &Value, low: f64, high: f64) -> Value {
    let fraction = crate::math_functions::random_double().unwrap_or(0.0);
    let drawn = low + (high - low) * fraction;
    match like {
        Value::SmallInt(_) => Value::SmallInt(drawn.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16),
        Value::Integer(_) => Value::Integer(drawn.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32),
        Value::BigInt(_) => Value::BigInt(drawn.round() as i64),
        Value::Real(_) => Value::Real(drawn as f32),
        Value::Numeric(_) => Numeric::parse(&format!("{drawn:.4}")).map_or(Value::Null, Value::Numeric),
        _ => Value::Double(drawn),
    }
}

fn default_mask(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Boolean(_) => Value::Boolean(false),
        Value::SmallInt(_) => Value::SmallInt(0),
        Value::Integer(_) => Value::Integer(0),
        Value::BigInt(_) => Value::BigInt(0),
        Value::Real(_) => Value::Real(0.0),
        Value::Double(_) => Value::Double(0.0),
        Value::Numeric(_) => Numeric::parse("0").map_or(Value::Null, Value::Numeric),
        Value::Text(text) => Value::Text(DEFAULT_TEXT.chars().take(text.chars().count().min(4)).collect()),
        Value::Bpchar(text) => Value::Bpchar(DEFAULT_TEXT.chars().take(text.chars().count().min(4)).collect()),
        Value::Date(_) => Value::Date(DEFAULT_DATE_DAYS),
        Value::Timestamp(_) => Value::Timestamp(DEFAULT_TIMESTAMP_MICROS),
        Value::TimestampTz(_) => Value::TimestampTz(DEFAULT_TIMESTAMP_MICROS),
        Value::Time(_) => Value::Time(0),
        Value::TimeTz { .. } => Value::TimeTz { micros: 0, offset_seconds: 0 },
        Value::Bytea(_) => Value::Bytea(vec![0]),
        Value::Json(_) | Value::Jsonb(_) => Value::Text(DEFAULT_TEXT.to_owned()),
        _ => Value::Text(DEFAULT_TEXT.to_owned()),
    }
}

fn space(kind: u8) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(COLUMN_MASK_SPACE);
    key.push(kind);
    key
}

fn mask_key(table: &str, column: &str) -> Vec<u8> {
    let mut key = space(MASK_RECORDS);
    key.extend_from_slice(table.as_bytes());
    key.push(0);
    key.extend_from_slice(column.as_bytes());
    key
}

fn unmask_key(role: &str) -> Vec<u8> {
    let mut key = space(UNMASK_RECORDS);
    key.extend_from_slice(role.as_bytes());
    key
}

/// A column's mask, as stored.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnMask {
    pub(crate) table: String,
    pub(crate) column: String,
    pub(crate) function: MaskFunction,
}

/// Masks `table.column` with `function`, replacing its mask.
pub(crate) fn set_mask(writer: &mut dyn KeyValueWrite, table: &str, column: &str, function: &MaskFunction) -> SqlResult<()> {
    let mut record = vec![RECORD_VERSION];
    record.extend_from_slice(function.text().as_bytes());
    writer.put(&mask_key(table, column), &record)
}

/// Removes the mask of `table.column`; whether there was one.
pub(crate) fn drop_mask(writer: &mut dyn KeyValueWrite, table: &str, column: &str) -> SqlResult<bool> {
    writer.delete(&mask_key(table, column))
}

/// Every mask of every table.
pub(crate) fn every_mask(reader: &dyn KeyValueRead) -> SqlResult<Vec<ColumnMask>> {
    let start = space(MASK_RECORDS);
    let end = prefix_end(&start);
    let mut masks = Vec::new();
    let mut failure = None;
    reader.scan(&start, &end, ScanDirection::Forward, &mut |key, value| {
        let named = &key[start.len()..];
        let parsed = named
            .iter()
            .position(|byte| *byte == 0)
            .and_then(|at| {
                let table = std::str::from_utf8(&named[..at]).ok()?;
                let column = std::str::from_utf8(&named[at + 1..]).ok()?;
                let text = std::str::from_utf8(value.strip_prefix(&[RECORD_VERSION])?).ok()?;
                Some((table.to_owned(), column.to_owned(), text.to_owned()))
            });
        match parsed {
            Some((table, column, text)) => match MaskFunction::parse(&text) {
                Ok(function) => masks.push(ColumnMask { table, column, function }),
                Err(error) => failure = Some(error),
            },
            None => failure = Some(crate::encoding::corrupt("column mask")),
        }
        Ok(failure.is_none())
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(masks),
    }
}

/// Removes every mask of `table`, when the table goes.
pub(crate) fn forget_table(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    let mut start = space(MASK_RECORDS);
    start.extend_from_slice(table.as_bytes());
    start.push(0);
    let end = prefix_end(&start);
    let mut keys = Vec::new();
    writer.scan(&start, &end, ScanDirection::Forward, &mut |key, _| {
        keys.push(key.to_vec());
        Ok(true)
    })?;
    for key in keys {
        writer.delete(&key)?;
    }
    Ok(())
}

/// Moves the masks of table `from` to its new name `to`.
pub(crate) fn move_table(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    if from == to {
        return Ok(());
    }
    let mut start = space(MASK_RECORDS);
    start.extend_from_slice(from.as_bytes());
    start.push(0);
    let end = prefix_end(&start);
    let mut moved = Vec::new();
    writer.scan(&start, &end, ScanDirection::Forward, &mut |key, value| {
        moved.push((key.to_vec(), value.to_vec()));
        Ok(true)
    })?;
    for (key, value) in moved {
        let column = std::str::from_utf8(&key[start.len()..]).map_err(|_| crate::encoding::corrupt("column mask"))?;
        writer.put(&mask_key(to, column), &value)?;
        writer.delete(&key)?;
    }
    Ok(())
}

/// Moves the mask of `table.from` to the column's new name `to`.
pub(crate) fn rename_column(writer: &mut dyn KeyValueWrite, table: &str, from: &str, to: &str) -> SqlResult<()> {
    let old = mask_key(table, from);
    if let Some(value) = writer.get(&old)? {
        writer.put(&mask_key(table, to), &value)?;
        writer.delete(&old)?;
    }
    Ok(())
}

/// Gives `role` the `UNMASK` permission on the whole database.
pub(crate) fn grant_unmask(writer: &mut dyn KeyValueWrite, role: &str) -> SqlResult<()> {
    writer.put(&unmask_key(role), &[RECORD_VERSION])
}

/// Takes the database-wide `UNMASK` permission from `role`; whether it held it.
pub(crate) fn revoke_unmask(writer: &mut dyn KeyValueWrite, role: &str) -> SqlResult<bool> {
    writer.delete(&unmask_key(role))
}

/// Whether `role`, or a role it is a member of, holds `UNMASK`.
pub(crate) fn holds_unmask(reader: &dyn KeyValueRead, role: &str) -> SqlResult<bool> {
    for held in crate::roles::role_closure(reader, role)? {
        if reader.get(&unmask_key(&held))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a statement's results show a role without `UNMASK`: each masked column named in the
/// statement keeps its function, the masked tables' other columns show as they are, and every
/// other result column, which could be computed from a masked one, gets `default()` (D1348).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ResultMasks {
    functions: HashMap<String, MaskFunction>,
    unmasked: HashSet<String>,
}

impl ResultMasks {
    /// The masks for a statement whose identifiers (lowercased) are `named`, from `masks`
    /// minus those of tables the role may see unmasked; `None` when the statement names no
    /// masked column, so its results show as they are. Only when `plain` (every result column
    /// is named after the column it shows) do the masked tables' other columns show as they
    /// are; otherwise every result column without a mask of its own gets `default()`.
    pub(crate) fn for_statement(
        reader: &dyn KeyValueRead,
        masks: &[ColumnMask],
        exempt_tables: &HashSet<String>,
        named: &HashSet<String>,
        plain: bool,
    ) -> SqlResult<Option<Self>> {
        let mentioned: Vec<&ColumnMask> = masks
            .iter()
            .filter(|mask| !exempt_tables.contains(&mask.table))
            .filter(|mask| named.contains(&mask.column.to_ascii_lowercase()))
            .collect();
        if mentioned.is_empty() {
            return Ok(None);
        }
        let mut result = Self::default();
        for mask in &mentioned {
            let column = mask.column.to_ascii_lowercase();
            let function = match result.functions.get(&column) {
                Some(known) if *known != mask.function => MaskFunction::Default,
                _ => mask.function.clone(),
            };
            result.functions.insert(column, function);
        }
        if !plain {
            return Ok(Some(result));
        }
        let tables: HashSet<&str> = mentioned.iter().map(|mask| mask.table.as_str()).collect();
        for table in tables {
            let Some(bytes) = reader.get(&crate::encoding::table_key(table))? else {
                continue;
            };
            let Ok(definition) = crate::encoding::TableDefinition::decode(table, &bytes) else {
                continue;
            };
            for column in &definition.columns {
                let name = column.name.to_ascii_lowercase();
                if !masks
                    .iter()
                    .any(|mask| mask.table == table && mask.column.eq_ignore_ascii_case(&name))
                {
                    result.unmasked.insert(name);
                }
            }
        }
        result.unmasked.retain(|name| !result.functions.contains_key(name));
        Ok(Some(result))
    }

    /// Masks the values of `result` in place.
    pub(crate) fn apply(&self, result: &mut QueryResult) {
        let Some(columns) = &result.columns else {
            return;
        };
        let plan: Vec<Option<MaskFunction>> = columns
            .iter()
            .map(|column| {
                let name = column.name.to_ascii_lowercase();
                match self.functions.get(&name) {
                    Some(function) => Some(function.clone()),
                    None if self.unmasked.contains(&name) => None,
                    None => Some(MaskFunction::Default),
                }
            })
            .collect();
        for row in &mut result.rows {
            for (value, function) in row.iter_mut().zip(&plan) {
                if let Some(function) = function {
                    *value = function.apply(value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MaskFunction, partial_mask};
    use crate::types::Value;

    #[test]
    fn the_mask_functions_hide_values_as_sql_server_does() {
        let email = MaskFunction::parse("email()").unwrap();
        assert_eq!(email.apply(&Value::Text("ada@example.com".to_owned())), Value::Text("aXXX@XXXX.com".to_owned()));
        let default = MaskFunction::parse(" default() ").unwrap();
        assert_eq!(default.apply(&Value::Text("swordfish".to_owned())), Value::Text("xxxx".to_owned()));
        assert_eq!(default.apply(&Value::Text("ab".to_owned())), Value::Text("xx".to_owned()));
        assert_eq!(default.apply(&Value::Integer(42)), Value::Integer(0));
        assert_eq!(default.apply(&Value::Null), Value::Null);
        let partial = MaskFunction::parse("partial(1, \"XXX-XX-\", 4)").unwrap();
        assert_eq!(
            partial.apply(&Value::Text("555-12-3456".to_owned())),
            Value::Text("5XXX-XX-3456".to_owned())
        );
        assert_eq!(partial_mask("abc", 2, "**", 2), "**");
        let random = MaskFunction::parse("random(1, 12)").unwrap();
        let Value::Integer(drawn) = random.apply(&Value::Integer(500)) else {
            panic!("an integer");
        };
        assert!((1..=12).contains(&drawn));
        assert_eq!(random.text(), "random(1, 12)");
        for wrong in ["mask()", "random(5, 1)", "partial(1, XXX, 2)", "email", "default(1)"] {
            assert!(MaskFunction::parse(wrong).is_err(), "{wrong}");
        }
    }
}
