//! The server-level `sys` views SQL Server Management Studio reads when it connects and fills
//! Object Explorer: configuration, sessions, principals, files and file groups, partitions and
//! statistics, and the catalogs of features EggDB does not have (answered empty), so its queries
//! run unchanged.

use super::{CatalogRow, bit, int, text};
use crate::pg_catalog::{CatalogColumn, CatalogSnapshot};
use crate::types::{SqlType, Value};

const SYSNAME: SqlType = SqlType::Varchar(Some(128));
const DESCRIPTION: SqlType = SqlType::Varchar(Some(255));
const BIT: SqlType = SqlType::Boolean;
const INT: SqlType = SqlType::Integer;
const BIGINT: SqlType = SqlType::BigInt;
const STAMP: SqlType = SqlType::Timestamp(None);
const DATABASE_ID: i64 = 5;
const SESSION_ID: i64 = 51;
const PRIMARY_FILEGROUP: i64 = 1;

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

/// A server-level `sys` view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ServerView {
    Configurations,
    ExecSessions,
    ServerPrincipals,
    DatabasePrincipals,
    Filegroups,
    DataSpaces,
    DatabaseFiles,
    MasterFiles,
    Partitions,
    Stats,
    ComputedColumns,
    XmlIndexes,
    FulltextIndexes,
    ChangeTrackingTables,
    ChangeTrackingDatabases,
    Periods,
    Synonyms,
    AssemblyTypes,
    TableTypes,
}

const CONFIGURATIONS: &[CatalogColumn] = &[
    column("configuration_id", INT),
    column("name", SYSNAME),
    column("value", INT),
    column("minimum", INT),
    column("maximum", INT),
    column("value_in_use", INT),
    column("description", DESCRIPTION),
    column("is_dynamic", BIT),
    column("is_advanced", BIT),
];

const EXEC_SESSIONS: &[CatalogColumn] = &[
    column("session_id", INT),
    column("login_time", STAMP),
    column("host_name", SYSNAME),
    column("program_name", SYSNAME),
    column("login_name", SYSNAME),
    column("status", SYSNAME),
    column("database_id", INT),
    column("is_user_process", BIT),
];

const PRINCIPALS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("principal_id", INT),
    column("sid", SqlType::Bytea),
    column("type", SqlType::Varchar(Some(1))),
    column("type_desc", SYSNAME),
    column("is_disabled", BIT),
    column("create_date", STAMP),
    column("modify_date", STAMP),
    column("default_database_name", SYSNAME),
    column("default_schema_name", SYSNAME),
    column("is_fixed_role", BIT),
];

const DATA_SPACES: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("data_space_id", INT),
    column("type", SqlType::Varchar(Some(2))),
    column("type_desc", SYSNAME),
    column("is_default", BIT),
    column("is_system", BIT),
    column("is_read_only", BIT),
];

const FILES: &[CatalogColumn] = &[
    column("database_id", INT),
    column("file_id", INT),
    column("type", SqlType::SmallInt),
    column("type_desc", SYSNAME),
    column("data_space_id", INT),
    column("name", SYSNAME),
    column("physical_name", SqlType::Varchar(Some(260))),
    column("state", SqlType::SmallInt),
    column("state_desc", SYSNAME),
    column("size", INT),
    column("max_size", INT),
    column("growth", INT),
    column("is_percent_growth", BIT),
    column("is_read_only", BIT),
];

const PARTITIONS: &[CatalogColumn] = &[
    column("partition_id", BIGINT),
    column("object_id", INT),
    column("index_id", INT),
    column("partition_number", INT),
    column("rows", BIGINT),
    column("data_compression", SqlType::SmallInt),
    column("data_compression_desc", SYSNAME),
];

const STATS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("stats_id", INT),
    column("auto_created", BIT),
    column("user_created", BIT),
    column("no_recompute", BIT),
];

const NAMED_OBJECT_COLUMNS: &[CatalogColumn] = &[
    column("object_id", INT),
    column("name", SYSNAME),
    column("column_id", INT),
    column("definition", SqlType::Text),
];

