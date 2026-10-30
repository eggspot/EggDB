use super::types::{SYSTEM_TYPES, SqlServerTypeName};
use super::{
    COLLATION, CatalogRow, DATABASE_ID, DBO_PRINCIPAL_ID, ObjectKind, ObjectSource,
    SqlServerObject, SqlServerObjects, UNKNOWN_DATE, bit, column, foreign_key_facts, index_id_of,
    int, optional_int, referential_action, relation_columns, small, text,
};
use crate::pg_catalog::{
    CatalogColumn, CatalogSnapshot, ConstraintSource, RelationSource, index_columns,
    index_is_unique, index_sort_options,
};
use crate::types::{SqlType, Value};

const SYSNAME: SqlType = SqlType::Varchar(Some(128));
const INT: SqlType = SqlType::Integer;
const SMALLINT: SqlType = SqlType::SmallInt;
const TINYINT: SqlType = SqlType::SmallInt;
const BIGINT: SqlType = SqlType::BigInt;
const BIT: SqlType = SqlType::Boolean;
const CHAR2: SqlType = SqlType::Bpchar(Some(2));
const DESCRIPTION: SqlType = SqlType::Varchar(Some(60));
const DATETIME: SqlType = SqlType::Timestamp(Some(3));
const NVARCHAR_MAX: SqlType = SqlType::Text;
const VARBINARY: SqlType = SqlType::Bytea;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SystemView {
    Objects,
    Tables,
    Views,
    Columns,
    Types,
    Schemas,
    Indexes,
    IndexColumns,
    ForeignKeys,
    ForeignKeyColumns,
    KeyConstraints,
    DefaultConstraints,
    CheckConstraints,
    IdentityColumns,
    Sequences,
    Databases,
    SqlModules,
    Procedures,
    Parameters,
    Triggers,
    ExtendedProperties,
}

impl SystemView {
    pub(crate) const ALL: [Self; 21] = [
        Self::Objects,
        Self::Tables,
        Self::Views,
        Self::Columns,
        Self::Types,
        Self::Schemas,
        Self::Indexes,
        Self::IndexColumns,
        Self::ForeignKeys,
        Self::ForeignKeyColumns,
        Self::KeyConstraints,
        Self::DefaultConstraints,
        Self::CheckConstraints,
        Self::IdentityColumns,
        Self::Sequences,
        Self::Databases,
        Self::SqlModules,
        Self::Procedures,
        Self::Parameters,
        Self::Triggers,
        Self::ExtendedProperties,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Objects => "objects",
            Self::Tables => "tables",
            Self::Views => "views",
            Self::Columns => "columns",
            Self::Types => "types",
            Self::Schemas => "schemas",
            Self::Indexes => "indexes",
            Self::IndexColumns => "index_columns",
            Self::ForeignKeys => "foreign_keys",
            Self::ForeignKeyColumns => "foreign_key_columns",
            Self::KeyConstraints => "key_constraints",
            Self::DefaultConstraints => "default_constraints",
            Self::CheckConstraints => "check_constraints",
            Self::IdentityColumns => "identity_columns",
            Self::Sequences => "sequences",
            Self::Databases => "databases",
            Self::SqlModules => "sql_modules",
            Self::Procedures => "procedures",
            Self::Parameters => "parameters",
            Self::Triggers => "triggers",
            Self::ExtendedProperties => "extended_properties",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.name() == name)
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Objects => OBJECT_COLUMNS,
            Self::Tables => TABLE_COLUMNS,
            Self::Views => VIEW_COLUMNS,
            Self::Columns => COLUMN_COLUMNS,
            Self::Types => TYPE_COLUMNS,
            Self::Schemas => SCHEMA_COLUMNS,
            Self::Indexes => INDEX_COLUMNS,
            Self::IndexColumns => INDEX_COLUMN_COLUMNS,
            Self::ForeignKeys => FOREIGN_KEY_COLUMNS,
            Self::ForeignKeyColumns => FOREIGN_KEY_COLUMN_COLUMNS,
            Self::KeyConstraints => KEY_CONSTRAINT_COLUMNS,
            Self::DefaultConstraints => DEFAULT_CONSTRAINT_COLUMNS,
            Self::CheckConstraints => CHECK_CONSTRAINT_COLUMNS,
            Self::IdentityColumns => IDENTITY_COLUMN_COLUMNS,
            Self::Sequences => SEQUENCE_COLUMNS,
            Self::Databases => DATABASE_COLUMNS,
            Self::SqlModules => SQL_MODULE_COLUMNS,
            Self::Procedures => PROCEDURE_COLUMNS,
            Self::Parameters => PARAMETER_COLUMNS,
            Self::Triggers => TRIGGER_COLUMNS,
            Self::ExtendedProperties => EXTENDED_PROPERTY_COLUMNS,
        }
    }
}

const OBJECT_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
];

