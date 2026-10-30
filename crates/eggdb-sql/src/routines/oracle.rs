use super::{RoutineDefinition, RoutineKind, compiled_program, resolve, save, stored};
use crate::ast::{AlterRoutine, CreateRoutine, DropRoutine};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::swap_uniform_case;
use crate::plsql::{name_in_use, object_missing};
use crate::schemas::base_name;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};
use std::sync::Arc;

const CALL_ERROR_CODE: u16 = 6553;
const NOT_CALLABLE_CODE: u16 = 6576;

pub(super) fn create_routine(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateRoutine,
    definition: RoutineDefinition,
) -> SqlResult<QueryResult> {
    let position = create.name.position;
    if tables.relation(&*writer, &definition.name)?.is_some()
        || crate::plsql::stored_package(&*writer, &definition.name)?.is_some()
    {
        return Err(name_in_use(position));
    }
    let existing = stored(&*writer, &definition.name)?;
    let replaceable = match existing.as_slice() {
        [] => true,
        [known] => {
            create.replace && known.dialect == Dialect::Oracle && known.kind == definition.kind
        }
        _ => false,
    };
    if !replaceable {
        return Err(name_in_use(position));
    }
    save(writer, &definition.name, &[definition.stored()])?;
    Ok(QueryResult::command(format!(
        "CREATE {}",
        definition.kind.keyword()
    )))
}

pub(super) fn drop_routines(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    drop: &DropRoutine,
) -> SqlResult<QueryResult> {
    let kind = drop.kind.unwrap_or(RoutineKind::Procedure);
    for target in &drop.targets {
        let found = resolve(&*writer, tables, &target.name.text)?
            .filter(|named| named.routines.iter().any(|routine| routine.kind == kind));
        let Some(named) = found else {
            if drop.if_exists {
                continue;
            }
            return Err(object_missing(&target.name.text, target.name.position));
        };
        let mut routines = stored(&*writer, &named.name)?;
        routines.retain(|routine| routine.kind != kind);
        save(writer, &named.name, &routines)?;
    }
    Ok(QueryResult::command(format!("DROP {}", kind.keyword())))
}

pub(super) fn alter_routine(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    alter: &AlterRoutine,
) -> SqlResult<QueryResult> {
    let kind = alter.kind.unwrap_or(RoutineKind::Procedure);
    let target = &alter.target.name;
    let named = resolve(reader, tables, &target.text)?;
    let found = named
        .as_ref()
        .and_then(|named| named.routines.iter().find(|routine| routine.kind == kind));
    let Some(routine) = found.cloned() else {
        return Err(object_missing(&target.text, target.position));
    };
    compiled_program(tables, &routine)?;
    Ok(QueryResult::command(format!("ALTER {}", kind.keyword())))
}

pub(super) fn wrong_arguments(
    kind: RoutineKind,
    name: &str,
    routines: &[Arc<RoutineDefinition>],
) -> SqlError {
    let shown = swap_uniform_case(base_name(name));
    let same_kind = routines.iter().any(|routine| routine.kind == kind);
    match (same_kind, kind) {
        (true, _) => call_error(format!(
            "PLS-306: wrong number or types of arguments in call to '{shown}'"
        )),
        (false, RoutineKind::Function) => {
            call_error(format!("PLS-221: '{shown}' is not a procedure or is undefined"))
        }
        (false, RoutineKind::Procedure) => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "not a valid function or procedure name",
        )
        .with_oracle_code(NOT_CALLABLE_CODE),
    }
}

pub(super) fn too_many_matches(name: &str) -> SqlError {
    call_error(format!(
        "PLS-307: too many declarations of '{}' match this call",
        swap_uniform_case(base_name(name))
    ))
}

fn call_error(message: String) -> SqlError {
    SqlError::new(SqlState::UNDEFINED_FUNCTION, message).with_oracle_code(CALL_ERROR_CODE)
}
