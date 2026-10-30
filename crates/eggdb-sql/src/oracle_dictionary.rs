#[cfg(test)]
mod tests;

use crate::encoding::{CommentKind, SequenceDefinition, TableDefinition, ViewDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::swap_uniform_case;
use crate::pg_catalog::{
    CatalogColumn, CatalogRow, CatalogSnapshot, ConstraintSource, IndexSource, RelationEntry,
    RelationSource,
};
use crate::schemas::{DEFAULT_SCHEMA, QUALIFIER};
use crate::store::KeyValueRead;
use crate::types::{SqlType, Value};
use std::collections::HashMap;

const VARCHAR2: SqlType = SqlType::Text;
const NUMBER: SqlType = SqlType::BigInt;
const DATE: SqlType = SqlType::Timestamp(Some(0));
const UNKNOWN_DATE: Value = Value::Timestamp(0);
const UNKNOWN_TIMESTAMP_TEXT: &str = "2000-01-01:00:00:00";
const DICTIONARY_SCHEMA: &str = "sys";
const HIDDEN_SCHEMAS: [&str; 3] = ["pg_catalog", "pg_toast", "information_schema"];
const TABLESPACE: &str = "USERS";
const TEMPORARY_TABLESPACE: &str = "TEMP";
const CHARACTER_SET: &str = "AL32UTF8";
const NATIONAL_CHARACTER_SET: &str = "AL16UTF16";
const COMPATIBLE_RELEASE: &str = "23.0.0.0.0";
const TIME_FORMAT: &str = "HH.MI.SSXFF AM";
const TIME_TZ_FORMAT: &str = "HH.MI.SSXFF AM TZR";

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

/// Which objects a data dictionary view lists: `USER_` the session user's own, `ALL_` and
/// `DBA_` every schema's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DictionaryScope {
    User,
    All,
    Dba,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DictionaryView {
    Tables,
    TabColumns,
    TabCols,
    Constraints,
    ConsColumns,
    Indexes,
    IndColumns,
    Sequences,
    Views,
    Objects,
    TabComments,
    ColComments,
    TabIdentityCols,
    Users,
}

/// A relation of Oracle's data dictionary or dynamic performance views that the Oracle
/// dialect answers from the catalog snapshot of the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OracleRelation {
    Dictionary(DictionaryScope, DictionaryView),
    Version,
    NlsSessionParameters,
    NlsDatabaseParameters,
}

const fn user(view: DictionaryView) -> OracleRelation {
    OracleRelation::Dictionary(DictionaryScope::User, view)
}

const fn all(view: DictionaryView) -> OracleRelation {
    OracleRelation::Dictionary(DictionaryScope::All, view)
}

const fn dba(view: DictionaryView) -> OracleRelation {
    OracleRelation::Dictionary(DictionaryScope::Dba, view)
}

static NAMED: &[(&str, OracleRelation)] = &[
    ("user_tables", user(DictionaryView::Tables)),
    ("all_tables", all(DictionaryView::Tables)),
    ("dba_tables", dba(DictionaryView::Tables)),
    ("user_tab_columns", user(DictionaryView::TabColumns)),
    ("all_tab_columns", all(DictionaryView::TabColumns)),
    ("dba_tab_columns", dba(DictionaryView::TabColumns)),
    ("user_tab_cols", user(DictionaryView::TabCols)),
    ("all_tab_cols", all(DictionaryView::TabCols)),
    ("dba_tab_cols", dba(DictionaryView::TabCols)),
    ("user_constraints", user(DictionaryView::Constraints)),
    ("all_constraints", all(DictionaryView::Constraints)),
    ("dba_constraints", dba(DictionaryView::Constraints)),
    ("user_cons_columns", user(DictionaryView::ConsColumns)),
    ("all_cons_columns", all(DictionaryView::ConsColumns)),
    ("dba_cons_columns", dba(DictionaryView::ConsColumns)),
    ("user_indexes", user(DictionaryView::Indexes)),
    ("all_indexes", all(DictionaryView::Indexes)),
    ("dba_indexes", dba(DictionaryView::Indexes)),
    ("user_ind_columns", user(DictionaryView::IndColumns)),
    ("all_ind_columns", all(DictionaryView::IndColumns)),
    ("dba_ind_columns", dba(DictionaryView::IndColumns)),
    ("user_sequences", user(DictionaryView::Sequences)),
    ("all_sequences", all(DictionaryView::Sequences)),
    ("dba_sequences", dba(DictionaryView::Sequences)),
    ("user_views", user(DictionaryView::Views)),
    ("all_views", all(DictionaryView::Views)),
    ("dba_views", dba(DictionaryView::Views)),
    ("user_objects", user(DictionaryView::Objects)),
    ("all_objects", all(DictionaryView::Objects)),
    ("dba_objects", dba(DictionaryView::Objects)),
    ("user_tab_comments", user(DictionaryView::TabComments)),
    ("all_tab_comments", all(DictionaryView::TabComments)),
    ("dba_tab_comments", dba(DictionaryView::TabComments)),
    ("user_col_comments", user(DictionaryView::ColComments)),
    ("all_col_comments", all(DictionaryView::ColComments)),
    ("dba_col_comments", dba(DictionaryView::ColComments)),
    ("user_tab_identity_cols", user(DictionaryView::TabIdentityCols)),
    ("all_tab_identity_cols", all(DictionaryView::TabIdentityCols)),
    ("dba_tab_identity_cols", dba(DictionaryView::TabIdentityCols)),
    ("user_users", user(DictionaryView::Users)),
    ("all_users", all(DictionaryView::Users)),
    ("dba_users", dba(DictionaryView::Users)),
    ("tabs", user(DictionaryView::Tables)),
    ("cols", user(DictionaryView::TabColumns)),
    ("ind", user(DictionaryView::Indexes)),
    ("seq", user(DictionaryView::Sequences)),
    ("obj", user(DictionaryView::Objects)),
    ("v$version", OracleRelation::Version),
    ("nls_session_parameters", OracleRelation::NlsSessionParameters),
    ("nls_database_parameters", OracleRelation::NlsDatabaseParameters),
];

impl OracleRelation {
    /// The relation an unqualified name (in EggDB's lower-case form) names, such as
    /// `user_tables` or `v$version`.
    pub(crate) fn named(name: &str) -> Option<Self> {
        NAMED
            .iter()
            .find(|(known, _)| *known == name)
            .map(|&(_, relation)| relation)
    }