const TABLE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("lob_data_space_id", INT),
    column("filestream_data_space_id", INT),
    column("max_column_id_used", INT),
    column("lock_on_bulk_load", BIT),
    column("uses_ansi_nulls", BIT),
    column("is_replicated", BIT),
    column("has_replication_filter", BIT),
    column("is_merge_published", BIT),
    column("is_sync_tran_subscribed", BIT),
    column("has_unchecked_assembly_data", BIT),
    column("text_in_row_limit", INT),
    column("large_value_types_out_of_row", BIT),
    column("is_tracked_by_cdc", BIT),
    column("lock_escalation", TINYINT),
    column("lock_escalation_desc", DESCRIPTION),
    column("is_filetable", BIT),
    column("is_memory_optimized", BIT),
    column("durability", TINYINT),
    column("durability_desc", DESCRIPTION),
    column("temporal_type", TINYINT),
    column("temporal_type_desc", DESCRIPTION),
    column("history_table_id", INT),
    column("is_remote_data_archive_enabled", BIT),
    column("is_external", BIT),
    column("history_retention_period", INT),
    column("history_retention_period_unit", INT),
    column("history_retention_period_unit_desc", DESCRIPTION),
    column("is_node", BIT),
    column("is_edge", BIT),
    column("data_retention_period", INT),
    column("data_retention_period_unit", INT),
    column("data_retention_period_unit_desc", DESCRIPTION),
    column("ledger_type", TINYINT),
    column("ledger_type_desc", DESCRIPTION),
    column("ledger_view_id", INT),
    column("is_dropped_ledger_table", BIT),
];

const VIEW_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("is_replicated", BIT),
    column("has_replication_filter", BIT),
    column("has_opaque_metadata", BIT),
    column("has_unchecked_assembly_data", BIT),
    column("with_check_option", BIT),
    column("is_date_correlation_view", BIT),
    column("is_tracked_by_cdc", BIT),
    column("has_snapshot", BIT),
    column("ledger_view_type", TINYINT),
    column("ledger_view_type_desc", DESCRIPTION),
    column("is_dropped_ledger_view", BIT),
];

const COLUMN_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("column_id", INT),
    column("system_type_id", TINYINT),
    column("user_type_id", INT),
    column("max_length", SMALLINT),
    column("precision", TINYINT),
    column("scale", TINYINT),
    column("collation_name", SYSNAME),
    column("is_nullable", BIT),
    column("is_ansi_padded", BIT),
    column("is_rowguidcol", BIT),
    column("is_identity", BIT),
    column("is_computed", BIT),
    column("is_filestream", BIT),
    column("is_replicated", BIT),
    column("is_non_sql_subscribed", BIT),
    column("is_merge_published", BIT),
    column("is_dts_replicated", BIT),
    column("is_xml_document", BIT),
    column("xml_collection_id", INT),
    column("default_object_id", INT),
    column("rule_object_id", INT),
    column("is_sparse", BIT),
    column("is_column_set", BIT),
    column("generated_always_type", TINYINT),
    column("generated_always_type_desc", DESCRIPTION),
    column("encryption_type", INT),
    column("encryption_type_desc", DESCRIPTION),
    column("encryption_algorithm_name", SYSNAME),
    column("column_encryption_key_id", INT),
    column("column_encryption_key_database_name", SYSNAME),
    column("is_hidden", BIT),
    column("is_masked", BIT),
    column("graph_type", INT),
    column("graph_type_desc", DESCRIPTION),
    column("is_data_deletion_filter_column", BIT),
    column("ledger_view_column_type", INT),
    column("ledger_view_column_type_desc", DESCRIPTION),
    column("is_dropped_ledger_column", BIT),
];

const IDENTITY_COLUMN_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("column_id", INT),
    column("system_type_id", TINYINT),
    column("user_type_id", INT),
    column("max_length", SMALLINT),
    column("precision", TINYINT),
    column("scale", TINYINT),
    column("collation_name", SYSNAME),
    column("is_nullable", BIT),
    column("is_ansi_padded", BIT),
    column("is_rowguidcol", BIT),
    column("is_identity", BIT),
    column("is_computed", BIT),
    column("is_filestream", BIT),
    column("is_replicated", BIT),
    column("is_non_sql_subscribed", BIT),
    column("is_merge_published", BIT),
    column("is_dts_replicated", BIT),
    column("is_xml_document", BIT),
    column("xml_collection_id", INT),
    column("default_object_id", INT),
    column("rule_object_id", INT),
    column("is_sparse", BIT),
    column("is_column_set", BIT),
    column("generated_always_type", TINYINT),
    column("generated_always_type_desc", DESCRIPTION),
    column("encryption_type", INT),
    column("encryption_type_desc", DESCRIPTION),
    column("encryption_algorithm_name", SYSNAME),
    column("column_encryption_key_id", INT),
    column("column_encryption_key_database_name", SYSNAME),
    column("is_hidden", BIT),
    column("is_masked", BIT),
    column("graph_type", INT),
    column("graph_type_desc", DESCRIPTION),
    column("is_data_deletion_filter_column", BIT),
    column("ledger_view_column_type", INT),
    column("ledger_view_column_type_desc", DESCRIPTION),
    column("is_dropped_ledger_column", BIT),
    column("seed_value", BIGINT),
    column("increment_value", BIGINT),
    column("last_value", BIGINT),
    column("is_not_for_replication", BIT),
];

