use crate::encoding::{IdentityGeneration, TableDefinition, integer_type_range};
use crate::lexer::TokenKind;
use crate::routines::{ParameterMode, ReturnType, RoutineKind, RoutineLanguage, RoutineReturns, Volatility};
use crate::pg_catalog::{
    CatalogColumn, CatalogSnapshot, CatalogType, ConstraintSource, RelationEntry, RelationSource,
    RoutineEntry, builtin_type, check_text, default_text, format_type, type_modifier, type_oid,
};
use crate::triggers::{TriggerAction, TriggerDefinition, TriggerEvent, TriggerLevel, TriggerTiming};
use crate::types::{SqlType, Value};
use std::collections::HashMap;

#[cfg(test)]
mod tests;

const SQL_IDENTIFIER: SqlType = SqlType::Catalog(CatalogType::Name);
const CARDINAL_NUMBER: SqlType = SqlType::Integer;
const CHARACTER_DATA: SqlType = SqlType::Varchar(None);
const YES_OR_NO: SqlType = SqlType::Varchar(Some(3));
const TIME_STAMP: SqlType = SqlType::TimestampTz(Some(2));
const MAX_TEXT_OCTETS: i64 = 1 << 30;
const UTF8_MAX_CHARACTER_BYTES: i64 = 4;
const FIRST_RELATION_OID: u32 = 13100;

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InformationRelation {
    Schemata,
    Tables,
    Columns,
    Views,
    Sequences,
    TableConstraints,
    KeyColumnUsage,
    ReferentialConstraints,
    ConstraintColumnUsage,
    CheckConstraints,
    Routines,
    Parameters,
    Triggers,
}

impl InformationRelation {
    pub(crate) const ALL: [Self; 13] = [
        Self::Schemata,
        Self::Tables,
        Self::Columns,
        Self::Views,
        Self::Sequences,
        Self::TableConstraints,
        Self::KeyColumnUsage,
        Self::ReferentialConstraints,
        Self::ConstraintColumnUsage,
        Self::CheckConstraints,
        Self::Routines,
        Self::Parameters,
        Self::Triggers,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Schemata => "schemata",
            Self::Tables => "tables",
            Self::Columns => "columns",
            Self::Views => "views",
            Self::Sequences => "sequences",
            Self::TableConstraints => "table_constraints",
            Self::KeyColumnUsage => "key_column_usage",
            Self::ReferentialConstraints => "referential_constraints",
            Self::ConstraintColumnUsage => "constraint_column_usage",
            Self::CheckConstraints => "check_constraints",
            Self::Routines => "routines",
            Self::Parameters => "parameters",
            Self::Triggers => "triggers",
        }
    }

    pub(crate) fn oid(self) -> u32 {
        let position = Self::ALL
            .iter()
            .position(|relation| *relation == self)
            .unwrap_or(0);
        FIRST_RELATION_OID + u32::try_from(position).unwrap_or(0)
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|relation| relation.name() == name)
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Schemata => SCHEMATA_COLUMNS,
            Self::Tables => TABLES_COLUMNS,
            Self::Columns => COLUMNS_COLUMNS,
            Self::Views => VIEWS_COLUMNS,
            Self::Sequences => SEQUENCES_COLUMNS,
            Self::TableConstraints => TABLE_CONSTRAINTS_COLUMNS,
            Self::KeyColumnUsage => KEY_COLUMN_USAGE_COLUMNS,
            Self::ReferentialConstraints => REFERENTIAL_CONSTRAINTS_COLUMNS,
            Self::ConstraintColumnUsage => CONSTRAINT_COLUMN_USAGE_COLUMNS,
            Self::CheckConstraints => CHECK_CONSTRAINTS_COLUMNS,
            Self::Routines => ROUTINES_COLUMNS,
            Self::Parameters => PARAMETERS_COLUMNS,
            Self::Triggers => TRIGGERS_COLUMNS,
        }
    }
}

const SCHEMATA_COLUMNS: &[CatalogColumn] = &[
    column("catalog_name", SQL_IDENTIFIER),
    column("schema_name", SQL_IDENTIFIER),
    column("schema_owner", SQL_IDENTIFIER),
    column("default_character_set_catalog", SQL_IDENTIFIER),
    column("default_character_set_schema", SQL_IDENTIFIER),
    column("default_character_set_name", SQL_IDENTIFIER),
    column("sql_path", CHARACTER_DATA),
];

const TABLES_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("table_type", CHARACTER_DATA),
    column("self_referencing_column_name", SQL_IDENTIFIER),
    column("reference_generation", CHARACTER_DATA),
    column("user_defined_type_catalog", SQL_IDENTIFIER),
    column("user_defined_type_schema", SQL_IDENTIFIER),
    column("user_defined_type_name", SQL_IDENTIFIER),
    column("is_insertable_into", YES_OR_NO),
    column("is_typed", YES_OR_NO),
    column("commit_action", CHARACTER_DATA),
];

const COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("column_name", SQL_IDENTIFIER),
    column("ordinal_position", CARDINAL_NUMBER),
    column("column_default", CHARACTER_DATA),
    column("is_nullable", YES_OR_NO),
    column("data_type", CHARACTER_DATA),
    column("character_maximum_length", CARDINAL_NUMBER),
    column("character_octet_length", CARDINAL_NUMBER),
    column("numeric_precision", CARDINAL_NUMBER),
    column("numeric_precision_radix", CARDINAL_NUMBER),
    column("numeric_scale", CARDINAL_NUMBER),
    column("datetime_precision", CARDINAL_NUMBER),
    column("interval_type", CHARACTER_DATA),
    column("interval_precision", CARDINAL_NUMBER),
    column("character_set_catalog", SQL_IDENTIFIER),
    column("character_set_schema", SQL_IDENTIFIER),
    column("character_set_name", SQL_IDENTIFIER),
    column("collation_catalog", SQL_IDENTIFIER),
    column("collation_schema", SQL_IDENTIFIER),
    column("collation_name", SQL_IDENTIFIER),
    column("domain_catalog", SQL_IDENTIFIER),
    column("domain_schema", SQL_IDENTIFIER),
    column("domain_name", SQL_IDENTIFIER),
    column("udt_catalog", SQL_IDENTIFIER),
    column("udt_schema", SQL_IDENTIFIER),
    column("udt_name", SQL_IDENTIFIER),
    column("scope_catalog", SQL_IDENTIFIER),
    column("scope_schema", SQL_IDENTIFIER),
    column("scope_name", SQL_IDENTIFIER),
    column("maximum_cardinality", CARDINAL_NUMBER),
    column("dtd_identifier", SQL_IDENTIFIER),
    column("is_self_referencing", YES_OR_NO),
    column("is_identity", YES_OR_NO),
    column("identity_generation", CHARACTER_DATA),
    column("identity_start", CHARACTER_DATA),
    column("identity_increment", CHARACTER_DATA),
    column("identity_maximum", CHARACTER_DATA),
    column("identity_minimum", CHARACTER_DATA),
    column("identity_cycle", YES_OR_NO),
    column("is_generated", CHARACTER_DATA),
    column("generation_expression", CHARACTER_DATA),
    column("is_updatable", YES_OR_NO),
];

