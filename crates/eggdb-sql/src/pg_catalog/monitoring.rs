//! The statistics views, extension catalogs and administration functions that PostgreSQL
//! administration tools such as pgAdmin read on every connection and dashboard refresh. EggDB
//! answers them truthfully: one database, one user with every privilege, this session as the
//! only activity, no foreign data, no replication, and as extensions `plpgsql` and the built-in
//! types of `citext` and `hstore`. `pg_stat_statements` and `pg_stat_statements_info` read the
//! process's statement statistics (D286) for this database.

use super::snapshot::{self, CatalogSnapshot};
use super::types::{RegKind, RegValue};
use super::{CatalogColumn, CatalogRow, CatalogType, oids};
use crate::arrays::ArrayElement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::sync::OnceLock;

const OID: SqlType = SqlType::Catalog(CatalogType::Oid);
const NAME: SqlType = SqlType::Catalog(CatalogType::Name);
const CHAR: SqlType = SqlType::Catalog(CatalogType::InternalChar);
const XID: SqlType = SqlType::Catalog(CatalogType::Xid);
const TEXTS: SqlType = SqlType::Array(ArrayElement::Text);
const OIDS: SqlType = SqlType::Array(ArrayElement::Oid);
const ACL_ITEMS: SqlType = SqlType::Array(ArrayElement::AclItem);
const STAMP: SqlType = SqlType::TimestampTz(None);
const PLPGSQL_EXTENSION: u32 = 13_581;
const CITEXT_EXTENSION: u32 = 9_100;
const HSTORE_EXTENSION: u32 = 9_101;
const FIRST_OID: u32 = 12_500;

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

/// A monitoring or extension relation in `pg_catalog`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MonitoringRelation {
    StatActivity,
    StatDatabase,
    Locks,
    PreparedXacts,
    Extension,
    EventTrigger,
    ForeignDataWrapper,
    ForeignServer,
    ForeignTable,
    UserMapping,
    Cast,
    Aggregate,
    Operator,
    User,
    ShDescription,
    ReplicationSlots,
    StatReplication,
    PartitionedTable,
    Subscription,
    StatUserTables,
    StatUserIndexes,
    TsConfig,
    TsDict,
    TsParser,
    TsTemplate,
    Conversion,
    Opfamily,
    Tables,
    Views,
    Matviews,
    Sequences,
    StatStatements,
    StatStatementsInfo,
    Policies,
    Stats,
}

const POLICIES: [CatalogColumn; 8] = [
    column("schemaname", NAME),
    column("tablename", NAME),
    column("policyname", NAME),
    column("permissive", SqlType::Text),
    column("roles", SqlType::Array(ArrayElement::Name)),
    column("cmd", SqlType::Text),
    column("qual", SqlType::Text),
    column("with_check", SqlType::Text),
];

const STATS: [CatalogColumn; 14] = [
    column("schemaname", NAME),
    column("tablename", NAME),
    column("attname", NAME),
    column("inherited", SqlType::Boolean),
    column("null_frac", SqlType::Real),
    column("avg_width", SqlType::Integer),
    column("n_distinct", SqlType::Real),
    column("most_common_vals", TEXTS),
    column("most_common_freqs", SqlType::Array(ArrayElement::Real)),
    column("histogram_bounds", TEXTS),
    column("correlation", SqlType::Real),
    column("most_common_elems", TEXTS),
    column("most_common_elem_freqs", SqlType::Array(ArrayElement::Real)),
    column("elem_count_histogram", SqlType::Array(ArrayElement::Real)),
];

const STAT_STATEMENTS: [CatalogColumn; 12] = [
    column("userid", OID),
    column("dbid", OID),
    column("toplevel", SqlType::Boolean),
    column("queryid", SqlType::BigInt),
    column("query", SqlType::Text),
    column("calls", SqlType::BigInt),
    column("total_exec_time", SqlType::Double),
    column("min_exec_time", SqlType::Double),
    column("max_exec_time", SqlType::Double),
    column("mean_exec_time", SqlType::Double),
    column("stddev_exec_time", SqlType::Double),
    column("rows", SqlType::BigInt),
];

const STAT_STATEMENTS_INFO: [CatalogColumn; 2] = [
    column("dealloc", SqlType::BigInt),
    column("stats_reset", STAMP),
];

const TABLES: [CatalogColumn; 8] = [
    column("schemaname", NAME),
    column("tablename", NAME),
    column("tableowner", NAME),
    column("tablespace", NAME),
    column("hasindexes", SqlType::Boolean),
    column("hasrules", SqlType::Boolean),
    column("hastriggers", SqlType::Boolean),
    column("rowsecurity", SqlType::Boolean),
];

const VIEWS: [CatalogColumn; 4] = [
    column("schemaname", NAME),
    column("viewname", NAME),
    column("viewowner", NAME),
    column("definition", SqlType::Text),
];

const SEQUENCES: [CatalogColumn; 11] = [
    column("schemaname", NAME),
    column("sequencename", NAME),
    column("sequenceowner", NAME),
    column(
        "data_type",
        SqlType::Catalog(CatalogType::Reg(RegKind::Type)),
    ),
    column("start_value", SqlType::BigInt),
    column("min_value", SqlType::BigInt),
    column("max_value", SqlType::BigInt),
    column("increment_by", SqlType::BigInt),
    column("cycle", SqlType::Boolean),
    column("cache_size", SqlType::BigInt),
    column("last_value", SqlType::BigInt),
];

