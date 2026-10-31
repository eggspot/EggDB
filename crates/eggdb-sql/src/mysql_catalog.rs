//! MySQL's `information_schema` for the MySQL dialect, as DBeaver, MySQL Workbench and
//! Connector/J read it: the tables, columns, keys and constraints of the database, built from the
//! same catalog snapshot as PostgreSQL's `information_schema`, in MySQL's shapes and words
//! (`BASE TABLE`, `int`, `varchar(40)`, `PRI`, `auto_increment`, the constraint `PRIMARY`), plus
//! the fixed answers about engines, character sets, collations and the process list.

use crate::information_schema::{self, InformationRelation};
use crate::pg_catalog::{CatalogColumn, CatalogSnapshot};
use crate::types::{SqlType, Value};
use std::collections::HashMap;

const NAME: SqlType = SqlType::Varchar(Some(64));
const TEXT: SqlType = SqlType::Text;
const NUMBER: SqlType = SqlType::BigInt;
const STAMP: SqlType = SqlType::Timestamp(None);
const CATALOG: &str = "def";
const ENGINE: &str = "InnoDB";
const CHARACTER_SET: &str = "utf8mb4";
const COLLATION: &str = "utf8mb4_0900_ai_ci";
const FIRST_OID: u32 = 13_400;
const POSTGRESQL_DEFAULT_SCHEMA: &str = "public";

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

/// A view of MySQL's `information_schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MySqlRelation {
    Schemata,
    Tables,
    Columns,
    Statistics,
    KeyColumnUsage,
    TableConstraints,
    ReferentialConstraints,
    Views,
    Routines,
    Parameters,
    Triggers,
    Events,
    Partitions,
    Engines,
    CharacterSets,
    Collations,
    Plugins,
    ProcessList,
}

const SCHEMATA: &[CatalogColumn] = &[
    column("catalog_name", NAME),
    column("schema_name", NAME),
    column("default_character_set_name", NAME),
    column("default_collation_name", NAME),
    column("sql_path", TEXT),
    column("default_encryption", NAME),
];

const TABLES: &[CatalogColumn] = &[
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("table_type", NAME),
    column("engine", NAME),
    column("version", NUMBER),
    column("row_format", NAME),
    column("table_rows", NUMBER),
    column("avg_row_length", NUMBER),
    column("data_length", NUMBER),
    column("max_data_length", NUMBER),
    column("index_length", NUMBER),
    column("data_free", NUMBER),
    column("auto_increment", NUMBER),
    column("create_time", STAMP),
    column("update_time", STAMP),
    column("check_time", STAMP),
    column("table_collation", NAME),
    column("checksum", NUMBER),
    column("create_options", TEXT),
    column("table_comment", TEXT),
];

const COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("column_name", NAME),
    column("ordinal_position", NUMBER),
    column("column_default", TEXT),
    column("is_nullable", NAME),
    column("data_type", NAME),
    column("character_maximum_length", NUMBER),
    column("character_octet_length", NUMBER),
    column("numeric_precision", NUMBER),
    column("numeric_scale", NUMBER),
    column("datetime_precision", NUMBER),
    column("character_set_name", NAME),
    column("collation_name", NAME),
    column("column_type", TEXT),
    column("column_key", NAME),
    column("extra", NAME),
    column("privileges", NAME),
    column("column_comment", TEXT),
    column("generation_expression", TEXT),
    column("srs_id", NUMBER),
];

const STATISTICS: &[CatalogColumn] = &[
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("non_unique", NUMBER),
    column("index_schema", NAME),
    column("index_name", NAME),
    column("seq_in_index", NUMBER),
    column("column_name", NAME),
    column("collation", NAME),
    column("cardinality", NUMBER),
    column("sub_part", NUMBER),
    column("packed", NAME),
    column("nullable", NAME),
    column("index_type", NAME),
    column("comment", NAME),
    column("index_comment", NAME),
    column("is_visible", NAME),
    column("expression", TEXT),
];