    pub(crate) fn name(self) -> &'static str {
        NAMED
            .iter()
            .find(|(_, relation)| *relation == self)
            .map_or("", |&(name, _)| name)
    }

    pub(crate) fn schema(self) -> &'static str {
        DICTIONARY_SCHEMA
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Version => VERSION_COLUMNS,
            Self::NlsSessionParameters | Self::NlsDatabaseParameters => NLS_COLUMNS,
            Self::Dictionary(scope, view) => dictionary_columns(scope, view),
        }
    }
}

fn dictionary_columns(scope: DictionaryScope, view: DictionaryView) -> &'static [CatalogColumn] {
    let user = scope == DictionaryScope::User;
    let without_owner = |columns: &'static [CatalogColumn]| {
        if user {
            columns.get(1..).unwrap_or(columns)
        } else {
            columns
        }
    };
    match view {
        DictionaryView::Tables => without_owner(TABLES_COLUMNS),
        DictionaryView::TabColumns => without_owner(TAB_COLUMNS_COLUMNS),
        DictionaryView::TabCols => without_owner(TAB_COLS_COLUMNS),
        DictionaryView::Constraints => CONSTRAINTS_COLUMNS,
        DictionaryView::ConsColumns => CONS_COLUMNS_COLUMNS,
        DictionaryView::Indexes => without_owner(INDEXES_COLUMNS),
        DictionaryView::IndColumns if user => USER_IND_COLUMNS_COLUMNS,
        DictionaryView::IndColumns => IND_COLUMNS_COLUMNS,
        DictionaryView::Sequences => without_owner(SEQUENCES_COLUMNS),
        DictionaryView::Views => without_owner(VIEWS_COLUMNS),
        DictionaryView::Objects => without_owner(OBJECTS_COLUMNS),
        DictionaryView::TabComments => without_owner(TAB_COMMENTS_COLUMNS),
        DictionaryView::ColComments => without_owner(COL_COMMENTS_COLUMNS),
        DictionaryView::TabIdentityCols => without_owner(TAB_IDENTITY_COLS_COLUMNS),
        DictionaryView::Users if scope == DictionaryScope::All => ALL_USERS_COLUMNS,
        DictionaryView::Users => ACCOUNT_COLUMNS,
    }
}

static TABLES_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("tablespace_name", VARCHAR2),
    column("cluster_name", VARCHAR2),
    column("iot_name", VARCHAR2),
    column("status", VARCHAR2),
    column("pct_free", NUMBER),
    column("logging", VARCHAR2),
    column("num_rows", NUMBER),
    column("blocks", NUMBER),
    column("avg_row_len", NUMBER),
    column("last_analyzed", DATE),
    column("partitioned", VARCHAR2),
    column("iot_type", VARCHAR2),
    column("temporary", VARCHAR2),
    column("secondary", VARCHAR2),
    column("nested", VARCHAR2),
    column("row_movement", VARCHAR2),
    column("dependencies", VARCHAR2),
    column("dropped", VARCHAR2),
    column("read_only", VARCHAR2),
];

static TAB_COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("data_type", VARCHAR2),
    column("data_type_mod", VARCHAR2),
    column("data_type_owner", VARCHAR2),
    column("data_length", NUMBER),
    column("data_precision", NUMBER),
    column("data_scale", NUMBER),
    column("nullable", VARCHAR2),
    column("column_id", NUMBER),
    column("default_length", NUMBER),
    column("data_default", VARCHAR2),
    column("num_distinct", NUMBER),
    column("char_col_decl_length", NUMBER),
    column("char_length", NUMBER),
    column("char_used", VARCHAR2),
    column("default_on_null", VARCHAR2),
    column("identity_column", VARCHAR2),
];

static TAB_COLS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("data_type", VARCHAR2),
    column("data_type_mod", VARCHAR2),
    column("data_type_owner", VARCHAR2),
    column("data_length", NUMBER),
    column("data_precision", NUMBER),
    column("data_scale", NUMBER),
    column("nullable", VARCHAR2),
    column("column_id", NUMBER),
    column("default_length", NUMBER),
    column("data_default", VARCHAR2),
    column("num_distinct", NUMBER),
    column("char_col_decl_length", NUMBER),
    column("char_length", NUMBER),
    column("char_used", VARCHAR2),
    column("default_on_null", VARCHAR2),
    column("identity_column", VARCHAR2),
    column("hidden_column", VARCHAR2),
    column("virtual_column", VARCHAR2),
    column("segment_column_id", NUMBER),
    column("internal_column_id", NUMBER),
    column("user_generated", VARCHAR2),
];

static CONSTRAINTS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("constraint_name", VARCHAR2),
    column("constraint_type", VARCHAR2),
    column("table_name", VARCHAR2),
    column("search_condition", VARCHAR2),
    column("search_condition_vc", VARCHAR2),
    column("r_owner", VARCHAR2),
    column("r_constraint_name", VARCHAR2),
    column("delete_rule", VARCHAR2),
    column("status", VARCHAR2),
    column("deferrable", VARCHAR2),
    column("deferred", VARCHAR2),
    column("validated", VARCHAR2),
    column("generated", VARCHAR2),
    column("bad", VARCHAR2),
    column("rely", VARCHAR2),
    column("last_change", DATE),
    column("index_owner", VARCHAR2),
    column("index_name", VARCHAR2),
    column("invalid", VARCHAR2),
    column("view_related", VARCHAR2),
];

static CONS_COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("constraint_name", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("position", NUMBER),
];

static INDEXES_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("index_name", VARCHAR2),
    column("index_type", VARCHAR2),
    column("table_owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("table_type", VARCHAR2),
    column("uniqueness", VARCHAR2),
    column("compression", VARCHAR2),
    column("prefix_length", NUMBER),
    column("tablespace_name", VARCHAR2),
    column("logging", VARCHAR2),
    column("status", VARCHAR2),
    column("num_rows", NUMBER),
    column("distinct_keys", NUMBER),
    column("last_analyzed", DATE),
    column("partitioned", VARCHAR2),
    column("temporary", VARCHAR2),
    column("generated", VARCHAR2),
    column("visibility", VARCHAR2),
];

static IND_COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("index_owner", VARCHAR2),
    column("index_name", VARCHAR2),
    column("table_owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("column_position", NUMBER),
    column("column_length", NUMBER),
    column("char_length", NUMBER),
    column("descend", VARCHAR2),
];