const MATVIEWS: [CatalogColumn; 7] = [
    column("schemaname", NAME),
    column("matviewname", NAME),
    column("matviewowner", NAME),
    column("tablespace", NAME),
    column("hasindexes", SqlType::Boolean),
    column("ispopulated", SqlType::Boolean),
    column("definition", SqlType::Text),
];

const STAT_ACTIVITY: [CatalogColumn; 22] = [
    column("datid", OID),
    column("datname", NAME),
    column("pid", SqlType::Integer),
    column("leader_pid", SqlType::Integer),
    column("usesysid", OID),
    column("usename", NAME),
    column("application_name", SqlType::Text),
    column("client_addr", SqlType::Text),
    column("client_hostname", SqlType::Text),
    column("client_port", SqlType::Integer),
    column("backend_start", STAMP),
    column("xact_start", STAMP),
    column("query_start", STAMP),
    column("state_change", STAMP),
    column("wait_event_type", SqlType::Text),
    column("wait_event", SqlType::Text),
    column("state", SqlType::Text),
    column("backend_xid", XID),
    column("backend_xmin", XID),
    column("query_id", SqlType::BigInt),
    column("query", SqlType::Text),
    column("backend_type", SqlType::Text),
];

const STAT_DATABASE: [CatalogColumn; 19] = [
    column("datid", OID),
    column("datname", NAME),
    column("numbackends", SqlType::Integer),
    column("xact_commit", SqlType::BigInt),
    column("xact_rollback", SqlType::BigInt),
    column("blks_read", SqlType::BigInt),
    column("blks_hit", SqlType::BigInt),
    column("tup_returned", SqlType::BigInt),
    column("tup_fetched", SqlType::BigInt),
    column("tup_inserted", SqlType::BigInt),
    column("tup_updated", SqlType::BigInt),
    column("tup_deleted", SqlType::BigInt),
    column("conflicts", SqlType::BigInt),
    column("temp_files", SqlType::BigInt),
    column("temp_bytes", SqlType::BigInt),
    column("deadlocks", SqlType::BigInt),
    column("blk_read_time", SqlType::Double),
    column("blk_write_time", SqlType::Double),
    column("stats_reset", STAMP),
];

const LOCKS: [CatalogColumn; 16] = [
    column("locktype", SqlType::Text),
    column("database", OID),
    column("relation", OID),
    column("page", SqlType::Integer),
    column("tuple", SqlType::SmallInt),
    column("virtualxid", SqlType::Text),
    column("transactionid", XID),
    column("classid", OID),
    column("objid", OID),
    column("objsubid", SqlType::SmallInt),
    column("virtualtransaction", SqlType::Text),
    column("pid", SqlType::Integer),
    column("mode", SqlType::Text),
    column("granted", SqlType::Boolean),
    column("fastpath", SqlType::Boolean),
    column("waitstart", STAMP),
];

const PREPARED_XACTS: [CatalogColumn; 5] = [
    column("transaction", XID),
    column("gid", SqlType::Text),
    column("prepared", STAMP),
    column("owner", NAME),
    column("database", NAME),
];

const EXTENSION: [CatalogColumn; 8] = [
    column("oid", OID),
    column("extname", NAME),
    column("extowner", OID),
    column("extnamespace", OID),
    column("extrelocatable", SqlType::Boolean),
    column("extversion", SqlType::Text),
    column("extconfig", OIDS),
    column("extcondition", TEXTS),
];

const EVENT_TRIGGER: [CatalogColumn; 7] = [
    column("oid", OID),
    column("evtname", NAME),
    column("evtevent", NAME),
    column("evtowner", OID),
    column("evtfoid", OID),
    column("evtenabled", CHAR),
    column("evttags", TEXTS),
];

const FOREIGN_DATA_WRAPPER: [CatalogColumn; 7] = [
    column("oid", OID),
    column("fdwname", NAME),
    column("fdwowner", OID),
    column("fdwhandler", OID),
    column("fdwvalidator", OID),
    column("fdwacl", ACL_ITEMS),
    column("fdwoptions", TEXTS),
];

const FOREIGN_SERVER: [CatalogColumn; 8] = [
    column("oid", OID),
    column("srvname", NAME),
    column("srvowner", OID),
    column("srvfdw", OID),
    column("srvtype", SqlType::Text),
    column("srvversion", SqlType::Text),
    column("srvacl", ACL_ITEMS),
    column("srvoptions", TEXTS),
];

const FOREIGN_TABLE: [CatalogColumn; 3] = [
    column("ftrelid", OID),
    column("ftserver", OID),
    column("ftoptions", TEXTS),
];

const USER_MAPPING: [CatalogColumn; 4] = [
    column("oid", OID),
    column("umuser", OID),
    column("umserver", OID),
    column("umoptions", TEXTS),
];

const CAST: [CatalogColumn; 6] = [
    column("oid", OID),
    column("castsource", OID),
    column("casttarget", OID),
    column("castfunc", OID),
    column("castcontext", CHAR),
    column("castmethod", CHAR),
];