const KEY_COLUMN_USAGE: &[CatalogColumn] = &[
    column("constraint_catalog", NAME),
    column("constraint_schema", NAME),
    column("constraint_name", NAME),
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("column_name", NAME),
    column("ordinal_position", NUMBER),
    column("position_in_unique_constraint", NUMBER),
    column("referenced_table_schema", NAME),
    column("referenced_table_name", NAME),
    column("referenced_column_name", NAME),
];

const TABLE_CONSTRAINTS: &[CatalogColumn] = &[
    column("constraint_catalog", NAME),
    column("constraint_schema", NAME),
    column("constraint_name", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("constraint_type", NAME),
    column("enforced", NAME),
];

const REFERENTIAL_CONSTRAINTS: &[CatalogColumn] = &[
    column("constraint_catalog", NAME),
    column("constraint_schema", NAME),
    column("constraint_name", NAME),
    column("unique_constraint_catalog", NAME),
    column("unique_constraint_schema", NAME),
    column("unique_constraint_name", NAME),
    column("match_option", NAME),
    column("update_rule", NAME),
    column("delete_rule", NAME),
    column("table_name", NAME),
    column("referenced_table_name", NAME),
];

const VIEWS: &[CatalogColumn] = &[
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("view_definition", TEXT),
    column("check_option", NAME),
    column("is_updatable", NAME),
    column("definer", NAME),
    column("security_type", NAME),
    column("character_set_client", NAME),
    column("collation_connection", NAME),
];

const ROUTINES: &[CatalogColumn] = &[
    column("specific_name", NAME),
    column("routine_catalog", NAME),
    column("routine_schema", NAME),
    column("routine_name", NAME),
    column("routine_type", NAME),
    column("data_type", NAME),
    column("routine_definition", TEXT),
    column("definer", NAME),
    column("routine_comment", TEXT),
];

const PARAMETERS: &[CatalogColumn] = &[
    column("specific_catalog", NAME),
    column("specific_schema", NAME),
    column("specific_name", NAME),
    column("ordinal_position", NUMBER),
    column("parameter_mode", NAME),
    column("parameter_name", NAME),
    column("data_type", NAME),
    column("dtd_identifier", TEXT),
    column("routine_type", NAME),
];

const TRIGGERS: &[CatalogColumn] = &[
    column("trigger_catalog", NAME),
    column("trigger_schema", NAME),
    column("trigger_name", NAME),
    column("event_manipulation", NAME),
    column("event_object_schema", NAME),
    column("event_object_table", NAME),
    column("action_statement", TEXT),
    column("action_timing", NAME),
];

const EVENTS: &[CatalogColumn] = &[
    column("event_catalog", NAME),
    column("event_schema", NAME),
    column("event_name", NAME),
    column("status", NAME),
];

const PARTITIONS: &[CatalogColumn] = &[
    column("table_catalog", NAME),
    column("table_schema", NAME),
    column("table_name", NAME),
    column("partition_name", NAME),
    column("partition_method", NAME),
    column("table_rows", NUMBER),
];

const ENGINES: &[CatalogColumn] = &[
    column("engine", NAME),
    column("support", NAME),
    column("comment", TEXT),
    column("transactions", NAME),
    column("xa", NAME),
    column("savepoints", NAME),
];

const CHARACTER_SETS: &[CatalogColumn] = &[
    column("character_set_name", NAME),
    column("default_collate_name", NAME),
    column("description", NAME),
    column("maxlen", NUMBER),
];

const COLLATIONS: &[CatalogColumn] = &[
    column("collation_name", NAME),
    column("character_set_name", NAME),
    column("id", NUMBER),
    column("is_default", NAME),
    column("is_compiled", NAME),
    column("sortlen", NUMBER),
    column("pad_attribute", NAME),
];

const PLUGINS: &[CatalogColumn] = &[
    column("plugin_name", NAME),
    column("plugin_version", NAME),
    column("plugin_status", NAME),
    column("plugin_type", NAME),
    column("plugin_library", NAME),
];

const PROCESS_LIST: &[CatalogColumn] = &[
    column("id", NUMBER),
    column("user", NAME),
    column("host", NAME),
    column("db", NAME),
    column("command", NAME),
    column("time", NUMBER),
    column("state", NAME),
    column("info", TEXT),
];

impl MySqlRelation {
    pub(crate) const ALL: [Self; 18] = [
        Self::Schemata,
        Self::Tables,
        Self::Columns,
        Self::Statistics,
        Self::KeyColumnUsage,
        Self::TableConstraints,
        Self::ReferentialConstraints,
        Self::Views,
        Self::Routines,
        Self::Parameters,
        Self::Triggers,
        Self::Events,
        Self::Partitions,
        Self::Engines,
        Self::CharacterSets,
        Self::Collations,
        Self::Plugins,
        Self::ProcessList,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Schemata => "schemata",
            Self::Tables => "tables",
            Self::Columns => "columns",
            Self::Statistics => "statistics",
            Self::KeyColumnUsage => "key_column_usage",
            Self::TableConstraints => "table_constraints",
            Self::ReferentialConstraints => "referential_constraints",
            Self::Views => "views",
            Self::Routines => "routines",
            Self::Parameters => "parameters",
            Self::Triggers => "triggers",
            Self::Events => "events",
            Self::Partitions => "partitions",
            Self::Engines => "engines",
            Self::CharacterSets => "character_sets",
            Self::Collations => "collations",
            Self::Plugins => "plugins",
            Self::ProcessList => "processlist",
        }
    }

    /// The view `name` refers to, written `information_schema.<view>` in any case.
    pub(crate) fn referenced(name: &str) -> Option<Self> {
        let (schema, base) = name.split_once(crate::schemas::QUALIFIER)?;
        if !schema.eq_ignore_ascii_case(crate::schemas::INFORMATION_SCHEMA) {
            return None;
        }
        Self::ALL.into_iter().find(|relation| relation.name().eq_ignore_ascii_case(base))
    }

    pub(crate) fn oid(self) -> u32 {
        FIRST_OID + Self::ALL.iter().position(|relation| *relation == self).unwrap_or(0) as u32
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Schemata => SCHEMATA,
            Self::Tables => TABLES,
            Self::Columns => COLUMNS,
            Self::Statistics => STATISTICS,
            Self::KeyColumnUsage => KEY_COLUMN_USAGE,
            Self::TableConstraints => TABLE_CONSTRAINTS,
            Self::ReferentialConstraints => REFERENTIAL_CONSTRAINTS,
            Self::Views => VIEWS,
            Self::Routines => ROUTINES,
            Self::Parameters => PARAMETERS,
            Self::Triggers => TRIGGERS,
            Self::Events => EVENTS,
            Self::Partitions => PARTITIONS,
            Self::Engines => ENGINES,
            Self::CharacterSets => CHARACTER_SETS,
            Self::Collations => COLLATIONS,
            Self::Plugins => PLUGINS,
            Self::ProcessList => PROCESS_LIST,
        }
    }
}