const VIEWS_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("view_definition", CHARACTER_DATA),
    column("check_option", CHARACTER_DATA),
    column("is_updatable", YES_OR_NO),
    column("is_insertable_into", YES_OR_NO),
    column("is_trigger_updatable", YES_OR_NO),
    column("is_trigger_deletable", YES_OR_NO),
    column("is_trigger_insertable_into", YES_OR_NO),
];

const TRIGGERS_COLUMNS: &[CatalogColumn] = &[
    column("trigger_catalog", SQL_IDENTIFIER),
    column("trigger_schema", SQL_IDENTIFIER),
    column("trigger_name", SQL_IDENTIFIER),
    column("event_manipulation", CHARACTER_DATA),
    column("event_object_catalog", SQL_IDENTIFIER),
    column("event_object_schema", SQL_IDENTIFIER),
    column("event_object_table", SQL_IDENTIFIER),
    column("action_order", CARDINAL_NUMBER),
    column("action_condition", CHARACTER_DATA),
    column("action_statement", CHARACTER_DATA),
    column("action_orientation", CHARACTER_DATA),
    column("action_timing", CHARACTER_DATA),
    column("action_reference_old_table", SQL_IDENTIFIER),
    column("action_reference_new_table", SQL_IDENTIFIER),
    column("action_reference_old_row", SQL_IDENTIFIER),
    column("action_reference_new_row", SQL_IDENTIFIER),
    column("created", TIME_STAMP),
];

const SEQUENCES_COLUMNS: &[CatalogColumn] = &[
    column("sequence_catalog", SQL_IDENTIFIER),
    column("sequence_schema", SQL_IDENTIFIER),
    column("sequence_name", SQL_IDENTIFIER),
    column("data_type", CHARACTER_DATA),
    column("numeric_precision", CARDINAL_NUMBER),
    column("numeric_precision_radix", CARDINAL_NUMBER),
    column("numeric_scale", CARDINAL_NUMBER),
    column("start_value", CHARACTER_DATA),
    column("minimum_value", CHARACTER_DATA),
    column("maximum_value", CHARACTER_DATA),
    column("increment", CHARACTER_DATA),
    column("cycle_option", YES_OR_NO),
];

const TABLE_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SQL_IDENTIFIER),
    column("constraint_schema", SQL_IDENTIFIER),
    column("constraint_name", SQL_IDENTIFIER),
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("constraint_type", CHARACTER_DATA),
    column("is_deferrable", YES_OR_NO),
    column("initially_deferred", YES_OR_NO),
    column("enforced", YES_OR_NO),
    column("nulls_distinct", YES_OR_NO),
];

const KEY_COLUMN_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SQL_IDENTIFIER),
    column("constraint_schema", SQL_IDENTIFIER),
    column("constraint_name", SQL_IDENTIFIER),
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("column_name", SQL_IDENTIFIER),
    column("ordinal_position", CARDINAL_NUMBER),
    column("position_in_unique_constraint", CARDINAL_NUMBER),
];

const REFERENTIAL_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SQL_IDENTIFIER),
    column("constraint_schema", SQL_IDENTIFIER),
    column("constraint_name", SQL_IDENTIFIER),
    column("unique_constraint_catalog", SQL_IDENTIFIER),
    column("unique_constraint_schema", SQL_IDENTIFIER),
    column("unique_constraint_name", SQL_IDENTIFIER),
    column("match_option", CHARACTER_DATA),
    column("update_rule", CHARACTER_DATA),
    column("delete_rule", CHARACTER_DATA),
];

const CONSTRAINT_COLUMN_USAGE_COLUMNS: &[CatalogColumn] = &[
    column("table_catalog", SQL_IDENTIFIER),
    column("table_schema", SQL_IDENTIFIER),
    column("table_name", SQL_IDENTIFIER),
    column("column_name", SQL_IDENTIFIER),
    column("constraint_catalog", SQL_IDENTIFIER),
    column("constraint_schema", SQL_IDENTIFIER),
    column("constraint_name", SQL_IDENTIFIER),
];

const CHECK_CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("constraint_catalog", SQL_IDENTIFIER),
    column("constraint_schema", SQL_IDENTIFIER),
    column("constraint_name", SQL_IDENTIFIER),
    column("check_clause", CHARACTER_DATA),
];

const ROUTINES_COLUMNS: &[CatalogColumn] = &[
    column("specific_catalog", SQL_IDENTIFIER),
    column("specific_schema", SQL_IDENTIFIER),
    column("specific_name", SQL_IDENTIFIER),
    column("routine_catalog", SQL_IDENTIFIER),
    column("routine_schema", SQL_IDENTIFIER),
    column("routine_name", SQL_IDENTIFIER),
    column("routine_type", CHARACTER_DATA),
    column("module_catalog", SQL_IDENTIFIER),
    column("module_schema", SQL_IDENTIFIER),
    column("module_name", SQL_IDENTIFIER),
    column("udt_catalog", SQL_IDENTIFIER),
    column("udt_schema", SQL_IDENTIFIER),
    column("udt_name", SQL_IDENTIFIER),
    column("data_type", CHARACTER_DATA),
    column("character_maximum_length", CARDINAL_NUMBER),
    column("character_octet_length", CARDINAL_NUMBER),
    column("character_set_catalog", SQL_IDENTIFIER),
    column("character_set_schema", SQL_IDENTIFIER),
    column("character_set_name", SQL_IDENTIFIER),
    column("collation_catalog", SQL_IDENTIFIER),
    column("collation_schema", SQL_IDENTIFIER),
    column("collation_name", SQL_IDENTIFIER),
    column("numeric_precision", CARDINAL_NUMBER),
    column("numeric_precision_radix", CARDINAL_NUMBER),
    column("numeric_scale", CARDINAL_NUMBER),
    column("datetime_precision", CARDINAL_NUMBER),
    column("interval_type", CHARACTER_DATA),
    column("interval_precision", CARDINAL_NUMBER),
    column("type_udt_catalog", SQL_IDENTIFIER),
    column("type_udt_schema", SQL_IDENTIFIER),
    column("type_udt_name", SQL_IDENTIFIER),
    column("scope_catalog", SQL_IDENTIFIER),
    column("scope_schema", SQL_IDENTIFIER),
    column("scope_name", SQL_IDENTIFIER),
    column("maximum_cardinality", CARDINAL_NUMBER),
    column("dtd_identifier", SQL_IDENTIFIER),
    column("routine_body", CHARACTER_DATA),
    column("routine_definition", CHARACTER_DATA),
    column("external_name", CHARACTER_DATA),
    column("external_language", CHARACTER_DATA),
    column("parameter_style", CHARACTER_DATA),
    column("is_deterministic", YES_OR_NO),
    column("sql_data_access", CHARACTER_DATA),
    column("is_null_call", YES_OR_NO),
    column("sql_path", CHARACTER_DATA),
    column("schema_level_routine", YES_OR_NO),
    column("max_dynamic_result_sets", CARDINAL_NUMBER),
    column("is_user_defined_cast", YES_OR_NO),
    column("is_implicitly_invocable", YES_OR_NO),
    column("security_type", CHARACTER_DATA),
    column("to_sql_specific_catalog", SQL_IDENTIFIER),
    column("to_sql_specific_schema", SQL_IDENTIFIER),
    column("to_sql_specific_name", SQL_IDENTIFIER),
    column("as_locator", YES_OR_NO),
    column("created", TIME_STAMP),
    column("last_altered", TIME_STAMP),
    column("new_savepoint_level", YES_OR_NO),
    column("is_udt_dependent", YES_OR_NO),
    column("result_cast_from_data_type", CHARACTER_DATA),
    column("result_cast_as_locator", YES_OR_NO),
    column("result_cast_char_max_length", CARDINAL_NUMBER),
    column("result_cast_char_octet_length", CARDINAL_NUMBER),
    column("result_cast_char_set_catalog", SQL_IDENTIFIER),
    column("result_cast_char_set_schema", SQL_IDENTIFIER),
    column("result_cast_char_set_name", SQL_IDENTIFIER),
    column("result_cast_collation_catalog", SQL_IDENTIFIER),
    column("result_cast_collation_schema", SQL_IDENTIFIER),
    column("result_cast_collation_name", SQL_IDENTIFIER),
    column("result_cast_numeric_precision", CARDINAL_NUMBER),
    column("result_cast_numeric_precision_radix", CARDINAL_NUMBER),
    column("result_cast_numeric_scale", CARDINAL_NUMBER),
    column("result_cast_datetime_precision", CARDINAL_NUMBER),
    column("result_cast_interval_type", CHARACTER_DATA),
    column("result_cast_interval_precision", CARDINAL_NUMBER),
    column("result_cast_type_udt_catalog", SQL_IDENTIFIER),
    column("result_cast_type_udt_schema", SQL_IDENTIFIER),
    column("result_cast_type_udt_name", SQL_IDENTIFIER),
    column("result_cast_scope_catalog", SQL_IDENTIFIER),
    column("result_cast_scope_schema", SQL_IDENTIFIER),
    column("result_cast_scope_name", SQL_IDENTIFIER),
    column("result_cast_maximum_cardinality", CARDINAL_NUMBER),
    column("result_cast_dtd_identifier", SQL_IDENTIFIER),
];