static USER_IND_COLUMNS_COLUMNS: &[CatalogColumn] = &[
    column("index_name", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("column_position", NUMBER),
    column("column_length", NUMBER),
    column("char_length", NUMBER),
    column("descend", VARCHAR2),
];

static SEQUENCES_COLUMNS: &[CatalogColumn] = &[
    column("sequence_owner", VARCHAR2),
    column("sequence_name", VARCHAR2),
    column("min_value", NUMBER),
    column("max_value", NUMBER),
    column("increment_by", NUMBER),
    column("cycle_flag", VARCHAR2),
    column("order_flag", VARCHAR2),
    column("cache_size", NUMBER),
    column("last_number", NUMBER),
    column("scale_flag", VARCHAR2),
    column("extend_flag", VARCHAR2),
    column("sharded_flag", VARCHAR2),
    column("session_flag", VARCHAR2),
    column("keep_value", VARCHAR2),
];

static VIEWS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("view_name", VARCHAR2),
    column("text_length", NUMBER),
    column("text", VARCHAR2),
    column("text_vc", VARCHAR2),
    column("read_only", VARCHAR2),
    column("editioning_view", VARCHAR2),
    column("bequeath", VARCHAR2),
];

static OBJECTS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("object_name", VARCHAR2),
    column("subobject_name", VARCHAR2),
    column("object_id", NUMBER),
    column("data_object_id", NUMBER),
    column("object_type", VARCHAR2),
    column("created", DATE),
    column("last_ddl_time", DATE),
    column("timestamp", VARCHAR2),
    column("status", VARCHAR2),
    column("temporary", VARCHAR2),
    column("generated", VARCHAR2),
    column("secondary", VARCHAR2),
    column("namespace", NUMBER),
    column("edition_name", VARCHAR2),
    column("oracle_maintained", VARCHAR2),
];

static TAB_COMMENTS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("table_type", VARCHAR2),
    column("comments", VARCHAR2),
];

static COL_COMMENTS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("comments", VARCHAR2),
];

static TAB_IDENTITY_COLS_COLUMNS: &[CatalogColumn] = &[
    column("owner", VARCHAR2),
    column("table_name", VARCHAR2),
    column("column_name", VARCHAR2),
    column("generation_type", VARCHAR2),
    column("sequence_name", VARCHAR2),
    column("identity_options", VARCHAR2),
];

static ALL_USERS_COLUMNS: &[CatalogColumn] = &[
    column("username", VARCHAR2),
    column("user_id", NUMBER),
    column("created", DATE),
    column("common", VARCHAR2),
    column("oracle_maintained", VARCHAR2),
];

static ACCOUNT_COLUMNS: &[CatalogColumn] = &[
    column("username", VARCHAR2),
    column("user_id", NUMBER),
    column("account_status", VARCHAR2),
    column("lock_date", DATE),
    column("expiry_date", DATE),
    column("default_tablespace", VARCHAR2),
    column("temporary_tablespace", VARCHAR2),
    column("created", DATE),
    column("common", VARCHAR2),
    column("oracle_maintained", VARCHAR2),
];

static VERSION_COLUMNS: &[CatalogColumn] = &[
    column("banner", VARCHAR2),
    column("banner_full", VARCHAR2),
    column("banner_legacy", VARCHAR2),
    column("con_id", NUMBER),
];

static NLS_COLUMNS: &[CatalogColumn] = &[
    column("parameter", VARCHAR2),
    column("value", VARCHAR2),
];

/// The dictionary relation `name` names in the Oracle dialect: an unqualified dictionary name
/// that no table or view of the session's schemas takes (a user's own object comes first, as
/// a private object hides Oracle's public synonym), or one qualified with `SYS`.
pub(crate) fn reference(
    reader: &dyn KeyValueRead,
    name: &str,
) -> SqlResult<Option<OracleRelation>> {
    let (qualified, base) = match name.split_once(QUALIFIER) {
        Some((schema, base)) if schema == DICTIONARY_SCHEMA => (true, base),
        Some(_) => return Ok(None),
        None => (false, name),
    };
    let Some(relation) = OracleRelation::named(base) else {
        return Ok(None);
    };
    if !qualified {
        for schema in crate::schemas::relation_path(reader)? {
            let candidate = crate::schemas::qualified(&schema, base);
            if reader
                .get(&crate::encoding::table_key(&candidate))?
                .is_some()
            {
                return Ok(None);
            }
        }
    }
    Ok(Some(relation))
}

/// The user an Oracle session shows as the owner of the objects of `schema`: the session user
/// for its own schema (`user_schema`), `PUBLIC` for the default schema when the user has a
/// schema of its own, and the schema's name in Oracle's case otherwise.
pub(crate) fn owner_name(schema: &str, user: &str, user_schema: &str) -> String {
    if schema == user_schema {
        user.to_owned()
    } else if schema == DEFAULT_SCHEMA {
        "PUBLIC".to_owned()
    } else {
        swap_uniform_case(schema)
    }
}

/// The schema EggDB keeps an Oracle session user's objects in: the schema named after the
/// user when `exists`, the default schema otherwise.
pub(crate) fn user_schema_of(user: &str, exists: bool) -> Option<String> {
    let internal = swap_uniform_case(&user.to_uppercase());
    (exists && !internal.is_empty() && internal != DEFAULT_SCHEMA).then_some(internal)
}

fn oracle_type_name(sql_type: SqlType) -> &'static str {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => "NUMBER",
        SqlType::Real => "BINARY_FLOAT",
        SqlType::Double => "BINARY_DOUBLE",
        SqlType::Boolean => "BOOLEAN",
        SqlType::Text | SqlType::Varchar(None) => "CLOB",
        SqlType::Varchar(Some(_)) => "VARCHAR2",
        SqlType::Numeric(_) => "NUMBER",
        SqlType::Date => "DATE",
        SqlType::Timestamp(Some(0)) => "DATE",
        SqlType::Timestamp(_) => "TIMESTAMP",
        SqlType::TimestampTz(_) => "TIMESTAMP WITH TIME ZONE",
        SqlType::Json | SqlType::Jsonb => "JSON",
        SqlType::Bpchar(_) => "CHAR",
        SqlType::Bytea => "BLOB",
        SqlType::Time(_)
        | SqlType::TimeTz(_)
        | SqlType::Interval { .. }
        | SqlType::Uuid
        | SqlType::Array(_)
        | SqlType::Catalog(_)
        | SqlType::Defined(_) => "VARCHAR2",
    }
}

fn precision_of(sql_type: SqlType) -> Option<i64> {
    match sql_type {
        SqlType::SmallInt => Some(5),
        SqlType::Integer => Some(10),
        SqlType::BigInt => Some(19),
        _ => None,
    }
}