const TYPE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("system_type_id", TINYINT),
    column("user_type_id", INT),
    column("schema_id", INT),
    column("principal_id", INT),
    column("max_length", SMALLINT),
    column("precision", TINYINT),
    column("scale", TINYINT),
    column("collation_name", SYSNAME),
    column("is_nullable", BIT),
    column("is_user_defined", BIT),
    column("is_assembly_type", BIT),
    column("default_object_id", INT),
    column("rule_object_id", INT),
    column("is_table_type", BIT),
];

const SCHEMA_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("schema_id", INT),
    column("principal_id", INT),
];

const INDEX_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("index_id", INT),
    column("type", TINYINT),
    column("type_desc", DESCRIPTION),
    column("is_unique", BIT),
    column("data_space_id", INT),
    column("ignore_dup_key", BIT),
    column("is_primary_key", BIT),
    column("is_unique_constraint", BIT),
    column("fill_factor", TINYINT),
    column("is_padded", BIT),
    column("is_disabled", BIT),
    column("is_hypothetical", BIT),
    column("is_ignored_in_optimization", BIT),
    column("allow_row_locks", BIT),
    column("allow_page_locks", BIT),
    column("has_filter", BIT),
    column("filter_definition", NVARCHAR_MAX),
    column("compression_delay", INT),
    column("suppress_dup_key_messages", BIT),
    column("auto_created", BIT),
    column("optimize_for_sequential_key", BIT),
];

const INDEX_COLUMN_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("index_id", INT),
    column("index_column_id", INT),
    column("column_id", INT),
    column("key_ordinal", TINYINT),
    column("partition_ordinal", TINYINT),
    column("is_descending_key", BIT),
    column("is_included_column", BIT),
    column("column_store_order_ordinal", TINYINT),
];

const FOREIGN_KEY_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("referenced_object_id", INT),
    column("key_index_id", INT),
    column("is_disabled", BIT),
    column("is_not_for_replication", BIT),
    column("is_not_trusted", BIT),
    column("delete_referential_action", TINYINT),
    column("delete_referential_action_desc", DESCRIPTION),
    column("update_referential_action", TINYINT),
    column("update_referential_action_desc", DESCRIPTION),
    column("is_system_named", BIT),
];

const FOREIGN_KEY_COLUMN_COLUMNS: &[CatalogColumn] = &[
    column("constraint_object_id", INT),
    column("constraint_column_id", INT),
    column("parent_object_id", INT),
    column("parent_column_id", INT),
    column("referenced_object_id", INT),
    column("referenced_column_id", INT),
];

const KEY_CONSTRAINT_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("unique_index_id", INT),
    column("is_system_named", BIT),
    column("is_enforced", BIT),
];

const DEFAULT_CONSTRAINT_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("parent_column_id", INT),
    column("definition", NVARCHAR_MAX),
    column("is_system_named", BIT),
];

const CHECK_CONSTRAINT_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("is_disabled", BIT),
    column("is_not_for_replication", BIT),
    column("is_not_trusted", BIT),
    column("parent_column_id", INT),
    column("definition", NVARCHAR_MAX),
    column("uses_database_collation", BIT),
    column("is_system_named", BIT),
];

const SEQUENCE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("start_value", BIGINT),
    column("increment", BIGINT),
    column("minimum_value", BIGINT),
    column("maximum_value", BIGINT),
    column("is_cycling", BIT),
    column("is_cached", BIT),
    column("cache_size", INT),
    column("system_type_id", TINYINT),
    column("user_type_id", INT),
    column("precision", TINYINT),
    column("scale", TINYINT),
    column("current_value", BIGINT),
    column("is_exhausted", BIT),
    column("last_used_value", BIGINT),
];