const AGGREGATE: [CatalogColumn; 6] = [
    column("aggfnoid", OID),
    column("aggkind", CHAR),
    column("aggnumdirectargs", SqlType::SmallInt),
    column("aggtransfn", OID),
    column("aggfinalfn", OID),
    column("aggtranstype", OID),
];

const OPERATOR: [CatalogColumn; 9] = [
    column("oid", OID),
    column("oprname", NAME),
    column("oprnamespace", OID),
    column("oprowner", OID),
    column("oprkind", CHAR),
    column("oprleft", OID),
    column("oprright", OID),
    column("oprresult", OID),
    column("oprcode", OID),
];

const USER: [CatalogColumn; 9] = [
    column("usename", NAME),
    column("usesysid", OID),
    column("usecreatedb", SqlType::Boolean),
    column("usesuper", SqlType::Boolean),
    column("userepl", SqlType::Boolean),
    column("usebypassrls", SqlType::Boolean),
    column("passwd", SqlType::Text),
    column("valuntil", STAMP),
    column("useconfig", TEXTS),
];

const SH_DESCRIPTION: [CatalogColumn; 3] = [
    column("objoid", OID),
    column("classoid", OID),
    column("description", SqlType::Text),
];

const REPLICATION_SLOTS: [CatalogColumn; 8] = [
    column("slot_name", NAME),
    column("plugin", NAME),
    column("slot_type", SqlType::Text),
    column("datoid", OID),
    column("database", NAME),
    column("temporary", SqlType::Boolean),
    column("active", SqlType::Boolean),
    column("active_pid", SqlType::Integer),
];

const STAT_REPLICATION: [CatalogColumn; 6] = [
    column("pid", SqlType::Integer),
    column("usesysid", OID),
    column("usename", NAME),
    column("application_name", SqlType::Text),
    column("client_addr", SqlType::Text),
    column("state", SqlType::Text),
];

const PARTITIONED_TABLE: [CatalogColumn; 3] = [
    column("partrelid", OID),
    column("partstrat", CHAR),
    column("partnatts", SqlType::SmallInt),
];

const SUBSCRIPTION: [CatalogColumn; 16] = [
    column("oid", OID),
    column("subdbid", OID),
    column("subname", NAME),
    column("subowner", OID),
    column("subenabled", SqlType::Boolean),
    column("subbinary", SqlType::Boolean),
    column("substream", CHAR),
    column("subtwophasestate", CHAR),
    column("subdisableonerr", SqlType::Boolean),
    column("subpasswordrequired", SqlType::Boolean),
    column("subrunasowner", SqlType::Boolean),
    column("subconninfo", SqlType::Text),
    column("subslotname", NAME),
    column("subsynccommit", SqlType::Text),
    column("subpublications", TEXTS),
    column("suborigin", SqlType::Text),
];

const STAT_USER_TABLES: [CatalogColumn; 16] = [
    column("relid", OID),
    column("schemaname", NAME),
    column("relname", NAME),
    column("seq_scan", SqlType::BigInt),
    column("seq_tup_read", SqlType::BigInt),
    column("idx_scan", SqlType::BigInt),
    column("idx_tup_fetch", SqlType::BigInt),
    column("n_tup_ins", SqlType::BigInt),
    column("n_tup_upd", SqlType::BigInt),
    column("n_tup_del", SqlType::BigInt),
    column("n_live_tup", SqlType::BigInt),
    column("n_dead_tup", SqlType::BigInt),
    column("last_vacuum", STAMP),
    column("last_autovacuum", STAMP),
    column("last_analyze", STAMP),
    column("last_autoanalyze", STAMP),
];

const STAT_USER_INDEXES: [CatalogColumn; 8] = [
    column("relid", OID),
    column("indexrelid", OID),
    column("schemaname", NAME),
    column("relname", NAME),
    column("indexrelname", NAME),
    column("idx_scan", SqlType::BigInt),
    column("idx_tup_read", SqlType::BigInt),
    column("idx_tup_fetch", SqlType::BigInt),
];

const CONVERSION: [CatalogColumn; 8] = [
    column("oid", OID),
    column("conname", NAME),
    column("connamespace", OID),
    column("conowner", OID),
    column("conforencoding", SqlType::Integer),
    column("contoencoding", SqlType::Integer),
    column("conproc", OID),
    column("condefault", SqlType::Boolean),
];

const TS_CONFIG: [CatalogColumn; 5] = [
    column("oid", OID),
    column("cfgname", NAME),
    column("cfgnamespace", OID),
    column("cfgowner", OID),
    column("cfgparser", OID),
];

const TS_DICT: [CatalogColumn; 5] = [
    column("oid", OID),
    column("dictname", NAME),
    column("dictnamespace", OID),
    column("dictowner", OID),
    column("dicttemplate", OID),
];

const TS_PARSER: [CatalogColumn; 3] = [
    column("oid", OID),
    column("prsname", NAME),
    column("prsnamespace", OID),
];

const TS_TEMPLATE: [CatalogColumn; 3] = [
    column("oid", OID),
    column("tmplname", NAME),
    column("tmplnamespace", OID),
];

const OPFAMILY: [CatalogColumn; 5] = [
    column("oid", OID),
    column("opfmethod", OID),
    column("opfname", NAME),
    column("opfnamespace", OID),
    column("opfowner", OID),
];