fn length_of(sql_type: SqlType) -> i64 {
    match sql_type {
        SqlType::Varchar(Some(length)) => i64::from(length),
        SqlType::Real => 4,
        SqlType::Double => 8,
        SqlType::Text | SqlType::Varchar(None) => 4000,
        SqlType::Boolean => 1,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt | SqlType::Numeric(_) => 22,
        SqlType::Date | SqlType::Timestamp(Some(0)) => 7,
        SqlType::Timestamp(_) => 11,
        SqlType::TimestampTz(_) => 13,
        SqlType::Json | SqlType::Jsonb => 8200,
        SqlType::Bpchar(length) => length.map_or(2000, i64::from),
        SqlType::Bytea => 4000,
        SqlType::Time(_)
        | SqlType::TimeTz(_)
        | SqlType::Interval { .. }
        | SqlType::Uuid
        | SqlType::Array(_)
        | SqlType::Catalog(_)
        | SqlType::Defined(_) => 64,
    }
}

/// The type `sql_type` is shown as in Oracle's SQL, such as `NUMBER(10)` or
/// `VARCHAR2(20 CHAR)`.
pub(crate) fn type_text(sql_type: SqlType) -> String {
    match (sql_type, precision_of(sql_type)) {
        (_, Some(precision)) => format!("NUMBER({precision})"),
        (SqlType::Varchar(Some(length)), _) => format!("VARCHAR2({length} CHAR)"),
        (SqlType::Bpchar(Some(length)), _) => format!("CHAR({length} CHAR)"),
        (SqlType::Numeric(Some(modifier)), _) => {
            format!("NUMBER({},{})", modifier.precision, modifier.scale)
        }
        (SqlType::Timestamp(Some(precision)) | SqlType::TimestampTz(Some(precision)), _)
            if precision != 0 =>
        {
            format!("{}({precision})", oracle_type_name(sql_type))
        }
        (SqlType::Interval { .. }, _) => interval_type_text(sql_type),
        _ => oracle_type_name(sql_type).to_owned(),
    }
}

fn interval_type_text(sql_type: SqlType) -> String {
    match crate::oracle_intervals::kind_of_type(sql_type) {
        Some(crate::oracle_intervals::IntervalKind::YearToMonth) => {
            "INTERVAL YEAR(2) TO MONTH".to_owned()
        }
        _ => {
            let digits = match sql_type {
                SqlType::Interval {
                    precision: Some(precision),
                    ..
                } => precision,
                _ => crate::oracle_intervals::DEFAULT_FRACTION_DIGITS,
            };
            format!("INTERVAL DAY(2) TO SECOND({digits})")
        }
    }
}

fn data_type(sql_type: SqlType) -> String {
    match sql_type {
        SqlType::Timestamp(Some(0)) => "DATE".to_owned(),
        SqlType::Timestamp(precision) => format!("TIMESTAMP({})", precision.unwrap_or(6)),
        SqlType::TimestampTz(precision) => {
            format!("TIMESTAMP({}) WITH TIME ZONE", precision.unwrap_or(6))
        }
        SqlType::Interval { .. } => interval_type_text(sql_type),
        other => oracle_type_name(other).to_owned(),
    }
}

fn precision_and_scale(sql_type: SqlType) -> (Option<i64>, Option<i64>) {
    match sql_type {
        SqlType::Numeric(Some(modifier)) => (
            Some(i64::from(modifier.precision)),
            Some(i64::from(modifier.scale)),
        ),
        SqlType::Timestamp(Some(0)) => (None, None),
        SqlType::Timestamp(precision) | SqlType::TimestampTz(precision) => {
            (None, Some(i64::from(precision.unwrap_or(6))))
        }
        other => (precision_of(other), precision_of(other).map(|_| 0)),
    }
}

fn character_length(sql_type: SqlType) -> Option<i64> {
    match sql_type {
        SqlType::Varchar(Some(length)) | SqlType::Bpchar(Some(length)) => Some(i64::from(length)),
        SqlType::Bpchar(None) => Some(1),
        _ => None,
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn number(value: i64) -> Value {
    Value::BigInt(value)
}

fn optional_number(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::BigInt)
}

fn optional_text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, text)
}

fn flag(value: bool, yes: &str, no: &str) -> Value {
    text(if value { yes } else { no })
}

fn shown(name: &str) -> Value {
    Value::Text(swap_uniform_case(name))
}

struct ColumnFacts<'a> {
    name: &'a str,
    sql_type: SqlType,
    nullable: bool,
    default: Option<&'a str>,
    identity: bool,
}

fn relation_columns(entry: &RelationEntry) -> Vec<ColumnFacts<'_>> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| ColumnFacts {
                name: &column.name,
                sql_type: column.sql_type,
                nullable: !(column.not_null || table.primary_key.contains(&position)),
                default: column.default.as_deref(),
                identity: column.auto_increment,
            })
            .collect(),
        RelationSource::View(view) => view
            .columns
            .iter()
            .map(|column| ColumnFacts {
                name: &column.name,
                sql_type: column.sql_type.unwrap_or(SqlType::Text),
                nullable: true,
                default: None,
                identity: false,
            })
            .collect(),
        RelationSource::Sequence(_)
        | RelationSource::Index { .. }
        | RelationSource::Catalog(_) => Vec::new(),
    }
}

struct Dictionary<'a> {
    snapshot: &'a CatalogSnapshot,
    columns: &'static [CatalogColumn],
    scope: DictionaryScope,
    user: String,
    user_schema: String,
}

impl<'a> Dictionary<'a> {
    fn new(snapshot: &'a CatalogSnapshot, relation: OracleRelation) -> Self {
        let scope = match relation {
            OracleRelation::Dictionary(scope, _) => scope,
            _ => DictionaryScope::All,
        };
        Self {
            snapshot,
            columns: relation.columns(),
            scope,
            user: snapshot.user.to_uppercase(),
            user_schema: snapshot
                .settings
                .oracle_user_schema
                .clone()
                .unwrap_or_else(|| DEFAULT_SCHEMA.to_owned()),
        }
    }

    fn row(&self) -> CatalogRow {
        CatalogRow::new(self.columns)
    }

    fn includes(&self, schema: &str) -> bool {
        self.scope != DictionaryScope::User || schema == self.user_schema
    }

    fn owner(&self, schema: &str) -> Value {
        Value::Text(owner_name(schema, &self.user, &self.user_schema))
    }

