use super::routines::{
    module_definition, parameter_facts, routine_definition, scalar_return_type,
};
use super::{
    CatalogRow, ObjectKind, ObjectSource, SqlServerColumnType, SqlServerObject, SqlServerObjects,
    column, default_definition, foreign_key_facts, int, optional_int, optional_small,
    relation_columns, text,
};
use crate::pg_catalog::{
    CatalogColumn, CatalogSnapshot, ConstraintSource, IndexSource, RelationSource,
};
use crate::types::{SqlType, Value};

const SYSNAME: SqlType = SqlType::Varchar(Some(128));
const NVARCHAR_4000: SqlType = SqlType::Varchar(Some(4000));
const VARCHAR_2: SqlType = SqlType::Varchar(Some(2));
const VARCHAR_3: SqlType = SqlType::Varchar(Some(3));
const VARCHAR_7: SqlType = SqlType::Varchar(Some(7));
const VARCHAR_10: SqlType = SqlType::Varchar(Some(10));
const VARCHAR_11: SqlType = SqlType::Varchar(Some(11));
const VARCHAR_20: SqlType = SqlType::Varchar(Some(20));
const INT: SqlType = SqlType::Integer;
const SMALLINT: SqlType = SqlType::SmallInt;
const TINYINT: SqlType = SqlType::SmallInt;
const BIGINT: SqlType = SqlType::BigInt;
const BIT: SqlType = SqlType::Boolean;
const DATETIME: SqlType = SqlType::Timestamp(Some(3));
const UNICODE_CHARACTER_SET: &str = "UNICODE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SqlServerInformation {
    Schemata,
    Tables,
    Columns,
    Views,
    ViewTableUsage,
    Sequences,
    TableConstraints,
    KeyColumnUsage,
    ReferentialConstraints,
    ConstraintColumnUsage,
    ConstraintTableUsage,
    CheckConstraints,
    Routines,
    Parameters,
}

impl SqlServerInformation {
    pub(crate) const ALL: [Self; 14] = [
        Self::Schemata,
        Self::Tables,
        Self::Columns,
        Self::Views,
        Self::ViewTableUsage,
        Self::Sequences,
        Self::TableConstraints,
        Self::KeyColumnUsage,
        Self::ReferentialConstraints,
        Self::ConstraintColumnUsage,
        Self::ConstraintTableUsage,
        Self::CheckConstraints,
        Self::Routines,
        Self::Parameters,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Schemata => "schemata",
            Self::Tables => "tables",
            Self::Columns => "columns",
            Self::Views => "views",
            Self::ViewTableUsage => "view_table_usage",
            Self::Sequences => "sequences",
            Self::TableConstraints => "table_constraints",
            Self::KeyColumnUsage => "key_column_usage",
            Self::ReferentialConstraints => "referential_constraints",
            Self::ConstraintColumnUsage => "constraint_column_usage",
            Self::ConstraintTableUsage => "constraint_table_usage",
            Self::CheckConstraints => "check_constraints",
            Self::Routines => "routines",
            Self::Parameters => "parameters",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.name() == name)
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Schemata => SCHEMATA_COLUMNS,
            Self::Tables => TABLES_COLUMNS,
            Self::Columns => COLUMNS_COLUMNS,
            Self::Views => VIEWS_COLUMNS,
            Self::ViewTableUsage => VIEW_TABLE_USAGE_COLUMNS,
            Self::Sequences => SEQUENCES_COLUMNS,
            Self::TableConstraints => TABLE_CONSTRAINTS_COLUMNS,
            Self::KeyColumnUsage => KEY_COLUMN_USAGE_COLUMNS,
            Self::ReferentialConstraints => REFERENTIAL_CONSTRAINTS_COLUMNS,
            Self::ConstraintColumnUsage => CONSTRAINT_COLUMN_USAGE_COLUMNS,
            Self::ConstraintTableUsage => CONSTRAINT_TABLE_USAGE_COLUMNS,
            Self::CheckConstraints => CHECK_CONSTRAINTS_COLUMNS,
            Self::Routines => ROUTINES_COLUMNS,
            Self::Parameters => PARAMETERS_COLUMNS,
        }
    }
}

