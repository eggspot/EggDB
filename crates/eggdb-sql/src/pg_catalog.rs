mod deparse;
mod functions;
mod monitoring;
mod oids;
mod relations;
mod routines;
mod snapshot;
mod types;

#[cfg(test)]
mod deparse_tests;
#[cfg(test)]
mod routine_tests;
#[cfg(test)]
mod tests;

use crate::arrays::ArrayElement;
use crate::types::SqlType;

use crate::information_schema::InformationRelation;
use crate::oracle_dictionary::OracleRelation;
use crate::sqlserver_catalog::SqlServerRelation;
pub(crate) use deparse::{check_text, default_text};
pub(crate) use monitoring::{function as monitoring_function, result_type as monitoring_result_type};
pub(crate) use functions::{
    argument_types, function, partition_ancestors_rows, partition_ancestors_type, result_type,
};
pub(crate) use oids::{OidRequest, Space as OidSpace, assign as assign_oids};
pub(crate) use oids::{builtin_type, builtin_type_named, format_type, type_modifier};
pub(crate) use relations::generated_rows;
pub(crate) use relations::{index_columns, index_is_unique, index_sort_options};
pub(crate) use snapshot::{
    CatalogSnapshot, ConstraintSource, IndexSource, PackageEntry, RelationEntry, RelationKind,
    RelationSource, RoutineEntry, TriggerEntry, current as current_snapshot, enter_statement, load_for_stored_expressions, load_if_wanted,
    mentions_catalog, names_non_sequence_relation, routine_oid, snapshot_for, statement_settings,
    session_user, text_needs_catalog,
};
pub use types::{CatalogType, RegKind, RegValue};

/// The type OID PostgreSQL gives `sql_type`, such as 23 for `integer`, 1007 for `integer[]`
/// and 2205 for `regclass`.
pub fn type_oid(sql_type: SqlType) -> u32 {
    oids::type_oid(sql_type)
}
pub(crate) fn operator_operand_type_name(sql_type: SqlType) -> String {
    match sql_type {
        SqlType::Bpchar(_) => "character".to_owned(),
        other => oids::format_type(oids::type_oid(other), Some(-1)),
    }
}

pub(crate) use types::{
    cast_type_named, check_expression_tree_argument, convert, convert_from, oid_number,
    parse_input, read_only, reg_value, system_catalog,
};

pub(crate) fn catalog_reference(
    reader: &dyn crate::store::KeyValueRead,
    name: &str,
) -> crate::error::SqlResult<Option<CatalogRelation>> {
    match crate::constraints::current_dialect() {
        crate::Dialect::PostgreSql => postgresql_catalog_reference(reader, name),
        crate::Dialect::SqlServer => {
            Ok(SqlServerRelation::referenced(name).map(CatalogRelation::SqlServer))
        }
        crate::Dialect::Oracle => {
            Ok(crate::oracle_dictionary::reference(reader, name)?.map(CatalogRelation::Oracle))
        }
        crate::Dialect::MySql => Ok(crate::mysql_catalog::MySqlRelation::referenced(name).map(CatalogRelation::MySql)),
        crate::Dialect::Sqlite => Ok(None),
    }
}

