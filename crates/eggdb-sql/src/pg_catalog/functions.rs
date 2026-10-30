use super::oids;
use super::relations::{index_columns, index_is_unique, index_method, index_sort_options};
use super::snapshot::{self, CatalogSnapshot, ConstraintSource, RelationSource};
use super::types::oid_number;
use super::{CatalogRelation, CatalogType, RegKind, deparse};
use crate::ast::ReferentialAction;
use crate::error::SqlResult;
use crate::pg_keywords::quote_ident;
use crate::types::{SqlType, Value};

const OID: Option<SqlType> = Some(SqlType::Catalog(CatalogType::Oid));
const NAME: Option<SqlType> = Some(SqlType::Catalog(CatalogType::Name));
const NODE_TREE: Option<SqlType> = Some(SqlType::Catalog(CatalogType::NodeTree));
const REGCLASS: Option<SqlType> = Some(SqlType::Catalog(CatalogType::Reg(RegKind::Class)));
const INTEGER: Option<SqlType> = Some(SqlType::Integer);
const BOOLEAN: Option<SqlType> = Some(SqlType::Boolean);
const TEXT: Option<SqlType> = Some(SqlType::Text);
const NOT_FOUND: [&str; 3] = ["42P01", "3F000", "42704"];
const PG_CLASS: u32 = 1259;
const PG_NAMESPACE: u32 = 2615;
const PG_CONSTRAINT: u32 = 2606;

pub(crate) fn argument_types(name: &str) -> Option<&'static [Option<SqlType>]> {
    Some(match name {
        "format_type" => &[OID, INTEGER],
        "pg_get_userbyid" | "pg_table_is_visible" | "pg_type_is_visible" => &[OID],
        "pg_get_statisticsobjdef_columns" => &[OID],
        "pg_get_expr" => &[NODE_TREE, OID, BOOLEAN],
        "pg_get_constraintdef" | "pg_get_viewdef" | "pg_get_triggerdef" | "pg_get_ruledef" => {
            &[OID, BOOLEAN]
        }
        "pg_get_indexdef" => &[OID, INTEGER, BOOLEAN],
        "pg_relation_is_publishable" => &[REGCLASS],
        "pg_encoding_to_char" => &[INTEGER],
        "obj_description" | "shobj_description" => &[OID, NAME],
        "col_description" => &[OID, INTEGER],
        "pg_get_serial_sequence" => &[TEXT, TEXT],
        "to_regclass" | "to_regtype" => &[TEXT],
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "pg_get_userbyid" | "pg_encoding_to_char" => SqlType::Catalog(CatalogType::Name),
        "pg_table_is_visible" | "pg_type_is_visible" | "pg_relation_is_publishable" => {
            SqlType::Boolean
        }
        "format_type"
        | "pg_get_expr"
        | "pg_get_constraintdef"
        | "pg_get_viewdef"
        | "pg_get_triggerdef"
        | "pg_get_ruledef"
        | "pg_get_indexdef"
        | "pg_get_statisticsobjdef_columns"
        | "obj_description"
        | "shobj_description"
        | "col_description"
        | "pg_get_serial_sequence" => SqlType::Text,
        "to_regclass" => SqlType::Catalog(CatalogType::Reg(RegKind::Class)),
        "to_regtype" => SqlType::Catalog(CatalogType::Reg(RegKind::Type)),
        _ => return None,
    })
}

pub(crate) fn partition_ancestors_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [_] => Ok(SqlType::Catalog(CatalogType::Reg(RegKind::Class))),
        _ => Err(crate::error::SqlError::new(
            crate::error::SqlState::UNDEFINED_FUNCTION,
            "function pg_partition_ancestors() takes one argument",
        )),
    }
}

pub(crate) fn partition_ancestors_rows(
    _: &[Value],
    _: SqlType,
    _: &mut (dyn FnMut(Value) -> SqlResult<()> + '_),
) -> SqlResult<()> {
    Ok(())
}

fn identifier(value: &Value) -> Option<u32> {
    oid_number(value).and_then(|number| u32::try_from(number).ok())
}

fn pretty(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Boolean(true)))
}