    fn entries(&self) -> Vec<(usize, &'a RelationEntry)> {
        self.snapshot
            .relations
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                !matches!(
                    entry.source,
                    RelationSource::Catalog(_) | RelationSource::Index { .. }
                ) && !HIDDEN_SCHEMAS.contains(&entry.schema.as_str())
                    && self.includes(&entry.schema)
            })
            .collect()
    }

    fn tables(&self) -> Vec<(usize, &'a RelationEntry, &'a TableDefinition)> {
        self.entries()
            .into_iter()
            .filter_map(|(at, entry)| entry.table().map(|table| (at, entry, table)))
            .collect()
    }

    fn views(&self) -> Vec<(&'a RelationEntry, &'a ViewDefinition)> {
        self.entries()
            .into_iter()
            .filter_map(|(_, entry)| match &entry.source {
                RelationSource::View(view) => Some((entry, &**view)),
                _ => None,
            })
            .collect()
    }

    fn sequences(&self) -> Vec<(&'a RelationEntry, &'a SequenceDefinition)> {
        self.entries()
            .into_iter()
            .filter_map(|(_, entry)| match &entry.source {
                RelationSource::Sequence(sequence) => Some((entry, &**sequence)),
                _ => None,
            })
            .collect()
    }
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: OracleRelation) -> Vec<Vec<Value>> {
    let dictionary = Dictionary::new(snapshot, relation);
    match relation {
        OracleRelation::Version => version_rows(&dictionary),
        OracleRelation::NlsSessionParameters => nls_rows(&dictionary, true),
        OracleRelation::NlsDatabaseParameters => nls_rows(&dictionary, false),
        OracleRelation::Dictionary(_, view) => match view {
            DictionaryView::Tables => table_rows(&dictionary),
            DictionaryView::TabColumns | DictionaryView::TabCols => column_rows(&dictionary),
            DictionaryView::Constraints => constraint_rows(&dictionary),
            DictionaryView::ConsColumns => constraint_column_rows(&dictionary),
            DictionaryView::Indexes => index_rows(&dictionary),
            DictionaryView::IndColumns => index_column_rows(&dictionary),
            DictionaryView::Sequences => sequence_rows(&dictionary),
            DictionaryView::Views => view_rows(&dictionary),
            DictionaryView::Objects => object_rows(&dictionary),
            DictionaryView::TabComments => table_comment_rows(&dictionary),
            DictionaryView::ColComments => column_comment_rows(&dictionary),
            DictionaryView::TabIdentityCols => identity_column_rows(&dictionary),
            DictionaryView::Users => user_rows(&dictionary),
        },
    }
}

fn table_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    dictionary
        .tables()
        .into_iter()
        .map(|(_, entry, _)| {
            dictionary
                .row()
                .set("owner", dictionary.owner(&entry.schema))
                .set("table_name", shown(&entry.name))
                .set("tablespace_name", text(TABLESPACE))
                .set("status", text("VALID"))
                .set("pct_free", number(10))
                .set("logging", text("YES"))
                .set("partitioned", text("NO"))
                .set("temporary", text("N"))
                .set("secondary", text("N"))
                .set("nested", text("NO"))
                .set("row_movement", text("DISABLED"))
                .set("dependencies", text("DISABLED"))
                .set("dropped", text("NO"))
                .set("read_only", text("NO"))
                .done()
        })
        .collect()
}

fn column_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (_, entry) in dictionary.entries() {
        for (position, facts) in relation_columns(entry).into_iter().enumerate() {
            let column_id = position as i64 + 1;
            let (precision, scale) = precision_and_scale(facts.sql_type);
            let characters = character_length(facts.sql_type);
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("table_name", shown(&entry.name))
                    .set("column_name", shown(facts.name))
                    .set("data_type", Value::Text(data_type(facts.sql_type)))
                    .set("data_length", number(length_of(facts.sql_type)))
                    .set("data_precision", optional_number(precision))
                    .set("data_scale", optional_number(scale))
                    .set("nullable", flag(facts.nullable, "Y", "N"))
                    .set("column_id", number(column_id))
                    .set(
                        "default_length",
                        optional_number(facts.default.map(|default| default.len() as i64)),
                    )
                    .set("data_default", optional_text(facts.default))
                    .set("char_col_decl_length", optional_number(characters))
                    .set("char_length", number(characters.unwrap_or(0)))
                    .set("char_used", optional_text(characters.map(|_| "C")))
                    .set("default_on_null", text("NO"))
                    .set("identity_column", flag(facts.identity, "YES", "NO"))
                    .set("hidden_column", text("NO"))
                    .set("virtual_column", text("NO"))
                    .set("segment_column_id", number(column_id))
                    .set("internal_column_id", number(column_id))
                    .set("user_generated", text("YES"))
                    .done(),
            );
        }
    }
    rows
}

fn constraint_code(source: ConstraintSource) -> &'static str {
    match source {
        ConstraintSource::Primary => "P",
        ConstraintSource::Unique(_) => "U",
        ConstraintSource::Check(_) => "C",
        ConstraintSource::Foreign(_) => "R",
    }
}

fn delete_rule(action: crate::ast::ReferentialAction) -> &'static str {
    match action {
        crate::ast::ReferentialAction::Cascade => "CASCADE",
        crate::ast::ReferentialAction::SetNull => "SET NULL",
        crate::ast::ReferentialAction::SetDefault
        | crate::ast::ReferentialAction::NoAction
        | crate::ast::ReferentialAction::Restrict => "NO ACTION",
    }
}

fn is_generated_name(name: &str) -> bool {
    name.get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sys_c"))
}

struct ReferencedConstraint {
    owner_schema: String,
    name: String,
}

fn referenced_constraint(
    snapshot: &CatalogSnapshot,
    table: usize,
    key_at: usize,
) -> Option<ReferencedConstraint> {
    let facts = crate::sqlserver_catalog::foreign_key_facts(snapshot, table, key_at)?;
    let parent = snapshot.relations.get(facts.referenced_relation?)?;
    let parent_table = parent.table()?;
    let name = match facts.referenced_index? {
        IndexSource::Primary => parent_table.primary_key_constraint_name(),
        IndexSource::Secondary(at) => parent_table.indexes.get(at)?.name.clone(),
    };
    Some(ReferencedConstraint {
        owner_schema: parent.schema.clone(),
        name,
    })
}

