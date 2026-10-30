use super::frame::{RoutineFrame, Slot, record_of};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plsql::{NO_DATA_FOUND_CODE, predefined_by_code};
use crate::records::{RECORD_OID, RecordValue, record_value};
use crate::types::{Conversion, DefinedValue, SqlType, Value, convert};
use std::cmp::Ordering;

pub(crate) const MAX_COLLECTION_ELEMENTS: usize = 1 << 22;
const COLLECTION_IS_NULL_CODE: u16 = 6531;
const SUBSCRIPT_OUTSIDE_LIMIT_CODE: u16 = 6532;
const SUBSCRIPT_BEYOND_COUNT_CODE: u16 = 6533;
const VALUE_ERROR_CODE: u16 = 6502;
const INVALID_CURSOR_CODE: u16 = 1001;
const BULK_EXCEPTION_FIELDS: [&str; 2] = ["error_index", "error_code"];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CollectionShape {
    pub(crate) kind: CollectionKind,
    pub(crate) element: Slot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    NestedTable,
    Varray(u32),
    IndexedByInteger,
    IndexedByText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CollectionKey {
    Integer(i64),
    Text(String),
}

impl CollectionKey {
    fn name(&self) -> String {
        match self {
            Self::Integer(key) => key.to_string(),
            Self::Text(key) => key.clone(),
        }
    }

    fn compare_name(&self, name: &str) -> Ordering {
        match self {
            Self::Integer(key) => name.parse::<i64>().unwrap_or(i64::MIN).cmp(key),
            Self::Text(key) => name.cmp(key.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CursorAttributes {
    pub(crate) found: Option<bool>,
    pub(crate) rowcount: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PlsqlFrame {
    pub(crate) implicit: CursorAttributes,
    pub(crate) cursors: Vec<(String, CursorAttributes)>,
    pub(crate) constructors: Vec<(String, CollectionShape)>,
    pub(crate) bulk_rowcounts: Option<Value>,
    pub(crate) bulk_exceptions: Option<Value>,
}

pub(crate) fn oracle_failure(state: SqlState, code: u16) -> SqlError {
    let message = predefined_by_code(code).map_or("", |predefined| predefined.message);
    SqlError::new(state, message).with_oracle_code(code)
}

pub(crate) fn collection_is_null() -> SqlError {
    oracle_failure(SqlState::NULL_VALUE_NOT_ALLOWED, COLLECTION_IS_NULL_CODE)
}

pub(crate) fn subscript_outside_limit() -> SqlError {
    oracle_failure(SqlState::ARRAY_SUBSCRIPT_ERROR, SUBSCRIPT_OUTSIDE_LIMIT_CODE)
}

pub(crate) fn subscript_beyond_count() -> SqlError {
    oracle_failure(SqlState::ARRAY_SUBSCRIPT_ERROR, SUBSCRIPT_BEYOND_COUNT_CODE)
}

fn no_data_found() -> SqlError {
    oracle_failure(SqlState::NO_DATA_FOUND, NO_DATA_FOUND_CODE)
}

fn invalid_cursor() -> SqlError {
    oracle_failure(SqlState::INVALID_CURSOR_NAME, INVALID_CURSOR_CODE)
}

pub(crate) fn value_error(detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("PL/SQL: numeric or value error: {detail}"),
    )
    .with_oracle_code(VALUE_ERROR_CODE)
}

pub(crate) fn wrong_arguments(method: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("PLS-00306: wrong number or types of arguments in call to '{method}'"),
    )
}

pub(crate) fn too_many_elements() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a PL/SQL collection holds at most {MAX_COLLECTION_ELEMENTS} elements"),
    )
}

pub(crate) fn empty_collection() -> Value {
    record_value(RECORD_OID, Vec::new(), Vec::new())
}

pub(crate) fn collection_of(elements: Vec<Value>) -> Value {
    let names = (1..=elements.len()).map(|key| key.to_string()).collect();
    record_value(RECORD_OID, elements, names)
}

pub(crate) fn entries(value: &Value) -> Option<&RecordValue> {
    record_of(value)
}

pub(crate) fn entries_mut(value: &mut Value) -> Option<&mut RecordValue> {
    let Value::Defined(defined) = value else {
        return None;
    };
    match defined.as_mut() {
        DefinedValue::Record(record) => Some(record),
        _ => None,
    }
}

fn integer_value(key: i64) -> Value {
    match i32::try_from(key) {
        Ok(key) => Value::Integer(key),
        Err(_) => Value::BigInt(key),
    }
}

fn integer_of(value: &Value) -> SqlResult<i64> {
    let whole = match value {
        Value::Numeric(number) => number.to_i128_rounded(),
        Value::SmallInt(number) => Some(i128::from(*number)),
        Value::Integer(number) => Some(i128::from(*number)),
        Value::BigInt(number) => Some(i128::from(*number)),
        other => match convert(other.clone(), SqlType::BigInt, Conversion::Assignment) {
            Ok(Value::BigInt(number)) => Some(i128::from(number)),
            _ => return Err(value_error("character to number conversion error")),
        },
    };
    whole
        .and_then(|whole| i64::try_from(whole).ok())
        .filter(|whole| i32::try_from(*whole).is_ok())
        .ok_or_else(|| value_error("number precision too large"))
}

impl CollectionShape {
    pub(crate) fn is_associative(&self) -> bool {
        matches!(
            self.kind,
            CollectionKind::IndexedByInteger | CollectionKind::IndexedByText
        )
    }

    pub(crate) fn nested(&self) -> Option<&CollectionShape> {
        match &self.element {
            Slot::Collection(inner) => Some(inner.as_ref()),
            _ => None,
        }
    }

    pub(crate) fn initial_value(&self) -> Value {
        if self.is_associative() {
            empty_collection()
        } else {
            Value::Null
        }
    }

    pub(crate) fn limit(&self) -> Option<u32> {
        match self.kind {
            CollectionKind::Varray(limit) => Some(limit),
            _ => None,
        }
    }

    pub(crate) fn key(&self, value: &Value) -> SqlResult<CollectionKey> {
        if value.is_null() {
            return Err(value_error("NULL index table key value"));
        }
        match self.kind {
            CollectionKind::IndexedByText => Ok(CollectionKey::Text(
                value.to_text().unwrap_or_default(),
            )),
            _ => integer_of(value).map(CollectionKey::Integer),
        }
    }

    pub(crate) fn key_value(&self, name: &str) -> Value {
        match self.kind {
            CollectionKind::IndexedByText => Value::Text(name.to_owned()),
            _ => integer_value(name.parse().unwrap_or_default()),
        }
    }

    fn check_dense_key(&self, key: &CollectionKey, last: i64) -> SqlResult<()> {
        let CollectionKey::Integer(key) = key else {
            return Ok(());
        };
        if self.is_associative() {
            return Ok(());
        }
        if *key < 1 || self.limit().is_some_and(|limit| *key > i64::from(limit)) {
            return Err(subscript_outside_limit());
        }
        if *key > last {
            return Err(subscript_beyond_count());
        }
        Ok(())
    }

    pub(crate) fn construct(&self, elements: Vec<Value>) -> SqlResult<Value> {
        if let Some(limit) = self.limit()
            && elements.len() > limit as usize
        {
            return Err(subscript_outside_limit());
        }
        if elements.len() > MAX_COLLECTION_ELEMENTS {
            return Err(too_many_elements());
        }
        Ok(collection_of(elements))
    }

    pub(crate) fn element(&self, collection: &Value, key: &Value) -> SqlResult<Value> {
        let Some(record) = entries(collection) else {
            return Err(collection_is_null());
        };
        let key = self.key(key)?;
        match position(record, &key) {
            Ok(at) => Ok(record.fields.get(at).cloned().unwrap_or(Value::Null)),
            Err(_) if self.is_associative() => Err(no_data_found()),
            Err(_) => {
                self.check_dense_key(&key, last_integer(record))?;
                Err(no_data_found())
            }
        }
    }

    pub(crate) fn put(&self, collection: &mut Value, key: &Value, element: Value) -> SqlResult<()> {
        let key = self.key(key)?;
        if collection.is_null() {
            if !self.is_associative() {
                return Err(collection_is_null());
            }
            *collection = empty_collection();
        }
        let Some(record) = entries_mut(collection) else {
            return Err(collection_is_null());
        };
        match position(record, &key) {
            Ok(at) => {
                record.fields[at] = element;
                Ok(())
            }
            Err(at) => {
                self.check_dense_key(&key, last_integer(record))?;
                if record.fields.len() >= MAX_COLLECTION_ELEMENTS {
                    return Err(too_many_elements());
                }
                record.names.insert(at, key.name());
                record.fields.insert(at, element);
                Ok(())
            }
        }
    }

    pub(crate) fn method(
        &self,
        collection: &Value,
        method: &str,
        argument: Option<&Value>,
    ) -> Option<SqlResult<Value>> {
        let record = entries(collection);
        Some(match (method, argument) {
            ("exists", Some(key)) => Ok(Value::Boolean(match record {
                Some(record) if !key.is_null() => self
                    .key(key)
                    .map(|key| position(record, &key).is_ok())
                    .unwrap_or(false),
                _ => false,
            })),
            ("limit", None) => match (record, self.limit()) {
                (None, _) => Err(collection_is_null()),
                (Some(_), Some(limit)) => Ok(Value::Integer(i32::try_from(limit).unwrap_or(i32::MAX))),
                (Some(_), None) => Ok(Value::Null),
            },
            ("count" | "first" | "last", None) | ("next" | "prior", Some(_)) => {
                let Some(record) = record else {
                    return Some(Err(collection_is_null()));
                };
                self.navigate(record, method, argument)
            }
            _ => return None,
        })
    }

    fn navigate(
        &self,
        record: &RecordValue,
        method: &str,
        argument: Option<&Value>,
    ) -> SqlResult<Value> {
        let names = &record.names;
        let named = |at: Option<usize>| {
            at.and_then(|at| names.get(at))
                .map_or(Value::Null, |name| self.key_value(name))
        };
        Ok(match (method, argument) {
            ("count", _) => Value::Integer(i32::try_from(names.len()).unwrap_or(i32::MAX)),
            ("first", _) => named((!names.is_empty()).then_some(0)),
            ("last", _) => named(names.len().checked_sub(1)),
            (_, Some(key)) if key.is_null() => Value::Null,
            ("next", Some(key)) => {
                let key = self.key(key)?;
                let at = match position(record, &key) {
                    Ok(at) => at + 1,
                    Err(at) => at,
                };
                named(Some(at))
            }
            ("prior", Some(key)) => {
                let key = self.key(key)?;
                let at = match position(record, &key) {
                    Ok(at) | Err(at) => at.checked_sub(1),
                };
                named(at)
            }
            _ => Value::Null,
        })
    }

    pub(crate) fn extend(
        &self,
        collection: &mut Value,
        count: usize,
        copy_of: Option<&Value>,
    ) -> SqlResult<()> {
        if self.is_associative() {
            return Err(wrong_arguments("EXTEND"));
        }
        let copied = match copy_of {
            Some(key) => Some(self.element(collection, key)?),
            None => None,
        };
        let Some(record) = entries_mut(collection) else {
            return Err(collection_is_null());
        };
        let total = record.fields.len().saturating_add(count);
        if self.limit().is_some_and(|limit| total > limit as usize) {
            return Err(subscript_outside_limit());
        }
        if total > MAX_COLLECTION_ELEMENTS {
            return Err(too_many_elements());
        }
        let last = last_integer(record);
        for offset in 1..=count as i64 {
            record.names.push((last + offset).to_string());
            record.fields.push(copied.clone().unwrap_or(Value::Null));
        }
        Ok(())
    }

    pub(crate) fn trim(&self, collection: &mut Value, count: usize) -> SqlResult<()> {
        if self.is_associative() {
            return Err(wrong_arguments("TRIM"));
        }
        let Some(record) = entries_mut(collection) else {
            return Err(collection_is_null());
        };
        if count > record.fields.len() {
            return Err(subscript_beyond_count());
        }
        let kept = record.fields.len() - count;
        record.fields.truncate(kept);
        record.names.truncate(kept);
        Ok(())
    }

    pub(crate) fn delete(&self, collection: &mut Value, range: Option<(&Value, &Value)>) -> SqlResult<()> {
        if range.is_some() && matches!(self.kind, CollectionKind::Varray(_)) {
            return Err(wrong_arguments("DELETE"));
        }
        if collection.is_null() {
            return Err(collection_is_null());
        }
        let Some(record) = entries_mut(collection) else {
            return Err(collection_is_null());
        };
        let Some((lower, upper)) = range else {
            record.fields.clear();
            record.names.clear();
            return Ok(());
        };
        if lower.is_null() || upper.is_null() {
            return Ok(());
        }
        let lower = self.key(lower)?;
        let upper = self.key(upper)?;
        let from = match position(record, &lower) {
            Ok(at) | Err(at) => at,
        };
        let to = match position(record, &upper) {
            Ok(at) => at + 1,
            Err(at) => at,
        };
        if from < to {
            record.fields.drain(from..to);
            record.names.drain(from..to);
        }
        Ok(())
    }

    pub(crate) fn keys(&self, collection: &Value) -> SqlResult<Vec<Value>> {
        let Some(record) = entries(collection) else {
            return Err(collection_is_null());
        };
        Ok(record.names.iter().map(|name| self.key_value(name)).collect())
    }

    pub(crate) fn elements(&self, collection: &Value) -> SqlResult<Vec<Value>> {
        let Some(record) = entries(collection) else {
            return Err(collection_is_null());
        };
        Ok(record.fields.clone())
    }
}

fn position(record: &RecordValue, key: &CollectionKey) -> Result<usize, usize> {
    record.names.binary_search_by(|name| key.compare_name(name))
}

fn last_integer(record: &RecordValue) -> i64 {
    record
        .names
        .last()
        .and_then(|name| name.parse().ok())
        .unwrap_or(0)
}

pub(crate) fn bulk_rowcount_shape() -> CollectionShape {
    CollectionShape {
        kind: CollectionKind::IndexedByInteger,
        element: Slot::Typed(SqlType::Integer),
    }
}

pub(crate) fn bulk_exceptions_shape() -> CollectionShape {
    CollectionShape {
        kind: CollectionKind::NestedTable,
        element: Slot::Row(
            BULK_EXCEPTION_FIELDS
                .iter()
                .map(|name| ((*name).to_owned(), SqlType::Integer))
                .collect(),
        ),
    }
}

pub(crate) fn bulk_exception(ordinal: usize, code: u16) -> Value {
    record_value(
        RECORD_OID,
        vec![
            Value::Integer(i32::try_from(ordinal).unwrap_or(i32::MAX)),
            Value::Integer(i32::from(code)),
        ],
        BULK_EXCEPTION_FIELDS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    )
}

impl RoutineFrame {
    pub(crate) fn plsql_frame(&self) -> Option<&PlsqlFrame> {
        self.plsql.as_deref()
    }

    pub(crate) fn collection(&self, name: &str) -> Option<(&CollectionShape, &Value)> {
        let variable = &self.variables[self.index_of(name)?];
        match &variable.slot {
            Slot::Collection(shape) => Some((shape.as_ref(), &variable.value)),
            _ => None,
        }
    }

    pub(crate) fn constructor(&self, name: &str) -> Option<&CollectionShape> {
        self.plsql_frame()?
            .constructors
            .iter()
            .rev()
            .find(|(known, _)| known == name)
            .map(|(_, shape)| shape)
    }

    pub(crate) fn cursor_attribute(&self, name: &str, attribute: &str) -> Option<SqlResult<Value>> {
        if !matches!(attribute, "found" | "notfound" | "rowcount" | "isopen") {
            return None;
        }
        let variable = &self.variables[self.index_of(name)?];
        if !matches!(variable.slot, Slot::Cursor(_)) {
            return None;
        }
        let open = match &variable.value {
            Value::Text(portal) => self.plsql_frame().and_then(|frame| {
                frame
                    .cursors
                    .iter()
                    .find(|(known, _)| known == portal)
                    .map(|(_, attributes)| *attributes)
            }),
            _ => None,
        };
        Some(match (open, attribute) {
            (open, "isopen") => Ok(Value::Boolean(open.is_some())),
            (None, _) => Err(invalid_cursor()),
            (Some(attributes), attribute) => Ok(attribute_value(attributes, attribute)),
        })
    }

    pub(crate) fn implicit_attribute(&self, attribute: &str) -> Option<Value> {
        let attributes = self
            .plsql_frame()
            .map(|frame| frame.implicit)
            .unwrap_or_default();
        match attribute {
            "isopen" => Some(Value::Boolean(false)),
            "found" | "notfound" | "rowcount" => Some(attribute_value(attributes, attribute)),
            _ => None,
        }
    }

    pub(crate) fn bulk_rowcounts(&self) -> Value {
        self.plsql_frame()
            .and_then(|frame| frame.bulk_rowcounts.clone())
            .unwrap_or_else(empty_collection)
    }

    pub(crate) fn bulk_exceptions(&self) -> Value {
        self.plsql_frame()
            .and_then(|frame| frame.bulk_exceptions.clone())
            .unwrap_or_else(empty_collection)
    }
}

fn attribute_value(attributes: CursorAttributes, attribute: &str) -> Value {
    match attribute {
        "found" => attributes.found.map_or(Value::Null, Value::Boolean),
        "notfound" => attributes.found.map_or(Value::Null, |found| Value::Boolean(!found)),
        _ => attributes.rowcount.map_or(Value::Null, |rows| {
            integer_value(i64::try_from(rows).unwrap_or(i64::MAX))
        }),
    }
}
