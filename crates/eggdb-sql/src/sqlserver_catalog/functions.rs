use super::types::{named_system_type, system_type_with_id};
use super::{
    COLLATION, DATABASE_ID, DBO, DBO_SCHEMA_ID, ObjectKind, SqlServerObject, SqlServerObjects,
    relation_columns,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};
use crate::pg_catalog::{CatalogSnapshot, ConstraintSource};
use crate::types::{SqlType, Value};
use std::ops::RangeInclusive;

const DATABASE_VERSION: i32 = 957;
const FIXED_SERVER_ROLES: [&str; 9] = [
    "sysadmin",
    "serveradmin",
    "securityadmin",
    "processadmin",
    "setupadmin",
    "bulkadmin",
    "diskadmin",
    "dbcreator",
    "public",
];
const FIXED_DATABASE_ROLES: [&str; 10] = [
    "db_owner",
    "db_accessadmin",
    "db_securityadmin",
    "db_ddladmin",
    "db_backupoperator",
    "db_datareader",
    "db_datawriter",
    "db_denydatareader",
    "db_denydatawriter",
    "public",
];

/// The SQL Server metadata functions of the Transact-SQL dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataFunction {
    ServerProperty,
    DatabasePropertyEx,
    DbName,
    DbId,
    ObjectId,
    ObjectName,
    ObjectSchemaName,
    ObjectDefinition,
    SchemaName,
    SchemaId,
    TypeName,
    TypeId,
    ColumnProperty,
    ObjectProperty,
    ObjectPropertyEx,
    ColName,
    ColLength,
    IsServerRoleMember,
    IsMember,
    HasDatabaseAccess,
}

impl MetadataFunction {
    pub(crate) const ALL: [Self; 20] = [
        Self::ServerProperty,
        Self::DatabasePropertyEx,
        Self::DbName,
        Self::DbId,
        Self::ObjectId,
        Self::ObjectName,
        Self::ObjectSchemaName,
        Self::ObjectDefinition,
        Self::SchemaName,
        Self::SchemaId,
        Self::TypeName,
        Self::TypeId,
        Self::ColumnProperty,
        Self::ObjectProperty,
        Self::ObjectPropertyEx,
        Self::ColName,
        Self::ColLength,
        Self::IsServerRoleMember,
        Self::IsMember,
        Self::HasDatabaseAccess,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ServerProperty => "serverproperty",
            Self::DatabasePropertyEx => "databasepropertyex",
            Self::DbName => "db_name",
            Self::DbId => "db_id",
            Self::ObjectId => "object_id",
            Self::ObjectName => "object_name",
            Self::ObjectSchemaName => "object_schema_name",
            Self::ObjectDefinition => "object_definition",
            Self::SchemaName => "schema_name",
            Self::SchemaId => "schema_id",
            Self::TypeName => "type_name",
            Self::TypeId => "type_id",
            Self::ColumnProperty => "columnproperty",
            Self::ObjectProperty => "objectproperty",
            Self::ObjectPropertyEx => "objectpropertyex",
            Self::ColName => "col_name",
            Self::ColLength => "col_length",
            Self::IsServerRoleMember => "is_srvrolemember",
            Self::IsMember => "is_member",
            Self::HasDatabaseAccess => "has_dbaccess",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|function| function.name() == name)
    }

    pub(crate) fn arguments(self) -> RangeInclusive<usize> {
        match self {
            Self::DbName | Self::DbId | Self::SchemaName | Self::SchemaId => 0..=1,
            Self::ServerProperty
            | Self::TypeName
            | Self::TypeId
            | Self::ObjectSchemaName
            | Self::ObjectDefinition
            | Self::HasDatabaseAccess => 1..=1,
            Self::ObjectId | Self::ObjectName | Self::IsServerRoleMember => 1..=2,
            Self::IsMember => 1..=1,
            Self::DatabasePropertyEx
            | Self::ObjectProperty
            | Self::ObjectPropertyEx
            | Self::ColName
            | Self::ColLength => 2..=2,
            Self::ColumnProperty => 3..=3,
        }
    }

    pub(crate) fn takes_the_database(self) -> bool {
        matches!(
            self,
            Self::DbName | Self::DbId | Self::DatabasePropertyEx | Self::HasDatabaseAccess
        )
    }

    pub(crate) fn needs_catalog(self) -> bool {
        matches!(
            self,
            Self::ObjectId
                | Self::ObjectName
                | Self::ObjectSchemaName
                | Self::ObjectDefinition
                | Self::SchemaName
                | Self::SchemaId
                | Self::ColumnProperty
                | Self::ObjectProperty
                | Self::ObjectPropertyEx
                | Self::ColName
                | Self::ColLength
        )
    }

    pub(crate) fn result_type(self) -> Option<SqlType> {
        Some(match self {
            Self::DbName
            | Self::ObjectName
            | Self::ObjectSchemaName
            | Self::SchemaName
            | Self::TypeName
            | Self::ColName => SqlType::Varchar(Some(128)),
            Self::ObjectDefinition => SqlType::Text,
            Self::DbId | Self::ColLength => SqlType::SmallInt,
            Self::ObjectId
            | Self::SchemaId
            | Self::TypeId
            | Self::ColumnProperty
            | Self::ObjectProperty
            | Self::IsServerRoleMember
            | Self::IsMember
            | Self::HasDatabaseAccess => SqlType::Integer,
            Self::ServerProperty | Self::DatabasePropertyEx | Self::ObjectPropertyEx => {
                return None;
            }
        })
    }
}