const PARAMETERS_COLUMNS: &[CatalogColumn] = &[
    column("specific_catalog", SQL_IDENTIFIER),
    column("specific_schema", SQL_IDENTIFIER),
    column("specific_name", SQL_IDENTIFIER),
    column("ordinal_position", CARDINAL_NUMBER),
    column("parameter_mode", CHARACTER_DATA),
    column("is_result", YES_OR_NO),
    column("as_locator", YES_OR_NO),
    column("parameter_name", SQL_IDENTIFIER),
    column("data_type", CHARACTER_DATA),
    column("character_maximum_length", CARDINAL_NUMBER),
    column("character_octet_length", CARDINAL_NUMBER),
    column("character_set_catalog", SQL_IDENTIFIER),
    column("character_set_schema", SQL_IDENTIFIER),
    column("character_set_name", SQL_IDENTIFIER),
    column("collation_catalog", SQL_IDENTIFIER),
    column("collation_schema", SQL_IDENTIFIER),
    column("collation_name", SQL_IDENTIFIER),
    column("numeric_precision", CARDINAL_NUMBER),
    column("numeric_precision_radix", CARDINAL_NUMBER),
    column("numeric_scale", CARDINAL_NUMBER),
    column("datetime_precision", CARDINAL_NUMBER),
    column("interval_type", CHARACTER_DATA),
    column("interval_precision", CARDINAL_NUMBER),
    column("udt_catalog", SQL_IDENTIFIER),
    column("udt_schema", SQL_IDENTIFIER),
    column("udt_name", SQL_IDENTIFIER),
    column("scope_catalog", SQL_IDENTIFIER),
    column("scope_schema", SQL_IDENTIFIER),
    column("scope_name", SQL_IDENTIFIER),
    column("maximum_cardinality", CARDINAL_NUMBER),
    column("dtd_identifier", SQL_IDENTIFIER),
    column("parameter_default", CHARACTER_DATA),
];

struct InformationRow {
    relation: InformationRelation,
    values: Vec<Value>,
}

impl InformationRow {
    fn new(relation: InformationRelation) -> Self {
        Self {
            relation,
            values: vec![Value::Null; relation.columns().len()],
        }
    }

    fn set(mut self, name: &str, value: Value) -> Self {
        if let Some(at) = self
            .relation
            .columns()
            .iter()
            .position(|column| column.name == name)
            && let Some(slot) = self.values.get_mut(at)
        {
            *slot = value;
        }
        self
    }

    fn done(self) -> Vec<Value> {
        self.values
    }
}

fn identifier(text: &str) -> Value {
    Value::Text(text.to_owned())
}

fn character_data(text: &str) -> Value {
    Value::Text(text.to_owned())
}

fn cardinal(number: i64) -> Value {
    i32::try_from(number).map_or(Value::Null, Value::Integer)
}

fn optional_cardinal(number: Option<i64>) -> Value {
    number.map_or(Value::Null, cardinal)
}

fn yes_or_no(yes: bool) -> Value {
    character_data(if yes { "YES" } else { "NO" })
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: InformationRelation) -> Vec<Vec<Value>> {
    match relation {
        InformationRelation::Schemata => schemata_rows(snapshot),
        InformationRelation::Tables => tables_rows(snapshot),
        InformationRelation::Columns => columns_rows(snapshot),
        InformationRelation::Views => views_rows(snapshot),
        InformationRelation::Sequences => sequences_rows(snapshot),
        InformationRelation::TableConstraints => table_constraints_rows(snapshot),
        InformationRelation::KeyColumnUsage => key_column_usage_rows(snapshot),
        InformationRelation::ReferentialConstraints => referential_constraints_rows(snapshot),
        InformationRelation::ConstraintColumnUsage => constraint_column_usage_rows(snapshot),
        InformationRelation::CheckConstraints => check_constraints_rows(snapshot),
        InformationRelation::Routines => routines_rows(snapshot),
        InformationRelation::Parameters => parameters_rows(snapshot),
        InformationRelation::Triggers => triggers_rows(snapshot),
    }
}

fn schemata_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .schemas
        .iter()
        .map(|schema| {
            InformationRow::new(InformationRelation::Schemata)
                .set("catalog_name", identifier(&snapshot.database))
                .set("schema_name", identifier(&schema.name))
                .set(
                    "schema_owner",
                    identifier(&snapshot.role_name(schema.owner).unwrap_or_default()),
                )
                .done()
        })
        .collect()
}

fn is_view(entry: &RelationEntry) -> Option<bool> {
    match &entry.source {
        RelationSource::Table(_) => Some(false),
        RelationSource::View(_) => Some(true),
        RelationSource::Catalog(relation) => Some(relation.is_view()),
        RelationSource::Sequence(_) | RelationSource::Index { .. } => None,
    }
}