const CHANGE_TRACKING_TABLES: &[CatalogColumn] = &[
    column("object_id", INT),
    column("is_track_columns_updated_on", BIT),
    column("min_valid_version", BIGINT),
];

const CHANGE_TRACKING_DATABASES: &[CatalogColumn] = &[
    column("database_id", INT),
    column("is_auto_cleanup_on", BIT),
    column("retention_period", INT),
    column("retention_period_units_desc", SYSNAME),
];

const TYPE_COLUMNS: &[CatalogColumn] = &[
    column("name", SYSNAME),
    column("user_type_id", INT),
    column("schema_id", INT),
    column("is_table_type", BIT),
];

impl ServerView {
    pub(crate) const ALL: [Self; 19] = [
        Self::Configurations,
        Self::ExecSessions,
        Self::ServerPrincipals,
        Self::DatabasePrincipals,
        Self::Filegroups,
        Self::DataSpaces,
        Self::DatabaseFiles,
        Self::MasterFiles,
        Self::Partitions,
        Self::Stats,
        Self::ComputedColumns,
        Self::XmlIndexes,
        Self::FulltextIndexes,
        Self::ChangeTrackingTables,
        Self::ChangeTrackingDatabases,
        Self::Periods,
        Self::Synonyms,
        Self::AssemblyTypes,
        Self::TableTypes,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Configurations => "configurations",
            Self::ExecSessions => "dm_exec_sessions",
            Self::ServerPrincipals => "server_principals",
            Self::DatabasePrincipals => "database_principals",
            Self::Filegroups => "filegroups",
            Self::DataSpaces => "data_spaces",
            Self::DatabaseFiles => "database_files",
            Self::MasterFiles => "master_files",
            Self::Partitions => "partitions",
            Self::Stats => "stats",
            Self::ComputedColumns => "computed_columns",
            Self::XmlIndexes => "xml_indexes",
            Self::FulltextIndexes => "fulltext_indexes",
            Self::ChangeTrackingTables => "change_tracking_tables",
            Self::ChangeTrackingDatabases => "change_tracking_databases",
            Self::Periods => "periods",
            Self::Synonyms => "synonyms",
            Self::AssemblyTypes => "assembly_types",
            Self::TableTypes => "table_types",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.name().eq_ignore_ascii_case(name))
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Configurations => CONFIGURATIONS,
            Self::ExecSessions => EXEC_SESSIONS,
            Self::ServerPrincipals | Self::DatabasePrincipals => PRINCIPALS,
            Self::Filegroups | Self::DataSpaces => DATA_SPACES,
            Self::DatabaseFiles | Self::MasterFiles => FILES,
            Self::Partitions => PARTITIONS,
            Self::Stats => STATS,
            Self::ComputedColumns | Self::XmlIndexes | Self::FulltextIndexes | Self::Periods | Self::Synonyms => {
                NAMED_OBJECT_COLUMNS
            }
            Self::ChangeTrackingTables => CHANGE_TRACKING_TABLES,
            Self::ChangeTrackingDatabases => CHANGE_TRACKING_DATABASES,
            Self::AssemblyTypes | Self::TableTypes => TYPE_COLUMNS,
        }
    }
}

fn principal(columns: &'static [CatalogColumn], name: &str, id: i64, kind: &str, kind_desc: &str, fixed_role: bool, database: &str) -> Vec<Value> {
    CatalogRow::new(columns)
        .set("name", text(name))
        .set("principal_id", int(id))
        .set("type", text(kind))
        .set("type_desc", text(kind_desc))
        .set("is_disabled", bit(false))
        .set("default_database_name", text(database))
        .set("default_schema_name", text("dbo"))
        .set("is_fixed_role", bit(fixed_role))
        .done()
}