type NamedRow = HashMap<&'static str, Value>;

fn standard_rows(snapshot: &CatalogSnapshot, relation: InformationRelation) -> Vec<NamedRow> {
    let names: Vec<&'static str> = relation.columns().iter().map(|column| column.name).collect();
    information_schema::rows(snapshot, relation)
        .into_iter()
        .map(|values| names.iter().copied().zip(values).collect())
        .filter(|row: &NamedRow| {
            !matches!(
                text_of(row, "table_schema").or_else(|| text_of(row, "constraint_schema")).as_deref(),
                Some("pg_catalog" | "information_schema")
            )
        })
        .collect()
}

fn text_of(row: &NamedRow, name: &str) -> Option<String> {
    row.get(name).and_then(Value::to_text)
}

fn number_of(row: &NamedRow, name: &str) -> Value {
    row.get(name).and_then(Value::as_integer).map_or(Value::Null, Value::BigInt)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn optional(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

fn mysql_schema(snapshot: &CatalogSnapshot, schema: Option<String>) -> Value {
    match schema.as_deref() {
        None | Some(POSTGRESQL_DEFAULT_SCHEMA) => Value::Text(snapshot.database.clone()),
        Some(other) => Value::Text(other.to_owned()),
    }
}

fn build(columns: &'static [CatalogColumn], values: Vec<(&str, Value)>) -> Vec<Value> {
    let mut row = vec![Value::Null; columns.len()];
    for (name, value) in values {
        if let Some(at) = columns.iter().position(|column| column.name == name) {
            row[at] = value;
        }
    }
    row
}

fn mysql_data_type(standard: &str) -> &'static str {
    match standard {
        "integer" => "int",
        "smallint" => "smallint",
        "bigint" => "bigint",
        "character varying" => "varchar",
        "character" => "char",
        "text" => "longtext",
        "numeric" => "decimal",
        "real" => "float",
        "double precision" => "double",
        "boolean" => "tinyint",
        "date" => "date",
        "time without time zone" | "time with time zone" => "time",
        "timestamp without time zone" => "datetime",
        "timestamp with time zone" => "timestamp",
        "bytea" => "longblob",
        "json" | "jsonb" => "json",
        "uuid" => "char",
        _ => "varchar",
    }
}

fn mysql_column_type(data_type: &str, row: &NamedRow) -> String {
    let length = row.get("character_maximum_length").and_then(Value::as_integer);
    let precision = row.get("numeric_precision").and_then(Value::as_integer);
    let scale = row.get("numeric_scale").and_then(Value::as_integer);
    match data_type {
        "varchar" | "char" => format!("{data_type}({})", length.unwrap_or(255)),
        "decimal" => format!("decimal({},{})", precision.unwrap_or(65), scale.unwrap_or(0)),
        "tinyint" => "tinyint(1)".to_owned(),
        other => other.to_owned(),
    }
}

struct Keys {
    primary: HashMap<(String, String), Vec<String>>,
    unique: Vec<(String, String, String)>,
    constraint_types: HashMap<String, String>,
}

fn keys(snapshot: &CatalogSnapshot) -> Keys {
    let constraint_types: HashMap<String, String> = standard_rows(snapshot, InformationRelation::TableConstraints)
        .iter()
        .filter_map(|row| Some((text_of(row, "constraint_name")?, text_of(row, "constraint_type")?)))
        .collect();
    let mut primary: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut unique = Vec::new();
    for row in standard_rows(snapshot, InformationRelation::KeyColumnUsage) {
        let (Some(constraint), Some(table), Some(column)) =
            (text_of(&row, "constraint_name"), text_of(&row, "table_name"), text_of(&row, "column_name"))
        else {
            continue;
        };
        let schema = text_of(&row, "table_schema").unwrap_or_default();
        match constraint_types.get(&constraint).map(String::as_str) {
            Some("PRIMARY KEY") => primary.entry((schema, table)).or_default().push(column),
            Some("UNIQUE") => unique.push((schema, table, column)),
            _ => {}
        }
    }
    Keys { primary, unique, constraint_types }
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: MySqlRelation) -> Vec<Vec<Value>> {
    let columns = relation.columns();
    match relation {
        MySqlRelation::Schemata => [snapshot.database.clone(), "information_schema".to_owned()]
            .into_iter()
            .map(|schema| {
                build(columns, vec![
                    ("catalog_name", text(CATALOG)),
                    ("schema_name", Value::Text(schema)),
                    ("default_character_set_name", text(CHARACTER_SET)),
                    ("default_collation_name", text(COLLATION)),
                    ("default_encryption", text("NO")),
                ])
            })
            .collect(),
        MySqlRelation::Tables => standard_rows(snapshot, InformationRelation::Tables)
            .iter()
            .map(|row| {
                let is_view = text_of(row, "table_type").as_deref() == Some("VIEW");
                build(columns, vec![
                    ("table_catalog", text(CATALOG)),
                    ("table_schema", mysql_schema(snapshot, text_of(row, "table_schema"))),
                    ("table_name", optional(text_of(row, "table_name"))),
                    ("table_type", text(if is_view { "VIEW" } else { "BASE TABLE" })),
                    ("engine", if is_view { Value::Null } else { text(ENGINE) }),
                    ("version", if is_view { Value::Null } else { Value::BigInt(10) }),
                    ("row_format", if is_view { Value::Null } else { text("Dynamic") }),
                    ("table_rows", Value::BigInt(0)),
                    ("avg_row_length", Value::BigInt(0)),
                    ("data_length", Value::BigInt(0)),
                    ("max_data_length", Value::BigInt(0)),
                    ("index_length", Value::BigInt(0)),
                    ("data_free", Value::BigInt(0)),
                    ("table_collation", if is_view { Value::Null } else { text(COLLATION) }),
                    ("create_options", text("")),
                    ("table_comment", text(if is_view { "VIEW" } else { "" })),
                ])
            })
            .collect(),
        MySqlRelation::Columns => {
            let keys = keys(snapshot);
            standard_rows(snapshot, InformationRelation::Columns)
                .iter()
                .map(|row| {
                    let standard = text_of(row, "data_type").unwrap_or_default();
                    let data_type = mysql_data_type(&standard);
                    let schema = text_of(row, "table_schema").unwrap_or_default();
                    let table = text_of(row, "table_name").unwrap_or_default();
                    let name = text_of(row, "column_name").unwrap_or_default();
                    let key = if keys.primary.get(&(schema.clone(), table.clone())).is_some_and(|key| key.contains(&name)) {
                        "PRI"
                    } else if keys.unique.contains(&(schema.clone(), table.clone(), name.clone())) {
                        "UNI"
                    } else {
                        ""
                    };
                    let default = text_of(row, "column_default");
                    let generated = text_of(row, "is_identity").as_deref() == Some("YES")
                        || default.as_deref().is_some_and(|value| value.starts_with("nextval("));
                    let textual = matches!(data_type, "varchar" | "char" | "longtext");
                    build(columns, vec![
                        ("table_catalog", text(CATALOG)),
                        ("table_schema", mysql_schema(snapshot, Some(schema))),
                        ("table_name", Value::Text(table)),
                        ("column_name", Value::Text(name)),
                        ("ordinal_position", number_of(row, "ordinal_position")),
                        ("column_default", if generated { Value::Null } else { optional(default) }),
                        ("is_nullable", optional(text_of(row, "is_nullable"))),
                        ("data_type", text(data_type)),
                        ("character_maximum_length", number_of(row, "character_maximum_length")),
                        ("character_octet_length", number_of(row, "character_octet_length")),
                        ("numeric_precision", number_of(row, "numeric_precision")),
                        ("numeric_scale", number_of(row, "numeric_scale")),
                        ("datetime_precision", number_of(row, "datetime_precision")),
                        ("character_set_name", if textual { text(CHARACTER_SET) } else { Value::Null }),
                        ("collation_name", if textual { text(COLLATION) } else { Value::Null }),
                        ("column_type", Value::Text(mysql_column_type(data_type, row))),
                        ("column_key", text(key)),
                        ("extra", text(if generated { "auto_increment" } else { "" })),
                        ("privileges", text("select,insert,update,references")),
                        ("column_comment", text("")),
                        ("generation_expression", text("")),
                    ])
                })
                .collect()
        }
        MySqlRelation::Statistics => {
            let keys = keys(snapshot);
            let mut built = Vec::new();
            let mut primaries: Vec<_> = keys.primary.iter().collect();
            primaries.sort();
            for ((schema, table), key) in primaries {
                for (position, column) in key.iter().enumerate() {
                    built.push(build(columns, vec![
                        ("table_catalog", text(CATALOG)),
                        ("table_schema", mysql_schema(snapshot, Some(schema.clone()))),
                        ("table_name", Value::Text(table.clone())),
                        ("non_unique", Value::BigInt(0)),
                        ("index_schema", mysql_schema(snapshot, Some(schema.clone()))),
                        ("index_name", text("PRIMARY")),
                        ("seq_in_index", Value::BigInt(position as i64 + 1)),
                        ("column_name", Value::Text(column.clone())),
                        ("collation", text("A")),
                        ("cardinality", Value::BigInt(0)),
                        ("nullable", text("")),
                        ("index_type", text("BTREE")),
                        ("comment", text("")),
                        ("index_comment", text("")),
                        ("is_visible", text("YES")),
                    ]));
                }
            }
            built
        }
        MySqlRelation::KeyColumnUsage => {
            let keys = keys(snapshot);
            let references: HashMap<String, String> = standard_rows(snapshot, InformationRelation::ReferentialConstraints)
                .iter()
                .filter_map(|row| Some((text_of(row, "constraint_name")?, text_of(row, "unique_constraint_name")?)))
                .collect();
            let usage = standard_rows(snapshot, InformationRelation::KeyColumnUsage);
            let unique_columns: HashMap<(String, i64), (String, String, String)> = usage
                .iter()
                .filter_map(|row| {
                    Some((
                        (text_of(row, "constraint_name")?, row.get("ordinal_position")?.as_integer()?),
                        (text_of(row, "table_schema")?, text_of(row, "table_name")?, text_of(row, "column_name")?),
                    ))
                })
                .collect();
            usage
                .iter()
                .map(|row| {
                    let constraint = text_of(row, "constraint_name").unwrap_or_default();
                    let is_primary = keys.constraint_types.get(&constraint).map(String::as_str) == Some("PRIMARY KEY");
                    let referenced = references.get(&constraint).and_then(|unique| {
                        let position = row.get("position_in_unique_constraint").and_then(Value::as_integer)?;
                        unique_columns.get(&(unique.clone(), position))
                    });
                    build(columns, vec![
                        ("constraint_catalog", text(CATALOG)),
                        ("constraint_schema", mysql_schema(snapshot, text_of(row, "constraint_schema"))),
                        ("constraint_name", Value::Text(if is_primary { "PRIMARY".to_owned() } else { constraint })),
                        ("table_catalog", text(CATALOG)),
                        ("table_schema", mysql_schema(snapshot, text_of(row, "table_schema"))),
                        ("table_name", optional(text_of(row, "table_name"))),
                        ("column_name", optional(text_of(row, "column_name"))),
                        ("ordinal_position", number_of(row, "ordinal_position")),
                        ("position_in_unique_constraint", number_of(row, "position_in_unique_constraint")),
                        ("referenced_table_schema", referenced.map_or(Value::Null, |(schema, _, _)| mysql_schema(snapshot, Some(schema.clone())))),
                        ("referenced_table_name", referenced.map_or(Value::Null, |(_, table, _)| Value::Text(table.clone()))),
                        ("referenced_column_name", referenced.map_or(Value::Null, |(_, _, column)| Value::Text(column.clone()))),
                    ])
                })
                .collect()
        }
        MySqlRelation::TableConstraints => standard_rows(snapshot, InformationRelation::TableConstraints)
            .iter()
            .filter(|row| text_of(row, "constraint_type").as_deref() != Some("CHECK") || !text_of(row, "constraint_name").unwrap_or_default().ends_with("_not_null"))
            .map(|row| {
                let kind = text_of(row, "constraint_type").unwrap_or_default();
                let name = if kind == "PRIMARY KEY" { "PRIMARY".to_owned() } else { text_of(row, "constraint_name").unwrap_or_default() };
                build(columns, vec![
                    ("constraint_catalog", text(CATALOG)),
                    ("constraint_schema", mysql_schema(snapshot, text_of(row, "constraint_schema"))),
                    ("constraint_name", Value::Text(name)),
                    ("table_schema", mysql_schema(snapshot, text_of(row, "table_schema"))),
                    ("table_name", optional(text_of(row, "table_name"))),
                    ("constraint_type", Value::Text(kind)),
                    ("enforced", text("YES")),
                ])
            })
            .collect(),
        MySqlRelation::ReferentialConstraints => {
            let tables: HashMap<String, String> = standard_rows(snapshot, InformationRelation::TableConstraints)
                .iter()
                .filter_map(|row| Some((text_of(row, "constraint_name")?, text_of(row, "table_name")?)))
                .collect();
            standard_rows(snapshot, InformationRelation::ReferentialConstraints)
                .iter()
                .map(|row| {
                    let name = text_of(row, "constraint_name").unwrap_or_default();
                    let unique = text_of(row, "unique_constraint_name").unwrap_or_default();
                    build(columns, vec![
                        ("constraint_catalog", text(CATALOG)),
                        ("constraint_schema", mysql_schema(snapshot, text_of(row, "constraint_schema"))),
                        ("constraint_name", Value::Text(name.clone())),
                        ("unique_constraint_catalog", text(CATALOG)),
                        ("unique_constraint_schema", mysql_schema(snapshot, text_of(row, "unique_constraint_schema"))),
                        ("unique_constraint_name", Value::Text(unique.clone())),
                        ("match_option", text("NONE")),
                        ("update_rule", optional(text_of(row, "update_rule"))),
                        ("delete_rule", optional(text_of(row, "delete_rule"))),
                        ("table_name", optional(tables.get(&name).cloned())),
                        ("referenced_table_name", optional(tables.get(&unique).cloned())),
                    ])
                })
                .collect()
        }
        MySqlRelation::Views => standard_rows(snapshot, InformationRelation::Views)
            .iter()
            .map(|row| {
                build(columns, vec![
                    ("table_catalog", text(CATALOG)),
                    ("table_schema", mysql_schema(snapshot, text_of(row, "table_schema"))),
                    ("table_name", optional(text_of(row, "table_name"))),
                    ("view_definition", optional(text_of(row, "view_definition"))),
                    ("check_option", text("NONE")),
                    ("is_updatable", optional(text_of(row, "is_updatable"))),
                    ("definer", text("eggdb@%")),
                    ("security_type", text("DEFINER")),
                    ("character_set_client", text(CHARACTER_SET)),
                    ("collation_connection", text(COLLATION)),
                ])
            })
            .collect(),
        MySqlRelation::Engines => vec![build(columns, vec![
            ("engine", text(ENGINE)),
            ("support", text("DEFAULT")),
            ("comment", text("EggDB's storage engine, shown as InnoDB: transactions, row-level consistency and foreign keys")),
            ("transactions", text("YES")),
            ("xa", text("NO")),
            ("savepoints", text("NO")),
        ])],
        MySqlRelation::CharacterSets => vec![
            build(columns, vec![
                ("character_set_name", text(CHARACTER_SET)),
                ("default_collate_name", text(COLLATION)),
                ("description", text("UTF-8 Unicode")),
                ("maxlen", Value::BigInt(4)),
            ]),
            build(columns, vec![
                ("character_set_name", text("binary")),
                ("default_collate_name", text("binary")),
                ("description", text("Binary pseudo charset")),
                ("maxlen", Value::BigInt(1)),
            ]),
        ],
        MySqlRelation::Collations => vec![
            build(columns, vec![
                ("collation_name", text(COLLATION)),
                ("character_set_name", text(CHARACTER_SET)),
                ("id", Value::BigInt(255)),
                ("is_default", text("Yes")),
                ("is_compiled", text("Yes")),
                ("sortlen", Value::BigInt(0)),
                ("pad_attribute", text("NO PAD")),
            ]),
            build(columns, vec![
                ("collation_name", text("binary")),
                ("character_set_name", text("binary")),
                ("id", Value::BigInt(63)),
                ("is_default", text("Yes")),
                ("is_compiled", text("Yes")),
                ("sortlen", Value::BigInt(1)),
                ("pad_attribute", text("NO PAD")),
            ]),
        ],
        MySqlRelation::ProcessList => vec![build(columns, vec![
            ("id", Value::BigInt(i64::from(std::process::id()))),
            ("user", Value::Text(crate::pg_catalog::session_user().unwrap_or_else(|| "eggdb".to_owned()))),
            ("host", text("localhost")),
            ("db", Value::Text(snapshot.database.clone())),
            ("command", text("Query")),
            ("time", Value::BigInt(0)),
            ("state", text("executing")),
        ])],
        MySqlRelation::Routines
        | MySqlRelation::Parameters
        | MySqlRelation::Triggers
        | MySqlRelation::Events
        | MySqlRelation::Partitions
        | MySqlRelation::Plugins => Vec::new(),
    }
}

/// The columns of a `SHOW` statement that reads one of the fixed views: each label and the view
/// column it shows.
pub(crate) fn show_columns(view: MySqlRelation) -> &'static [(&'static str, &'static str)] {
    match view {
        MySqlRelation::Engines => &[
            ("Engine", "engine"),
            ("Support", "support"),
            ("Comment", "comment"),
            ("Transactions", "transactions"),
            ("XA", "xa"),
            ("Savepoints", "savepoints"),
        ],
        MySqlRelation::CharacterSets => &[
            ("Charset", "character_set_name"),
            ("Description", "description"),
            ("Default collation", "default_collate_name"),
            ("Maxlen", "maxlen"),
        ],
        MySqlRelation::Collations => &[
            ("Collation", "collation_name"),
            ("Charset", "character_set_name"),
            ("Id", "id"),
            ("Default", "is_default"),
            ("Compiled", "is_compiled"),
            ("Sortlen", "sortlen"),
            ("Pad_attribute", "pad_attribute"),
        ],
        MySqlRelation::Plugins => &[
            ("Name", "plugin_name"),
            ("Status", "plugin_status"),
            ("Type", "plugin_type"),
            ("Library", "plugin_library"),
        ],
        MySqlRelation::ProcessList => &[
            ("Id", "id"),
            ("User", "user"),
            ("Host", "host"),
            ("db", "db"),
            ("Command", "command"),
            ("Time", "time"),
            ("State", "state"),
            ("Info", "info"),
        ],
        _ => &[],
    }
}