fn tables_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| {
            let view = is_view(entry)?;
            Some(
                InformationRow::new(InformationRelation::Tables)
                    .set("table_catalog", identifier(&snapshot.database))
                    .set("table_schema", identifier(&entry.schema))
                    .set("table_name", identifier(&entry.name))
                    .set(
                        "table_type",
                        character_data(if view { "VIEW" } else { "BASE TABLE" }),
                    )
                    .set("is_insertable_into", yes_or_no(!view))
                    .set("is_typed", yes_or_no(false))
                    .done(),
            )
        })
        .collect()
}

struct ColumnFacts<'a> {
    name: &'a str,
    sql_type: SqlType,
    not_null: bool,
    default: Option<String>,
    identity: Option<IdentityFacts>,
    domain: Option<u32>,
}

struct IdentityFacts {
    generation: IdentityGeneration,
    start: i64,
    increment: i64,
    maximum: i64,
    minimum: i64,
    cycle: bool,
}

fn identity_facts(
    snapshot: &CatalogSnapshot,
    table: &TableDefinition,
    position: usize,
) -> Option<IdentityFacts> {
    let generation = table.identity_of(position)?;
    let column = table.columns.get(position)?;
    let sequence = snapshot
        .relations
        .iter()
        .find_map(|entry| match &entry.source {
            RelationSource::Sequence(sequence)
                if sequence
                    .owned_by
                    .as_ref()
                    .is_some_and(|(owner, owned)| *owner == table.name && *owned == column.name) =>
            {
                Some(sequence)
            }
            _ => None,
        });
    Some(match sequence {
        Some(sequence) => IdentityFacts {
            generation,
            start: sequence.start,
            increment: sequence.increment,
            maximum: sequence.max,
            minimum: sequence.min,
            cycle: sequence.cycle,
        },
        None => {
            let seed = table.identity_seed();
            let (low, high) = integer_type_range(column.sql_type).unwrap_or((i64::MIN, i64::MAX));
            let (minimum, maximum) = if seed.increment > 0 {
                (seed.seed.min(1), high)
            } else {
                (low, seed.seed.max(-1))
            };
            IdentityFacts {
                generation,
                start: seed.seed,
                increment: seed.increment,
                maximum,
                minimum,
                cycle: false,
            }
        }
    })
}

fn relation_columns<'a>(
    snapshot: &CatalogSnapshot,
    at: usize,
    entry: &'a RelationEntry,
) -> Vec<ColumnFacts<'a>> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| ColumnFacts {
                name: &column.name,
                sql_type: column.sql_type,
                not_null: is_not_null(table, position),
                default: column
                    .default
                    .as_ref()
                    .filter(|_| table.identity_of(position).is_none())
                    .map(|_| default_text(snapshot, at, position)),
                identity: identity_facts(snapshot, table, position),
                domain: table.domain_of(position),
            })
            .collect(),
        RelationSource::View(view) => view
            .columns
            .iter()
            .map(|column| ColumnFacts {
                name: &column.name,
                sql_type: column.sql_type.unwrap_or(SqlType::Text),
                not_null: false,
                default: None,
                identity: None,
                domain: None,
            })
            .collect(),
        RelationSource::Catalog(relation) => relation
            .columns()
            .iter()
            .map(|column| ColumnFacts {
                name: column.name,
                sql_type: column.sql_type,
                not_null: false,
                default: None,
                identity: None,
                domain: None,
            })
            .collect(),
        RelationSource::Sequence(_) | RelationSource::Index { .. } => Vec::new(),
    }
}

fn is_not_null(table: &TableDefinition, position: usize) -> bool {
    table
        .columns
        .get(position)
        .is_some_and(|column| column.not_null)
        || table.primary_key.contains(&position)
}

pub(crate) struct TypeDescription {
    pub(crate) data_type: String,
    pub(crate) udt_name: Option<&'static str>,
    pub(crate) character_maximum_length: Option<i64>,
    pub(crate) character_octet_length: Option<i64>,
    pub(crate) numeric_precision: Option<i64>,
    pub(crate) numeric_precision_radix: Option<i64>,
    pub(crate) numeric_scale: Option<i64>,
    pub(crate) datetime_precision: Option<i64>,
    pub(crate) interval_type: Option<String>,
}

const TEXT: u32 = 25;
const BPCHAR: u32 = 1042;
const VARCHAR: u32 = 1043;
const SMALLINT: u32 = 21;
const INTEGER: u32 = 23;
const BIGINT: u32 = 20;
const REAL: u32 = 700;
const DOUBLE: u32 = 701;
const NUMERIC: u32 = 1700;
const DATE: u32 = 1082;
const TIME: u32 = 1083;
const TIMESTAMP: u32 = 1114;
const TIMESTAMPTZ: u32 = 1184;
const TIMETZ: u32 = 1266;
const INTERVAL: u32 = 1186;
const FULL_INTERVAL_PRECISION: i64 = 0xFFFF;
const DEFAULT_FRACTIONAL_DIGITS: i64 = 6;

pub(crate) fn describe_type(sql_type: SqlType) -> TypeDescription {
    let oid = type_oid(sql_type);
    let modifier = i64::from(type_modifier(sql_type));
    let builtin = builtin_type(oid);
    let is_array = builtin.is_some_and(|found| found.element != 0 && found.length == -1);
    let data_type = if is_array {
        "ARRAY".to_owned()
    } else if builtin.is_some() {
        format_type(oid, None)
    } else {
        "USER-DEFINED".to_owned()
    };
    let character_maximum_length =
        (matches!(oid, BPCHAR | VARCHAR) && modifier >= 0).then_some(modifier - 4);
    let character_octet_length = matches!(oid, TEXT | BPCHAR | VARCHAR).then(|| {
        character_maximum_length.map_or(MAX_TEXT_OCTETS, |length| length * UTF8_MAX_CHARACTER_BYTES)
    });
    let numeric_modifier = (oid == NUMERIC && modifier >= 0).then_some(modifier - 4);
    let numeric_precision = match oid {
        SMALLINT => Some(16),
        INTEGER => Some(32),
        BIGINT => Some(64),
        REAL => Some(24),
        DOUBLE => Some(53),
        NUMERIC => numeric_modifier.map(|packed| (packed >> 16) & 0xFFFF),
        _ => None,
    };
    let numeric_precision_radix = match oid {
        SMALLINT | INTEGER | BIGINT | REAL | DOUBLE => Some(2),
        NUMERIC => Some(10),
        _ => None,
    };
    let numeric_scale = match oid {
        SMALLINT | INTEGER | BIGINT => Some(0),
        NUMERIC => numeric_modifier.map(|packed| packed & 0xFFFF),
        _ => None,
    };
    let datetime_precision = match oid {
        DATE => Some(0),
        TIME | TIMETZ | TIMESTAMP | TIMESTAMPTZ => Some(if modifier < 0 {
            DEFAULT_FRACTIONAL_DIGITS
        } else {
            modifier
        }),
        INTERVAL => Some(
            if modifier < 0 || modifier & 0xFFFF == FULL_INTERVAL_PRECISION {
                DEFAULT_FRACTIONAL_DIGITS
            } else {
                modifier & 0xFFFF
            },
        ),
        _ => None,
    };
    let interval_type = (oid == INTERVAL && modifier >= 0)
        .then(|| interval_fields(&format_type(oid, i32::try_from(modifier).ok())))
        .flatten();
    TypeDescription {
        data_type,
        udt_name: builtin.map(|found| found.typname),
        character_maximum_length,
        character_octet_length,
        numeric_precision,
        numeric_precision_radix,
        numeric_scale,
        datetime_precision,
        interval_type,
    }
}