const DATABASE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("database_id", INT),
    column("source_database_id", INT),
    column("owner_sid", VARBINARY),
    column("create_date", DATETIME),
    column("compatibility_level", TINYINT),
    column("collation_name", SYSNAME),
    column("user_access", TINYINT),
    column("user_access_desc", DESCRIPTION),
    column("is_read_only", BIT),
    column("is_auto_close_on", BIT),
    column("is_auto_shrink_on", BIT),
    column("state", TINYINT),
    column("state_desc", DESCRIPTION),
    column("is_in_standby", BIT),
    column("is_cleanly_shutdown", BIT),
    column("is_supplemental_logging_enabled", BIT),
    column("snapshot_isolation_state", TINYINT),
    column("snapshot_isolation_state_desc", DESCRIPTION),
    column("is_read_committed_snapshot_on", BIT),
    column("recovery_model", TINYINT),
    column("recovery_model_desc", DESCRIPTION),
    column("page_verify_option", TINYINT),
    column("page_verify_option_desc", DESCRIPTION),
    column("is_auto_create_stats_on", BIT),
    column("is_auto_update_stats_on", BIT),
    column("is_ansi_null_default_on", BIT),
    column("is_ansi_nulls_on", BIT),
    column("is_ansi_padding_on", BIT),
    column("is_ansi_warnings_on", BIT),
    column("is_arithabort_on", BIT),
    column("is_concat_null_yields_null_on", BIT),
    column("is_quoted_identifier_on", BIT),
    column("is_fulltext_enabled", BIT),
    column("is_trustworthy_on", BIT),
    column("is_encrypted", BIT),
    column("containment", TINYINT),
    column("containment_desc", DESCRIPTION),
    column("catalog_collation_type", INT),
    column("catalog_collation_type_desc", DESCRIPTION),
];

const SQL_MODULE_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("definition", NVARCHAR_MAX),
    column("uses_ansi_nulls", BIT),
    column("uses_quoted_identifier", BIT),
    column("is_schema_bound", BIT),
    column("uses_database_collation", BIT),
    column("is_recompiled", BIT),
    column("null_on_null_input", BIT),
    column("execute_as_principal_id", INT),
    column("uses_native_compilation", BIT),
    column("inline_type", BIT),
    column("is_inlineable", BIT),
];

const PROCEDURE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("principal_id", INT),
    column("schema_id", INT),
    column("parent_object_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_published", BIT),
    column("is_schema_published", BIT),
    column("is_auto_executed", BIT),
    column("is_execution_replicated", BIT),
    column("is_repl_serializable_only", BIT),
    column("skips_repl_constraints", BIT),
];

const PARAMETER_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("parameter_id", INT),
    column("system_type_id", TINYINT),
    column("user_type_id", INT),
    column("max_length", SMALLINT),
    column("precision", TINYINT),
    column("scale", TINYINT),
    column("is_output", BIT),
    column("is_cursor_ref", BIT),
    column("has_default_value", BIT),
    column("is_xml_document", BIT),
    column("default_value", NVARCHAR_MAX),
    column("xml_collection_id", INT),
    column("is_readonly", BIT),
    column("is_nullable", BIT),
];

const TRIGGER_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("object_id", INT),
    column("parent_class", TINYINT),
    column("parent_class_desc", DESCRIPTION),
    column("parent_id", INT),
    column("type", CHAR2),
    column("type_desc", DESCRIPTION),
    column("create_date", DATETIME),
    column("modify_date", DATETIME),
    column("is_ms_shipped", BIT),
    column("is_disabled", BIT),
    column("is_not_for_replication", BIT),
    column("is_instead_of_trigger", BIT),
];

const EXTENDED_PROPERTY_COLUMNS: &[CatalogColumn] = &[
    column("class", TINYINT),
    column("class_desc", DESCRIPTION),
    column("major_id", INT),
    column("minor_id", INT),
    column("name", SYSNAME),
    column("value", NVARCHAR_MAX),
];

pub(super) fn rows(snapshot: &CatalogSnapshot, view: SystemView) -> Vec<Vec<Value>> {
    let objects = snapshot.sqlserver_objects();
    match view {
        SystemView::Objects => objects
            .objects
            .iter()
            .map(|object| object_row(OBJECT_COLUMNS, object).done())
            .collect(),
        SystemView::Tables => table_rows(snapshot, objects),
        SystemView::Views => view_rows(objects),
        SystemView::Columns => column_rows(snapshot, objects, false),
        SystemView::IdentityColumns => column_rows(snapshot, objects, true),
        SystemView::Types => type_rows(),
        SystemView::Schemas => schema_rows(objects),
        SystemView::Indexes => index_rows(snapshot, objects),
        SystemView::IndexColumns => index_column_rows(snapshot, objects),
        SystemView::ForeignKeys => foreign_key_rows(snapshot, objects),
        SystemView::ForeignKeyColumns => foreign_key_column_rows(snapshot, objects),
        SystemView::KeyConstraints => key_constraint_rows(snapshot, objects),
        SystemView::DefaultConstraints => default_constraint_rows(snapshot, objects),
        SystemView::CheckConstraints => check_constraint_rows(snapshot, objects),
        SystemView::Sequences => sequence_rows(snapshot, objects),
        SystemView::Databases => vec![database_row(snapshot)],
        SystemView::SqlModules => sql_module_rows(snapshot, objects),
        SystemView::Procedures
        | SystemView::Parameters
        | SystemView::Triggers
        | SystemView::ExtendedProperties => Vec::new(),
    }
}

