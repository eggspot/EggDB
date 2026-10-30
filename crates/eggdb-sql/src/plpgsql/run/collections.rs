use super::super::collections::{
    CollectionKind, CollectionShape, CursorAttributes, MAX_COLLECTION_ELEMENTS, PlsqlFrame,
    bulk_exception, bulk_rowcount_shape, collection_of, empty_collection, entries_mut,
    subscript_outside_limit, too_many_elements, value_error, wrong_arguments,
};
use super::super::frame::{FrameVariable, Slot, missing_field, row_record};
use super::super::{
    CollectionMethod, Forall, ForallBounds, Fragment, IntoTargets, Target, TargetPart,
    TypeDeclaration, VariableType,
};
use super::{Flow, Run, coerce, unknown_variable};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::plsql::NO_DATA_FOUND_CODE;
use crate::records::{self, RECORD_OID};
use crate::types::{Conversion, SqlType, Value, convert};

const ARRAY_DML_ERRORS_CODE: u16 = 24381;
const UNMAPPED_ERROR_CODE: u16 = 600;
const NOT_ENOUGH_VALUES_CODE: u16 = 947;
const TOO_MANY_VALUES_CODE: u16 = 913;

enum PathPart {
    Key(Value),
    Field(String),
}

enum ForallIndexes {
    Range(i64, i64),
    Listed(Vec<Value>),
}

struct ForallProgress {
    rowcounts: Value,
    failures: Vec<Value>,
    total: u64,
}

fn wrong_type() -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        "PLS-00382: expression is of wrong type",
    )
}

fn access_into_null() -> SqlError {
    super::super::collections::oracle_failure(SqlState::NULL_VALUE_NOT_ALLOWED, 6530)
}

fn integer_value(number: i64) -> Value {
    Value::Integer(i32::try_from(number).unwrap_or(i32::MAX))
}

fn key_number(key: &Value) -> Option<i64> {
    key.as_integer()
}

fn count_of(value: &Value) -> SqlResult<usize> {
    if value.is_null() {
        return Ok(0);
    }
    let count = match convert(value.clone(), SqlType::BigInt, Conversion::Assignment) {
        Ok(Value::BigInt(count)) => count,
        _ => return Err(value_error("character to number conversion error")),
    };
    if count < 0 {
        return Err(subscript_outside_limit());
    }
    let count = usize::try_from(count).map_err(|_| too_many_elements())?;
    if count > MAX_COLLECTION_ELEMENTS {
        return Err(too_many_elements());
    }
    Ok(count)
}

pub(super) fn coerce_collection(shape: &CollectionShape, value: Value) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(shape.initial_value());
    }
    let mut value = value;
    let Some(record) = entries_mut(&mut value) else {
        return Err(wrong_type());
    };
    if shape
        .limit()
        .is_some_and(|limit| record.fields.len() > limit as usize)
    {
        return Err(subscript_outside_limit());
    }
    if !matches!(shape.element, Slot::Untyped | Slot::Typed(SqlType::Dynamic)) {
        let fields = std::mem::take(&mut record.fields);
        let mut coerced = Vec::with_capacity(fields.len());
        for field in fields {
            coerced.push(coerce(&shape.element, field)?);
        }
        record.fields = coerced;
    }
    Ok(value)
}

fn apply_method(
    shape: &CollectionShape,
    collection: &mut Value,
    method: CollectionMethod,
    values: &[Value],
) -> SqlResult<()> {
    match (method, values) {
        (CollectionMethod::Extend, []) => shape.extend(collection, 1, None),
        (CollectionMethod::Extend, [count]) => shape.extend(collection, count_of(count)?, None),
        (CollectionMethod::Extend, [count, copied]) => {
            shape.extend(collection, count_of(count)?, Some(copied))
        }
        (CollectionMethod::Trim, []) => shape.trim(collection, 1),
        (CollectionMethod::Trim, [count]) => shape.trim(collection, count_of(count)?),
        (CollectionMethod::Delete, []) => shape.delete(collection, None),
        (CollectionMethod::Delete, [key]) => shape.delete(collection, Some((key, key))),
        (CollectionMethod::Delete, [lower, upper]) => {
            shape.delete(collection, Some((lower, upper)))
        }
        (method, _) => Err(wrong_arguments(method.name())),
    }
}