const SCHEMATA_COLUMNS: &[CatalogColumn] = &[
    column("catalog_name", SYSNAME),
    column("schema_name", SYSNAME),
    column("schema_owner", SYSNAME),
    column("default_character_set_catalog", SYSNAME),
    column("default_character_set_schema", SYSNAME),
    column("default_character_set_name", SYSNAME),
];

const TABLES_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("table_type", VARCHAR_10),
];

const COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("column_name", SYSNAME),
    column("ordinal_position", INT),
    column("column_default", NVARCHAR_4000),
    column("is_nullable", VARCHAR_3),
    column("data_type", SYSNAME),
    column("character_maximum_length", INT),
    column("character_octet_length", INT),
    column("numeric_precision", TINYINT),
    column("numeric_precision_radix", SMALLINT),
    column("numeric_scale", INT),
    column("datetime_precision", SMALLINT),
    column("character_set_catalog", SYSNAME),
    column("character_set_schema", SYSNAME),
    column("character_set_name", SYSNAME),
    column("collation_catalog", SYSNAME),
    column("collation_schema", SYSNAME),
    column("collation_name", SYSNAME),
    column("domain_catalog", SYSNAME),
    column("domain_schema", SYSNAME),
    column("domain_name", SYSNAME),
];

const VIEWS_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("view_definition", NVARCHAR_4000),
    column("check_option", VARCHAR_7),
    column("is_updatable", VARCHAR_2),
];

const VIEW_TABLE_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("view_catalog", SYSNAME),
    column("view_schema", SYSNAME),
    column("view_name", SYSNAME),
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
];

const SEQUENCES_COLUMNS: &[CatalogColumn] = &[
    column("sequence_catalog", SYSNAME),
    column("sequence_schema", SYSNAME),
    column("sequence_name", SYSNAME),
    column("data_type", SYSNAME),
    column("numeric_precision", TINYINT),
    column("numeric_precision_radix", SMALLINT),
    column("numeric_scale", INT),
    column("start_value", BIGINT),
    column("minimum_value", BIGINT),
    column("maximum_value", BIGINT),
    column("increment", BIGINT),
    column("cycle_option", BIT),
    column("declared_data_type", INT),
    column("declared_numeric_precision", INT),
    column("declared_numeric_scale", INT),
];

const TABLE_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("constraint_type", VARCHAR_11),
    column("is_deferrable", VARCHAR_2),
    column("initially_deferred", VARCHAR_2),
];

const KEY_COLUMN_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("column_name", SYSNAME),
    column("ordinal_position", INT),
];

const REFERENTIAL_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
    column("unique_constraint_catalog", SYSNAME),
    column("unique_constraint_schema", SYSNAME),
    column("unique_constraint_name", SYSNAME),
    column("match_option", VARCHAR_7),
    column("update_rule", VARCHAR_11),
    column("delete_rule", VARCHAR_11),
];

const CONSTRAINT_COLUMN_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("column_name", SYSNAME),
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
];

const CONSTRAINT_TABLE_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SYSNAME),
    column("table_schema", SYSNAME),
    column("table_name", SYSNAME),
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
];

const CHECK_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SYSNAME),
    column("constraint_schema", SYSNAME),
    column("constraint_name", SYSNAME),
    column("check_clause", NVARCHAR_4000),
];

const ROUTINES_COLUMNS: &[CatalogColumn] = &[
    column("specific_catalog", SYSNAME),
    column("specific_schema", SYSNAME),
    column("specific_name", SYSNAME),
    column("routine_catalog", SYSNAME),
    column("routine_schema", SYSNAME),
    column("routine_name", SYSNAME),
    column("routine_type", VARCHAR_20),
    column("data_type", SYSNAME),
    column("routine_body", VARCHAR_20),
    column("routine_definition", NVARCHAR_4000),
    column("created", DATETIME),
    column("last_altered", DATETIME),
];