impl MonitoringRelation {
    pub(crate) const ALL: [Self; 35] = [
        Self::StatActivity,
        Self::StatDatabase,
        Self::Locks,
        Self::PreparedXacts,
        Self::Extension,
        Self::EventTrigger,
        Self::ForeignDataWrapper,
        Self::ForeignServer,
        Self::ForeignTable,
        Self::UserMapping,
        Self::Cast,
        Self::Aggregate,
        Self::Operator,
        Self::User,
        Self::ShDescription,
        Self::ReplicationSlots,
        Self::StatReplication,
        Self::PartitionedTable,
        Self::Subscription,
        Self::StatUserTables,
        Self::StatUserIndexes,
        Self::TsConfig,
        Self::TsDict,
        Self::TsParser,
        Self::TsTemplate,
        Self::Conversion,
        Self::Opfamily,
        Self::Tables,
        Self::Views,
        Self::Matviews,
        Self::Sequences,
        Self::StatStatements,
        Self::StatStatementsInfo,
        Self::Policies,
        Self::Stats,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::StatActivity => "pg_stat_activity",
            Self::StatDatabase => "pg_stat_database",
            Self::Locks => "pg_locks",
            Self::PreparedXacts => "pg_prepared_xacts",
            Self::Extension => "pg_extension",
            Self::EventTrigger => "pg_event_trigger",
            Self::ForeignDataWrapper => "pg_foreign_data_wrapper",
            Self::ForeignServer => "pg_foreign_server",
            Self::ForeignTable => "pg_foreign_table",
            Self::UserMapping => "pg_user_mapping",
            Self::Cast => "pg_cast",
            Self::Aggregate => "pg_aggregate",
            Self::Operator => "pg_operator",
            Self::User => "pg_user",
            Self::ShDescription => "pg_shdescription",
            Self::ReplicationSlots => "pg_replication_slots",
            Self::StatReplication => "pg_stat_replication",
            Self::PartitionedTable => "pg_partitioned_table",
            Self::Subscription => "pg_subscription",
            Self::StatUserTables => "pg_stat_user_tables",
            Self::StatUserIndexes => "pg_stat_user_indexes",
            Self::TsConfig => "pg_ts_config",
            Self::TsDict => "pg_ts_dict",
            Self::TsParser => "pg_ts_parser",
            Self::TsTemplate => "pg_ts_template",
            Self::Conversion => "pg_conversion",
            Self::Opfamily => "pg_opfamily",
            Self::Tables => "pg_tables",
            Self::Views => "pg_views",
            Self::Matviews => "pg_matviews",
            Self::Sequences => "pg_sequences",
            Self::StatStatements => "pg_stat_statements",
            Self::StatStatementsInfo => "pg_stat_statements_info",
            Self::Policies => "pg_policies",
            Self::Stats => "pg_stats",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|relation| relation.name() == name)
    }

    pub(crate) fn oid(self) -> u32 {
        FIRST_OID
            + Self::ALL
                .iter()
                .position(|relation| *relation == self)
                .unwrap_or(0) as u32
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::StatActivity => &STAT_ACTIVITY,
            Self::StatDatabase => &STAT_DATABASE,
            Self::Locks => &LOCKS,
            Self::PreparedXacts => &PREPARED_XACTS,
            Self::Extension => &EXTENSION,
            Self::EventTrigger => &EVENT_TRIGGER,
            Self::ForeignDataWrapper => &FOREIGN_DATA_WRAPPER,
            Self::ForeignServer => &FOREIGN_SERVER,
            Self::ForeignTable => &FOREIGN_TABLE,
            Self::UserMapping => &USER_MAPPING,
            Self::Cast => &CAST,
            Self::Aggregate => &AGGREGATE,
            Self::Operator => &OPERATOR,
            Self::User => &USER,
            Self::ShDescription => &SH_DESCRIPTION,
            Self::ReplicationSlots => &REPLICATION_SLOTS,
            Self::StatReplication => &STAT_REPLICATION,
            Self::PartitionedTable => &PARTITIONED_TABLE,
            Self::Subscription => &SUBSCRIPTION,
            Self::StatUserTables => &STAT_USER_TABLES,
            Self::StatUserIndexes => &STAT_USER_INDEXES,
            Self::TsConfig => &TS_CONFIG,
            Self::TsDict => &TS_DICT,
            Self::TsParser => &TS_PARSER,
            Self::TsTemplate => &TS_TEMPLATE,
            Self::Conversion => &CONVERSION,
            Self::Opfamily => &OPFAMILY,
            Self::Tables => &TABLES,
            Self::Views => &VIEWS,
            Self::Matviews => &MATVIEWS,
            Self::Sequences => &SEQUENCES,
            Self::StatStatements => &STAT_STATEMENTS,
            Self::StatStatementsInfo => &STAT_STATEMENTS_INFO,
            Self::Policies => &POLICIES,
            Self::Stats => &STATS,
        }
    }

    pub(crate) fn is_view(self) -> bool {
        matches!(
            self,
            Self::StatActivity
                | Self::StatDatabase
                | Self::Locks
                | Self::PreparedXacts
                | Self::User
                | Self::ReplicationSlots
                | Self::StatReplication
                | Self::StatUserTables
                | Self::Tables
                | Self::Views
                | Self::Matviews
                | Self::Sequences
                | Self::StatUserIndexes
                | Self::StatStatements
                | Self::StatStatementsInfo
                | Self::Policies
                | Self::Stats
        )
    }
}