fn constraint_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let snapshot = dictionary.snapshot;
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let Some(entry) = snapshot.relations.get(constraint.table) else {
            continue;
        };
        let Some(table) = entry.table() else {
            continue;
        };
        if !dictionary.includes(&entry.schema) || HIDDEN_SCHEMAS.contains(&entry.schema.as_str())
        {
            continue;
        }
        let mut row = dictionary
            .row()
            .set("owner", dictionary.owner(&entry.schema))
            .set("constraint_name", shown(&constraint.name))
            .set("constraint_type", text(constraint_code(constraint.source)))
            .set("table_name", shown(&entry.name))
            .set("status", text("ENABLED"))
            .set("deferred", text("IMMEDIATE"))
            .set("validated", text("VALIDATED"))
            .set(
                "generated",
                flag(
                    is_generated_name(&constraint.name),
                    "GENERATED NAME",
                    "USER NAME",
                ),
            )
            .set("last_change", UNKNOWN_DATE);
        let mut deferrable = false;
        match constraint.source {
            ConstraintSource::Check(at) => {
                let condition = table.checks.get(at).map(|check| check.expression.as_str());
                row = row
                    .set("search_condition", optional_text(condition))
                    .set("search_condition_vc", optional_text(condition));
            }
            ConstraintSource::Foreign(at) => {
                if let Some(key) = table.foreign_keys.get(at) {
                    deferrable = key.deferrable;
                    row = row.set("delete_rule", text(delete_rule(key.on_delete)));
                }
                if let Some(referenced) = referenced_constraint(snapshot, constraint.table, at) {
                    row = row
                        .set("r_owner", dictionary.owner(&referenced.owner_schema))
                        .set("r_constraint_name", shown(&referenced.name));
                }
            }
            ConstraintSource::Unique(at) => {
                if let Some(index) = table.indexes.get(at) {
                    row = row
                        .set("index_owner", dictionary.owner(&entry.schema))
                        .set("index_name", shown(&index.name));
                }
            }
            ConstraintSource::Primary => {}
        }
        rows.push(
            row.set(
                "deferrable",
                flag(deferrable, "DEFERRABLE", "NOT DEFERRABLE"),
            )
            .done(),
        );
    }
    rows
}

fn constraint_column_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let snapshot = dictionary.snapshot;
    let mut rows = Vec::new();
    for constraint in &snapshot.constraints {
        let Some(entry) = snapshot.relations.get(constraint.table) else {
            continue;
        };
        let Some(table) = entry.table() else {
            continue;
        };
        if !dictionary.includes(&entry.schema) || HIDDEN_SCHEMAS.contains(&entry.schema.as_str())
        {
            continue;
        }
        let (columns, positioned): (Vec<usize>, bool) = match constraint.source {
            ConstraintSource::Primary => (table.primary_key.clone(), true),
            ConstraintSource::Unique(at) => (
                table
                    .indexes
                    .get(at)
                    .map(|index| index.columns.clone())
                    .unwrap_or_default(),
                true,
            ),
            ConstraintSource::Foreign(at) => (
                table
                    .foreign_keys
                    .get(at)
                    .map(|key| key.columns.clone())
                    .unwrap_or_default(),
                true,
            ),
            ConstraintSource::Check(at) => {
                (crate::information_schema::check_columns(table, at), false)
            }
        };
        for (position, column_at) in columns.into_iter().enumerate() {
            let Some(column) = table.columns.get(column_at) else {
                continue;
            };
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("constraint_name", shown(&constraint.name))
                    .set("table_name", shown(&entry.name))
                    .set("column_name", shown(&column.name))
                    .set(
                        "position",
                        optional_number(positioned.then_some(position as i64 + 1)),
                    )
                    .done(),
            );
        }
    }
    rows
}

fn index_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (_, entry, table) in dictionary.tables() {
        for index in &table.indexes {
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("index_name", shown(&index.name))
                    .set("index_type", text("NORMAL"))
                    .set("table_owner", dictionary.owner(&entry.schema))
                    .set("table_name", shown(&entry.name))
                    .set("table_type", text("TABLE"))
                    .set("uniqueness", flag(index.unique, "UNIQUE", "NONUNIQUE"))
                    .set("compression", text("DISABLED"))
                    .set("tablespace_name", text(TABLESPACE))
                    .set("logging", text("YES"))
                    .set("status", text("VALID"))
                    .set("partitioned", text("NO"))
                    .set("temporary", text("N"))
                    .set("generated", text("N"))
                    .set("visibility", text("VISIBLE"))
                    .done(),
            );
        }
    }
    rows
}

fn index_column_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (_, entry, table) in dictionary.tables() {
        for index in &table.indexes {
            for (position, column_at) in index.columns.iter().enumerate() {
                let Some(column) = table.columns.get(*column_at) else {
                    continue;
                };
                rows.push(
                    dictionary
                        .row()
                        .set("index_owner", dictionary.owner(&entry.schema))
                        .set("index_name", shown(&index.name))
                        .set("table_owner", dictionary.owner(&entry.schema))
                        .set("table_name", shown(&entry.name))
                        .set("column_name", shown(&column.name))
                        .set("column_position", number(position as i64 + 1))
                        .set("column_length", number(length_of(column.sql_type)))
                        .set(
                            "char_length",
                            number(character_length(column.sql_type).unwrap_or(0)),
                        )
                        .set(
                            "descend",
                            text(if index.sort_options.is_descending(position) {
                                "DESC"
                            } else {
                                "ASC"
                            }),
                        )
                        .done(),
                );
            }
        }
    }
    rows
}

fn sequence_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    dictionary
        .sequences()
        .into_iter()
        .map(|(entry, sequence)| {
            let last_number = if sequence.is_called {
                sequence.last_value.saturating_add(sequence.increment)
            } else {
                sequence.last_value
            };
            dictionary
                .row()
                .set("sequence_owner", dictionary.owner(&entry.schema))
                .set("sequence_name", shown(&entry.name))
                .set("min_value", number(sequence.min))
                .set("max_value", number(sequence.max))
                .set("increment_by", number(sequence.increment))
                .set("cycle_flag", flag(sequence.cycle, "Y", "N"))
                .set("order_flag", text("N"))
                .set("cache_size", number(sequence.cache))
                .set("last_number", number(last_number))
                .set("scale_flag", text("N"))
                .set("extend_flag", text("N"))
                .set("sharded_flag", text("N"))
                .set("session_flag", text("N"))
                .set("keep_value", text("N"))
                .done()
        })
        .collect()
}

fn view_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    dictionary
        .views()
        .into_iter()
        .map(|(entry, view)| {
            dictionary
                .row()
                .set("owner", dictionary.owner(&entry.schema))
                .set("view_name", shown(&entry.name))
                .set("text_length", number(view.query.len() as i64))
                .set("text", text(&view.query))
                .set("text_vc", text(&view.query))
                .set("read_only", text("N"))
                .set("editioning_view", text("N"))
                .set("bequeath", text("DEFINER"))
                .done()
        })
        .collect()
}