fn with_flags(mut row: CatalogRow, names: &[&str], value: bool) -> CatalogRow {
    for name in names {
        row = row.set(name, bit(value));
    }
    row
}

fn object_row(columns: &'static [CatalogColumn], object: &SqlServerObject) -> CatalogRow {
    let row = CatalogRow::new(columns)
        .set("name", text(&object.name))
        .set("object_id", Value::Integer(object.object_id))
        .set("schema_id", Value::Integer(object.schema_id))
        .set("parent_object_id", Value::Integer(object.parent_object_id))
        .set("type", Value::Bpchar(object.kind.code().to_owned()))
        .set("type_desc", text(object.kind.description()))
        .set("create_date", UNKNOWN_DATE)
        .set("modify_date", UNKNOWN_DATE);
    with_flags(
        row,
        &["is_ms_shipped", "is_published", "is_schema_published"],
        false,
    )
}

fn of_kind(objects: &SqlServerObjects, kind: ObjectKind) -> impl Iterator<Item = &SqlServerObject> {
    objects
        .objects
        .iter()
        .filter(move |object| object.kind == kind)
}

fn table_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::Table)
        .map(|object| {
            let columns = snapshot
                .relations
                .get(object.relation)
                .map_or_else(Vec::new, relation_columns);
            let has_large_values = columns.iter().any(|column| column.column_type.is_max());
            let row = object_row(TABLE_COLUMNS, object)
                .set("lob_data_space_id", int(i64::from(has_large_values)))
                .set("max_column_id_used", int(columns.len() as i64))
                .set("text_in_row_limit", int(0))
                .set("lock_escalation", small(0))
                .set("lock_escalation_desc", text("TABLE"))
                .set("durability", small(0))
                .set("durability_desc", text("SCHEMA_AND_DATA"))
                .set("temporal_type", small(0))
                .set("temporal_type_desc", text("NON_TEMPORAL_TABLE"))
                .set("ledger_type", small(0))
                .set("ledger_type_desc", text("NON_LEDGER_TABLE"))
                .set("uses_ansi_nulls", bit(true));
            with_flags(
                row,
                &[
                    "lock_on_bulk_load",
                    "is_replicated",
                    "has_replication_filter",
                    "is_merge_published",
                    "is_sync_tran_subscribed",
                    "has_unchecked_assembly_data",
                    "large_value_types_out_of_row",
                    "is_tracked_by_cdc",
                    "is_filetable",
                    "is_memory_optimized",
                    "is_remote_data_archive_enabled",
                    "is_external",
                    "is_node",
                    "is_edge",
                    "is_dropped_ledger_table",
                ],
                false,
            )
            .done()
        })
        .collect()
}

fn view_rows(objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::View)
        .map(|object| {
            with_flags(
                object_row(VIEW_COLUMNS, object),
                &[
                    "is_replicated",
                    "has_replication_filter",
                    "has_opaque_metadata",
                    "has_unchecked_assembly_data",
                    "with_check_option",
                    "is_date_correlation_view",
                    "is_tracked_by_cdc",
                    "has_snapshot",
                    "is_dropped_ledger_view",
                ],
                false,
            )
            .done()
        })
        .collect()
}

fn column_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
    identity_only: bool,
) -> Vec<Vec<Value>> {
    let columns = if identity_only {
        IDENTITY_COLUMN_COLUMNS
    } else {
        COLUMN_COLUMNS
    };
    let mut rows = Vec::new();
    for object in objects.relations() {
        if !matches!(object.kind, ObjectKind::Table | ObjectKind::View) {
            continue;
        }
        let Some(entry) = snapshot.relations.get(object.relation) else {
            continue;
        };
        for (position, facts) in relation_columns(entry).into_iter().enumerate() {
            if identity_only && !facts.identity {
                continue;
            }
            let column_type = facts.column_type;
            let collated = column_type.name.is_character();
            let padded = collated || matches!(column_type.name, SqlServerTypeName::VarBinary);
            let default_object = objects
                .of_default(object.relation, position)
                .map_or(0, |found| found.object_id);
            let mut row = CatalogRow::new(columns)
                .set("object_id", Value::Integer(object.object_id))
                .set("name", text(facts.name))
                .set("column_id", int(position as i64 + 1))
                .set("system_type_id", small(column_type.system_type_id()))
                .set("user_type_id", int(column_type.system_type_id()))
                .set("max_length", small(column_type.max_length()))
                .set("precision", small(column_type.precision))
                .set("scale", small(column_type.scale))
                .set(
                    "collation_name",
                    if collated {
                        text(COLLATION)
                    } else {
                        Value::Null
                    },
                )
                .set("is_nullable", bit(facts.nullable))
                .set("is_ansi_padded", bit(padded))
                .set("is_identity", bit(facts.identity))
                .set("xml_collection_id", int(0))
                .set("default_object_id", Value::Integer(default_object))
                .set("rule_object_id", int(0))
                .set("generated_always_type", small(0))
                .set("generated_always_type_desc", text("NOT_APPLICABLE"));
            row = with_flags(
                row,
                &[
                    "is_rowguidcol",
                    "is_computed",
                    "is_filestream",
                    "is_replicated",
                    "is_non_sql_subscribed",
                    "is_merge_published",
                    "is_dts_replicated",
                    "is_xml_document",
                    "is_sparse",
                    "is_column_set",
                    "is_hidden",
                    "is_masked",
                    "is_data_deletion_filter_column",
                    "is_dropped_ledger_column",
                ],
                false,
            );
            if identity_only {
                row = row
                    .set("seed_value", Value::BigInt(facts.identity_seed.seed))
                    .set("increment_value", Value::BigInt(facts.identity_seed.increment))
                    .set("is_not_for_replication", bit(false));
            }
            rows.push(row.done());
        }
    }
    rows
}