fn now() -> Value {
    Value::TimestampTz(crate::datetime::statement_now())
}

fn started_at() -> &'static Value {
    static STARTED: OnceLock<Value> = OnceLock::new();
    STARTED.get_or_init(now)
}

fn session_user_name() -> String {
    snapshot::session_user().unwrap_or_else(|| "eggdb".to_owned())
}

/// Each installed extension's OID and the description `pg_description` holds for it.
pub(crate) fn extension_descriptions() -> [(u32, &'static str); 3] {
    [
        (PLPGSQL_EXTENSION, "PL/pgSQL procedural language"),
        (
            CITEXT_EXTENSION,
            "data type for case-insensitive character strings",
        ),
        (
            HSTORE_EXTENSION,
            "data type for storing sets of (key, value) pairs",
        ),
    ]
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: MonitoringRelation) -> Vec<Vec<Value>> {
    let row = || CatalogRow::new(relation.columns());
    match relation {
        MonitoringRelation::StatActivity if !crate::session_activity::sessions().is_empty() => {
            crate::session_activity::sessions()
                .into_iter()
                .map(|session| {
                    let (address, port) = match session.client.rsplit_once(':') {
                        Some((address, port)) => (
                            address.trim_matches(['[', ']']).to_owned(),
                            port.parse::<i32>().ok(),
                        ),
                        None => (session.client.clone(), None),
                    };
                    let mut built = row()
                        .set(
                            "datid",
                            Value::Oid(if session.database == snapshot.database {
                                oids::DATABASE
                            } else {
                                0
                            }),
                        )
                        .set("datname", Value::Text(session.database))
                        .set("pid", Value::Integer(session.pid))
                        .set("usename", Value::Text(session.user))
                        .set("application_name", Value::Text(session.application))
                        .set("client_addr", Value::Text(address))
                        .set("backend_start", Value::TimestampTz(session.backend_start))
                        .set("state_change", Value::TimestampTz(session.state_change))
                        .set("state", Value::Text(session.state.to_owned()))
                        .set("query", Value::Text(session.query))
                        .set("backend_type", Value::Text("client backend".to_owned()));
                    if let Some(port) = port {
                        built = built.set("client_port", Value::Integer(port));
                    }
                    if let Some(start) = session.query_start {
                        built = built.set("query_start", Value::TimestampTz(start));
                    }
                    built.done()
                })
                .collect()
        }
        MonitoringRelation::StatActivity => vec![
            row()
                .set("datid", Value::Oid(oids::DATABASE))
                .set("datname", Value::Text(snapshot.database.clone()))
                .set(
                    "pid",
                    Value::Integer(i32::try_from(std::process::id()).unwrap_or(i32::MAX)),
                )
                .set("usesysid", Value::Oid(oids::SESSION_USER))
                .set("usename", Value::Text(session_user_name()))
                .set("application_name", Value::Text(String::new()))
                .set("backend_start", started_at().clone())
                .set("query_start", now())
                .set("state_change", now())
                .set("state", Value::Text("active".to_owned()))
                .set("backend_type", Value::Text("client backend".to_owned()))
                .done(),
        ],
        MonitoringRelation::StatDatabase => {
            let mut built = row()
                .set("datid", Value::Oid(oids::DATABASE))
                .set("datname", Value::Text(snapshot.database.clone()))
                .set(
                    "numbackends",
                    Value::Integer(
                        i32::try_from(
                            crate::session_activity::sessions()
                                .iter()
                                .filter(|session| session.database == snapshot.database)
                                .count()
                                .max(1),
                        )
                        .unwrap_or(i32::MAX),
                    ),
                )
                .set("blk_read_time", Value::Double(0.0))
                .set("blk_write_time", Value::Double(0.0));
            for counter in [
                "xact_commit",
                "xact_rollback",
                "blks_read",
                "blks_hit",
                "tup_returned",
                "tup_fetched",
                "tup_inserted",
                "tup_updated",
                "tup_deleted",
                "conflicts",
                "temp_files",
                "temp_bytes",
                "deadlocks",
            ] {
                built = built.set(counter, Value::BigInt(0));
            }
            vec![built.done()]
        }
        MonitoringRelation::Extension => [
            (PLPGSQL_EXTENSION, "plpgsql", oids::PG_CATALOG, false, "1.0"),
            (CITEXT_EXTENSION, "citext", oids::PUBLIC, true, "1.6"),
            (HSTORE_EXTENSION, "hstore", oids::PUBLIC, true, "1.8"),
        ]
        .into_iter()
        .map(|(oid, name, namespace, relocatable, version)| {
            row()
                .set("oid", Value::Oid(oid))
                .set("extname", Value::Text(name.to_owned()))
                .set("extowner", Value::Oid(oids::SESSION_USER))
                .set("extnamespace", Value::Oid(namespace))
                .set("extrelocatable", Value::Boolean(relocatable))
                .set("extversion", Value::Text(version.to_owned()))
                .done()
        })
        .collect(),
        MonitoringRelation::User => vec![
            row()
                .set("usename", Value::Text(session_user_name()))
                .set("usesysid", Value::Oid(oids::SESSION_USER))
                .set("usecreatedb", Value::Boolean(true))
                .set("usesuper", Value::Boolean(true))
                .set("userepl", Value::Boolean(false))
                .set("usebypassrls", Value::Boolean(true))
                .set("passwd", Value::Text("********".to_owned()))
                .done(),
        ],
        MonitoringRelation::PartitionedTable => super::relations::partitioned_table_rows(snapshot)
            .into_iter()
            .map(|(relation, strategy, columns)| {
                row()
                    .set("partrelid", Value::Oid(relation))
                    .set("partstrat", Value::Text(strategy.to_owned()))
                    .set(
                        "partnatts",
                        Value::SmallInt(i16::try_from(columns).unwrap_or(i16::MAX)),
                    )
                    .done()
            })
            .collect(),
        MonitoringRelation::Tables => table_rows(snapshot),
        MonitoringRelation::Stats => statistics_rows(snapshot),
        MonitoringRelation::Views => view_rows(snapshot),
        MonitoringRelation::Sequences => sequence_rows(snapshot),
        MonitoringRelation::StatStatements => statement_rows(snapshot),
        MonitoringRelation::StatStatementsInfo => {
            let info = crate::statement_statistics::info();
            vec![
                row()
                    .set(
                        "dealloc",
                        Value::BigInt(i64::try_from(info.deallocations).unwrap_or(i64::MAX)),
                    )
                    .set("stats_reset", Value::TimestampTz(info.reset_at))
                    .done(),
            ]
        }
        _ => Vec::new(),
    }
}

const STATISTICS_READER_ROLES: [&str; 2] = ["pg_read_all_stats", "pg_monitor"];
const HIDDEN_STATEMENT: &str = "<insufficient privilege>";

fn belongs_to_any(roles: &[crate::roles::StoredRole], user: &str, wanted: &[&str]) -> bool {
    let mut held: Vec<&str> = vec![user];
    let mut at = 0;
    while let Some(&name) = held.get(at) {
        if wanted.contains(&name) {
            return true;
        }
        for role in roles.iter().filter(|role| role.name == name) {
            for parent in &role.member_of {
                if !held.contains(&parent.as_str()) {
                    held.push(parent);
                }
            }
        }
        at += 1;
    }
    false
}

fn sees_every_statement(snapshot: &CatalogSnapshot) -> bool {
    snapshot
        .settings
        .checked_role
        .as_deref()
        .is_none_or(|role| belongs_to_any(&snapshot.stored_roles, role, &STATISTICS_READER_ROLES))
}

fn statement_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let count = |count: u64| Value::BigInt(i64::try_from(count).unwrap_or(i64::MAX));
    let sees_every_statement = sees_every_statement(snapshot);
    crate::statement_statistics::statistics()
        .into_iter()
        .filter(|entry| entry.database == snapshot.database)
        .map(|entry| {
            let own = entry.user == snapshot.user;
            let user = if own {
                oids::SESSION_USER
            } else {
                crate::roles::role_oid(&entry.user)
            };
            let (query_id, query) = if own || sees_every_statement {
                (Value::BigInt(entry.query_id), Value::Text(entry.query))
            } else {
                (Value::Null, Value::Text(HIDDEN_STATEMENT.to_owned()))
            };
            CatalogRow::new(&STAT_STATEMENTS)
                .set("userid", Value::Oid(user))
                .set("dbid", Value::Oid(oids::DATABASE))
                .set("toplevel", Value::Boolean(true))
                .set("queryid", query_id)
                .set("query", query)
                .set("calls", count(entry.calls))
                .set("total_exec_time", Value::Double(entry.total_ms))
                .set("min_exec_time", Value::Double(entry.min_ms))
                .set("max_exec_time", Value::Double(entry.max_ms))
                .set("mean_exec_time", Value::Double(entry.mean_ms))
                .set("stddev_exec_time", Value::Double(entry.stddev_ms))
                .set("rows", count(entry.rows))
                .done()
        })
        .collect()
}