/// The rows of such a `SHOW` statement, each value as text.
pub(crate) fn show_rows(snapshot: &CatalogSnapshot, view: MySqlRelation) -> Vec<Vec<Value>> {
    let shown = show_columns(view);
    let positions: Vec<Option<usize>> = shown
        .iter()
        .map(|(_, source)| view.columns().iter().position(|column| column.name == *source))
        .collect();
    rows(snapshot, view)
        .into_iter()
        .map(|row| {
            positions
                .iter()
                .map(|position| {
                    position
                        .and_then(|at| row.get(at))
                        .and_then(Value::to_text)
                        .map_or(Value::Null, Value::Text)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Dialect, Session};

    fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.join("\n")
    }

    #[test]
    fn database_tools_read_mysql_information_schema_and_show_statements() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::MySql);
        session.identify("eggdb", "shop");
        run(&mut session, "CREATE TABLE customers (id int AUTO_INCREMENT PRIMARY KEY, email varchar(80) UNIQUE)");
        run(&mut session, "CREATE TABLE orders (id int PRIMARY KEY, customer_id int, amount decimal(10,2), FOREIGN KEY (customer_id) REFERENCES customers (id))");
        let tables = run(&mut session, "SELECT table_schema, table_name, table_type, engine FROM information_schema.tables ORDER BY table_name");
        assert!(tables.contains("shop") && tables.contains("customers") && tables.contains("BASE TABLE") && tables.contains("InnoDB"), "{tables}");
        let columns = run(&mut session, "SELECT column_name, column_type, column_key, extra FROM information_schema.columns WHERE table_name = 'customers' ORDER BY ordinal_position");
        assert!(columns.contains("int") && columns.contains("PRI") && columns.contains("auto_increment"), "{columns}");
        assert!(columns.contains("varchar(80)") && columns.contains("UNI"), "{columns}");
        let keys = run(&mut session, "SELECT constraint_name, referenced_table_name FROM information_schema.key_column_usage WHERE table_name = 'orders' ORDER BY ordinal_position, constraint_name");
        assert!(keys.contains("PRIMARY") && keys.contains("customers"), "{keys}");
        let statistics = run(&mut session, "SELECT index_name, column_name FROM information_schema.statistics WHERE table_name = 'customers'");
        assert!(statistics.contains("PRIMARY"), "{statistics}");
        assert!(run(&mut session, "SHOW ENGINES").contains("InnoDB"));
        assert!(run(&mut session, "SHOW CHARACTER SET LIKE 'utf8%'").contains("utf8mb4"));
        assert!(run(&mut session, "SHOW COLLATION").contains("utf8mb4_0900_ai_ci"));
        run(&mut session, "SHOW PLUGINS");
        assert!(run(&mut session, "SHOW FULL PROCESSLIST").contains("shop"));
        run(&mut session, "SHOW GLOBAL VARIABLES LIKE 'version%'");
        assert!(run(&mut session, "SELECT schema_name FROM information_schema.schemata").contains("shop"));
    }
}