const PARAMETERS_COLUMNS: &[CatalogColumn] = &[
    column("specific_catalog", SYSNAME),
    column("specific_schema", SYSNAME),
    column("specific_name", SYSNAME),
    column("ordinal_position", INT),
    column("parameter_mode", VARCHAR_10),
    column("is_result", VARCHAR_10),
    column("as_locator", VARCHAR_10),
    column("parameter_name", SYSNAME),
    column("data_type", SYSNAME),
    column("character_maximum_length", INT),
    column("numeric_precision", TINYINT),
    column("numeric_scale", INT),
];

pub(super) fn rows(snapshot: &CatalogSnapshot, view: SqlServerInformation) -> Vec<Vec<Value>> {
    let objects = snapshot.sqlserver_objects();
    let catalog = snapshot.database.as_str();
    match view {
        SqlServerInformation::Schemata => schemata_rows(catalog, objects),
        SqlServerInformation::Tables => tables_rows(catalog, objects),
        SqlServerInformation::Columns => columns_rows(snapshot, objects),
        SqlServerInformation::Views => views_rows(snapshot, objects),
        SqlServerInformation::ViewTableUsage => view_table_usage_rows(snapshot, objects),
        SqlServerInformation::Sequences => sequences_rows(snapshot, objects),
        SqlServerInformation::TableConstraints => table_constraints_rows(snapshot, objects),
        SqlServerInformation::KeyColumnUsage => key_column_usage_rows(snapshot, objects),
        SqlServerInformation::ReferentialConstraints => {
            referential_constraints_rows(snapshot, objects)
        }
        SqlServerInformation::ConstraintColumnUsage => {
            constraint_column_usage_rows(snapshot, objects)
        }
        SqlServerInformation::ConstraintTableUsage => {
            constraint_table_usage_rows(snapshot, objects)
        }
        SqlServerInformation::CheckConstraints => check_constraints_rows(snapshot, objects),
        SqlServerInformation::Routines => routines_rows(snapshot, catalog, objects),
        SqlServerInformation::Parameters => parameters_rows(snapshot, catalog, objects),
    }
}

fn schemata_rows(catalog: &str, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    objects
        .schemas
        .iter()
        .map(|schema| {
            let owner = if schema.schema_id <= super::SYS_SCHEMA_ID {
                schema.name.as_str()
            } else {
                super::DBO
            };
            CatalogRow::new(SCHEMATA_COLUMNS)
                .set("catalog_name", text(catalog))
                .set("schema_name", text(&schema.name))
                .set("schema_owner", text(owner))
                .set("default_character_set_name", text("iso_1"))
                .done()
        })
        .collect()
}

fn located(
    row: CatalogRow,
    prefix: &str,
    catalog: &str,
    objects: &SqlServerObjects,
    object: &SqlServerObject,
) -> CatalogRow {
    row.set(&format!("{prefix}_catalog"), text(catalog))
        .set(
            &format!("{prefix}_schema"),
            text(objects.schema_name(object.schema_id)),
        )
        .set(&format!("{prefix}_name"), text(&object.name))
}

fn tables_rows(catalog: &str, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    objects
        .relations()
        .filter(|object| matches!(object.kind, ObjectKind::Table | ObjectKind::View))
        .map(|object| {
            let kind = if object.kind == ObjectKind::View {
                "VIEW"
            } else {
                "BASE TABLE"
            };
            located(
                CatalogRow::new(TABLES_COLUMNS),
                "table",
                catalog,
                objects,
                object,
            )
            .set("table_type", text(kind))
            .done()
        })
        .collect()
}