fn text(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    argument_types(name)?;
    if values.first().is_some_and(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let first = values.first().and_then(identifier);
    Some(match (name, values) {
        ("format_type", [_, modifier]) => Ok(match first {
            Some(type_oid) => Value::Text(oids::format_type(
                type_oid,
                modifier
                    .as_integer()
                    .and_then(|modifier| i32::try_from(modifier).ok()),
            )),
            None => Value::Null,
        }),
        ("pg_get_userbyid", [_]) => {
            let role = first.unwrap_or(0);
            let shown = snapshot::current()
                .and_then(|snapshot| snapshot.role_name(role))
                .or_else(|| {
                    (role == oids::SESSION_USER)
                        .then(snapshot::session_user)
                        .flatten()
                })
                .or_else(|| {
                    oids::PREDEFINED_ROLES
                        .iter()
                        .find(|(found, _)| *found == role)
                        .map(|(_, name)| (*name).to_owned())
                })
                .unwrap_or_else(|| format!("unknown (OID={role})"));
            Ok(Value::Text(shown))
        }
        ("pg_encoding_to_char", [encoding]) => Ok(Value::Text(
            match encoding.as_integer() {
                Some(6) => "UTF8",
                Some(0) => "SQL_ASCII",
                _ => "",
            }
            .to_owned(),
        )),
        ("pg_type_is_visible", [_]) => Ok(match first {
            Some(type_oid) if oids::builtin_type(type_oid).is_some() => Value::Boolean(true),
            Some(type_oid) => crate::user_types::is_visible(type_oid)
                .map_or(Value::Null, Value::Boolean),
            None => Value::Null,
        }),
        ("pg_get_expr", [tree, ..]) => Ok(match tree {
            Value::Text(text) => Value::Text(text.clone()),
            _ => Value::Null,
        }),
        ("pg_get_triggerdef" | "pg_get_ruledef" | "pg_get_statisticsobjdef_columns", _) => {
            Ok(Value::Null)
        }
        ("shobj_description", _) => Ok(Value::Null),
        ("pg_get_serial_sequence", [table, column]) => serial_sequence(table, column),
        ("to_regclass", [text]) => registered(RegKind::Class, text),
        ("to_regtype", [text]) => registered(RegKind::Type, text),
        _ => with_snapshot(name, values, first),
    })
}

fn registered(kind: RegKind, text: &Value) -> SqlResult<Value> {
    let Some(text) = text.to_text() else {
        return Ok(Value::Null);
    };
    match snapshot::reg_from_text(kind, &text) {
        Ok(found) => Ok(Value::Reg(Box::new(found))),
        Err(error) if NOT_FOUND.contains(&error.state().code()) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

fn serial_sequence(table: &Value, column: &Value) -> SqlResult<Value> {
    let (Some(table), Some(column)) = (table.to_text(), column.to_text()) else {
        return Ok(Value::Null);
    };
    let snapshot = snapshot::require()?;
    let (schema, name) = super::types::split_object_name(&table, RegKind::Class)?;
    let owner = snapshot
        .find_relation(schema.as_deref(), &name)
        .and_then(|at| snapshot.relations.get(at))
        .ok_or_else(|| {
            crate::error::SqlError::new(
                crate::error::SqlState::UNDEFINED_TABLE,
                format!("relation \"{table}\" does not exist"),
            )
        })?;
    if owner
        .table()
        .is_some_and(|definition| definition.column_index(&column).is_none())
    {
        return Err(crate::error::SqlError::new(
            crate::error::SqlState::UNDEFINED_COLUMN,
            format!(
                "column \"{column}\" of relation \"{}\" does not exist",
                owner.name
            ),
        ));
    }
    let sequence = snapshot.relations.iter().find(|entry| {
        matches!(&entry.source, RelationSource::Sequence(sequence)
        if sequence.owned_by.as_ref().is_some_and(|(owner_table, owner_column)| {
            *owner_table == owner.internal && *owner_column == column
        }))
    });
    Ok(sequence.map_or(Value::Null, |entry| {
        Value::Text(format!(
            "{}.{}",
            quote_ident(&entry.schema),
            quote_ident(&entry.name)
        ))
    }))
}

fn with_snapshot(name: &str, values: &[Value], first: Option<u32>) -> SqlResult<Value> {
    let snapshot = snapshot::require()?;
    let Some(object) = first else {
        return Ok(Value::Null);
    };
    Ok(match (name, values) {
        ("pg_table_is_visible", [_]) => match snapshot.relation_index(object) {
            Some(at) => Value::Boolean(snapshot.is_visible(at)),
            None => Value::Null,
        },
        ("pg_relation_is_publishable", [_]) => Value::Boolean(
            snapshot
                .relation(object)
                .is_some_and(|entry| entry.table().is_some()),
        ),
        ("pg_get_constraintdef", [_, rest @ ..]) => text(constraint_definition(
            &snapshot,
            object,
            pretty(rest.first()),
        )),
        ("pg_get_indexdef", [_]) => text(index_definition(&snapshot, object, 0, false)),
        ("pg_get_indexdef", [_, column, rest @ ..]) => text(index_definition(
            &snapshot,
            object,
            column.as_integer().unwrap_or(0),
            pretty(rest.first()),
        )),
        ("pg_get_viewdef", _) => text(snapshot.relation(object).and_then(
            |entry| match &entry.source {
                RelationSource::View(view) => Some(view.query.clone()),
                _ => None,
            },
        )),
        ("obj_description", [_]) => text(description(&snapshot, object, None, 0)?),
        ("obj_description", [_, catalog]) => {
            let class = match catalog {
                Value::Text(name) => match name.as_str() {
                    "pg_class" => PG_CLASS,
                    "pg_namespace" => PG_NAMESPACE,
                    "pg_constraint" => PG_CONSTRAINT,
                    _ => return Ok(Value::Null),
                },
                _ => return Ok(Value::Null),
            };
            text(description(&snapshot, object, Some(class), 0)?)
        }
        ("col_description", [_, column]) => match column.as_integer() {
            Some(column) => text(description(&snapshot, object, Some(PG_CLASS), column)?),
            None => Value::Null,
        },
        _ => Value::Null,
    })
}

fn description(
    snapshot: &CatalogSnapshot,
    object: u32,
    class: Option<u32>,
    sub: i64,
) -> SqlResult<Option<String>> {
    let rows = snapshot.rows(CatalogRelation::Description)?;
    let column = |name: &str| CatalogRelation::Description.column_named(name);
    let (Some(objoid), Some(classoid), Some(objsubid), Some(text)) = (
        column("objoid"),
        column("classoid"),
        column("objsubid"),
        column("description"),
    ) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find(|row| {
            row.get(objoid).and_then(identifier) == Some(object)
                && class.is_none_or(|class| row.get(classoid).and_then(identifier) == Some(class))
                && row.get(objsubid).and_then(Value::as_integer) == Some(sub)
        })
        .and_then(|row| row.get(text))
        .and_then(Value::to_text))
}

fn column_list(table: &crate::encoding::TableDefinition, positions: &[usize]) -> String {
    positions
        .iter()
        .filter_map(|at| table.columns.get(*at))
        .map(|column| quote_ident(&column.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn action(action: ReferentialAction) -> Option<&'static str> {
    match action {
        ReferentialAction::NoAction => None,
        ReferentialAction::Restrict => Some("RESTRICT"),
        ReferentialAction::Cascade => Some("CASCADE"),
        ReferentialAction::SetNull => Some("SET NULL"),
        ReferentialAction::SetDefault => Some("SET DEFAULT"),
    }
}

pub(crate) fn constraint_definition(
    snapshot: &CatalogSnapshot,
    oid: u32,
    pretty: bool,
) -> Option<String> {
    let constraint = snapshot.constraint(oid)?;
    let table = snapshot.relations.get(constraint.table)?.table()?;
    Some(match constraint.source {
        ConstraintSource::Primary => {
            format!("PRIMARY KEY ({})", column_list(table, &table.primary_key))
        }
        ConstraintSource::Unique(at) => {
            format!(
                "UNIQUE ({})",
                column_list(table, &table.indexes.get(at)?.columns)
            )
        }
        ConstraintSource::Check(at) => format!(
            "CHECK ({})",
            deparse::check_text(snapshot, constraint.table, at, pretty)
        ),
        ConstraintSource::Foreign(at) => {
            let key = table.foreign_keys.get(at)?;
            let mut shown = format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                column_list(table, &key.columns),
                snapshot.display_internal(&key.parent),
                key.parent_columns
                    .iter()
                    .map(|name| quote_ident(name))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if let Some(update) = action(key.on_update) {
                shown.push_str(" ON UPDATE ");
                shown.push_str(update);
            }
            if let Some(delete) = action(key.on_delete) {
                shown.push_str(" ON DELETE ");
                shown.push_str(delete);
            }
            if key.deferrable {
                shown.push_str(" DEFERRABLE");
            }
            if table.is_initially_deferred(&key.name) {
                shown.push_str(" INITIALLY DEFERRED");
            }
            shown
        }
    })
    .map(|mut shown| {
        if !table.is_validated(&constraint.name) {
            shown.push_str(" NOT VALID");
        }
        shown
    })
}

pub(crate) fn index_definition(
    snapshot: &CatalogSnapshot,
    oid: u32,
    column: i64,
    pretty: bool,
) -> Option<String> {
    let entry = snapshot.relation(oid)?;
    let RelationSource::Index { table, index } = &entry.source else {
        return None;
    };
    let owner = snapshot.relations.get(*table)?.table()?;
    let columns = index_columns(owner, *index);
    if column > 0 {
        let position = usize::try_from(column - 1).ok()?;
        let at = columns.get(position)?;
        return owner.columns.get(*at).map(|found| quote_ident(&found.name));
    }
    let table_name = if pretty {
        snapshot.display_name(*table)
    } else {
        snapshot.qualified_name(*table)
    };
    let sort_options = index_sort_options(owner, *index);
    let keys: Vec<String> = columns
        .iter()
        .enumerate()
        .filter_map(|(position, at)| {
            let column = owner.columns.get(*at)?;
            let descending = sort_options.is_descending(position);
            let nulls_first = sort_options.has_nulls_first(position);
            let order = match (descending, nulls_first) {
                (false, false) | (true, true) => "",
                (false, true) => " NULLS FIRST",
                (true, false) => " NULLS LAST",
            };
            Some(format!(
                "{}{}{order}",
                quote_ident(&column.name),
                if descending { " DESC" } else { "" }
            ))
        })
        .collect();
    Some(format!(
        "CREATE {}INDEX {} ON {table_name} USING {} ({})",
        if index_is_unique(owner, *index) {
            "UNIQUE "
        } else {
            ""
        },
        quote_ident(&entry.name),
        index_method(owner, *index).name(),
        keys.join(", ")
    ))
}