fn type_rows() -> Vec<Vec<Value>> {
    SYSTEM_TYPES
        .iter()
        .map(|found| {
            let row = CatalogRow::new(TYPE_COLUMNS)
                .set("name", text(found.name.name()))
                .set("system_type_id", small(found.system_type_id))
                .set("user_type_id", int(found.user_type_id))
                .set("schema_id", int(i64::from(super::SYS_SCHEMA_ID)))
                .set("max_length", small(found.max_length))
                .set("precision", small(found.precision))
                .set("scale", small(found.scale))
                .set(
                    "collation_name",
                    if found.collated {
                        text(COLLATION)
                    } else {
                        Value::Null
                    },
                )
                .set("is_nullable", bit(found.name != SqlServerTypeName::SysName))
                .set("default_object_id", int(0))
                .set("rule_object_id", int(0));
            with_flags(
                row,
                &["is_user_defined", "is_assembly_type", "is_table_type"],
                false,
            )
            .done()
        })
        .collect()
}

fn schema_rows(objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    objects
        .schemas
        .iter()
        .map(|schema| {
            let principal = if schema.schema_id <= super::SYS_SCHEMA_ID {
                schema.schema_id
            } else {
                DBO_PRINCIPAL_ID
            };
            CatalogRow::new(SCHEMA_COLUMNS)
                .set("name", text(&schema.name))
                .set("schema_id", Value::Integer(schema.schema_id))
                .set("principal_id", Value::Integer(principal))
                .done()
        })
        .collect()
}

fn index_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for object in of_kind(objects, ObjectKind::Table) {
        let Some(table) = snapshot
            .relations
            .get(object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        for index in objects.indexes(snapshot, object.relation) {
            let (kind, description) = if index.is_heap() {
                (0, "HEAP")
            } else if index.is_clustered() {
                (1, "CLUSTERED")
            } else {
                (2, "NONCLUSTERED")
            };
            let unique = index
                .source
                .is_some_and(|source| index_is_unique(table, source));
            let primary = index.is_clustered();
            let row = CatalogRow::new(INDEX_COLUMNS)
                .set("object_id", Value::Integer(object.object_id))
                .set(
                    "name",
                    objects
                        .index_name(snapshot, object.relation, &index)
                        .map_or(Value::Null, Value::Text),
                )
                .set("index_id", Value::Integer(index.index_id))
                .set("type", small(kind))
                .set("type_desc", text(description))
                .set("is_unique", bit(unique))
                .set("data_space_id", int(1))
                .set("is_primary_key", bit(primary))
                .set("is_unique_constraint", bit(unique && !primary))
                .set("fill_factor", small(0))
                .set("allow_row_locks", bit(true))
                .set("allow_page_locks", bit(true));
            rows.push(
                with_flags(
                    row,
                    &[
                        "ignore_dup_key",
                        "is_padded",
                        "is_disabled",
                        "is_hypothetical",
                        "is_ignored_in_optimization",
                        "has_filter",
                        "suppress_dup_key_messages",
                        "auto_created",
                        "optimize_for_sequential_key",
                    ],
                    false,
                )
                .done(),
            );
        }
    }
    rows
}

fn index_column_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for object in of_kind(objects, ObjectKind::Table) {
        let Some(table) = snapshot
            .relations
            .get(object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        for index in objects.indexes(snapshot, object.relation) {
            let Some(source) = index.source else {
                continue;
            };
            let sort_options = index_sort_options(table, source);
            for (ordinal, column) in index_columns(table, source).iter().enumerate() {
                rows.push(
                    CatalogRow::new(INDEX_COLUMN_COLUMNS)
                        .set("object_id", Value::Integer(object.object_id))
                        .set("index_id", Value::Integer(index.index_id))
                        .set("index_column_id", int(ordinal as i64 + 1))
                        .set("column_id", int(*column as i64 + 1))
                        .set("key_ordinal", small(ordinal as i64 + 1))
                        .set("partition_ordinal", small(0))
                        .set(
                            "is_descending_key",
                            bit(sort_options.is_descending(ordinal)),
                        )
                        .set("is_included_column", bit(false))
                        .set("column_store_order_ordinal", small(0))
                        .done(),
                );
            }
        }
    }
    rows
}

