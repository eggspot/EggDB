//! The statistics views, extension catalogs and administration functions that PostgreSQL
//! administration tools such as pgAdmin read on every connection and dashboard refresh. EggDB
//! answers them truthfully: one database, one user with every privilege, this session as the
//! only activity, no foreign data, no replication, no extensions but `plpgsql`.

use super::snapshot::{self, CatalogSnapshot};
use super::{CatalogColumn, CatalogRow, CatalogType, oids};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{ArrayElement, SqlType, Value};
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
}

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

const SUBSCRIPTION: [CatalogColumn; 5] = [
    column("oid", OID),
    column("subdbid", OID),
    column("subname", NAME),
    column("subowner", OID),
    column("subenabled", SqlType::Boolean),
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
    pub(crate) const ALL: [Self; 27] = [
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
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|relation| relation.name() == name)
    }

    pub(crate) fn oid(self) -> u32 {
        FIRST_OID + Self::ALL.iter().position(|relation| *relation == self).unwrap_or(0) as u32
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
                | Self::StatUserIndexes
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

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: MonitoringRelation) -> Vec<Vec<Value>> {
    let row = || CatalogRow::new(relation.columns());
    match relation {
        MonitoringRelation::StatActivity => vec![
            row()
                .set("datid", Value::Oid(oids::DATABASE))
                .set("datname", Value::Text(snapshot.database.clone()))
                .set("pid", Value::Integer(i32::try_from(std::process::id()).unwrap_or(i32::MAX)))
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
                .set("numbackends", Value::Integer(1))
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
        MonitoringRelation::Extension => vec![
            row()
                .set("oid", Value::Oid(PLPGSQL_EXTENSION))
                .set("extname", Value::Text("plpgsql".to_owned()))
                .set("extowner", Value::Oid(oids::SESSION_USER))
                .set("extnamespace", Value::Oid(oids::PG_CATALOG))
                .set("extrelocatable", Value::Boolean(false))
                .set("extversion", Value::Text("1.0".to_owned()))
                .done(),
        ],
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
        _ => Vec::new(),
    }
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
        | "has_any_column_privilege"
        | "pg_has_role"
        | "pg_is_in_recovery"
        | "pg_is_wal_replay_paused" => SqlType::Boolean,
        "pg_backend_pid" => SqlType::Integer,
        "pg_postmaster_start_time" | "pg_conf_load_time" => STAMP,
        "pg_size_pretty" | "pg_tablespace_location" | "pg_current_wal_lsn" => SqlType::Text,
        "pg_database_size" | "pg_total_relation_size" | "pg_relation_size" | "pg_table_size"
        | "pg_indexes_size" | "pg_tablespace_size" => SqlType::BigInt,
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
    if values.iter().any(Value::is_null) && name != "pg_size_pretty" {
        return Some(Ok(Value::Null));
    }
    Some(match name {
        "pg_is_in_recovery" | "pg_is_wal_replay_paused" => Ok(Value::Boolean(false)),
        "pg_backend_pid" => Ok(Value::Integer(i32::try_from(std::process::id()).unwrap_or(i32::MAX))),
        "pg_postmaster_start_time" | "pg_conf_load_time" => Ok(started_at().clone()),
        "pg_tablespace_location" => Ok(Value::Text(String::new())),
        "pg_current_wal_lsn" => Ok(Value::Text("0/0".to_owned())),
        "pg_size_pretty" => match values {
            [Value::Null] => Ok(Value::Null),
            [value] => match value.as_integer() {
                Some(bytes) => Ok(Value::Text(size_pretty(bytes))),
                None => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "pg_size_pretty takes a bigint")),
            },
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "pg_size_pretty takes one argument")),
        },
        _ if result_type == SqlType::BigInt => Ok(Value::BigInt(0)),
        _ => Ok(Value::Boolean(true)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut names: Vec<&str> = MonitoringRelation::ALL.iter().map(|relation| relation.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), MonitoringRelation::ALL.len());
        let mut oids: Vec<u32> = MonitoringRelation::ALL.iter().map(|relation| relation.oid()).collect();
        oids.sort_unstable();
        oids.dedup();
        assert_eq!(oids.len(), MonitoringRelation::ALL.len());
    }

    #[test]
    fn privileges_are_all_granted_and_unknowns_are_none() {
        assert_eq!(
            function("has_table_privilege", &[Value::Text("t".to_owned()), Value::Text("SELECT".to_owned())]).map(Result::ok),
            Some(Some(Value::Boolean(true)))
        );
        assert!(function("not_a_function", &[]).is_none());
        assert_eq!(function("pg_is_in_recovery", &[]).map(Result::ok), Some(Some(Value::Boolean(false))));
    }
}