fn file(columns: &'static [CatalogColumn], snapshot: &CatalogSnapshot, file_id: i64, log: bool) -> Vec<Value> {
    CatalogRow::new(columns)
        .set("database_id", int(DATABASE_ID))
        .set("file_id", int(file_id))
        .set("type", Value::SmallInt(i16::from(log)))
        .set("type_desc", text(if log { "LOG" } else { "ROWS" }))
        .set("data_space_id", int(if log { 0 } else { PRIMARY_FILEGROUP }))
        .set("name", text(&format!("{}{}", snapshot.database, if log { "_log" } else { "" })))
        .set("physical_name", text(&format!("{}.egg", snapshot.database)))
        .set("state", Value::SmallInt(0))
        .set("state_desc", text("ONLINE"))
        .set("size", int(128))
        .set("max_size", int(-1))
        .set("growth", int(8192))
        .set("is_percent_growth", bit(false))
        .set("is_read_only", bit(false))
        .done()
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, view: ServerView) -> Vec<Vec<Value>> {
    let columns = view.columns();
    let user = crate::pg_catalog::session_user().unwrap_or_else(|| "eggdb".to_owned());
    match view {
        ServerView::Configurations => [
            (1544, "max server memory (MB)", 2_147_483_647, "Maximum size of server memory (MB)", true, true),
            (1543, "min server memory (MB)", 0, "Minimum size of server memory (MB)", true, true),
            (518, "show advanced options", 0, "show advanced options", true, false),
            (1539, "max degree of parallelism", 0, "maximum degree of parallelism", true, true),
            (16_387, "xp_cmdshell", 0, "Enable or disable command shell", true, true),
            (1568, "default trace enabled", 0, "Enable or disable the default trace", true, true),
        ]
        .into_iter()
        .map(|(id, name, value, description, dynamic, advanced)| {
            CatalogRow::new(columns)
                .set("configuration_id", int(id))
                .set("name", text(name))
                .set("value", int(value))
                .set("minimum", int(0))
                .set("maximum", int(2_147_483_647))
                .set("value_in_use", int(value))
                .set("description", text(description))
                .set("is_dynamic", bit(dynamic))
                .set("is_advanced", bit(advanced))
                .done()
        })
        .collect(),
        ServerView::ExecSessions => vec![
            CatalogRow::new(columns)
                .set("session_id", int(SESSION_ID))
                .set("host_name", text("localhost"))
                .set("program_name", text(""))
                .set("login_name", text(&user))
                .set("status", text("running"))
                .set("database_id", int(DATABASE_ID))
                .set("is_user_process", bit(true))
                .done(),
        ],
        ServerView::ServerPrincipals => vec![
            principal(columns, &user, 1, "S", "SQL_LOGIN", false, &snapshot.database),
            principal(columns, "sysadmin", 3, "R", "SERVER_ROLE", true, "master"),
            principal(columns, "public", 2, "R", "SERVER_ROLE", true, "master"),
        ],
        ServerView::DatabasePrincipals => vec![
            principal(columns, "dbo", 1, "S", "SQL_USER", false, &snapshot.database),
            principal(columns, "public", 0, "R", "DATABASE_ROLE", false, &snapshot.database),
            principal(columns, "db_owner", 16_384, "R", "DATABASE_ROLE", true, &snapshot.database),
        ],
        ServerView::Filegroups | ServerView::DataSpaces => vec![
            CatalogRow::new(columns)
                .set("name", text("PRIMARY"))
                .set("data_space_id", int(PRIMARY_FILEGROUP))
                .set("type", text("FG"))
                .set("type_desc", text("ROWS_FILEGROUP"))
                .set("is_default", bit(true))
                .set("is_system", bit(false))
                .set("is_read_only", bit(false))
                .done(),
        ],
        ServerView::DatabaseFiles | ServerView::MasterFiles => {
            vec![file(columns, snapshot, 1, false), file(columns, snapshot, 2, true)]
        }
        ServerView::ChangeTrackingDatabases
        | ServerView::ChangeTrackingTables
        | ServerView::Partitions
        | ServerView::Stats
        | ServerView::ComputedColumns
        | ServerView::XmlIndexes
        | ServerView::FulltextIndexes
        | ServerView::Periods
        | ServerView::Synonyms
        | ServerView::AssemblyTypes
        | ServerView::TableTypes => Vec::new(),
    }
}