pub(super) fn column_values(row: CatalogRow, column_type: SqlServerColumnType) -> CatalogRow {
    let character_set = column_type.name.is_character().then(|| {
        if column_type.name.is_unicode() {
            UNICODE_CHARACTER_SET
        } else {
            "iso_1"
        }
    });
    row.set("data_type", text(column_type.name.name()))
        .set(
            "character_maximum_length",
            optional_int(column_type.character_length()),
        )
        .set(
            "character_octet_length",
            optional_int(column_type.octet_length()),
        )
        .set(
            "numeric_precision",
            optional_small(column_type.numeric_precision()),
        )
        .set(
            "numeric_precision_radix",
            optional_small(column_type.numeric_radix()),
        )
        .set("numeric_scale", optional_int(column_type.numeric_scale()))
        .set(
            "datetime_precision",
            optional_small(column_type.datetime_precision()),
        )
        .set(
            "character_set_name",
            character_set.map_or(Value::Null, text),
        )
        .set(
            "collation_name",
            if column_type.name.is_character() {
                text(super::COLLATION)
            } else {
                Value::Null
            },
        )
}

fn columns_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    let mut rows = Vec::new();
    for object in objects.relations() {
        if !matches!(object.kind, ObjectKind::Table | ObjectKind::View) {
            continue;
        }
        let Some(entry) = snapshot.relations.get(object.relation) else {
            continue;
        };
        let table = entry.table();
        for (position, facts) in relation_columns(entry).into_iter().enumerate() {
            let default = table
                .and_then(|table| table.columns.get(position))
                .and_then(|column| column.default.as_deref())
                .map(default_definition);
            let row = located(
                CatalogRow::new(COLUMNS_COLUMNS),
                "table",
                catalog,
                objects,
                object,
            )
            .set("column_name", text(facts.name))
            .set("ordinal_position", int(position as i64 + 1))
            .set(
                "column_default",
                default.as_deref().map_or(Value::Null, text),
            )
            .set(
                "is_nullable",
                text(if facts.nullable { "YES" } else { "NO" }),
            );
            rows.push(column_values(row, facts.column_type).done());
        }
    }
    rows
}

fn views_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    objects
        .relations()
        .filter(|object| object.kind == ObjectKind::View)
        .map(|object| {
            let definition = match snapshot
                .relations
                .get(object.relation)
                .map(|entry| &entry.source)
            {
                Some(RelationSource::View(view)) => Value::Text(format!(
                    "CREATE VIEW [{}].[{}] AS {}",
                    objects.schema_name(object.schema_id),
                    object.name,
                    view.query
                )),
                _ => Value::Null,
            };
            located(
                CatalogRow::new(VIEWS_COLUMNS),
                "table",
                catalog,
                objects,
                object,
            )
            .set("view_definition", definition)
            .set("check_option", text("NONE"))
            .set("is_updatable", text("NO"))
            .done()
        })
        .collect()
}

fn view_table_usage_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    let mut rows = Vec::new();
    for object in objects
        .relations()
        .filter(|object| object.kind == ObjectKind::View)
    {
        let Some(RelationSource::View(view)) = snapshot
            .relations
            .get(object.relation)
            .map(|entry| &entry.source)
        else {
            continue;
        };
        for reference in &view.references {
            let Some(used) = snapshot
                .find_internal(reference)
                .and_then(|at| objects.of_relation(at))
            else {
                continue;
            };
            rows.push(
                located(
                    located(
                        CatalogRow::new(VIEW_TABLE_USAGE_COLUMNS),
                        "view",
                        catalog,
                        objects,
                        object,
                    ),
                    "table",
                    catalog,
                    objects,
                    used,
                )
                .done(),
            );
        }
    }
    rows
}