fn interval_fields(shown: &str) -> Option<String> {
    let rest = shown.strip_prefix("interval")?;
    let rest = rest.trim_start_matches(|character: char| {
        character == '(' || character == ')' || character.is_ascii_digit()
    });
    let fields = rest.strip_prefix(' ')?;
    (!fields.is_empty()).then(|| fields.to_ascii_uppercase())
}

fn columns_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (at, entry) in snapshot.relations.iter().enumerate() {
        let Some(view) = is_view(entry) else {
            continue;
        };
        for (position, column) in relation_columns(snapshot, at, entry)
            .into_iter()
            .enumerate()
        {
            rows.push(column_row(snapshot, entry, view, position + 1, &column));
        }
    }
    rows
}

fn column_row(
    snapshot: &CatalogSnapshot,
    entry: &RelationEntry,
    view: bool,
    number: usize,
    column: &ColumnFacts<'_>,
) -> Vec<Value> {
    let described = describe_type(column.sql_type);
    let mut row = InformationRow::new(InformationRelation::Columns)
        .set("table_catalog", identifier(&snapshot.database))
        .set("table_schema", identifier(&entry.schema))
        .set("table_name", identifier(&entry.name))
        .set("column_name", identifier(column.name))
        .set("ordinal_position", cardinal(number as i64))
        .set(
            "column_default",
            column
                .default
                .as_deref()
                .map_or(Value::Null, character_data),
        )
        .set("is_nullable", yes_or_no(!column.not_null))
        .set("data_type", character_data(&described.data_type))
        .set(
            "character_maximum_length",
            optional_cardinal(described.character_maximum_length),
        )
        .set(
            "character_octet_length",
            optional_cardinal(described.character_octet_length),
        )
        .set(
            "numeric_precision",
            optional_cardinal(described.numeric_precision),
        )
        .set(
            "numeric_precision_radix",
            optional_cardinal(described.numeric_precision_radix),
        )
        .set("numeric_scale", optional_cardinal(described.numeric_scale))
        .set(
            "datetime_precision",
            optional_cardinal(described.datetime_precision),
        )
        .set(
            "interval_type",
            described
                .interval_type
                .as_deref()
                .map_or(Value::Null, character_data),
        )
        .set("dtd_identifier", identifier(&number.to_string()))
        .set("is_self_referencing", yes_or_no(false))
        .set("is_identity", yes_or_no(column.identity.is_some()))
        .set(
            "identity_cycle",
            yes_or_no(column.identity.as_ref().is_some_and(|identity| identity.cycle)),
        )
        .set("is_generated", character_data("NEVER"))
        .set("is_updatable", yes_or_no(!view));
    if let Some(udt_name) = described.udt_name {
        row = row
            .set("udt_catalog", identifier(&snapshot.database))
            .set("udt_schema", identifier(crate::schemas::CATALOG_SCHEMA))
            .set("udt_name", identifier(udt_name));
    } else if let Some(defined) = column.sql_type.defined()
        && let Some(found) = snapshot.user_types.of_oid(defined.oid)
    {
        row = row
            .set("udt_catalog", identifier(&snapshot.database))
            .set(
                "udt_schema",
                identifier(crate::schemas::schema_of(&found.name)),
            )
            .set(
                "udt_name",
                identifier(crate::schemas::base_name(&found.name)),
            );
    }
    if let Some(domain) = column.domain
        && let Some(found) = snapshot.user_types.of_oid(domain)
    {
        row = row
            .set("domain_catalog", identifier(&snapshot.database))
            .set(
                "domain_schema",
                identifier(crate::schemas::schema_of(&found.name)),
            )
            .set(
                "domain_name",
                identifier(crate::schemas::base_name(&found.name)),
            );
    }
    if let Some(identity) = &column.identity {
        let generation = match identity.generation {
            IdentityGeneration::Always => "ALWAYS",
            IdentityGeneration::ByDefault => "BY DEFAULT",
        };
        row = row
            .set("identity_generation", character_data(generation))
            .set("identity_start", character_data(&identity.start.to_string()))
            .set(
                "identity_increment",
                character_data(&identity.increment.to_string()),
            )
            .set(
                "identity_maximum",
                character_data(&identity.maximum.to_string()),
            )
            .set(
                "identity_minimum",
                character_data(&identity.minimum.to_string()),
            );
    }
    row.done()
}

fn views_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter(|entry| is_view(entry) == Some(true))
        .map(|entry| {
            let definition = match &entry.source {
                RelationSource::View(view) => character_data(&view.query),
                _ => Value::Null,
            };
            let mut row = InformationRow::new(InformationRelation::Views)
                .set("table_catalog", identifier(&snapshot.database))
                .set("table_schema", identifier(&entry.schema))
                .set("table_name", identifier(&entry.name))
                .set("view_definition", definition)
                .set("check_option", character_data("NONE"));
            for flag in [
                "is_updatable",
                "is_insertable_into",
                "is_trigger_updatable",
                "is_trigger_deletable",
                "is_trigger_insertable_into",
            ] {
                row = row.set(flag, yes_or_no(false));
            }
            row.done()
        })
        .collect()
}

fn trigger_timing_name(timing: TriggerTiming) -> &'static str {
    match timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    }
}

fn trigger_statement(definition: &TriggerDefinition) -> String {
    match &definition.action {
        TriggerAction::Function { name, arguments } => {
            format!("EXECUTE FUNCTION {name}({})", arguments.join(", "))
        }
        TriggerAction::Statements(body) | TriggerAction::Batch(body) => body.clone(),
    }
}

fn triggers_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    let mut order: HashMap<(usize, &str, TriggerTiming, TriggerLevel), i64> = HashMap::new();
    for trigger in &snapshot.triggers {
        let Some(owner) = snapshot.relations.get(trigger.relation) else {
            continue;
        };
        let definition = &trigger.definition;
        let statement = trigger_statement(definition);
        let row_level = definition.level == TriggerLevel::Row;
        for event in definition.events.listed() {
            if event == TriggerEvent::Truncate {
                continue;
            }
            let position = order
                .entry((
                    trigger.relation,
                    event.keyword(),
                    definition.timing,
                    definition.level,
                ))
                .or_insert(0);
            *position += 1;
            let mut row = InformationRow::new(InformationRelation::Triggers)
                .set("trigger_catalog", identifier(&snapshot.database))
                .set("trigger_schema", identifier(&owner.schema))
                .set("trigger_name", identifier(&definition.name))
                .set("event_manipulation", character_data(event.keyword()))
                .set("event_object_catalog", identifier(&snapshot.database))
                .set("event_object_schema", identifier(&owner.schema))
                .set("event_object_table", identifier(&owner.name))
                .set("action_order", cardinal(*position))
                .set("action_statement", character_data(&statement))
                .set(
                    "action_orientation",
                    character_data(if row_level { "ROW" } else { "STATEMENT" }),
                )
                .set(
                    "action_timing",
                    character_data(trigger_timing_name(definition.timing)),
                )
                .set("action_reference_old_row", identifier("OLD"))
                .set("action_reference_new_row", identifier("NEW"));
            if let Some(condition) = &definition.condition {
                row = row.set("action_condition", character_data(&format!("({condition})")));
            }
            if let Some(name) = &definition.old_table {
                row = row.set("action_reference_old_table", identifier(name));
            }
            if let Some(name) = &definition.new_table {
                row = row.set("action_reference_new_table", identifier(name));
            }
            rows.push(row.done());
        }
    }
    rows
}