fn reset_statement_statistics(values: &[Value]) -> SqlResult<Value> {
    if values.len() > 3 {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "pg_stat_statements_reset takes a user OID, a database OID and a query ID, all optional",
        ));
    }
    match values.get(2).and_then(Value::as_integer) {
        Some(query_id) if query_id != 0 => crate::statement_statistics::reset_query(query_id),
        _ => crate::statement_statistics::reset(),
    }
    Ok(Value::Text(String::new()))
}

fn table_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .enumerate()
        .filter_map(|(at, entry)| {
            let table = entry.table()?;
            let has_indexes = !table.primary_key.is_empty() || !table.indexes.is_empty();
            let has_triggers = snapshot
                .triggers
                .iter()
                .any(|trigger| trigger.relation == at);
            Some(
                CatalogRow::new(&TABLES)
                    .set("schemaname", Value::Text(entry.schema.clone()))
                    .set("tablename", Value::Text(entry.name.clone()))
                    .set("tableowner", Value::Text(session_user_name()))
                    .set("hasindexes", Value::Boolean(has_indexes))
                    .set("hasrules", Value::Boolean(false))
                    .set("hastriggers", Value::Boolean(has_triggers))
                    .set(
                        "rowsecurity",
                        Value::Boolean(
                            crate::row_security::switch_state(&snapshot.access, &entry.internal)
                                .is_ok_and(|switch| switch.enabled),
                        ),
                    )
                    .done(),
            )
        })
        .collect()
}