fn set_field(slot: &Slot, element: Value, field: &str, value: Value) -> SqlResult<Value> {
    let mut element = match (slot, element) {
        (Slot::Row(columns), Value::Null) => row_record(columns, vec![Value::Null; columns.len()]),
        (_, element) => element,
    };
    let Some(record) = entries_mut(&mut element) else {
        return Err(access_into_null());
    };
    let Some(at) = record.names.iter().position(|name| name == field) else {
        return Err(missing_field("element", field));
    };
    let declared = match slot {
        Slot::Row(columns) => columns.get(at).map(|(_, sql_type)| *sql_type),
        _ => None,
    };
    record.fields[at] = match declared {
        Some(SqlType::Dynamic) | None => value,
        Some(sql_type) => convert(value, sql_type, Conversion::Assignment)?,
    };
    Ok(element)
}

fn put_path(
    shape: &CollectionShape,
    collection: &mut Value,
    parts: &[PathPart],
    value: Value,
) -> SqlResult<()> {
    let [PathPart::Key(key), rest @ ..] = parts else {
        return Err(SqlError::not_supported(
            "assigning to a field of a PL/SQL collection",
        ));
    };
    if rest.is_empty() {
        let element = coerce(&shape.element, value)?;
        return shape.put(collection, key, element);
    }
    let element = match shape.element(collection, key) {
        Ok(element) => element,
        Err(error)
            if shape.is_associative() && error.vendor_code() == Some(NO_DATA_FOUND_CODE) =>
        {
            Value::Null
        }
        Err(error) => return Err(error),
    };
    let element = match (&shape.element, rest) {
        (Slot::Collection(inner), _) => {
            let mut element = match element {
                Value::Null => inner.initial_value(),
                element => element,
            };
            put_path(inner, &mut element, rest, value)?;
            element
        }
        (slot, [PathPart::Field(field)]) => set_field(slot, element, field, value)?,
        _ => {
            return Err(SqlError::not_supported(
                "assigning to a nested field of a PL/SQL collection element",
            ));
        }
    };
    shape.put(collection, key, element)
}