fn sequences_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| {
            let RelationSource::Sequence(sequence) = &entry.source else {
                return None;
            };
            let described = describe_type(sequence.sql_type);
            Some(
                InformationRow::new(InformationRelation::Sequences)
                    .set("sequence_catalog", identifier(&snapshot.database))
                    .set("sequence_schema", identifier(&entry.schema))
                    .set("sequence_name", identifier(&entry.name))
                    .set("data_type", character_data(&described.data_type))
                    .set(
                        "numeric_precision",
                        optional_cardinal(described.numeric_precision),
                    )
                    .set(
                        "numeric_precision_radix",
                        optional_cardinal(described.numeric_precision_radix),
                    )
                    .set("numeric_scale", optional_cardinal(described.numeric_scale))
                    .set("start_value", character_data(&sequence.start.to_string()))
                    .set("minimum_value", character_data(&sequence.min.to_string()))
                    .set("maximum_value", character_data(&sequence.max.to_string()))
                    .set("increment", character_data(&sequence.increment.to_string()))
                    .set("cycle_option", yes_or_no(sequence.cycle))
                    .done(),
            )
        })
        .collect()
}

struct ConstraintTable<'a> {
    entry: &'a RelationEntry,
    table: &'a TableDefinition,
}

fn constraint_table(snapshot: &CatalogSnapshot, at: usize) -> Option<ConstraintTable<'_>> {
    let entry = snapshot.relations.get(at)?;
    Some(ConstraintTable {
        entry,
        table: entry.table()?,
    })
}

struct NotNullConstraint<'a> {
    entry: &'a RelationEntry,
    column: &'a str,
    name: String,
}

fn not_null_constraints(snapshot: &CatalogSnapshot) -> Vec<NotNullConstraint<'_>> {
    let mut found = Vec::new();
    for entry in &snapshot.relations {
        let Some(table) = entry.table() else {
            continue;
        };
        for (position, column) in table.columns.iter().enumerate() {
            if is_not_null(table, position) {
                found.push(NotNullConstraint {
                    entry,
                    column: &column.name,
                    name: format!(
                        "{}_{}_{}_not_null",
                        entry.namespace,
                        entry.oid,
                        position + 1
                    ),
                });
            }
        }
    }
    found
}

fn constraint_type(source: ConstraintSource) -> &'static str {
    match source {
        ConstraintSource::Primary => "PRIMARY KEY",
        ConstraintSource::Unique(_) => "UNIQUE",
        ConstraintSource::Check(_) => "CHECK",
        ConstraintSource::Foreign(_) => "FOREIGN KEY",
    }
}

fn table_constraints_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let Some(owner) = constraint_table(snapshot, constraint.table) else {
            continue;
        };
        let deferrable = match constraint.source {
            ConstraintSource::Foreign(at) => owner
                .table
                .foreign_keys
                .get(at)
                .is_some_and(|key| key.deferrable),
            _ => false,
        };
        let nulls_distinct = match constraint.source {
            ConstraintSource::Unique(_) => yes_or_no(true),
            _ => Value::Null,
        };
        rows.push(table_constraint_row(
            snapshot,
            owner.entry,
            &constraint.name,
            constraint_type(constraint.source),
            deferrable,
            nulls_distinct,
        ));
    }
    for not_null in not_null_constraints(snapshot) {
        rows.push(table_constraint_row(
            snapshot,
            not_null.entry,
            &not_null.name,
            "CHECK",
            false,
            Value::Null,
        ));
    }
    rows
}

fn table_constraint_row(
    snapshot: &CatalogSnapshot,
    entry: &RelationEntry,
    name: &str,
    kind: &str,
    deferrable: bool,
    nulls_distinct: Value,
) -> Vec<Value> {
    InformationRow::new(InformationRelation::TableConstraints)
        .set("constraint_catalog", identifier(&snapshot.database))
        .set("constraint_schema", identifier(&entry.schema))
        .set("constraint_name", identifier(name))
        .set("table_catalog", identifier(&snapshot.database))
        .set("table_schema", identifier(&entry.schema))
        .set("table_name", identifier(&entry.name))
        .set("constraint_type", character_data(kind))
        .set("is_deferrable", yes_or_no(deferrable))
        .set("initially_deferred", yes_or_no(false))
        .set("enforced", yes_or_no(true))
        .set("nulls_distinct", nulls_distinct)
        .done()
}

enum UniqueKey {
    Primary,
    Index(usize),
}

struct ReferencedKey<'a> {
    entry: &'a RelationEntry,
    table: &'a TableDefinition,
    columns: Vec<usize>,
    unique: Option<(UniqueKey, &'a [usize])>,
}

impl ReferencedKey<'_> {
    fn unique_name(&self) -> Option<String> {
        match self.unique.as_ref()? {
            (UniqueKey::Primary, _) => Some(self.table.primary_key_constraint_name()),
            (UniqueKey::Index(at), _) => {
                self.table.indexes.get(*at).map(|index| index.name.clone())
            }
        }
    }

    fn position_in_unique(&self, key_position: usize) -> Option<i64> {
        let (_, unique_columns) = self.unique.as_ref()?;
        let column = self.columns.get(key_position)?;
        unique_columns
            .iter()
            .position(|found| found == column)
            .map(|position| position as i64 + 1)
    }
}

fn same_columns(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len() && left.iter().all(|column| right.contains(column))
}

fn unique_key<'a>(
    table: &'a TableDefinition,
    columns: &[usize],
) -> Option<(UniqueKey, &'a [usize])> {
    let candidates = move || {
        let primary = (!table.primary_key.is_empty())
            .then_some((UniqueKey::Primary, table.primary_key.as_slice()));
        primary.into_iter().chain(
            table
                .indexes
                .iter()
                .enumerate()
                .filter(|(_, index)| index.unique)
                .map(|(at, index)| (UniqueKey::Index(at), index.columns.as_slice())),
        )
    };
    candidates()
        .find(|(_, key)| *key == columns)
        .or_else(|| candidates().find(|(_, key)| same_columns(key, columns)))
}

fn referenced_key<'a>(
    snapshot: &'a CatalogSnapshot,
    parent: &str,
    names: &[String],
) -> Option<ReferencedKey<'a>> {
    let entry = snapshot.relations.get(snapshot.table_named(parent)?)?;
    let table = entry.table()?;
    let columns: Vec<usize> = names
        .iter()
        .map(|name| table.column_index(name))
        .collect::<Option<_>>()?;
    let unique = unique_key(table, &columns);
    Some(ReferencedKey {
        entry,
        table,
        columns,
        unique,
    })
}

