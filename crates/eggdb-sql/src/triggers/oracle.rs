use super::body::BodyEnd;
use super::functions::{FiredRow, returned_row};
use super::{StatementTriggerCall, TriggerAction, TriggerDefinition, TriggerEvent, TriggerTiming};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::{TokenKind, tokenize_in};
use crate::plpgsql::{PlsqlTriggerCall, PlsqlTriggerEnd, Program, run_plsql_trigger};
use crate::plsql::TriggerRecords;
use crate::records::{RECORD_OID, record_value};
use crate::routines::RoutineDefinition;
use crate::schemas::{qualified, schema_of};
use crate::session::OutputColumn;
use crate::settings::Settings;
use crate::store::KeyValueWrite;
use crate::types::Value;
use std::sync::Arc;

const CACHED_TRIGGER_PROGRAMS: usize = 128;

#[derive(Default)]
pub(super) struct CompiledTriggers {
    entries: Vec<CompiledTrigger>,
}

struct CompiledTrigger {
    table: String,
    name: String,
    text: String,
    definition: Arc<RoutineDefinition>,
    program: Arc<Program>,
}

fn records_of(new: &str, old: &str) -> TriggerRecords {
    TriggerRecords {
        new: new.to_owned(),
        old: old.to_owned(),
    }
}

pub(super) fn invalid_column(column: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("\"{}\": invalid identifier", column.to_uppercase()),
    )
    .with_oracle_code(904)
}

fn bad_bind_variable(record: &str, column: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!(
            "PLS-00049: bad bind variable '{}.{}'",
            record.to_uppercase(),
            column.to_uppercase()
        ),
    )
    .at(position)
}

pub(super) fn compile_body(body: &str, new: &str, old: &str) -> SqlResult<()> {
    crate::plsql::compile_trigger(body, &records_of(new, old)).map(drop)
}

pub(super) fn check_bind_variables(
    body: &str,
    new: &str,
    old: &str,
    columns: &[OutputColumn],
) -> SqlResult<()> {
    let tokens = tokenize_in(body, Dialect::Oracle)
        .map_err(|error| crate::plsql::compilation_error(body, error))?;
    for (at, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, TokenKind::NumberedParameter(_)) {
            continue;
        }
        let written = body.get(token.start + 1..token.end).unwrap_or_default();
        if !(written.eq_ignore_ascii_case(new) || written.eq_ignore_ascii_case(old)) {
            continue;
        }
        if tokens.get(at + 1).map(|dot| &dot.kind) != Some(&TokenKind::Dot) {
            continue;
        }
        let (column, known) = match tokens.get(at + 2).map(|field| &field.kind) {
            Some(TokenKind::Word(column)) => (
                column.to_string(),
                columns
                    .iter()
                    .any(|known| known.name.eq_ignore_ascii_case(column)),
            ),
            Some(TokenKind::QuotedIdentifier(column)) => (
                column.clone(),
                columns.iter().any(|known| known.name == *column),
            ),
            _ => continue,
        };
        if !known {
            let error = bad_bind_variable(written, &column, token.start);
            return Err(crate::plsql::compilation_error(body, error));
        }
    }
    Ok(())
}

fn compiled(
    tables: &mut TableCache,
    table: &str,
    trigger: &TriggerDefinition,
) -> SqlResult<(Arc<RoutineDefinition>, Arc<Program>)> {
    let TriggerAction::PlSql { body, new, old } = &trigger.action else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "the trigger has no PL/SQL body",
        ));
    };
    let cache = &mut tables.triggers.plsql;
    if let Some(found) = cache.entries.iter().find(|entry| {
        entry.table == table && entry.name == trigger.name && entry.text == trigger.text
    }) {
        return Ok((Arc::clone(&found.definition), Arc::clone(&found.program)));
    }
    let program = Arc::new(crate::plsql::compile_trigger(body, &records_of(new, old))?);
    let mut definition = crate::plsql::anonymous_block_definition(body);
    definition.name = qualified(schema_of(table), &trigger.name);
    let definition = Arc::new(definition);
    if cache.entries.len() >= CACHED_TRIGGER_PROGRAMS {
        cache.entries.clear();
    }
    cache.entries.push(CompiledTrigger {
        table: table.to_owned(),
        name: trigger.name.clone(),
        text: trigger.text.clone(),
        definition: Arc::clone(&definition),
        program: Arc::clone(&program),
    });
    Ok((definition, program))
}

fn row_event(old: Option<&[Value]>, new: Option<&[Value]>) -> TriggerEvent {
    match (old, new) {
        (None, _) => TriggerEvent::Insert,
        (Some(_), Some(_)) => TriggerEvent::Update,
        (Some(_), None) => TriggerEvent::Delete,
    }
}

pub(super) fn run_row_trigger(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    fired: FiredRow<'_>,
    updated: &[String],
    new: Option<&mut Vec<Value>>,
) -> SqlResult<BodyEnd> {
    let trigger = fired.trigger;
    let (definition, program) = compiled(tables, fired.table, trigger)?;
    let names: Vec<String> = fired
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let record = |row: Option<&[Value]>| {
        let fields = row.map_or_else(|| vec![Value::Null; names.len()], <[Value]>::to_vec);
        record_value(RECORD_OID, fields, names.clone())
    };
    let current = new.as_deref().map(Vec::as_slice);
    let event = row_event(fired.old, current);
    let call = PlsqlTriggerCall {
        definition: &definition,
        program: &program,
        condition: trigger.condition.as_deref(),
        old: record(fired.old),
        new: record(current),
        event,
        updated: if event == TriggerEvent::Update {
            updated
        } else {
            &[]
        },
    };
    let PlsqlTriggerEnd::Finished { new: changed } =
        run_plsql_trigger(writer, tables, settings, call)?
    else {
        return Ok(BodyEnd::Finished);
    };
    if trigger.timing == TriggerTiming::Before
        && let Some(new) = new
    {
        *new = returned_row(&changed, fired.columns)?;
    }
    Ok(BodyEnd::Finished)
}

pub(super) fn run_statement_trigger(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: &StatementTriggerCall,
) -> SqlResult<()> {
    let trigger = &call.trigger;
    let _active = super::enter_active(&call.table, trigger)?;
    let _dialect = crate::constraints::enter_with_dialect(Dialect::Oracle);
    crate::cancel::check()?;
    let (definition, program) = compiled(tables, &call.table, trigger)?;
    let fired = PlsqlTriggerCall {
        definition: &definition,
        program: &program,
        condition: None,
        old: Value::Null,
        new: Value::Null,
        event: call.event,
        updated: if call.event == TriggerEvent::Update {
            call.updated.as_slice()
        } else {
            &[]
        },
    };
    run_plsql_trigger(writer, tables, settings, fired).map(drop)
}