fn statistics_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let texts = |items: &[String]| {
        if items.is_empty() {
            return Value::Null;
        }
        Value::Array(Box::new(crate::arrays::ArrayValue::new(
            ArrayElement::Text,
            items.iter().cloned().map(Value::Text).collect(),
        )))
    };
    let mut rows = Vec::new();
    for entry in &snapshot.relations {
        let Some(table) = entry.table() else {
            continue;
        };
        let Some(columns) = snapshot.column_statistics.get(&table.name) else {
            continue;
        };
        for column in &table.columns {
            let Some(statistics) = columns.iter().find(|found| found.column == column.name) else {
                continue;
            };
            let frequencies = if statistics.most_common_frequencies.is_empty() {
                Value::Null
            } else {
                Value::Array(Box::new(crate::arrays::ArrayValue::new(
                    ArrayElement::Real,
                    statistics
                        .most_common_frequencies
                        .iter()
                        .map(|frequency| Value::Real(*frequency))
                        .collect(),
                )))
            };
            rows.push(
                CatalogRow::new(&STATS)
                    .set("schemaname", Value::Text(entry.schema.clone()))
                    .set("tablename", Value::Text(entry.name.clone()))
                    .set("attname", Value::Text(column.name.clone()))
                    .set("inherited", Value::Boolean(false))
                    .set("null_frac", Value::Real(statistics.null_fraction))
                    .set("avg_width", Value::Integer(statistics.average_width))
                    .set("n_distinct", Value::Real(statistics.distinct))
                    .set("most_common_vals", texts(&statistics.most_common_values))
                    .set("most_common_freqs", frequencies)
                    .set("histogram_bounds", texts(&statistics.histogram_bounds))
                    .set(
                        "correlation",
                        statistics.correlation.map_or(Value::Null, Value::Real),
                    )
                    .done(),
            );
        }
    }
    rows
}

fn view_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| match &entry.source {
            super::snapshot::RelationSource::View(view) => Some(
                CatalogRow::new(&VIEWS)
                    .set("schemaname", Value::Text(entry.schema.clone()))
                    .set("viewname", Value::Text(entry.name.clone()))
                    .set("viewowner", Value::Text(session_user_name()))
                    .set(
                        "definition",
                        view.shown_query()
                            .map_or(Value::Null, |query| Value::Text(query.to_owned())),
                    )
                    .done(),
            ),
            _ => None,
        })
        .collect()
}

fn sequence_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| match &entry.source {
            super::snapshot::RelationSource::Sequence(sequence) => {
                let type_oid = oids::type_oid(sequence.sql_type);
                Some(
                    CatalogRow::new(&SEQUENCES)
                        .set("schemaname", Value::Text(entry.schema.clone()))
                        .set("sequencename", Value::Text(entry.name.clone()))
                        .set("sequenceowner", Value::Text(session_user_name()))
                        .set(
                            "data_type",
                            Value::Reg(Box::new(RegValue {
                                kind: RegKind::Type,
                                oid: type_oid,
                                name: oids::format_type(type_oid, None),
                            })),
                        )
                        .set("start_value", Value::BigInt(sequence.start))
                        .set("min_value", Value::BigInt(sequence.min))
                        .set("max_value", Value::BigInt(sequence.max))
                        .set("increment_by", Value::BigInt(sequence.increment))
                        .set("cycle", Value::Boolean(sequence.cycle))
                        .set("cache_size", Value::BigInt(sequence.cache))
                        .set(
                            "last_value",
                            if sequence.is_called {
                                Value::BigInt(sequence.last_value)
                            } else {
                                Value::Null
                            },
                        )
                        .done(),
                )
            }
            _ => None,
        })
        .collect()
}

/// The result type of an administration function, if `name` is one.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "has_database_privilege"
        | "has_schema_privilege"
        | "has_table_privilege"
        | "has_sequence_privilege"
        | "has_function_privilege"
        | "has_column_privilege"
        | "has_tablespace_privilege"
        | "has_type_privilege"
        | "has_foreign_data_wrapper_privilege"
        | "has_server_privilege"
        | "has_parameter_privilege"
        | "has_any_column_privilege"
        | "pg_has_role"
        | "pg_is_in_recovery"
        | "pg_is_wal_replay_paused"
        | "pg_cancel_backend"
        | "pg_terminate_backend" => SqlType::Boolean,
        "pg_backend_pid" => SqlType::Integer,
        "pg_stat_statements_reset" => SqlType::VOID,
        "pg_postmaster_start_time" | "pg_conf_load_time" => STAMP,
        "pg_size_pretty" | "pg_tablespace_location" | "pg_current_wal_lsn" => SqlType::Text,
        "pg_database_size"
        | "pg_total_relation_size"
        | "pg_relation_size"
        | "pg_table_size"
        | "pg_indexes_size"
        | "pg_tablespace_size" => SqlType::BigInt,
        _ => return None,
    })
}

fn half_rounded(size: i64) -> i64 {
    (size + if size < 0 { -1 } else { 1 }) / 2
}