fn key_column_usage_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let Some(owner) = constraint_table(snapshot, constraint.table) else {
            continue;
        };
        let table = owner.table;
        let (columns, referenced): (&[usize], Option<ReferencedKey<'_>>) = match constraint.source {
            ConstraintSource::Primary => (table.primary_key.as_slice(), None),
            ConstraintSource::Unique(at) => match table.indexes.get(at) {
                Some(index) => (index.columns.as_slice(), None),
                None => continue,
            },
            ConstraintSource::Check(_) => continue,
            ConstraintSource::Foreign(at) => match table.foreign_keys.get(at) {
                Some(key) => (
                    key.columns.as_slice(),
                    referenced_key(snapshot, &key.parent, &key.parent_columns),
                ),
                None => continue,
            },
        };
        for (position, column_at) in columns.iter().enumerate() {
            let Some(column) = table.columns.get(*column_at) else {
                continue;
            };
            let in_unique = referenced
                .as_ref()
                .and_then(|referenced| referenced.position_in_unique(position));
            rows.push(
                InformationRow::new(InformationRelation::KeyColumnUsage)
                    .set("constraint_catalog", identifier(&snapshot.database))
                    .set("constraint_schema", identifier(&owner.entry.schema))
                    .set("constraint_name", identifier(&constraint.name))
                    .set("table_catalog", identifier(&snapshot.database))
                    .set("table_schema", identifier(&owner.entry.schema))
                    .set("table_name", identifier(&owner.entry.name))
                    .set("column_name", identifier(&column.name))
                    .set("ordinal_position", cardinal(position as i64 + 1))
                    .set(
                        "position_in_unique_constraint",
                        optional_cardinal(in_unique),
                    )
                    .done(),
            );
        }
    }
    rows
}

fn rule(action: crate::ast::ReferentialAction) -> &'static str {
    match action {
        crate::ast::ReferentialAction::NoAction => "NO ACTION",
        crate::ast::ReferentialAction::Restrict => "RESTRICT",
        crate::ast::ReferentialAction::Cascade => "CASCADE",
        crate::ast::ReferentialAction::SetNull => "SET NULL",
        crate::ast::ReferentialAction::SetDefault => "SET DEFAULT",
    }
}

fn referential_constraints_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let ConstraintSource::Foreign(at) = constraint.source else {
            continue;
        };
        let Some(owner) = constraint_table(snapshot, constraint.table) else {
            continue;
        };
        let Some(key) = owner.table.foreign_keys.get(at) else {
            continue;
        };
        let referenced = referenced_key(snapshot, &key.parent, &key.parent_columns);
        let unique_name = referenced.as_ref().and_then(ReferencedKey::unique_name);
        let mut row = InformationRow::new(InformationRelation::ReferentialConstraints)
            .set("constraint_catalog", identifier(&snapshot.database))
            .set("constraint_schema", identifier(&owner.entry.schema))
            .set("constraint_name", identifier(&constraint.name))
            .set("match_option", character_data("NONE"))
            .set("update_rule", character_data(rule(key.on_update)))
            .set("delete_rule", character_data(rule(key.on_delete)));
        if let (Some(referenced), Some(unique_name)) = (&referenced, unique_name) {
            row = row
                .set("unique_constraint_catalog", identifier(&snapshot.database))
                .set(
                    "unique_constraint_schema",
                    identifier(&referenced.entry.schema),
                )
                .set("unique_constraint_name", identifier(&unique_name));
        }
        rows.push(row.done());
    }
    rows
}

pub(crate) fn check_columns(table: &TableDefinition, check: usize) -> Vec<usize> {
    let Some(check) = table.checks.get(check) else {
        return Vec::new();
    };
    let Ok(tokens) = crate::lexer::tokenize_in(&check.expression, check.dialect) else {
        return Vec::new();
    };
    let mut found: Vec<usize> = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        let calls_function = matches!(
            tokens.get(at + 1).map(|next| &next.kind),
            Some(TokenKind::LeftParen)
        );
        let position = match &token.kind {
            TokenKind::Word(word) if !calls_function => table
                .column_index(word)
                .or_else(|| table.column_index(&word.to_ascii_lowercase())),
            TokenKind::QuotedIdentifier(word) => table.column_index(word),
            _ => None,
        };
        if let Some(position) = position
            && !found.contains(&position)
        {
            found.push(position);
        }
    }
    found.sort_unstable();
    found
}

fn constraint_column_usage_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let Some(owner) = constraint_table(snapshot, constraint.table) else {
            continue;
        };
        let table = owner.table;
        let (used_entry, used_table, columns): (&RelationEntry, &TableDefinition, Vec<usize>) =
            match constraint.source {
                ConstraintSource::Primary => (owner.entry, table, table.primary_key.clone()),
                ConstraintSource::Unique(at) => match table.indexes.get(at) {
                    Some(index) => (owner.entry, table, index.columns.clone()),
                    None => continue,
                },
                ConstraintSource::Check(at) => (owner.entry, table, check_columns(table, at)),
                ConstraintSource::Foreign(at) => {
                    let Some(referenced) = table
                        .foreign_keys
                        .get(at)
                        .and_then(|key| referenced_key(snapshot, &key.parent, &key.parent_columns))
                    else {
                        continue;
                    };
                    (referenced.entry, referenced.table, referenced.columns)
                }
            };
        for column_at in columns {
            let Some(column) = used_table.columns.get(column_at) else {
                continue;
            };
            rows.push(
                InformationRow::new(InformationRelation::ConstraintColumnUsage)
                    .set("table_catalog", identifier(&snapshot.database))
                    .set("table_schema", identifier(&used_entry.schema))
                    .set("table_name", identifier(&used_entry.name))
                    .set("column_name", identifier(&column.name))
                    .set("constraint_catalog", identifier(&snapshot.database))
                    .set("constraint_schema", identifier(&owner.entry.schema))
                    .set("constraint_name", identifier(&constraint.name))
                    .done(),
            );
        }
    }
    rows
}

fn check_constraints_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let ConstraintSource::Check(at) = constraint.source else {
            continue;
        };
        let Some(owner) = constraint_table(snapshot, constraint.table) else {
            continue;
        };
        let clause = format!("({})", check_text(snapshot, constraint.table, at, false));
        rows.push(check_constraint_row(
            snapshot,
            owner.entry,
            &constraint.name,
            &clause,
        ));
    }
    for not_null in not_null_constraints(snapshot) {
        let clause = format!("{} IS NOT NULL", not_null.column);
        rows.push(check_constraint_row(
            snapshot,
            not_null.entry,
            &not_null.name,
            &clause,
        ));
    }
    rows
}

fn check_constraint_row(
    snapshot: &CatalogSnapshot,
    entry: &RelationEntry,
    name: &str,
    clause: &str,
) -> Vec<Value> {
    InformationRow::new(InformationRelation::CheckConstraints)
        .set("constraint_catalog", identifier(&snapshot.database))
        .set("constraint_schema", identifier(&entry.schema))
        .set("constraint_name", identifier(name))
        .set("check_clause", character_data(clause))
        .done()
}

fn specific_name(routine: &RoutineEntry) -> String {
    format!("{}_{}", routine.name, routine.oid)
}