fn sequences_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    objects
        .relations()
        .filter(|object| object.kind == ObjectKind::Sequence)
        .filter_map(|object| {
            let RelationSource::Sequence(sequence) =
                &snapshot.relations.get(object.relation)?.source
            else {
                return None;
            };
            let column_type = SqlServerColumnType::of(sequence.sql_type);
            Some(
                located(
                    CatalogRow::new(SEQUENCES_COLUMNS),
                    "sequence",
                    catalog,
                    objects,
                    object,
                )
                .set("data_type", text(column_type.name.name()))
                .set(
                    "numeric_precision",
                    optional_small(column_type.numeric_precision()),
                )
                .set(
                    "numeric_precision_radix",
                    optional_small(column_type.numeric_radix()),
                )
                .set("numeric_scale", optional_int(column_type.numeric_scale()))
                .set("start_value", Value::BigInt(sequence.start))
                .set("minimum_value", Value::BigInt(sequence.min))
                .set("maximum_value", Value::BigInt(sequence.max))
                .set("increment", Value::BigInt(sequence.increment))
                .set("cycle_option", Value::Boolean(sequence.cycle))
                .set("declared_data_type", int(column_type.system_type_id()))
                .set(
                    "declared_numeric_precision",
                    optional_int(column_type.numeric_precision()),
                )
                .set(
                    "declared_numeric_scale",
                    optional_int(column_type.numeric_scale()),
                )
                .done(),
            )
        })
        .collect()
}

fn constraint_kind(kind: ObjectKind) -> Option<&'static str> {
    Some(match kind {
        ObjectKind::PrimaryKey => "PRIMARY KEY",
        ObjectKind::Unique => "UNIQUE",
        ObjectKind::ForeignKey => "FOREIGN KEY",
        ObjectKind::Check => "CHECK",
        _ => return None,
    })
}

fn constraints(objects: &SqlServerObjects) -> impl Iterator<Item = &SqlServerObject> {
    objects
        .objects
        .iter()
        .filter(|object| constraint_kind(object.kind).is_some())
}

fn owner<'a>(
    objects: &'a SqlServerObjects,
    object: &SqlServerObject,
) -> Option<&'a SqlServerObject> {
    objects.object(object.parent_object_id)
}

fn table_constraints_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    constraints(objects)
        .filter_map(|object| {
            let table = owner(objects, object)?;
            Some(
                located(
                    located(
                        CatalogRow::new(TABLE_CONSTRAINTS_COLUMNS),
                        "constraint",
                        catalog,
                        objects,
                        object,
                    ),
                    "table",
                    catalog,
                    objects,
                    table,
                )
                .set("constraint_type", text(constraint_kind(object.kind)?))
                .set("is_deferrable", text("NO"))
                .set("initially_deferred", text("NO"))
                .done(),
            )
        })
        .collect()
}

fn constraint_columns(snapshot: &CatalogSnapshot, object: &SqlServerObject) -> Vec<usize> {
    let ObjectSource::Constraint(at) = object.source else {
        return Vec::new();
    };
    let Some(constraint) = snapshot.constraints.get(at) else {
        return Vec::new();
    };
    let Some(table) = snapshot
        .relations
        .get(object.relation)
        .and_then(|entry| entry.table())
    else {
        return Vec::new();
    };
    match constraint.source {
        ConstraintSource::Primary => {
            crate::pg_catalog::index_columns(table, IndexSource::Primary).to_vec()
        }
        ConstraintSource::Unique(position) => {
            crate::pg_catalog::index_columns(table, IndexSource::Secondary(position)).to_vec()
        }
        ConstraintSource::Foreign(position) => table
            .foreign_keys
            .get(position)
            .map_or_else(Vec::new, |key| key.columns.clone()),
        ConstraintSource::Check(position) => {
            crate::information_schema::check_columns(table, position)
        }
    }
}

fn column_name(snapshot: &CatalogSnapshot, relation: usize, column: usize) -> Option<&str> {
    snapshot
        .relations
        .get(relation)?
        .table()?
        .columns
        .get(column)
        .map(|found| found.name.as_str())
}