fn size_pretty(bytes: i64) -> String {
    const LIMIT: u64 = 10 * 1024;
    const DOUBLE_LIMIT: u64 = LIMIT * 2 - 1;
    if bytes.unsigned_abs() < LIMIT {
        return format!("{bytes} bytes");
    }
    let mut size = bytes >> 9;
    for unit in ["kB", "MB", "GB", "TB"] {
        if size.unsigned_abs() < DOUBLE_LIMIT {
            return format!("{} {unit}", half_rounded(size));
        }
        size >>= 10;
    }
    format!("{} PB", half_rounded(size))
}

/// The value of an administration function, if `name` is one.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let result_type = result_type(name)?;
    if name == "pg_stat_statements_reset" {
        return Some(reset_statement_statistics(values));
    }
    if values.iter().any(Value::is_null) && name != "pg_size_pretty" {
        return Some(Ok(Value::Null));
    }
    if super::privilege_inquiry::is_inquiry(name) {
        return Some(super::privilege_inquiry::function(name, values));
    }
    Some(match name {
        "pg_cancel_backend" | "pg_terminate_backend" => {
            let Some(pid) = values.first().and_then(Value::as_integer) else {
                return Some(Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("{name} takes a process id"),
                )));
            };
            let pid = i32::try_from(pid).unwrap_or(i32::MAX);
            crate::session_activity::signal(
                pid,
                name == "pg_terminate_backend",
                &session_user_name(),
                snapshot::session_is_superuser(),
            )
            .map(Value::Boolean)
        }
        "pg_is_in_recovery" | "pg_is_wal_replay_paused" => Ok(Value::Boolean(false)),
        "pg_backend_pid" => Ok(Value::Integer(
            i32::try_from(std::process::id()).unwrap_or(i32::MAX),
        )),
        "pg_postmaster_start_time" | "pg_conf_load_time" => Ok(started_at().clone()),
        "pg_tablespace_location" => Ok(Value::Text(String::new())),
        "pg_current_wal_lsn" => Ok(Value::Text("0/0".to_owned())),
        "pg_size_pretty" => match values {
            [Value::Null] => Ok(Value::Null),
            [value] => match value.as_integer() {
                Some(bytes) => Ok(Value::Text(size_pretty(bytes))),
                None => Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "pg_size_pretty takes a bigint",
                )),
            },
            _ => Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                "pg_size_pretty takes one argument",
            )),
        },
        _ if result_type == SqlType::BigInt => Ok(Value::BigInt(0)),
        _ => Ok(Value::Boolean(true)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str, member_of: &[&str]) -> crate::roles::StoredRole {
        crate::roles::StoredRole {
            name: name.to_owned(),
            login: true,
            superuser: false,
            create_database: false,
            scram_verifier: None,
            member_of: member_of
                .iter()
                .map(|parent| (*parent).to_owned())
                .collect(),
        }
    }

    #[test]
    fn statistics_readers_are_found_through_nested_and_circular_memberships() {
        let roles = [
            role("alice", &["staff"]),
            role("staff", &["pg_monitor", "alice"]),
            role("bob", &["clerks"]),
            role("clerks", &["bob"]),
        ];
        assert!(belongs_to_any(&roles, "alice", &STATISTICS_READER_ROLES));
        assert!(belongs_to_any(&roles, "staff", &STATISTICS_READER_ROLES));
        assert!(!belongs_to_any(&roles, "bob", &STATISTICS_READER_ROLES));
        assert!(!belongs_to_any(&roles, "nobody", &STATISTICS_READER_ROLES));
        assert!(belongs_to_any(
            &roles,
            "pg_read_all_stats",
            &STATISTICS_READER_ROLES
        ));
    }

    #[test]
    fn sizes_print_like_pg_size_pretty() {
        assert_eq!(size_pretty(0), "0 bytes");
        assert_eq!(size_pretty(10_239), "10239 bytes");
        assert_eq!(size_pretty(10_240), "10 kB");
        assert_eq!(size_pretty(1_048_576 * 15), "15 MB");
        assert_eq!(size_pretty(-20_480), "-20 kB");
    }

    #[test]
    fn every_relation_has_a_unique_name_and_oid() {
        let mut names: Vec<&str> = MonitoringRelation::ALL
            .iter()
            .map(|relation| relation.name())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), MonitoringRelation::ALL.len());
        let mut oids: Vec<u32> = MonitoringRelation::ALL
            .iter()
            .map(|relation| relation.oid())
            .collect();
        oids.sort_unstable();
        oids.dedup();
        assert_eq!(oids.len(), MonitoringRelation::ALL.len());
    }

    #[test]
    fn privilege_inquiries_need_the_catalog_and_unknowns_are_none() {
        assert_eq!(
            function(
                "has_tablespace_privilege",
                &[
                    Value::Text("pg_default".to_owned()),
                    Value::Text("CREATE".to_owned())
                ]
            )
            .map(|answer| answer.map_err(|error| error.state())),
            Some(Err(SqlState::FEATURE_NOT_SUPPORTED))
        );
        assert!(function("not_a_function", &[]).is_none());
        assert_eq!(
            function("pg_is_in_recovery", &[]).map(Result::ok),
            Some(Some(Value::Boolean(false)))
        );
    }
}