fn with_type_description(
    row: InformationRow,
    snapshot: &CatalogSnapshot,
    sql_type: SqlType,
    udt_prefix: &str,
) -> InformationRow {
    let described = describe_type(sql_type);
    let row = row
        .set("data_type", character_data(&described.data_type))
        .set(
            "character_maximum_length",
            optional_cardinal(described.character_maximum_length),
        )
        .set(
            "character_octet_length",
            optional_cardinal(described.character_octet_length),
        )
        .set(
            "numeric_precision",
            optional_cardinal(described.numeric_precision),
        )
        .set(
            "numeric_precision_radix",
            optional_cardinal(described.numeric_precision_radix),
        )
        .set("numeric_scale", optional_cardinal(described.numeric_scale))
        .set(
            "datetime_precision",
            optional_cardinal(described.datetime_precision),
        )
        .set(
            "interval_type",
            described
                .interval_type
                .as_deref()
                .map_or(Value::Null, character_data),
        );
    let (schema, name) = match (described.udt_name, sql_type.defined()) {
        (Some(udt_name), _) => (crate::schemas::CATALOG_SCHEMA.to_owned(), udt_name.to_owned()),
        (None, Some(defined)) => match snapshot.user_types.of_oid(defined.oid) {
            Some(found) => (
                crate::schemas::schema_of(&found.name).to_owned(),
                crate::schemas::base_name(&found.name).to_owned(),
            ),
            None => return row,
        },
        (None, None) => return row,
    };
    row.set(&format!("{udt_prefix}udt_catalog"), identifier(&snapshot.database))
        .set(&format!("{udt_prefix}udt_schema"), identifier(&schema))
        .set(&format!("{udt_prefix}udt_name"), identifier(&name))
}

fn pseudo_type_name(returned: &ReturnType) -> Option<&'static str> {
    match returned {
        ReturnType::Void => Some("void"),
        ReturnType::Trigger => Some("trigger"),
        ReturnType::Record => Some("record"),
        ReturnType::Type(_) | ReturnType::Relation(_) => None,
    }
}

fn with_return_type(row: InformationRow, snapshot: &CatalogSnapshot, routine: &RoutineEntry) -> InformationRow {
    let returned = match &routine.definition.returns {
        RoutineReturns::Nothing => return row,
        RoutineReturns::One(returned) | RoutineReturns::Set(returned) => returned.clone(),
        RoutineReturns::Table(columns) => match columns.as_slice() {
            [(_, only)] => ReturnType::Type(*only),
            _ => ReturnType::Record,
        },
    };
    match (&returned, pseudo_type_name(&returned)) {
        (_, Some(name)) => row
            .set("data_type", character_data(name))
            .set("type_udt_catalog", identifier(&snapshot.database))
            .set("type_udt_schema", identifier(crate::schemas::CATALOG_SCHEMA))
            .set("type_udt_name", identifier(name)),
        (ReturnType::Type(sql_type), None) => {
            with_type_description(row, snapshot, *sql_type, "type_")
        }
        (ReturnType::Relation(name), None) => row
            .set("data_type", character_data("USER-DEFINED"))
            .set("type_udt_catalog", identifier(&snapshot.database))
            .set("type_udt_schema", identifier(crate::schemas::schema_of(name)))
            .set("type_udt_name", identifier(crate::schemas::base_name(name))),
        _ => row,
    }
}

fn routines_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .routines
        .iter()
        .map(|routine| {
            let definition = &routine.definition;
            let function = definition.kind == RoutineKind::Function;
            let row = InformationRow::new(InformationRelation::Routines)
                .set("specific_catalog", identifier(&snapshot.database))
                .set("specific_schema", identifier(&routine.schema))
                .set("specific_name", identifier(&specific_name(routine)))
                .set("routine_catalog", identifier(&snapshot.database))
                .set("routine_schema", identifier(&routine.schema))
                .set("routine_name", identifier(&routine.name))
                .set("routine_type", character_data(definition.kind.keyword()))
                .set(
                    "dtd_identifier",
                    if function { identifier("0") } else { Value::Null },
                )
                .set(
                    "routine_body",
                    character_data(match definition.language {
                        RoutineLanguage::Sql | RoutineLanguage::Tsql => "SQL",
                        RoutineLanguage::PlPgSql => "EXTERNAL",
                    }),
                )
                .set("routine_definition", character_data(&definition.body))
                .set(
                    "external_language",
                    character_data(&definition.language.name().to_ascii_uppercase()),
                )
                .set("parameter_style", character_data("GENERAL"))
                .set(
                    "is_deterministic",
                    yes_or_no(definition.volatility == Volatility::Immutable),
                )
                .set("sql_data_access", character_data("MODIFIES"))
                .set(
                    "is_null_call",
                    if function {
                        yes_or_no(definition.strict)
                    } else {
                        Value::Null
                    },
                )
                .set("schema_level_routine", yes_or_no(true))
                .set("max_dynamic_result_sets", cardinal(0))
                .set("is_user_defined_cast", yes_or_no(false))
                .set("is_implicitly_invocable", yes_or_no(false))
                .set(
                    "security_type",
                    character_data(if definition.security_definer { "DEFINER" } else { "INVOKER" }),
                )
                .set("as_locator", yes_or_no(false))
                .set("is_udt_dependent", yes_or_no(false));
            with_return_type(row, snapshot, routine).done()
        })
        .collect()
}

fn parameter_mode_name(mode: ParameterMode) -> &'static str {
    match mode {
        ParameterMode::In | ParameterMode::Variadic => "IN",
        ParameterMode::Out => "OUT",
        ParameterMode::InOut => "INOUT",
    }
}

fn parameters_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for routine in &snapshot.routines {
        let definition = &routine.definition;
        let mut listed: Vec<(Option<&str>, &'static str, SqlType, Option<&str>)> = definition
            .parameters
            .iter()
            .map(|parameter| {
                (
                    parameter.name.as_deref(),
                    parameter_mode_name(parameter.mode),
                    parameter.sql_type,
                    parameter.default.as_deref().map(str::trim),
                )
            })
            .collect();
        if let RoutineReturns::Table(columns) = &definition.returns {
            listed.extend(
                columns
                    .iter()
                    .map(|(name, sql_type)| (Some(name.as_str()), "OUT", *sql_type, None)),
            );
        }
        for (at, (name, mode, sql_type, default)) in listed.into_iter().enumerate() {
            let position = at as i64 + 1;
            let row = InformationRow::new(InformationRelation::Parameters)
                .set("specific_catalog", identifier(&snapshot.database))
                .set("specific_schema", identifier(&routine.schema))
                .set("specific_name", identifier(&specific_name(routine)))
                .set("ordinal_position", cardinal(position))
                .set("parameter_mode", character_data(mode))
                .set("is_result", yes_or_no(false))
                .set("as_locator", yes_or_no(false))
                .set("parameter_name", name.map_or(Value::Null, identifier))
                .set("dtd_identifier", identifier(&position.to_string()))
                .set("parameter_default", default.map_or(Value::Null, character_data));
            rows.push(with_type_description(row, snapshot, sql_type, "").done());
        }
    }
    rows
}