fn object_row(
    dictionary: &Dictionary<'_>,
    schema: &str,
    name: &str,
    id: u32,
    kind: &str,
    stored: bool,
) -> Vec<Value> {
    let namespace = if kind == "INDEX" { 4 } else { 1 };
    dictionary
        .row()
        .set("owner", dictionary.owner(schema))
        .set("object_name", shown(name))
        .set("object_id", number(i64::from(id)))
        .set(
            "data_object_id",
            optional_number(stored.then_some(i64::from(id))),
        )
        .set("object_type", text(kind))
        .set("created", UNKNOWN_DATE)
        .set("last_ddl_time", UNKNOWN_DATE)
        .set("timestamp", text(UNKNOWN_TIMESTAMP_TEXT))
        .set("status", text("VALID"))
        .set("temporary", text("N"))
        .set("generated", text("N"))
        .set("secondary", text("N"))
        .set("namespace", number(namespace))
        .set("oracle_maintained", text("N"))
        .done()
}

fn object_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let snapshot = dictionary.snapshot;
    let mut rows = Vec::new();
    for (at, entry) in dictionary.entries() {
        let (kind, stored) = match &entry.source {
            RelationSource::Table(_) => ("TABLE", true),
            RelationSource::View(_) => ("VIEW", false),
            RelationSource::Sequence(_) => ("SEQUENCE", false),
            RelationSource::Index { .. } | RelationSource::Catalog(_) => continue,
        };
        rows.push(object_row(
            dictionary,
            &entry.schema,
            &entry.name,
            entry.oid,
            kind,
            stored,
        ));
        let Some(table) = entry.table() else {
            continue;
        };
        for position in 0..table.indexes.len() {
            let Some(index) = snapshot
                .index_relation(at, IndexSource::Secondary(position))
                .and_then(|index_at| snapshot.relations.get(index_at))
            else {
                continue;
            };
            rows.push(object_row(
                dictionary,
                &entry.schema,
                &index.name,
                index.oid,
                "INDEX",
                true,
            ));
        }
    }
    rows
}

struct StoredComments<'a> {
    relations: HashMap<&'a str, &'a str>,
    columns: HashMap<(&'a str, &'a str), &'a str>,
}

impl<'a> StoredComments<'a> {
    fn of(snapshot: &'a CatalogSnapshot) -> Self {
        let mut relations = HashMap::new();
        let mut columns = HashMap::new();
        for comment in &snapshot.comments {
            match comment.kind {
                CommentKind::Relation => {
                    relations.insert(comment.owner.as_str(), comment.text.as_str());
                }
                CommentKind::Column => {
                    columns.insert(
                        (comment.owner.as_str(), comment.part.as_str()),
                        comment.text.as_str(),
                    );
                }
                CommentKind::Index | CommentKind::Constraint | CommentKind::Schema => {}
            }
        }
        Self { relations, columns }
    }
}

fn commented_kind(entry: &RelationEntry) -> Option<&'static str> {
    match entry.source {
        RelationSource::Table(_) => Some("TABLE"),
        RelationSource::View(_) => Some("VIEW"),
        RelationSource::Sequence(_) | RelationSource::Index { .. } | RelationSource::Catalog(_) => {
            None
        }
    }
}

fn table_comment_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let comments = StoredComments::of(dictionary.snapshot);
    dictionary
        .entries()
        .into_iter()
        .filter_map(|(_, entry)| {
            let kind = commented_kind(entry)?;
            Some(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("table_name", shown(&entry.name))
                    .set("table_type", text(kind))
                    .set(
                        "comments",
                        optional_text(comments.relations.get(entry.internal.as_str()).copied()),
                    )
                    .done(),
            )
        })
        .collect()
}

fn column_comment_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let comments = StoredComments::of(dictionary.snapshot);
    let mut rows = Vec::new();
    for (_, entry) in dictionary.entries() {
        for facts in relation_columns(entry) {
            let comment = comments
                .columns
                .get(&(entry.internal.as_str(), facts.name))
                .copied();
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("table_name", shown(&entry.name))
                    .set("column_name", shown(facts.name))
                    .set("comments", optional_text(comment))
                    .done(),
            );
        }
    }
    rows
}

fn identity_column_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (_, entry, table) in dictionary.tables() {
        for (position, column) in table.columns.iter().enumerate() {
            let Some(generation) = table.identity_of(position) else {
                continue;
            };
            let generation = match generation {
                crate::encoding::IdentityGeneration::Always => "ALWAYS",
                crate::encoding::IdentityGeneration::ByDefault => "BY DEFAULT",
            };
            let seed = table.identity_seed();
            rows.push(
                dictionary
                    .row()
                    .set("owner", dictionary.owner(&entry.schema))
                    .set("table_name", shown(&entry.name))
                    .set("column_name", shown(&column.name))
                    .set("generation_type", text(generation))
                    .set("sequence_name", Value::Text(format!("ISEQ$$_{}", table.id)))
                    .set("identity_options", Value::Text(identity_options(seed)))
                    .done(),
            );
        }
    }
    rows
}

fn identity_options(seed: crate::encoding::IdentitySeed) -> String {
    let (maximum, minimum) = if seed.increment > 0 {
        ("9999999999999999999999999999".to_owned(), "1".to_owned())
    } else {
        ("-1".to_owned(), "-999999999999999999999999999".to_owned())
    };
    format!(
        "START WITH: {}, INCREMENT BY: {}, MAX_VALUE: {maximum}, MIN_VALUE: {minimum}, CYCLE_FLAG: N, CACHE_SIZE: 20, ORDER_FLAG: N, SCALE_FLAG: N, EXTEND_FLAG: N, SESSION_FLAG: N, KEEP_VALUE: N",
        seed.seed, seed.increment
    )
}

fn user_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    dictionary
        .snapshot
        .schemas
        .iter()
        .filter(|schema| {
            !HIDDEN_SCHEMAS.contains(&schema.name.as_str()) && dictionary.includes(&schema.name)
        })
        .map(|schema| {
            dictionary
                .row()
                .set("username", dictionary.owner(&schema.name))
                .set("user_id", number(i64::from(schema.oid)))
                .set("account_status", text("OPEN"))
                .set("default_tablespace", text(TABLESPACE))
                .set("temporary_tablespace", text(TEMPORARY_TABLESPACE))
                .set("created", UNKNOWN_DATE)
                .set("common", text("NO"))
                .set("oracle_maintained", text("N"))
                .done()
        })
        .collect()
}