fn key_column_usage_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    let mut rows = Vec::new();
    for object in constraints(objects).filter(|object| object.kind != ObjectKind::Check) {
        let Some(table) = owner(objects, object) else {
            continue;
        };
        for (ordinal, column) in constraint_columns(snapshot, object).into_iter().enumerate() {
            let Some(name) = column_name(snapshot, object.relation, column) else {
                continue;
            };
            rows.push(
                located(
                    located(
                        CatalogRow::new(KEY_COLUMN_USAGE_COLUMNS),
                        "constraint",
                        catalog,
                        objects,
                        object,
                    ),
                    "table",
                    catalog,
                    objects,
                    table,
                )
                .set("column_name", text(name))
                .set("ordinal_position", int(ordinal as i64 + 1))
                .done(),
            );
        }
    }
    rows
}

fn rule(action: crate::ast::ReferentialAction) -> &'static str {
    match action {
        crate::ast::ReferentialAction::NoAction | crate::ast::ReferentialAction::Restrict => {
            "NO ACTION"
        }
        crate::ast::ReferentialAction::Cascade => "CASCADE",
        crate::ast::ReferentialAction::SetNull => "SET NULL",
        crate::ast::ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

pub(super) fn referenced_constraint_name(
    snapshot: &CatalogSnapshot,
    relation: Option<usize>,
    index: Option<IndexSource>,
) -> Option<String> {
    let table = snapshot.relations.get(relation?)?.table()?;
    match index? {
        IndexSource::Primary => Some(table.primary_key_constraint_name()),
        IndexSource::Secondary(at) => table.indexes.get(at).map(|found| found.name.clone()),
    }
}

fn referential_constraints_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    objects
        .objects
        .iter()
        .filter(|object| object.kind == ObjectKind::ForeignKey)
        .filter_map(|object| {
            let ObjectSource::Constraint(at) = object.source else {
                return None;
            };
            let ConstraintSource::Foreign(key_at) = snapshot.constraints.get(at)?.source else {
                return None;
            };
            let facts = foreign_key_facts(snapshot, object.relation, key_at)?;
            let unique_name = referenced_constraint_name(
                snapshot,
                facts.referenced_relation,
                facts.referenced_index,
            );
            let referenced_schema = facts
                .referenced_relation
                .and_then(|relation| objects.of_relation(relation))
                .map(|found| objects.schema_name(found.schema_id).to_owned());
            let mut row = located(
                CatalogRow::new(REFERENTIAL_CONSTRAINTS_COLUMNS),
                "constraint",
                catalog,
                objects,
                object,
            )
            .set("match_option", text("SIMPLE"))
            .set("update_rule", text(rule(facts.key.on_update)))
            .set("delete_rule", text(rule(facts.key.on_delete)));
            if let (Some(name), Some(schema)) = (unique_name, referenced_schema) {
                row = row
                    .set("unique_constraint_catalog", text(catalog))
                    .set("unique_constraint_schema", text(&schema))
                    .set("unique_constraint_name", text(&name));
            }
            Some(row.done())
        })
        .collect()
}

fn constraint_column_usage_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    let mut rows = Vec::new();
    for object in constraints(objects) {
        let Some(table) = owner(objects, object) else {
            continue;
        };
        for column in constraint_columns(snapshot, object) {
            let Some(name) = column_name(snapshot, object.relation, column) else {
                continue;
            };
            rows.push(
                located(
                    located(
                        CatalogRow::new(CONSTRAINT_COLUMN_USAGE_COLUMNS),
                        "table",
                        catalog,
                        objects,
                        table,
                    ),
                    "constraint",
                    catalog,
                    objects,
                    object,
                )
                .set("column_name", text(name))
                .done(),
            );
        }
    }
    rows
}

fn constraint_table_usage_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    constraints(objects)
        .filter_map(|object| {
            let table = owner(objects, object)?;
            Some(
                located(
                    located(
                        CatalogRow::new(CONSTRAINT_TABLE_USAGE_COLUMNS),
                        "table",
                        catalog,
                        objects,
                        table,
                    ),
                    "constraint",
                    catalog,
                    objects,
                    object,
                )
                .done(),
            )
        })
        .collect()
}