fn foreign_key_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for object in of_kind(objects, ObjectKind::ForeignKey) {
        let ObjectSource::Constraint(at) = object.source else {
            continue;
        };
        let Some(ConstraintSource::Foreign(key_at)) = snapshot
            .constraints
            .get(at)
            .map(|constraint| constraint.source)
        else {
            continue;
        };
        let Some(facts) = foreign_key_facts(snapshot, object.relation, key_at) else {
            continue;
        };
        let referenced = facts
            .referenced_relation
            .and_then(|relation| objects.relation_object(relation));
        let (delete, delete_description) = referential_action(facts.key.on_delete);
        let (update, update_description) = referential_action(facts.key.on_update);
        let row = object_row(FOREIGN_KEY_COLUMNS, object)
            .set(
                "referenced_object_id",
                optional_int(referenced.map(i64::from)),
            )
            .set(
                "key_index_id",
                optional_int(facts.referenced_index.map(index_id_of)),
            )
            .set("delete_referential_action", small(delete))
            .set("delete_referential_action_desc", text(delete_description))
            .set("update_referential_action", small(update))
            .set("update_referential_action_desc", text(update_description));
        rows.push(
            with_flags(
                row,
                &[
                    "is_disabled",
                    "is_not_for_replication",
                    "is_not_trusted",
                    "is_system_named",
                ],
                false,
            )
            .done(),
        );
    }
    rows
}

fn foreign_key_column_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for object in of_kind(objects, ObjectKind::ForeignKey) {
        let ObjectSource::Constraint(at) = object.source else {
            continue;
        };
        let Some(ConstraintSource::Foreign(key_at)) = snapshot
            .constraints
            .get(at)
            .map(|constraint| constraint.source)
        else {
            continue;
        };
        let Some(facts) = foreign_key_facts(snapshot, object.relation, key_at) else {
            continue;
        };
        let referenced = facts
            .referenced_relation
            .and_then(|relation| objects.relation_object(relation));
        for (ordinal, column) in facts.key.columns.iter().enumerate() {
            rows.push(
                CatalogRow::new(FOREIGN_KEY_COLUMN_COLUMNS)
                    .set("constraint_object_id", Value::Integer(object.object_id))
                    .set("constraint_column_id", int(ordinal as i64 + 1))
                    .set("parent_object_id", Value::Integer(object.parent_object_id))
                    .set("parent_column_id", int(*column as i64 + 1))
                    .set(
                        "referenced_object_id",
                        optional_int(referenced.map(i64::from)),
                    )
                    .set(
                        "referenced_column_id",
                        optional_int(
                            facts
                                .referenced_columns
                                .get(ordinal)
                                .map(|column| *column as i64 + 1),
                        ),
                    )
                    .done(),
            );
        }
    }
    rows
}

fn key_constraint_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    objects
        .objects
        .iter()
        .filter(|object| matches!(object.kind, ObjectKind::PrimaryKey | ObjectKind::Unique))
        .filter_map(|object| {
            let ObjectSource::Constraint(at) = object.source else {
                return None;
            };
            let index = match snapshot.constraints.get(at)?.source {
                ConstraintSource::Primary => 1,
                ConstraintSource::Unique(position) => {
                    index_id_of(crate::pg_catalog::IndexSource::Secondary(position))
                }
                _ => return None,
            };
            let row = object_row(KEY_CONSTRAINT_COLUMNS, object)
                .set("unique_index_id", int(index))
                .set("is_system_named", bit(false));
            Some(row.done())
        })
        .collect()
}

fn default_constraint_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::Default)
        .filter_map(|object| {
            let ObjectSource::Default(column) = object.source else {
                return None;
            };
            let stored = snapshot
                .relations
                .get(object.relation)?
                .table()?
                .columns
                .get(column)?
                .default
                .clone()?;
            Some(
                object_row(DEFAULT_CONSTRAINT_COLUMNS, object)
                    .set("parent_column_id", int(column as i64 + 1))
                    .set("definition", text(&super::default_definition(&stored)))
                    .set("is_system_named", bit(true))
                    .done(),
            )
        })
        .collect()
}