/// Whether a Transact-SQL statement calls a metadata function that reads the catalog
/// (`OBJECT_ID`, `COLUMNPROPERTY`, ...), so the statement loads the catalog snapshot first.
pub(crate) fn mentions_metadata(tokens: &[Token<'_>]) -> bool {
    tokens.windows(2).any(|pair| {
        matches!(&pair[0].kind, TokenKind::Word(word)
            if MetadataFunction::named(&word.to_ascii_lowercase())
                .is_some_and(MetadataFunction::needs_catalog))
            && pair[1].kind == TokenKind::LeftParen
    })
}

fn catalog_missing() -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "the SQL Server catalog was not loaded for this statement",
    )
}

fn text_argument(arguments: &[Value], at: usize) -> Option<String> {
    match arguments.get(at)? {
        Value::Null => None,
        value => value.to_text(),
    }
}

fn number_argument(arguments: &[Value], at: usize) -> Option<i64> {
    arguments.get(at)?.as_integer()
}

fn flag(value: bool) -> Value {
    Value::Integer(i32::from(value))
}

pub(crate) fn server_property(name: &str) -> Value {
    let version = crate::parser::SQLSERVER_COMPATIBLE_VERSION;
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or("16");
    let minor = parts.next().unwrap_or("0");
    let build = parts.next().unwrap_or("0");
    match name.to_ascii_lowercase().as_str() {
        "productversion" => Value::Text(format!("{version}.0")),
        "productmajorversion" => Value::Text(major.to_owned()),
        "productminorversion" => Value::Text(minor.to_owned()),
        "productbuild" => Value::Text(build.to_owned()),
        "productlevel" => Value::Text("RTM".to_owned()),
        "edition" => Value::Text("EggDB (64-bit)".to_owned()),
        "engineedition" => Value::Integer(3),
        "editionid" => Value::BigInt(0),
        "servername" | "machinename" => Value::Text("EggDB".to_owned()),
        "collation" => Value::Text(COLLATION.to_owned()),
        "lcid" => Value::Integer(1033),
        "comparisonstyle" => Value::Integer(0),
        "processid" => Value::Integer(i32::try_from(std::process::id()).unwrap_or(0)),
        "isclustered"
        | "isfulltextinstalled"
        | "isintegratedsecurityonly"
        | "issingleuser"
        | "ishadrenabled"
        | "islocaldb"
        | "isxtpsupported"
        | "ispolybaseinstalled"
        | "isadvancedanalyticsinstalled"
        | "isbigdatacluster" => Value::Integer(0),
        "resourceversion" => Value::Text(format!("{major}.00.{build}")),
        _ => Value::Null,
    }
}