fn check_constraints_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let catalog = snapshot.database.as_str();
    objects
        .objects
        .iter()
        .filter(|object| object.kind == ObjectKind::Check)
        .filter_map(|object| {
            let ObjectSource::Constraint(at) = object.source else {
                return None;
            };
            let ConstraintSource::Check(check_at) = snapshot.constraints.get(at)?.source else {
                return None;
            };
            let check = snapshot
                .relations
                .get(object.relation)?
                .table()?
                .checks
                .get(check_at)?;
            Some(
                located(
                    CatalogRow::new(CHECK_CONSTRAINTS_COLUMNS),
                    "constraint",
                    catalog,
                    objects,
                    object,
                )
                .set(
                    "check_clause",
                    text(&super::check_definition(&check.expression)),
                )
                .done(),
            )
        })
        .collect()
}

const ROUTINE_DEFINITION_LENGTH: usize = 4000;

fn routines_rows(
    snapshot: &CatalogSnapshot,
    catalog: &str,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    objects
        .routines()
        .map(|object| {
            let (routine_type, data_type) = match object.kind {
                ObjectKind::Procedure => ("PROCEDURE", None),
                ObjectKind::InlineTableFunction | ObjectKind::TableFunction => {
                    ("FUNCTION", Some("TABLE"))
                }
                _ => (
                    "FUNCTION",
                    routine_definition(snapshot, object)
                        .and_then(scalar_return_type)
                        .map(|returned| returned.name.name()),
                ),
            };
            let definition: Option<String> = module_definition(snapshot, objects, object)
                .map(|text| text.chars().take(ROUTINE_DEFINITION_LENGTH).collect());
            let row = located(
                CatalogRow::new(ROUTINES_COLUMNS),
                "specific",
                catalog,
                objects,
                object,
            );
            located(row, "routine", catalog, objects, object)
                .set("routine_type", text(routine_type))
                .set("data_type", data_type.map_or(Value::Null, text))
                .set("routine_body", text("SQL"))
                .set("routine_definition", definition.map_or(Value::Null, Value::Text))
                .set("created", super::UNKNOWN_DATE)
                .set("last_altered", super::UNKNOWN_DATE)
                .done()
        })
        .collect()
}

fn parameters_rows(
    snapshot: &CatalogSnapshot,
    catalog: &str,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for object in objects.routines() {
        let Some(definition) = routine_definition(snapshot, object) else {
            continue;
        };
        let parameter_row = |ordinal: i64,
                             mode: &str,
                             is_result: bool,
                             name: &str,
                             column_type: SqlServerColumnType| {
            located(
                CatalogRow::new(PARAMETERS_COLUMNS),
                "specific",
                catalog,
                objects,
                object,
            )
            .set("ordinal_position", int(ordinal))
            .set("parameter_mode", text(mode))
            .set("is_result", text(if is_result { "YES" } else { "NO" }))
            .set("as_locator", text("NO"))
            .set("parameter_name", text(name))
            .set("data_type", text(column_type.name.name()))
            .set(
                "character_maximum_length",
                optional_int(column_type.character_length()),
            )
            .set(
                "numeric_precision",
                optional_small(column_type.numeric_precision()),
            )
            .set("numeric_scale", optional_int(column_type.numeric_scale()))
            .done()
        };
        if let Some(returned) = scalar_return_type(definition) {
            rows.push(parameter_row(0, "OUT", true, "", returned));
        }
        for (position, facts) in parameter_facts(definition).into_iter().enumerate() {
            let mode = if facts.output { "INOUT" } else { "IN" };
            let ordinal = i64::try_from(position).map_or(i64::MAX, |at| at + 1);
            rows.push(parameter_row(ordinal, mode, false, facts.name, facts.column_type));
        }
    }
    rows
}