fn check_constraint_rows(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::Check)
        .filter_map(|object| {
            let ObjectSource::Constraint(at) = object.source else {
                return None;
            };
            let ConstraintSource::Check(check_at) = snapshot.constraints.get(at)?.source else {
                return None;
            };
            let table = snapshot.relations.get(object.relation)?.table()?;
            let check = table.checks.get(check_at)?;
            let used = crate::information_schema::check_columns(table, check_at);
            let parent_column = match used.as_slice() {
                [only] => *only as i64 + 1,
                _ => 0,
            };
            let row = object_row(CHECK_CONSTRAINT_COLUMNS, object)
                .set("parent_column_id", int(parent_column))
                .set(
                    "definition",
                    text(&super::check_definition(&check.expression)),
                )
                .set("uses_database_collation", bit(true));
            Some(
                with_flags(
                    row,
                    &[
                        "is_disabled",
                        "is_not_for_replication",
                        "is_not_trusted",
                        "is_system_named",
                    ],
                    false,
                )
                .done(),
            )
        })
        .collect()
}

fn sequence_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::Sequence)
        .filter_map(|object| {
            let RelationSource::Sequence(sequence) =
                &snapshot.relations.get(object.relation)?.source
            else {
                return None;
            };
            let column_type = super::SqlServerColumnType::of(sequence.sql_type);
            let row = object_row(SEQUENCE_COLUMNS, object)
                .set("start_value", Value::BigInt(sequence.start))
                .set("increment", Value::BigInt(sequence.increment))
                .set("minimum_value", Value::BigInt(sequence.min))
                .set("maximum_value", Value::BigInt(sequence.max))
                .set("is_cycling", bit(sequence.cycle))
                .set("is_cached", bit(sequence.cache > 1))
                .set("cache_size", int(sequence.cache))
                .set("system_type_id", small(column_type.system_type_id()))
                .set("user_type_id", int(column_type.system_type_id()))
                .set("precision", small(column_type.precision))
                .set("scale", small(column_type.scale))
                .set("current_value", Value::BigInt(sequence.last_value))
                .set("is_exhausted", bit(false))
                .set(
                    "last_used_value",
                    if sequence.is_called {
                        Value::BigInt(sequence.last_value)
                    } else {
                        Value::Null
                    },
                );
            Some(row.done())
        })
        .collect()
}

pub(super) fn database_row(snapshot: &CatalogSnapshot) -> Vec<Value> {
    let row = CatalogRow::new(DATABASE_COLUMNS)
        .set("name", text(&snapshot.database))
        .set("database_id", Value::Integer(DATABASE_ID))
        .set("owner_sid", Value::Bytea(vec![1]))
        .set("create_date", UNKNOWN_DATE)
        .set("compatibility_level", small(160))
        .set("collation_name", text(COLLATION))
        .set("user_access", small(0))
        .set("user_access_desc", text("MULTI_USER"))
        .set("state", small(0))
        .set("state_desc", text("ONLINE"))
        .set("snapshot_isolation_state", small(0))
        .set("snapshot_isolation_state_desc", text("OFF"))
        .set("recovery_model", small(3))
        .set("recovery_model_desc", text("SIMPLE"))
        .set("page_verify_option", small(2))
        .set("page_verify_option_desc", text("CHECKSUM"))
        .set("containment", small(0))
        .set("containment_desc", text("NONE"))
        .set("catalog_collation_type", int(0))
        .set("catalog_collation_type_desc", text("DATABASE_DEFAULT"));
    let row = with_flags(
        row,
        &[
            "is_auto_create_stats_on",
            "is_auto_update_stats_on",
            "is_ansi_nulls_on",
            "is_ansi_padding_on",
            "is_ansi_warnings_on",
            "is_arithabort_on",
            "is_concat_null_yields_null_on",
            "is_quoted_identifier_on",
        ],
        true,
    );
    with_flags(
        row,
        &[
            "is_read_only",
            "is_auto_close_on",
            "is_auto_shrink_on",
            "is_in_standby",
            "is_cleanly_shutdown",
            "is_supplemental_logging_enabled",
            "is_read_committed_snapshot_on",
            "is_ansi_null_default_on",
            "is_fulltext_enabled",
            "is_trustworthy_on",
            "is_encrypted",
        ],
        false,
    )
    .done()
}

fn sql_module_rows(snapshot: &CatalogSnapshot, objects: &SqlServerObjects) -> Vec<Vec<Value>> {
    of_kind(objects, ObjectKind::View)
        .filter_map(|object| {
            let RelationSource::View(view) = &snapshot.relations.get(object.relation)?.source
            else {
                return None;
            };
            let definition = format!(
                "CREATE VIEW [{}].[{}] AS {}",
                objects.schema_name(object.schema_id),
                object.name,
                view.query
            );
            let row = CatalogRow::new(SQL_MODULE_COLUMNS)
                .set("object_id", Value::Integer(object.object_id))
                .set("definition", text(&definition))
                .set("uses_ansi_nulls", bit(true))
                .set("uses_quoted_identifier", bit(true));
            Some(
                with_flags(
                    row,
                    &[
                        "is_schema_bound",
                        "uses_database_collation",
                        "is_recompiled",
                        "null_on_null_input",
                        "uses_native_compilation",
                        "inline_type",
                        "is_inlineable",
                    ],
                    false,
                )
                .done(),
            )
        })
        .collect()
}