fn database_property(property: &str) -> Value {
    match property.to_ascii_lowercase().as_str() {
        "collation" => Value::Text(COLLATION.to_owned()),
        "status" => Value::Text("ONLINE".to_owned()),
        "updateability" => Value::Text("READ_WRITE".to_owned()),
        "useraccess" => Value::Text("MULTI_USER".to_owned()),
        "recovery" => Value::Text("SIMPLE".to_owned()),
        "version" => Value::Integer(DATABASE_VERSION),
        "comparisonstyle" => Value::Integer(0),
        "lcid" => Value::Integer(1033),
        "sqlsortorder" => Value::Integer(0),
        "isautoclose"
        | "isautoshrink"
        | "isfulltextenabled"
        | "isinstandby"
        | "isrecursivetriggersenabled"
        | "istornpagedetectionenabled"
        | "ispublished"
        | "issubscribed"
        | "ismergepublished"
        | "issyncwithbackup"
        | "isansinulldefault"
        | "isnullconcat" => Value::Integer(0),
        "isautocreatestatistics"
        | "isautoupdatestatistics"
        | "isansinullsenabled"
        | "isansipaddingenabled"
        | "isansiwarningsenabled"
        | "isarithmeticabortenabled"
        | "isquotedidentifiersenabled" => Value::Integer(1),
        _ => Value::Null,
    }
}

fn database_matches(arguments: &[Value], at: usize, database: Option<&String>) -> bool {
    match (text_argument(arguments, at), database) {
        (Some(given), Some(database)) => given.eq_ignore_ascii_case(database),
        _ => false,
    }
}

/// Runs a metadata function on its evaluated arguments. The functions that need the
/// database's name get it from the parser as one more, last argument.
pub(crate) fn call(function: MetadataFunction, arguments: &[Value]) -> SqlResult<Value> {
    let (arguments, database) = if function.takes_the_database() {
        match arguments.split_last() {
            Some((Value::Text(database), rest)) => (rest, Some(database)),
            _ => (arguments, None),
        }
    } else {
        (arguments, None)
    };
    Ok(match function {
        MetadataFunction::ServerProperty => {
            text_argument(arguments, 0).map_or(Value::Null, |name| server_property(&name))
        }
        MetadataFunction::DatabasePropertyEx => {
            if database_matches(arguments, 0, database) {
                text_argument(arguments, 1).map_or(Value::Null, |name| database_property(&name))
            } else {
                Value::Null
            }
        }
        MetadataFunction::DbName => match (arguments.first(), database) {
            (None, Some(database)) => Value::Text(database.clone()),
            (Some(id), Some(database)) if id.as_integer() == Some(i64::from(DATABASE_ID)) => {
                Value::Text(database.clone())
            }
            _ => Value::Null,
        },
        MetadataFunction::DbId => {
            if arguments.is_empty() || database_matches(arguments, 0, database) {
                Value::SmallInt(DATABASE_ID as i16)
            } else {
                Value::Null
            }
        }
        MetadataFunction::HasDatabaseAccess => {
            if database_matches(arguments, 0, database) {
                Value::Integer(1)
            } else {
                Value::Null
            }
        }
        MetadataFunction::TypeName => number_argument(arguments, 0)
            .and_then(system_type_with_id)
            .map_or(Value::Null, |found| {
                Value::Text(found.name.name().to_owned())
            }),
        MetadataFunction::TypeId => text_argument(arguments, 0)
            .and_then(|name| named_system_type(name.trim_matches(|c| c == '[' || c == ']')))
            .map_or(Value::Null, |found| {
                Value::Integer(i32::try_from(found.user_type_id).unwrap_or(0))
            }),
        MetadataFunction::IsServerRoleMember => {
            text_argument(arguments, 0).map_or(Value::Null, |role| {
                if FIXED_SERVER_ROLES.contains(&role.to_ascii_lowercase().as_str()) {
                    Value::Integer(1)
                } else {
                    Value::Null
                }
            })
        }
        MetadataFunction::IsMember => text_argument(arguments, 0).map_or(Value::Null, |role| {
            if FIXED_DATABASE_ROLES.contains(&role.to_ascii_lowercase().as_str()) {
                Value::Integer(1)
            } else {
                Value::Null
            }
        }),
        catalog_function => {
            let snapshot = crate::pg_catalog::current_snapshot().ok_or_else(catalog_missing)?;
            catalog_call(&snapshot, catalog_function, arguments)
        }
    })
}