fn postgresql_catalog_reference(
    reader: &dyn crate::store::KeyValueRead,
    name: &str,
) -> crate::error::SqlResult<Option<CatalogRelation>> {
    if let Some((schema, base)) = name.split_once(crate::schemas::QUALIFIER) {
        return Ok(CatalogRelation::named_in(schema, base));
    }
    let in_catalog = CatalogRelation::named_in(crate::schemas::CATALOG_SCHEMA, name);
    let in_information = InformationRelation::named(name);
    if in_catalog.is_none() && in_information.is_none() {
        return Ok(None);
    }
    let path = crate::schemas::effective_path(reader)?;
    let (relation, schema) = match (in_catalog, in_information) {
        (Some(relation), _) => (relation, crate::schemas::CATALOG_SCHEMA),
        (None, Some(relation))
            if path
                .iter()
                .any(|entry| entry == crate::schemas::INFORMATION_SCHEMA) =>
        {
            (
                CatalogRelation::Information(relation),
                crate::schemas::INFORMATION_SCHEMA,
            )
        }
        _ => return Ok(None),
    };
    if let Some(catalog_at) = path.iter().position(|entry| entry == schema) {
        for schema in &path[..catalog_at] {
            let candidate = crate::schemas::qualified(schema, name);
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

pub(crate) fn refuse_catalog_change(
    reader: &dyn crate::store::KeyValueRead,
    name: &crate::ast::Name,
) -> crate::error::SqlResult<()> {
    if crate::constraints::current_dialect() == crate::Dialect::Oracle {
        return Ok(());
    }
    match catalog_reference(reader, &name.text)? {
        Some(relation) => Err(system_catalog(relation.name())),
        None => Ok(()),
    }
}

pub(crate) fn copy_query(
    relation: CatalogRelation,
    columns: &[crate::ast::Name],
) -> crate::error::SqlResult<crate::ast::Query> {
    let list = if columns.is_empty() {
        "*".to_owned()
    } else {
        columns
            .iter()
            .map(|column| crate::pg_keywords::quote_ident(&column.text))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let text = format!(
        "SELECT {list} FROM {}.{}",
        relation.schema(),
        crate::pg_keywords::quote_ident(relation.name())
    );
    let tokens = crate::lexer::tokenize(&text)?;
    let mut parser =
        crate::parser::Parser::new(&text, &tokens).in_dialect(crate::Dialect::PostgreSql);
    let crate::ast::Statement::Select(select) = parser.statement()? else {
        return Err(crate::error::SqlError::new(
            crate::error::SqlState::INTERNAL_ERROR,
            "the query of a COPY of a catalog relation is not a SELECT",
        ));
    };
    Ok(crate::ast::Query {
        with: Vec::new(),
        recursive: false,
        body: crate::ast::SetExpr::Select(Box::new(select)),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        position: 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CatalogRelation {
    Class,
    Attribute,
    Type,
    Namespace,
    Index,
    Constraint,
    AttrDef,
    Description,
    Database,
    Roles,
    Am,
    Collation,
    Sequence,
    Depend,
    Tablespace,
    Inherits,
    Policy,
    StatisticExt,
    Publication,
    PublicationRel,
    PublicationNamespace,
    Trigger,
    Rewrite,
    AuthMembers,
    Settings,
    TimezoneNames,
    Opclass,
    Enum,
    Proc,
    Language,
    Range,
    Information(InformationRelation),
    SqlServer(SqlServerRelation),
    Oracle(OracleRelation),
    Monitoring(monitoring::MonitoringRelation),
    MySql(crate::mysql_catalog::MySqlRelation),
}

pub(crate) struct CatalogColumn {
    pub(crate) name: &'static str,
    pub(crate) sql_type: SqlType,
}

pub(crate) struct CatalogRow {
    columns: &'static [CatalogColumn],
    values: Vec<crate::types::Value>,
}

impl CatalogRow {
    pub(crate) fn new(columns: &'static [CatalogColumn]) -> Self {
        Self {
            columns,
            values: vec![crate::types::Value::Null; columns.len()],
        }
    }

    pub(crate) fn set(mut self, name: &str, value: crate::types::Value) -> Self {
        if let Some(at) = self.columns.iter().position(|column| column.name == name)
            && let Some(slot) = self.values.get_mut(at)
        {
            *slot = value;
        }
        self
    }

    pub(crate) fn done(self) -> Vec<crate::types::Value> {
        self.values
    }
}

const fn column(name: &'static str, sql_type: SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

const OID: SqlType = SqlType::Catalog(CatalogType::Oid);
const NAME: SqlType = SqlType::Catalog(CatalogType::Name);
const CHAR: SqlType = SqlType::Catalog(CatalogType::InternalChar);
const NODE_TREE: SqlType = SqlType::Catalog(CatalogType::NodeTree);
const REGPROC: SqlType = SqlType::Catalog(CatalogType::Reg(RegKind::Proc));
const XID: SqlType = SqlType::Catalog(CatalogType::Xid);
const ANY_ARRAY: SqlType = SqlType::Catalog(CatalogType::AnyArray);
const ACL_ITEMS: SqlType = SqlType::Array(ArrayElement::AclItem);
const TEXTS: SqlType = SqlType::Array(ArrayElement::Text);
const OIDS: SqlType = SqlType::Array(ArrayElement::Oid);
const CHARS: SqlType = SqlType::Array(ArrayElement::InternalChar);
const SMALLINTS: SqlType = SqlType::Array(ArrayElement::SmallInt);
const INT2_VECTOR: SqlType = SqlType::Array(ArrayElement::Int2Vector);
const OID_VECTOR: SqlType = SqlType::Array(ArrayElement::OidVector);

impl CatalogRelation {
    pub(crate) const IN_PG_CATALOG: [Self; 31] = [
        Self::Class,
        Self::Attribute,
        Self::Type,
        Self::Namespace,
        Self::Index,
        Self::Constraint,
        Self::AttrDef,
        Self::Description,
        Self::Database,
        Self::Roles,
        Self::Am,
        Self::Collation,
        Self::Sequence,
        Self::Depend,
        Self::Tablespace,
        Self::Inherits,
        Self::Policy,
        Self::StatisticExt,
        Self::Publication,
        Self::PublicationRel,
        Self::PublicationNamespace,
        Self::Trigger,
        Self::Rewrite,
        Self::AuthMembers,
        Self::Settings,
        Self::TimezoneNames,
        Self::Opclass,
        Self::Enum,
        Self::Proc,
        Self::Language,
        Self::Range,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Class => "pg_class",
            Self::Attribute => "pg_attribute",
            Self::Type => "pg_type",
            Self::Namespace => "pg_namespace",
            Self::Index => "pg_index",
            Self::Constraint => "pg_constraint",
            Self::AttrDef => "pg_attrdef",
            Self::Description => "pg_description",
            Self::Database => "pg_database",
            Self::Roles => "pg_roles",
            Self::Am => "pg_am",
            Self::Collation => "pg_collation",
            Self::Sequence => "pg_sequence",
            Self::Depend => "pg_depend",
            Self::Tablespace => "pg_tablespace",
            Self::Inherits => "pg_inherits",
            Self::Policy => "pg_policy",
            Self::StatisticExt => "pg_statistic_ext",
            Self::Publication => "pg_publication",
            Self::PublicationRel => "pg_publication_rel",
            Self::PublicationNamespace => "pg_publication_namespace",
            Self::Trigger => "pg_trigger",
            Self::Rewrite => "pg_rewrite",
            Self::AuthMembers => "pg_auth_members",
            Self::Settings => "pg_settings",
            Self::TimezoneNames => "pg_timezone_names",
            Self::Opclass => "pg_opclass",
            Self::Enum => "pg_enum",
            Self::Proc => "pg_proc",
            Self::Language => "pg_language",
            Self::Range => "pg_range",
            Self::Information(relation) => relation.name(),
            Self::Monitoring(relation) => relation.name(),
            Self::MySql(relation) => relation.name(),
            Self::SqlServer(relation) => relation.name(),
            Self::Oracle(relation) => relation.name(),
        }
    }

    pub(crate) fn schema(self) -> &'static str {
        match self {
            Self::Information(_) => crate::schemas::INFORMATION_SCHEMA,
            Self::SqlServer(relation) => relation.schema(),
            Self::Oracle(relation) => relation.schema(),
            Self::MySql(_) => crate::schemas::INFORMATION_SCHEMA,
            _ => crate::schemas::CATALOG_SCHEMA,
        }
    }

    pub(crate) fn every() -> impl Iterator<Item = Self> {
        Self::IN_PG_CATALOG
            .into_iter()
            .chain(InformationRelation::ALL.into_iter().map(Self::Information))
            .chain(monitoring::MonitoringRelation::ALL.into_iter().map(Self::Monitoring))
    }

    pub(crate) fn oid(self) -> u32 {
        match self {
            Self::Class => 1259,
            Self::Attribute => 1249,
            Self::Type => 1247,
            Self::Namespace => 2615,
            Self::Index => 2610,
            Self::Constraint => 2606,
            Self::AttrDef => 2604,
            Self::Description => 2609,
            Self::Database => 1262,
            Self::Roles => 12000,
            Self::Am => 2601,
            Self::Collation => 3456,
            Self::Sequence => 2224,
            Self::Depend => 2608,
            Self::Tablespace => 1213,
            Self::Inherits => 2611,
            Self::Policy => 3256,
            Self::StatisticExt => 3381,
            Self::Publication => 6104,
            Self::PublicationRel => 6106,
            Self::PublicationNamespace => 6237,
            Self::Trigger => 2620,
            Self::Rewrite => 2618,
            Self::AuthMembers => 1261,
            Self::Settings => 12001,
            Self::TimezoneNames => 12002,
            Self::Opclass => 2616,
            Self::Enum => 3501,
            Self::Proc => 1255,
            Self::Language => 2612,
            Self::Range => 3541,
            Self::Information(relation) => relation.oid(),
            Self::Monitoring(relation) => relation.oid(),
            Self::MySql(relation) => relation.oid(),
            Self::SqlServer(_) | Self::Oracle(_) => 0,
        }
    }

    pub(crate) fn is_view(self) -> bool {
        if let Self::Monitoring(relation) = self {
            return relation.is_view();
        }
        matches!(
            self,
            Self::Roles
                | Self::Settings
                | Self::TimezoneNames
                | Self::Information(_)
                | Self::MySql(_)
                | Self::SqlServer(_)
                | Self::Oracle(_)
        )
    }

    pub(crate) fn named_in(schema: &str, name: &str) -> Option<Self> {
        if schema == crate::schemas::INFORMATION_SCHEMA {
            return InformationRelation::named(name).map(Self::Information);
        }
        if schema != crate::schemas::CATALOG_SCHEMA {
            return None;
        }
        Self::IN_PG_CATALOG
            .into_iter()
            .find(|relation| relation.name() == name)
            .or_else(|| monitoring::MonitoringRelation::named(name).map(Self::Monitoring))
    }

    pub(crate) fn column_named(self, name: &str) -> Option<usize> {
        self.columns().iter().position(|column| column.name == name)
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::Tablespace => &TABLESPACE_COLUMNS,
            Self::Type => &TYPE_COLUMNS,
            Self::Attribute => &ATTRIBUTE_COLUMNS,
            Self::Class => &CLASS_COLUMNS,
            Self::AuthMembers => &AUTH_MEMBERS_COLUMNS,
            Self::Database => &DATABASE_COLUMNS,
            Self::Sequence => &SEQUENCE_COLUMNS,
            Self::Am => &AM_COLUMNS,
            Self::AttrDef => &ATTRDEF_COLUMNS,
            Self::Constraint => &CONSTRAINT_COLUMNS,
            Self::Depend => &DEPEND_COLUMNS,
            Self::Description => &DESCRIPTION_COLUMNS,
            Self::Index => &INDEX_COLUMNS,
            Self::Inherits => &INHERITS_COLUMNS,
            Self::Namespace => &NAMESPACE_COLUMNS,
            Self::Rewrite => &REWRITE_COLUMNS,
            Self::Trigger => &TRIGGER_COLUMNS,
            Self::Policy => &POLICY_COLUMNS,
            Self::StatisticExt => &STATISTIC_EXT_COLUMNS,
            Self::Collation => &COLLATION_COLUMNS,
            Self::Publication => &PUBLICATION_COLUMNS,
            Self::PublicationRel => &PUBLICATION_REL_COLUMNS,
            Self::PublicationNamespace => &PUBLICATION_NAMESPACE_COLUMNS,
            Self::Roles => &ROLES_COLUMNS,
            Self::Settings => &SETTINGS_COLUMNS,
            Self::TimezoneNames => &TIMEZONE_NAMES_COLUMNS,
            Self::Opclass => &OPCLASS_COLUMNS,
            Self::Enum => &ENUM_COLUMNS,
            Self::Proc => &PROC_COLUMNS,
            Self::Language => &LANGUAGE_COLUMNS,
            Self::Range => &RANGE_COLUMNS,
            Self::Information(relation) => relation.columns(),
            Self::Monitoring(relation) => relation.columns(),
            Self::MySql(relation) => relation.columns(),
            Self::SqlServer(relation) => relation.columns(),
            Self::Oracle(relation) => relation.columns(),
        }
    }
}

const TABLESPACE_COLUMNS: [CatalogColumn; 5] = [
    column("oid", OID),
    column("spcname", NAME),
    column("spcowner", OID),
    column("spcacl", ACL_ITEMS),
    column("spcoptions", TEXTS),
];

const TYPE_COLUMNS: [CatalogColumn; 32] = [
    column("oid", OID),
    column("typname", NAME),
    column("typnamespace", OID),
    column("typowner", OID),
    column("typlen", SqlType::SmallInt),
    column("typbyval", SqlType::Boolean),
    column("typtype", CHAR),
    column("typcategory", CHAR),
    column("typispreferred", SqlType::Boolean),
    column("typisdefined", SqlType::Boolean),
    column("typdelim", CHAR),
    column("typrelid", OID),
    column("typsubscript", REGPROC),
    column("typelem", OID),
    column("typarray", OID),
    column("typinput", REGPROC),
    column("typoutput", REGPROC),
    column("typreceive", REGPROC),
    column("typsend", REGPROC),
    column("typmodin", REGPROC),
    column("typmodout", REGPROC),
    column("typanalyze", REGPROC),
    column("typalign", CHAR),
    column("typstorage", CHAR),
    column("typnotnull", SqlType::Boolean),
    column("typbasetype", OID),
    column("typtypmod", SqlType::Integer),
    column("typndims", SqlType::Integer),
    column("typcollation", OID),
    column("typdefaultbin", NODE_TREE),
    column("typdefault", SqlType::Text),
    column("typacl", ACL_ITEMS),
];

const ATTRIBUTE_COLUMNS: [CatalogColumn; 26] = [
    column("attrelid", OID),
    column("attname", NAME),
    column("atttypid", OID),
    column("attlen", SqlType::SmallInt),
    column("attnum", SqlType::SmallInt),
    column("attcacheoff", SqlType::Integer),
    column("atttypmod", SqlType::Integer),
    column("attndims", SqlType::SmallInt),
    column("attbyval", SqlType::Boolean),
    column("attalign", CHAR),
    column("attstorage", CHAR),
    column("attcompression", CHAR),
    column("attnotnull", SqlType::Boolean),
    column("atthasdef", SqlType::Boolean),
    column("atthasmissing", SqlType::Boolean),
    column("attidentity", CHAR),
    column("attgenerated", CHAR),
    column("attisdropped", SqlType::Boolean),
    column("attislocal", SqlType::Boolean),
    column("attinhcount", SqlType::SmallInt),
    column("attstattarget", SqlType::SmallInt),
    column("attcollation", OID),
    column("attacl", ACL_ITEMS),
    column("attoptions", TEXTS),
    column("attfdwoptions", TEXTS),
    column("attmissingval", ANY_ARRAY),
];

const CLASS_COLUMNS: [CatalogColumn; 33] = [
    column("oid", OID),
    column("relname", NAME),
    column("relnamespace", OID),
    column("reltype", OID),
    column("reloftype", OID),
    column("relowner", OID),
    column("relam", OID),
    column("relfilenode", OID),
    column("reltablespace", OID),
    column("relpages", SqlType::Integer),
    column("reltuples", SqlType::Real),
    column("relallvisible", SqlType::Integer),
    column("reltoastrelid", OID),
    column("relhasindex", SqlType::Boolean),
    column("relisshared", SqlType::Boolean),
    column("relpersistence", CHAR),
    column("relkind", CHAR),
    column("relnatts", SqlType::SmallInt),
    column("relchecks", SqlType::SmallInt),
    column("relhasrules", SqlType::Boolean),
    column("relhastriggers", SqlType::Boolean),
    column("relhassubclass", SqlType::Boolean),
    column("relrowsecurity", SqlType::Boolean),
    column("relforcerowsecurity", SqlType::Boolean),
    column("relispopulated", SqlType::Boolean),
    column("relreplident", CHAR),
    column("relispartition", SqlType::Boolean),
    column("relrewrite", OID),
    column("relfrozenxid", XID),
    column("relminmxid", XID),
    column("relacl", ACL_ITEMS),
    column("reloptions", TEXTS),
    column("relpartbound", NODE_TREE),
];

const AUTH_MEMBERS_COLUMNS: [CatalogColumn; 7] = [
    column("oid", OID),
    column("roleid", OID),
    column("member", OID),
    column("grantor", OID),
    column("admin_option", SqlType::Boolean),
    column("inherit_option", SqlType::Boolean),
    column("set_option", SqlType::Boolean),
];

const DATABASE_COLUMNS: [CatalogColumn; 17] = [
    column("oid", OID),
    column("datname", NAME),
    column("datdba", OID),
    column("encoding", SqlType::Integer),
    column("datlocprovider", CHAR),
    column("datistemplate", SqlType::Boolean),
    column("datallowconn", SqlType::Boolean),
    column("datconnlimit", SqlType::Integer),
    column("datfrozenxid", XID),
    column("datminmxid", XID),
    column("dattablespace", OID),
    column("datcollate", SqlType::Text),
    column("datctype", SqlType::Text),
    column("daticulocale", SqlType::Text),
    column("daticurules", SqlType::Text),
    column("datcollversion", SqlType::Text),
    column("datacl", ACL_ITEMS),
];

const SEQUENCE_COLUMNS: [CatalogColumn; 8] = [
    column("seqrelid", OID),
    column("seqtypid", OID),
    column("seqstart", SqlType::BigInt),
    column("seqincrement", SqlType::BigInt),
    column("seqmax", SqlType::BigInt),
    column("seqmin", SqlType::BigInt),
    column("seqcache", SqlType::BigInt),
    column("seqcycle", SqlType::Boolean),
];

const AM_COLUMNS: [CatalogColumn; 4] = [
    column("oid", OID),
    column("amname", NAME),
    column("amhandler", REGPROC),
    column("amtype", CHAR),
];

const ATTRDEF_COLUMNS: [CatalogColumn; 4] = [
    column("oid", OID),
    column("adrelid", OID),
    column("adnum", SqlType::SmallInt),
    column("adbin", NODE_TREE),
];

const CONSTRAINT_COLUMNS: [CatalogColumn; 26] = [
    column("oid", OID),
    column("conname", NAME),
    column("connamespace", OID),
    column("contype", CHAR),
    column("condeferrable", SqlType::Boolean),
    column("condeferred", SqlType::Boolean),
    column("convalidated", SqlType::Boolean),
    column("conrelid", OID),
    column("contypid", OID),
    column("conindid", OID),
    column("conparentid", OID),
    column("confrelid", OID),
    column("confupdtype", CHAR),
    column("confdeltype", CHAR),
    column("confmatchtype", CHAR),
    column("conislocal", SqlType::Boolean),
    column("coninhcount", SqlType::SmallInt),
    column("connoinherit", SqlType::Boolean),
    column("conkey", SMALLINTS),
    column("confkey", SMALLINTS),
    column("conpfeqop", OIDS),
    column("conppeqop", OIDS),
    column("conffeqop", OIDS),
    column("confdelsetcols", SMALLINTS),
    column("conexclop", OIDS),
    column("conbin", NODE_TREE),
];

const DEPEND_COLUMNS: [CatalogColumn; 7] = [
    column("classid", OID),
    column("objid", OID),
    column("objsubid", SqlType::Integer),
    column("refclassid", OID),
    column("refobjid", OID),
    column("refobjsubid", SqlType::Integer),
    column("deptype", CHAR),
];

const DESCRIPTION_COLUMNS: [CatalogColumn; 4] = [
    column("objoid", OID),
    column("classoid", OID),
    column("objsubid", SqlType::Integer),
    column("description", SqlType::Text),
];

const INDEX_COLUMNS: [CatalogColumn; 21] = [
    column("indexrelid", OID),
    column("indrelid", OID),
    column("indnatts", SqlType::SmallInt),
    column("indnkeyatts", SqlType::SmallInt),
    column("indisunique", SqlType::Boolean),
    column("indnullsnotdistinct", SqlType::Boolean),
    column("indisprimary", SqlType::Boolean),
    column("indisexclusion", SqlType::Boolean),
    column("indimmediate", SqlType::Boolean),
    column("indisclustered", SqlType::Boolean),
    column("indisvalid", SqlType::Boolean),
    column("indcheckxmin", SqlType::Boolean),
    column("indisready", SqlType::Boolean),
    column("indislive", SqlType::Boolean),
    column("indisreplident", SqlType::Boolean),
    column("indkey", INT2_VECTOR),
    column("indcollation", OID_VECTOR),
    column("indclass", OID_VECTOR),
    column("indoption", INT2_VECTOR),
    column("indexprs", NODE_TREE),
    column("indpred", NODE_TREE),
];

const INHERITS_COLUMNS: [CatalogColumn; 4] = [
    column("inhrelid", OID),
    column("inhparent", OID),
    column("inhseqno", SqlType::Integer),
    column("inhdetachpending", SqlType::Boolean),
];

const NAMESPACE_COLUMNS: [CatalogColumn; 4] = [
    column("oid", OID),
    column("nspname", NAME),
    column("nspowner", OID),
    column("nspacl", ACL_ITEMS),
];

const REWRITE_COLUMNS: [CatalogColumn; 8] = [
    column("oid", OID),
    column("rulename", NAME),
    column("ev_class", OID),
    column("ev_type", CHAR),
    column("ev_enabled", CHAR),
    column("is_instead", SqlType::Boolean),
    column("ev_qual", NODE_TREE),
    column("ev_action", NODE_TREE),
];

const TRIGGER_COLUMNS: [CatalogColumn; 19] = [
    column("oid", OID),
    column("tgrelid", OID),
    column("tgparentid", OID),
    column("tgname", NAME),
    column("tgfoid", OID),
    column("tgtype", SqlType::SmallInt),
    column("tgenabled", CHAR),
    column("tgisinternal", SqlType::Boolean),
    column("tgconstrrelid", OID),
    column("tgconstrindid", OID),
    column("tgconstraint", OID),
    column("tgdeferrable", SqlType::Boolean),
    column("tginitdeferred", SqlType::Boolean),
    column("tgnargs", SqlType::SmallInt),
    column("tgattr", INT2_VECTOR),
    column("tgargs", SqlType::Bytea),
    column("tgqual", NODE_TREE),
    column("tgoldtable", NAME),
    column("tgnewtable", NAME),
];

const POLICY_COLUMNS: [CatalogColumn; 8] = [
    column("oid", OID),
    column("polname", NAME),
    column("polrelid", OID),
    column("polcmd", CHAR),
    column("polpermissive", SqlType::Boolean),
    column("polroles", OIDS),
    column("polqual", NODE_TREE),
    column("polwithcheck", NODE_TREE),
];

const STATISTIC_EXT_COLUMNS: [CatalogColumn; 9] = [
    column("oid", OID),
    column("stxrelid", OID),
    column("stxname", NAME),
    column("stxnamespace", OID),
    column("stxowner", OID),
    column("stxstattarget", SqlType::Integer),
    column("stxkeys", INT2_VECTOR),
    column("stxkind", CHARS),
    column("stxexprs", NODE_TREE),
];

const COLLATION_COLUMNS: [CatalogColumn; 12] = [
    column("oid", OID),
    column("collname", NAME),
    column("collnamespace", OID),
    column("collowner", OID),
    column("collprovider", CHAR),
    column("collisdeterministic", SqlType::Boolean),
    column("collencoding", SqlType::Integer),
    column("collcollate", SqlType::Text),
    column("collctype", SqlType::Text),
    column("colliculocale", SqlType::Text),
    column("collicurules", SqlType::Text),
    column("collversion", SqlType::Text),
];

const PUBLICATION_COLUMNS: [CatalogColumn; 9] = [
    column("oid", OID),
    column("pubname", NAME),
    column("pubowner", OID),
    column("puballtables", SqlType::Boolean),
    column("pubinsert", SqlType::Boolean),
    column("pubupdate", SqlType::Boolean),
    column("pubdelete", SqlType::Boolean),
    column("pubtruncate", SqlType::Boolean),
    column("pubviaroot", SqlType::Boolean),
];

const PUBLICATION_REL_COLUMNS: [CatalogColumn; 5] = [
    column("oid", OID),
    column("prpubid", OID),
    column("prrelid", OID),
    column("prqual", NODE_TREE),
    column("prattrs", INT2_VECTOR),
];

const PUBLICATION_NAMESPACE_COLUMNS: [CatalogColumn; 3] = [
    column("oid", OID),
    column("pnpubid", OID),
    column("pnnspid", OID),
];

const ROLES_COLUMNS: [CatalogColumn; 13] = [
    column("rolname", NAME),
    column("rolsuper", SqlType::Boolean),
    column("rolinherit", SqlType::Boolean),
    column("rolcreaterole", SqlType::Boolean),
    column("rolcreatedb", SqlType::Boolean),
    column("rolcanlogin", SqlType::Boolean),
    column("rolreplication", SqlType::Boolean),
    column("rolconnlimit", SqlType::Integer),
    column("rolpassword", SqlType::Text),
    column("rolvaliduntil", SqlType::TimestampTz(None)),
    column("rolbypassrls", SqlType::Boolean),
    column("rolconfig", TEXTS),
    column("oid", OID),
];

const TIMEZONE_NAMES_COLUMNS: [CatalogColumn; 4] = [
    column("name", SqlType::Text),
    column("abbrev", SqlType::Text),
    column(
        "utc_offset",
        SqlType::Interval {
            fields: None,
            precision: None,
        },
    ),
    column("is_dst", SqlType::Boolean),
];

const SETTINGS_COLUMNS: [CatalogColumn; 17] = [
    column("name", SqlType::Text),
    column("setting", SqlType::Text),
    column("unit", SqlType::Text),
    column("category", SqlType::Text),
    column("short_desc", SqlType::Text),
    column("extra_desc", SqlType::Text),
    column("context", SqlType::Text),
    column("vartype", SqlType::Text),
    column("source", SqlType::Text),
    column("min_val", SqlType::Text),
    column("max_val", SqlType::Text),
    column("enumvals", TEXTS),
    column("boot_val", SqlType::Text),
    column("reset_val", SqlType::Text),
    column("sourcefile", SqlType::Text),
    column("sourceline", SqlType::Integer),
    column("pending_restart", SqlType::Boolean),
];

const OPCLASS_COLUMNS: [CatalogColumn; 9] = [
    column("oid", OID),
    column("opcmethod", OID),
    column("opcname", NAME),
    column("opcnamespace", OID),
    column("opcowner", OID),
    column("opcfamily", OID),
    column("opcintype", OID),
    column("opcdefault", SqlType::Boolean),
    column("opckeytype", OID),
];

const ENUM_COLUMNS: [CatalogColumn; 4] = [
    column("oid", OID),
    column("enumtypid", OID),
    column("enumsortorder", SqlType::Real),
    column("enumlabel", NAME),
];

const PROC_COLUMNS: [CatalogColumn; 30] = [
    column("oid", OID),
    column("proname", NAME),
    column("pronamespace", OID),
    column("proowner", OID),
    column("prolang", OID),
    column("procost", SqlType::Real),
    column("prorows", SqlType::Real),
    column("provariadic", OID),
    column("prosupport", REGPROC),
    column("prokind", CHAR),
    column("prosecdef", SqlType::Boolean),
    column("proleakproof", SqlType::Boolean),
    column("proisstrict", SqlType::Boolean),
    column("proretset", SqlType::Boolean),
    column("provolatile", CHAR),
    column("proparallel", CHAR),
    column("pronargs", SqlType::SmallInt),
    column("pronargdefaults", SqlType::SmallInt),
    column("prorettype", OID),
    column("proargtypes", OID_VECTOR),
    column("proallargtypes", OIDS),
    column("proargmodes", CHARS),
    column("proargnames", TEXTS),
    column("proargdefaults", NODE_TREE),
    column("protrftypes", OIDS),
    column("prosrc", SqlType::Text),
    column("probin", SqlType::Text),
    column("prosqlbody", NODE_TREE),
    column("proconfig", TEXTS),
    column("proacl", ACL_ITEMS),
];

const LANGUAGE_COLUMNS: [CatalogColumn; 9] = [
    column("oid", OID),
    column("lanname", NAME),
    column("lanowner", OID),
    column("lanispl", SqlType::Boolean),
    column("lanpltrusted", SqlType::Boolean),
    column("lanplcallfoid", OID),
    column("laninline", OID),
    column("lanvalidator", OID),
    column("lanacl", ACL_ITEMS),
];

const RANGE_COLUMNS: [CatalogColumn; 7] = [
    column("rngtypid", OID),
    column("rngsubtype", OID),
    column("rngmultitypid", OID),
    column("rngcollation", OID),
    column("rngsubopc", OID),
    column("rngcanonical", REGPROC),
    column("rngsubdiff", REGPROC),
];
