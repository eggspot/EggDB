use super::{ObjectKind, ObjectSource, SqlServerColumnType, SqlServerObject, SqlServerObjects};
use crate::pg_catalog::{CatalogSnapshot, RelationSource};
use crate::routines::{ParameterMode, RoutineDefinition};
use crate::tsql::TsqlReturns;

pub(crate) struct ParameterFacts<'a> {
    pub(crate) name: &'a str,
    pub(crate) column_type: SqlServerColumnType,
    pub(crate) output: bool,
    pub(crate) default: Option<&'a str>,
}

pub(crate) fn routine_definition<'a>(
    snapshot: &'a CatalogSnapshot,
    object: &SqlServerObject,
) -> Option<&'a RoutineDefinition> {
    let ObjectSource::Routine(at) = object.source else {
        return None;
    };
    snapshot
        .routines
        .get(at)
        .map(|entry| entry.definition.as_ref())
}

pub(crate) fn parameter_facts(definition: &RoutineDefinition) -> Vec<ParameterFacts<'_>> {
    definition
        .parameters
        .iter()
        .map(|parameter| ParameterFacts {
            name: parameter.name.as_deref().unwrap_or_default(),
            column_type: parameter.declared.map_or_else(
                || SqlServerColumnType::of(parameter.sql_type),
                SqlServerColumnType::declared,
            ),
            output: parameter.mode != ParameterMode::In,
            default: parameter.default.as_deref(),
        })
        .collect()
}

pub(crate) fn scalar_return_type(definition: &RoutineDefinition) -> Option<SqlServerColumnType> {
    match &definition.tsql.as_deref()?.returns {
        TsqlReturns::Scalar(declared) => Some(SqlServerColumnType::declared(*declared)),
        TsqlReturns::Status | TsqlReturns::InlineTable | TsqlReturns::Table { .. } => None,
    }
}

pub(crate) fn module_definition(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
    object: &SqlServerObject,
) -> Option<String> {
    match object.source {
        ObjectSource::Trigger(at) => Some(snapshot.triggers.get(at)?.definition.text.clone()),
        ObjectSource::Routine(at) => Some(snapshot.routines.get(at)?.definition.text.clone()),
        ObjectSource::Relation if object.kind == ObjectKind::View => {
            let RelationSource::View(view) = &snapshot.relations.get(object.relation)?.source
            else {
                return None;
            };
            Some(format!(
                "CREATE VIEW [{}].[{}] AS {}",
                objects.schema_name(object.schema_id),
                object.name,
                view.query
            ))
        }
        ObjectSource::Relation | ObjectSource::Constraint(_) | ObjectSource::Default(_) => None,
    }
}