impl Run<'_> {
    pub(super) fn plsql_frame_mut(&mut self) -> &mut PlsqlFrame {
        self.frame.plsql.get_or_insert_with(Box::default)
    }

    pub(super) fn collection_shape(
        &mut self,
        declared: &TypeDeclaration,
    ) -> SqlResult<Option<Box<CollectionShape>>> {
        let (kind, element) = match declared {
            TypeDeclaration::Table {
                element,
                index: None,
            } => (CollectionKind::NestedTable, element),
            TypeDeclaration::Table {
                element,
                index: Some(index),
            } => (self.index_kind(index)?, element),
            TypeDeclaration::Varray { limit, element } => (CollectionKind::Varray(*limit), element),
            _ => return Ok(None),
        };
        let element = self.slot_of(element)?;
        Ok(Some(Box::new(CollectionShape { kind, element })))
    }

    fn index_kind(&mut self, index: &VariableType) -> SqlResult<CollectionKind> {
        Ok(match self.slot_of(index)? {
            Slot::Typed(SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_)) => {
                CollectionKind::IndexedByText
            }
            _ => CollectionKind::IndexedByInteger,
        })
    }

    pub(super) fn assign_collection(
        &mut self,
        index: usize,
        path: &[TargetPart],
        value: Value,
    ) -> SqlResult<()> {
        self.writable(index)?;
        let Slot::Collection(shape) = self.frame.variables[index].slot.clone() else {
            return Err(wrong_type());
        };
        let mut parts = Vec::with_capacity(path.len());
        for part in path {
            parts.push(match part {
                TargetPart::Index(subscript) => PathPart::Key(self.value(subscript)?),
                TargetPart::Field(field) => PathPart::Field(field.clone()),
            });
        }
        let mut collection = std::mem::replace(&mut self.frame.variables[index].value, Value::Null);
        let outcome = put_path(&shape, &mut collection, &parts, value);
        self.frame.variables[index].value = collection;
        outcome.map_err(|error| self.assignment_error(error))
    }

    fn collection_variable(&self, target: &Target) -> SqlResult<(usize, Box<CollectionShape>)> {
        let Some(index) = self.frame.target_index(&target.name) else {
            return Err(unknown_variable(&target.name));
        };
        match (&self.frame.variables[index].slot, target.path.is_empty()) {
            (Slot::Collection(shape), true) => Ok((index, shape.clone())),
            (_, true) => Err(wrong_type()),
            (_, false) => Err(SqlError::not_supported(
                "a collection method on a field of a PL/SQL variable",
            )),
        }
    }

    pub(super) fn collection_method(
        &mut self,
        target: &Target,
        method: CollectionMethod,
        arguments: &[Fragment],
    ) -> SqlResult<Flow> {
        let (index, shape) = self.collection_variable(target)?;
        self.writable(index)?;
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            values.push(self.value(argument)?);
        }
        let mut collection = std::mem::replace(&mut self.frame.variables[index].value, Value::Null);
        let outcome = apply_method(&shape, &mut collection, method, &values);
        self.frame.variables[index].value = collection;
        outcome.map(|()| Flow::Next)
    }

    fn collects_records(&self, target: &Target) -> bool {
        target.path.is_empty()
            && self.frame.target_index(&target.name).is_some_and(|index| {
                matches!(
                    &self.frame.variables[index].slot,
                    Slot::Collection(shape) if matches!(shape.element, Slot::Row(_) | Slot::Record)
                )
            })
    }

    pub(super) fn store_bulk(
        &mut self,
        targets: &[Target],
        names: &[String],
        rows: Vec<Vec<Value>>,
    ) -> SqlResult<()> {
        if rows.len() > MAX_COLLECTION_ELEMENTS {
            return Err(too_many_elements());
        }
        if let [target] = targets
            && (names.len() != 1 || self.collects_records(target))
        {
            let elements = rows
                .into_iter()
                .map(|row| records::record_value(RECORD_OID, row, names.to_vec()))
                .collect();
            return self.assign(target, collection_of(elements));
        }
        if targets.len() != names.len() {
            let (code, message) = if targets.len() > names.len() {
                (NOT_ENOUGH_VALUES_CODE, "not enough values")
            } else {
                (TOO_MANY_VALUES_CODE, "too many values")
            };
            return Err(SqlError::new(SqlState::SYNTAX_ERROR, message).with_oracle_code(code));
        }
        let mut columns: Vec<Vec<Value>> = targets
            .iter()
            .map(|_| Vec::with_capacity(rows.len()))
            .collect();
        for row in rows {
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        for (target, column) in targets.iter().zip(columns) {
            self.assign(target, collection_of(column))?;
        }
        Ok(())
    }

    pub(super) fn fetch_bulk(
        &mut self,
        cursor: &str,
        into: &IntoTargets,
        limit: Option<&Fragment>,
    ) -> SqlResult<Flow> {
        let limit = match limit {
            Some(limit) => {
                let value = self.value(limit)?;
                let count = count_of(&value)?;
                if count == 0 {
                    return Err(value_error("invalid LIMIT value"));
                }
                Some(count)
            }
            None => None,
        };
        let at = self.open_cursor(cursor)?;
        let opened = &mut self.cursors[at];
        let length = opened.rows.len();
        let start = opened.position.min(length);
        let end = limit.map_or(length, |limit| start.saturating_add(limit).min(length));
        let rows = opened.rows[start..end].to_vec();
        opened.position = end;
        opened.found = Some(limit.is_some_and(|limit| rows.len() == limit));
        let names = opened.names.clone();
        self.store_bulk(&into.targets, &names, rows)?;
        Ok(Flow::Next)
    }

    pub(super) fn refresh_cursor_attributes(&mut self) {
        if !self.oracle {
            return;
        }
        let stale = self
            .frame
            .plsql
            .as_ref()
            .is_some_and(|frame| !frame.cursors.is_empty());
        if self.cursors.is_empty() && !stale {
            return;
        }
        let cursors = self
            .cursors
            .iter()
            .map(|cursor| {
                let fetched = cursor.position.min(cursor.rows.len());
                let attributes = CursorAttributes {
                    found: cursor.found,
                    rowcount: Some(u64::try_from(fetched).unwrap_or(u64::MAX)),
                };
                (cursor.name.clone(), attributes)
            })
            .collect();
        self.plsql_frame_mut().cursors = cursors;
    }

    pub(super) fn note_implicit_rows(&mut self, rows: u64) {
        if self.oracle {
            self.plsql_frame_mut().implicit = CursorAttributes {
                found: Some(rows > 0),
                rowcount: Some(rows),
            };
        }
    }

    fn collection_value(&self, target: &Target) -> SqlResult<(Box<CollectionShape>, Value)> {
        let (index, shape) = self.collection_variable(target)?;
        Ok((shape, self.frame.variables[index].value.clone()))
    }

    fn forall_indexes(&mut self, bounds: &ForallBounds) -> SqlResult<ForallIndexes> {
        Ok(match bounds {
            ForallBounds::Range { lower, upper } => ForallIndexes::Range(
                self.integer(lower, "lower bound of FORALL cannot be null")?,
                self.integer(upper, "upper bound of FORALL cannot be null")?,
            ),
            ForallBounds::Indices {
                collection,
                between,
            } => {
                let (shape, value) = self.collection_value(collection)?;
                let mut keys = shape.keys(&value)?;
                if let Some((lower, upper)) = between {
                    let lower = self.integer(lower, "lower bound of FORALL cannot be null")?;
                    let upper = self.integer(upper, "upper bound of FORALL cannot be null")?;
                    keys.retain(|key| key_number(key).is_some_and(|key| (lower..=upper).contains(&key)));
                }
                ForallIndexes::Listed(keys)
            }
            ForallBounds::Values(collection) => {
                let (shape, value) = self.collection_value(collection)?;
                ForallIndexes::Listed(shape.elements(&value)?)
            }
        })
    }

    pub(super) fn forall(&mut self, forall: &Forall) -> SqlResult<Flow> {
        let indexes = self.forall_indexes(&forall.bounds)?;
        let index = self.frame.variables.len();
        let mut variable = FrameVariable::new(&forall.variable, Slot::Untyped, Value::Null);
        variable.constant = true;
        self.frame.variables.push(variable);
        if forall.save_exceptions {
            self.plsql.guards += 1;
        }
        let mut progress = ForallProgress {
            rowcounts: empty_collection(),
            failures: Vec::new(),
            total: 0,
        };
        let outcome = self.forall_iterations(forall, index, indexes, &mut progress);
        if forall.save_exceptions {
            self.plsql.guards -= 1;
        }
        self.frame.variables.truncate(index);
        let failed = !progress.failures.is_empty();
        let frame = self.plsql_frame_mut();
        frame.bulk_rowcounts = Some(progress.rowcounts);
        frame.bulk_exceptions = Some(collection_of(progress.failures));
        frame.implicit = CursorAttributes {
            found: Some(progress.total > 0),
            rowcount: Some(progress.total),
        };
        outcome?;
        if failed {
            return Err(SqlError::new(SqlState::RAISE_EXCEPTION, "error(s) in array DML")
                .with_oracle_code(ARRAY_DML_ERRORS_CODE));
        }
        Ok(Flow::Next)
    }

    fn forall_iterations(
        &mut self,
        forall: &Forall,
        index: usize,
        indexes: ForallIndexes,
        progress: &mut ForallProgress,
    ) -> SqlResult<()> {
        let keys: Box<dyn Iterator<Item = Value>> = match indexes {
            ForallIndexes::Range(lower, upper) => Box::new((lower..=upper).map(integer_value)),
            ForallIndexes::Listed(keys) => Box::new(keys.into_iter()),
        };
        let counts = bulk_rowcount_shape();
        for (ordinal, key) in keys.enumerate() {
            crate::cancel::check()?;
            self.frame.variables[index].value = key.clone();
            self.plsql_frame_mut().implicit = CursorAttributes::default();
            let rows = match self.step(&forall.statement) {
                Ok(_) => self
                    .frame
                    .plsql
                    .as_ref()
                    .and_then(|frame| frame.implicit.rowcount)
                    .unwrap_or(0),
                Err(error) if forall.save_exceptions => {
                    let code = self
                        .translated(error)
                        .vendor_code()
                        .unwrap_or(UNMAPPED_ERROR_CODE);
                    self.failure = None;
                    progress.failures.push(bulk_exception(ordinal + 1, code));
                    0
                }
                Err(error) => return Err(error),
            };
            progress.total = progress.total.saturating_add(rows);
            let counted = integer_value(i64::try_from(rows).unwrap_or(i64::MAX));
            counts.put(&mut progress.rowcounts, &key, counted)?;
        }
        Ok(())
    }
}