fn version_banner() -> String {
    format!(
        "EggDB {} - compatible with Oracle Database {} Release {COMPATIBLE_RELEASE}",
        env!("CARGO_PKG_VERSION"),
        crate::parser::ORACLE_COMPATIBLE_VERSION
    )
}

fn version_rows(dictionary: &Dictionary<'_>) -> Vec<Vec<Value>> {
    let banner = version_banner();
    vec![
        dictionary
            .row()
            .set("banner", text(&banner))
            .set(
                "banner_full",
                Value::Text(format!("{banner}\nVersion {COMPATIBLE_RELEASE}")),
            )
            .set("banner_legacy", text(&banner))
            .set("con_id", number(0))
            .done(),
    ]
}

fn nls_parameters(session: bool) -> Vec<(&'static str, String)> {
    let current = crate::oracle_nls::current();
    let defaults = crate::oracle_nls::NlsParameters::default();
    let chosen: &crate::oracle_nls::NlsParameters = if session { &current } else { &defaults };
    let mut parameters = vec![
        ("NLS_LANGUAGE", "AMERICAN".to_owned()),
        ("NLS_TERRITORY", "AMERICA".to_owned()),
        ("NLS_CURRENCY", "$".to_owned()),
        ("NLS_ISO_CURRENCY", "AMERICA".to_owned()),
        ("NLS_NUMERIC_CHARACTERS", ".,".to_owned()),
        ("NLS_CALENDAR", "GREGORIAN".to_owned()),
        ("NLS_DATE_FORMAT", chosen.date_format().to_owned()),
        ("NLS_DATE_LANGUAGE", "AMERICAN".to_owned()),
        ("NLS_SORT", "BINARY".to_owned()),
        ("NLS_TIME_FORMAT", TIME_FORMAT.to_owned()),
        ("NLS_TIMESTAMP_FORMAT", chosen.timestamp_format().to_owned()),
        ("NLS_TIME_TZ_FORMAT", TIME_TZ_FORMAT.to_owned()),
        (
            "NLS_TIMESTAMP_TZ_FORMAT",
            chosen.timestamp_tz_format().to_owned(),
        ),
        ("NLS_DUAL_CURRENCY", "$".to_owned()),
        ("NLS_COMP", "BINARY".to_owned()),
        ("NLS_LENGTH_SEMANTICS", "CHAR".to_owned()),
        ("NLS_NCHAR_CONV_EXCP", "FALSE".to_owned()),
    ];
    if !session {
        parameters.extend([
            ("NLS_CHARACTERSET", CHARACTER_SET.to_owned()),
            ("NLS_NCHAR_CHARACTERSET", NATIONAL_CHARACTER_SET.to_owned()),
            ("NLS_RDBMS_VERSION", COMPATIBLE_RELEASE.to_owned()),
        ]);
    }
    parameters
}

fn nls_session_parameter(name: &str) -> Option<String> {
    nls_parameters(true)
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, value)| value)
}

fn nls_rows(dictionary: &Dictionary<'_>, session: bool) -> Vec<Vec<Value>> {
    nls_parameters(session)
        .into_iter()
        .map(|(name, value)| {
            dictionary
                .row()
                .set("parameter", text(name))
                .set("value", Value::Text(value))
                .done()
        })
        .collect()
}

/// What `SYS_CONTEXT('USERENV', ...)` reads about the session.
pub(crate) struct SessionFacts<'a> {
    pub(crate) user: &'a str,
    pub(crate) current_schema: &'a str,
    pub(crate) database: &'a str,
    pub(crate) session_id: u32,
}

fn invalid_userenv_parameter() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "invalid USERENV parameter",
    )
    .with_oracle_code(2003)
}

/// The value of the `USERENV` namespace's `parameter` (any case) as `SYS_CONTEXT` returns it:
/// `None` for one Oracle documents but EggDB does not know (the client's host or program),
/// `ORA-02003` for one Oracle does not document.
pub(crate) fn userenv(parameter: &str, facts: &SessionFacts<'_>) -> SqlResult<Option<String>> {
    let upper = parameter.trim().to_ascii_uppercase();
    let value = match upper.as_str() {
        "CURRENT_USER" | "SESSION_USER" | "AUTHENTICATED_IDENTITY" => Some(facts.user.to_owned()),
        "CURRENT_EDITION_NAME" => Some("ORA$BASE".to_owned()),
        "CURRENT_SCHEMA" => Some(facts.current_schema.to_owned()),
        "DB_NAME" | "DB_UNIQUE_NAME" | "CON_NAME" | "INSTANCE_NAME" | "SERVICE_NAME" => {
            Some(facts.database.to_owned())
        }
        "SID" | "SESSIONID" => Some(facts.session_id.to_string()),
        "INSTANCE" | "CON_ID" => Some("1".to_owned()),
        "ISDBA" => Some("FALSE".to_owned()),
        "AUTHENTICATION_METHOD" => Some("PASSWORD".to_owned()),
        "DATABASE_ROLE" => Some("PRIMARY".to_owned()),
        "LANGUAGE" => Some(format!("AMERICAN_AMERICA.{CHARACTER_SET}")),
        "LANG" => Some("US".to_owned()),
        "NLS_TERRITORY" => Some("AMERICA".to_owned()),
        "NLS_CURRENCY" => Some("$".to_owned()),
        "NLS_CALENDAR" => Some("GREGORIAN".to_owned()),
        "NLS_SORT" => Some("BINARY".to_owned()),
        "NLS_DATE_LANGUAGE" => Some("AMERICAN".to_owned()),
        "NLS_DATE_FORMAT" => nls_session_parameter("NLS_DATE_FORMAT"),
        "DB_DOMAIN" | "HOST" | "OS_USER" | "TERMINAL" | "IP_ADDRESS" | "MODULE" | "ACTION"
        | "CLIENT_INFO" | "CLIENT_IDENTIFIER" | "CLIENT_PROGRAM_NAME" | "SERVER_HOST"
        | "GLOBAL_CONTEXT_MEMORY" | "BG_JOB_ID" | "FG_JOB_ID" | "ENTRYID" | "STATEMENTID"
        | "NETWORK_PROTOCOL" | "POLICY_INVOKER" | "PROXY_USER" | "PROXY_USERID"
        | "ENTERPRISE_IDENTITY" | "CURRENT_SQL" => None,
        _ => return Err(invalid_userenv_parameter()),
    };
    Ok(value)
}
