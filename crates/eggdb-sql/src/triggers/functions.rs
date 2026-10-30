use super::body::{self, BodyEnd};
use super::{StatementTriggerCall, TriggerAction, TriggerDefinition, TriggerTiming, errors};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::plpgsql::{self, Call, Returned, TriggerCall};
use crate::records::{RECORD_OID, record_value};
use crate::routines::{self, RoutineDefinition, RoutineKind};
use crate::schemas::{base_name, qualified, schema_of};
use crate::session::OutputColumn;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::{Conversion, DefinedValue, Value, convert};
use std::sync::Arc;

pub(super) struct FiredRow<'f> {
    pub(super) table: &'f str,
    pub(super) trigger: &'f TriggerDefinition,
    pub(super) columns: &'f [OutputColumn],
    pub(super) old: Option<&'f [Value]>,
}

fn must_return_trigger(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_OBJECT_DEFINITION,
        format!("function {name} must return type trigger"),
    )
}

fn transition_tables_not_supported() -> SqlError {
    SqlError::not_supported("transition tables (REFERENCING OLD TABLE or NEW TABLE) in a PostgreSQL trigger")
        .with_hint("Query the table itself from the trigger function, or use a row-level trigger.")
}

fn mismatched_row(returned: usize, expected: usize) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        "returned row structure does not match the structure of the triggering table",
    )
    .with_detail(format!(
        "Number of returned columns ({returned}) does not match expected column count ({expected})."
    ))
}

fn internal_name(written: &str) -> String {
    match written.split_once('.') {
        Some((schema, name)) => qualified(schema, name),
        None => written.to_owned(),
    }
}

pub(super) fn check_trigger_function(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    written: &str,
) -> SqlResult<()> {
    if routines::resolve_trigger_function(reader, tables, written)?.is_some() {
        return Ok(());
    }
    let without_arguments = routines::resolve(reader, tables, &internal_name(written))?
        .is_some_and(|named| {
            named.routines.iter().any(|routine| {
                routine.kind == RoutineKind::Function && routine.input_types().is_empty()
            })
        });
    if without_arguments {
        return Err(must_return_trigger(written));
    }
    Err(errors::missing_function(written))
}

fn trigger_function(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    trigger: &TriggerDefinition,
) -> SqlResult<(Arc<RoutineDefinition>, Vec<String>)> {
    let TriggerAction::Function { name, arguments } = &trigger.action else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "the trigger does not run a function",
        ));
    };
    if trigger.old_table.is_some() || trigger.new_table.is_some() {
        return Err(transition_tables_not_supported());
    }
    match routines::resolve_trigger_function(reader, tables, name)? {
        Some(definition) => Ok((definition, arguments.clone())),
        None => Err(errors::missing_function(name)),
    }
}

fn relation_oid(reader: &dyn KeyValueRead, table: &str) -> SqlResult<u32> {
    let snapshot = crate::pg_catalog::snapshot_for(reader)?;
    let found = snapshot
        .find_relation(Some(schema_of(table)), base_name(table))
        .and_then(|at| snapshot.relations.get(at));
    Ok(found.map_or(0, |entry| entry.oid))
}

fn timing_word(timing: TriggerTiming) -> &'static str {
    match timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    }
}

fn row_operation(old: Option<&[Value]>, new: Option<&[Value]>) -> &'static str {
    match (old, new) {
        (None, _) => "INSERT",
        (Some(_), Some(_)) => "UPDATE",
        (Some(_), None) => "DELETE",
    }
}

pub(super) fn returned_row(value: &Value, columns: &[OutputColumn]) -> SqlResult<Vec<Value>> {
    let record = match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Record(record) => record,
            _ => return Err(mismatched_row(1, columns.len())),
        },
        _ => return Err(mismatched_row(1, columns.len())),
    };
    if record.fields.len() != columns.len() {
        return Err(mismatched_row(record.fields.len(), columns.len()));
    }
    record
        .fields
        .iter()
        .zip(columns)
        .map(|(field, column)| match column.sql_type {
            Some(sql_type) => convert(field.clone(), sql_type, Conversion::Assignment),
            None => Ok(field.clone()),
        })
        .collect()
}

fn returned_value(returned: Returned) -> Value {
    match returned {
        Returned::Value(value) => value,
        Returned::Set(_) => Value::Null,
    }
}

pub(super) fn run_row_function(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    fired: FiredRow<'_>,
    new: Option<&mut Vec<Value>>,
) -> SqlResult<BodyEnd> {
    let (definition, arguments) = trigger_function(&*writer, tables, fired.trigger)?;
    let names: Vec<String> = fired
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let record = |row: &[Value]| record_value(RECORD_OID, row.to_vec(), names.clone());
    let current = new.as_deref().map(Vec::as_slice);
    let call = TriggerCall {
        new: current.map(record),
        old: fired.old.map(record),
        name: fired.trigger.name.clone(),
        when: timing_word(fired.trigger.timing),
        level: "ROW",
        operation: row_operation(fired.old, current),
        relation_oid: relation_oid(&*writer, fired.table)?,
        table_name: base_name(fired.table).to_owned(),
        table_schema: schema_of(fired.table).to_owned(),
        arguments,
    };
    let returned = plpgsql::invoke(
        writer,
        tables,
        settings,
        Call {
            definition: &definition,
            arguments: Vec::new(),
            trigger: Some(call),
        },
    )?;
    let value = returned_value(returned);
    match fired.trigger.timing {
        TriggerTiming::After => Ok(BodyEnd::Finished),
        _ if matches!(value, Value::Null) => Ok(BodyEnd::Ignored),
        TriggerTiming::InsteadOf => Ok(BodyEnd::Finished),
        TriggerTiming::Before => {
            if let Some(new) = new {
                *new = returned_row(&value, fired.columns)?;
            }
            Ok(BodyEnd::Finished)
        }
    }
}

pub(crate) fn run_statement_function(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: &StatementTriggerCall,
) -> SqlResult<()> {
    let trigger = &call.trigger;
    if let TriggerAction::PlSql { .. } = &trigger.action {
        return super::oracle::run_statement_trigger(writer, tables, settings, call);
    }
    let _active = super::enter_active(&call.table, trigger)?;
    let _dialect = crate::constraints::enter_with_dialect(Dialect::PostgreSql);
    crate::cancel::check()?;
    if !body::statement_condition_holds(writer, tables, settings, trigger, &call.columns)? {
        return Ok(());
    }
    let (definition, arguments) = trigger_function(&*writer, tables, trigger)?;
    let fired = TriggerCall {
        new: None,
        old: None,
        name: trigger.name.clone(),
        when: timing_word(trigger.timing),
        level: "STATEMENT",
        operation: call.event.keyword(),
        relation_oid: relation_oid(&*writer, &call.table)?,
        table_name: base_name(&call.table).to_owned(),
        table_schema: schema_of(&call.table).to_owned(),
        arguments,
    };
    plpgsql::invoke(
        writer,
        tables,
        settings,
        Call {
            definition: &definition,
            arguments: Vec::new(),
            trigger: Some(fired),
        },
    )?;
    Ok(())
}
