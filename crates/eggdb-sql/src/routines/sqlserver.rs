use super::{RoutineDefinition, RoutineKind, parse_definition, resolve, save, stored};
use crate::ast::{CreateRoutine, DropRoutine};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::schemas::{base_name, display};
use crate::session::QueryResult;
use crate::store::KeyValueWrite;

fn already_exists(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!(
            "There is already an object named '{}' in the database.",
            base_name(name)
        ),
    )
    .with_sqlserver_code(2714)
    .at(position)
}

fn incompatible(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!(
            "Cannot perform alter on '{}' because it is an incompatible object type.",
            display(name)
        ),
    )
    .with_sqlserver_code(2010)
    .at(position)
}

fn missing_object(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("Invalid object name '{}'.", display(name)),
    )
    .with_sqlserver_code(208)
    .at(position)
}

fn cannot_drop(kind: RoutineKind, name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "Cannot drop the {} '{}', because it does not exist or you do not have permission.",
            kind.noun(),
            display(name)
        ),
    )
    .with_sqlserver_code(3701)
    .at(position)
}

fn dropped_as_other_kind(
    wanted: RoutineKind,
    found: RoutineKind,
    name: &str,
    position: usize,
) -> SqlError {
    let shown = display(name);
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!(
            "Cannot use DROP {} with '{shown}' because '{shown}' is a {}. Use DROP {}.",
            wanted.keyword(),
            found.noun(),
            found.keyword()
        ),
    )
    .with_sqlserver_code(3705)
    .at(position)
}

fn same_shape(known: &RoutineDefinition, replacing: &RoutineDefinition) -> bool {
    known.dialect == Dialect::SqlServer
        && known.kind == replacing.kind
        && known.returns.is_set() == replacing.returns.is_set()
}

pub(super) fn create_routine(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateRoutine,
    definition: RoutineDefinition,
) -> SqlResult<QueryResult> {
    let position = create.name.position;
    if tables.relation(&*writer, &definition.name)?.is_some() {
        return Err(if create.replace {
            incompatible(&definition.name, position)
        } else {
            already_exists(&definition.name, position)
        });
    }
    let existing = stored(&*writer, &definition.name)?;
    match existing.first() {
        Some(_) if !create.replace => return Err(already_exists(&definition.name, position)),
        Some(found) => {
            let known = parse_definition(&found.text, found.dialect)?;
            if existing.len() > 1 || !same_shape(&known, &definition) {
                return Err(incompatible(&definition.name, position));
            }
        }
        None if create.must_exist => return Err(missing_object(&create.name.text, position)),
        None => {}
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
        let position = target.name.position;
        let Some(named) = resolve(&*writer, tables, &target.name.text)? else {
            if drop.if_exists {
                continue;
            }
            return Err(cannot_drop(kind, &target.name.text, position));
        };
        if let Some(other) = named.routines.iter().find(|routine| routine.kind != kind) {
            return Err(dropped_as_other_kind(
                kind,
                other.kind,
                &target.name.text,
                position,
            ));
        }
        save(writer, &named.name, &[])?;
    }
    Ok(QueryResult::command(format!("DROP {}", kind.keyword())))
}