fn catalog_call(
    snapshot: &CatalogSnapshot,
    function: MetadataFunction,
    arguments: &[Value],
) -> Value {
    let objects = snapshot.sqlserver_objects();
    let object = |at: usize| {
        number_argument(arguments, at)
            .and_then(|id| i32::try_from(id).ok())
            .and_then(|id| objects.object(id))
    };
    match function {
        MetadataFunction::ObjectId => object_id(snapshot, objects, arguments),
        MetadataFunction::ObjectName => match (object(0), number_argument(arguments, 1)) {
            (Some(found), None) => Value::Text(found.name.clone()),
            (Some(found), Some(database)) if database == i64::from(DATABASE_ID) => {
                Value::Text(found.name.clone())
            }
            _ => Value::Null,
        },
        MetadataFunction::ObjectSchemaName => object(0).map_or(Value::Null, |found| {
            Value::Text(objects.schema_name(found.schema_id).to_owned())
        }),
        MetadataFunction::ObjectDefinition => object(0)
            .and_then(|found| super::routines::module_definition(snapshot, objects, found))
            .map_or(Value::Null, Value::Text),
        MetadataFunction::SchemaName => match arguments.first() {
            None => Value::Text(DBO.to_owned()),
            Some(id) => id
                .as_integer()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(|id| objects.schema(id))
                .map_or(Value::Null, |schema| Value::Text(schema.name.clone())),
        },
        MetadataFunction::SchemaId => match arguments.first() {
            None => Value::Integer(DBO_SCHEMA_ID),
            Some(_) => text_argument(arguments, 0)
                .and_then(|name| objects.schema_named(&name))
                .map_or(Value::Null, |schema| Value::Integer(schema.schema_id)),
        },
        MetadataFunction::ColName => {
            let column = number_argument(arguments, 1);
            object(0)
                .and_then(|found| {
                    let at = usize::try_from(column?.checked_sub(1)?).ok()?;
                    let entry = snapshot.relations.get(found.relation)?;
                    relation_columns(entry)
                        .get(at)
                        .map(|facts| Value::Text(facts.name.to_owned()))
                })
                .unwrap_or(Value::Null)
        }
        MetadataFunction::ColLength => {
            let table = text_argument(arguments, 0).and_then(|name| objects.resolve_name(&name));
            let column = text_argument(arguments, 1);
            match (table, column) {
                (Some(found), Some(column)) => {
                    column_facts(snapshot, found, &column).map_or(Value::Null, |(_, facts)| {
                        Value::SmallInt(i16::try_from(facts.column_type.max_length()).unwrap_or(-1))
                    })
                }
                _ => Value::Null,
            }
        }
        MetadataFunction::ColumnProperty => {
            match (
                object(0),
                text_argument(arguments, 1),
                text_argument(arguments, 2),
            ) {
                (Some(found), Some(column), Some(property)) => {
                    column_property(snapshot, found, &column, &property)
                }
                _ => Value::Null,
            }
        }
        MetadataFunction::ObjectProperty | MetadataFunction::ObjectPropertyEx => {
            match (object(0), text_argument(arguments, 1)) {
                (Some(found), Some(property)) => object_property(
                    snapshot,
                    objects,
                    found,
                    &property,
                    function == MetadataFunction::ObjectPropertyEx,
                ),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

fn object_id(snapshot: &CatalogSnapshot, objects: &SqlServerObjects, arguments: &[Value]) -> Value {
    let Some(name) = text_argument(arguments, 0) else {
        return Value::Null;
    };
    let Some(parts) = super::split_multipart_name(&name) else {
        return Value::Null;
    };
    if parts.len() >= 3 {
        let database = &parts[parts.len() - 3];
        if !database.is_empty() && !database.eq_ignore_ascii_case(&snapshot.database) {
            return Value::Null;
        }
    }
    let Some(found) = objects.resolve_name(&name) else {
        return Value::Null;
    };
    if let Some(kind) = text_argument(arguments, 1)
        && !found.kind.code().trim().eq_ignore_ascii_case(kind.trim())
    {
        return Value::Null;
    }
    Value::Integer(found.object_id)
}

fn column_facts<'a>(
    snapshot: &'a CatalogSnapshot,
    object: &SqlServerObject,
    column: &str,
) -> Option<(usize, super::ColumnInfo<'a>)> {
    let entry = snapshot.relations.get(object.relation)?;
    relation_columns(entry)
        .into_iter()
        .enumerate()
        .find(|(_, facts)| facts.name.eq_ignore_ascii_case(column))
}

fn column_property(
    snapshot: &CatalogSnapshot,
    object: &SqlServerObject,
    column: &str,
    property: &str,
) -> Value {
    let Some((position, facts)) = column_facts(snapshot, object, column) else {
        return Value::Null;
    };
    let column_type = facts.column_type;
    match property.to_ascii_lowercase().as_str() {
        "allowsnull" => flag(facts.nullable),
        "isidentity" => flag(facts.identity),
        "columnid" => Value::Integer(i32::try_from(position + 1).unwrap_or(i32::MAX)),
        "precision" => Value::Integer(
            i32::try_from(match column_type.character_length() {
                Some(length) => length,
                None => column_type.precision,
            })
            .unwrap_or(-1),
        ),
        "scale" => column_type
            .numeric_scale()
            .or(column_type.datetime_precision())
            .and_then(|scale| i32::try_from(scale).ok())
            .map_or(Value::Null, Value::Integer),
        "charmaxlen" => column_type
            .character_length()
            .and_then(|length| i32::try_from(length).ok())
            .map_or(Value::Null, Value::Integer),
        "isprecise" => flag(!matches!(
            column_type.name,
            super::types::SqlServerTypeName::Real | super::types::SqlServerTypeName::Float
        )),
        "iscomputed"
        | "isrowguidcol"
        | "issparse"
        | "iscolumnset"
        | "isfulltextindexed"
        | "isidnotforrepl"
        | "ishidden"
        | "isxmlindexable"
        | "generatedalwaystype" => flag(false),
        _ => Value::Null,
    }
}

fn table_has(snapshot: &CatalogSnapshot, object: &SqlServerObject, wanted: ObjectKind) -> bool {
    snapshot
        .sqlserver_objects()
        .objects
        .iter()
        .any(|found| found.parent_object_id == object.object_id && found.kind == wanted)
}

fn object_property(
    snapshot: &CatalogSnapshot,
    objects: &SqlServerObjects,
    object: &SqlServerObject,
    property: &str,
    extended: bool,
) -> Value {
    let table = snapshot
        .relations
        .get(object.relation)
        .and_then(|entry| entry.table())
        .filter(|_| object.kind == ObjectKind::Table);
    let kind = object.kind;
    match property.to_ascii_lowercase().as_str() {
        "istable" | "isusertable" => flag(kind == ObjectKind::Table),
        "isview" => flag(kind == ObjectKind::View),
        "isconstraint" => flag(kind.is_constraint()),
        "isprimarykey" => flag(kind == ObjectKind::PrimaryKey),
        "isforeignkey" => flag(kind == ObjectKind::ForeignKey),
        "ischeckcnst" => flag(kind == ObjectKind::Check),
        "isdefaultcnst" => flag(kind == ObjectKind::Default),
        "isuniquecnst" => flag(kind == ObjectKind::Unique),
        "isprocedure" => flag(kind == ObjectKind::Procedure),
        "isscalarfunction" => flag(kind == ObjectKind::ScalarFunction),
        "istablefunction" => flag(matches!(
            kind,
            ObjectKind::InlineTableFunction | ObjectKind::TableFunction
        )),
        "isinlinefunction" => flag(kind == ObjectKind::InlineTableFunction),
        "istrigger" => flag(kind == ObjectKind::Trigger),
        "isexecuted" => flag(kind.is_routine() || kind == ObjectKind::Trigger || kind == ObjectKind::View),
        "isdeterministic" if kind.is_function() => flag(false),
        "issystemtable" | "ismsshipped" | "isreplproc" | "isextendedproc" | "isencrypted" => {
            flag(false)
        }
        "isquotedidenton" | "isansinullson" | "execisquotedidenton" | "execisansinullson" => flag(
            matches!(kind, ObjectKind::Table | ObjectKind::View | ObjectKind::Trigger)
                || kind.is_routine(),
        ),
        "ownerid" => Value::Integer(super::DBO_PRINCIPAL_ID),
        "schemaid" => Value::Integer(object.schema_id),
        "isindexed" | "tablehasindex" => {
            flag(table.is_some_and(|table| !table.primary_key.is_empty() || !table.indexes.is_empty()))
        }
        "tablehasprimarykey" | "tablehasclustindex" => {
            flag(table.is_some_and(|table| !table.primary_key.is_empty()))
        }
        "tablehasnonclustindex" => flag(table.is_some_and(|table| !table.indexes.is_empty())),
        "tablehasidentity" => {
            flag(table.is_some_and(|table| table.auto_increment_column().is_some()))
        }
        "tablehasforeignkey" => flag(table.is_some() && table_has(snapshot, object, ObjectKind::ForeignKey)),
        "tablehasforeignref" => flag(table.is_some() && objects.objects.iter().any(|found| {
            found.kind == ObjectKind::ForeignKey
                && matches!(found_constraint_parent(snapshot, found), Some(parent) if parent == object.relation)
        })),
        "tablehasuniquecnst" => flag(table.is_some() && table_has(snapshot, object, ObjectKind::Unique)),
        "tablehascheckcnst" => flag(table.is_some() && table_has(snapshot, object, ObjectKind::Check)),
        "tablehasdefaultcnst" => {
            flag(table.is_some() && table_has(snapshot, object, ObjectKind::Default))
        }
        "tablehastext" | "tablehastimestamp" | "tablehasrowguidcol" | "tablehasinserttrigger"
        | "tablehasupdatetrigger" | "tablehasdeletetrigger" | "tableisfake" => {
            if table.is_some() {
                flag(false)
            } else {
                Value::Null
            }
        }
        "basetype" if extended => Value::Text(kind.code().trim().to_owned()),
        _ => Value::Null,
    }
}

fn found_constraint_parent(snapshot: &CatalogSnapshot, object: &SqlServerObject) -> Option<usize> {
    let super::ObjectSource::Constraint(at) = object.source else {
        return None;
    };
    let ConstraintSource::Foreign(key_at) = snapshot.constraints.get(at)?.source else {
        return None;
    };
    super::foreign_key_facts(snapshot, object.relation, key_at)?.referenced_relation
}
