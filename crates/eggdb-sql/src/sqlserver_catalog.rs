mod functions;
mod information_schema;
mod procedures;
mod system_views;
mod types;

#[cfg(test)]
mod tests;

use crate::pg_catalog::{
    CatalogColumn, CatalogSnapshot, ConstraintSource, IndexSource, OidRequest, OidSpace,
    RelationEntry, RelationKind, RelationSource, assign_oids,
};
use crate::types::Value;
use std::collections::HashMap;

pub(crate) use functions::{MetadataFunction, call as call_metadata, mentions_metadata};
pub(crate) use information_schema::SqlServerInformation;
pub(crate) use procedures::{CatalogProcedure, bind_arguments, run_procedure};
pub(crate) use system_views::SystemView;
pub(crate) use types::SqlServerColumnType;

/// Whether `name` (a procedure's last name part, in any case) is one of the catalog procedures
/// EggDB answers in the SQL Server dialect (`sp_tables`, `sp_columns`, `sp_help`, ...), which a
/// TDS server runs when a client calls them by RPC.
pub fn is_catalog_procedure(name: &str) -> bool {
    CatalogProcedure::named(&name.to_ascii_lowercase()).is_some()
}

pub(crate) const SYS_SCHEMA: &str = "sys";
pub(crate) const DBO: &str = "dbo";
pub(crate) const DBO_SCHEMA_ID: i32 = 1;
pub(crate) const GUEST_SCHEMA_ID: i32 = 2;
pub(crate) const INFORMATION_SCHEMA_ID: i32 = 3;
pub(crate) const SYS_SCHEMA_ID: i32 = 4;
const FIRST_USER_SCHEMA_ID: i32 = 5;
pub(crate) const DATABASE_ID: i32 = 5;
pub(crate) const DBO_PRINCIPAL_ID: i32 = 1;
pub(crate) const COLLATION: &str = "Latin1_General_100_BIN2";
const RESERVED_SCHEMAS: [&str; 4] = [DBO, "guest", "information_schema", SYS_SCHEMA];
const HIDDEN_SCHEMAS: [&str; 3] = ["pg_catalog", "pg_toast", "information_schema"];

const fn column(name: &'static str, sql_type: crate::types::SqlType) -> CatalogColumn {
    CatalogColumn { name, sql_type }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SqlServerRelation {
    System(SystemView),
    Information(SqlServerInformation),
}

impl SqlServerRelation {
    pub(crate) fn referenced(name: &str) -> Option<Self> {
        let (schema, base) = name.split_once(crate::schemas::QUALIFIER)?;
        match schema {
            SYS_SCHEMA => SystemView::named(base).map(Self::System),
            crate::schemas::INFORMATION_SCHEMA => {
                SqlServerInformation::named(base).map(Self::Information)
            }
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::System(view) => view.name(),
            Self::Information(view) => view.name(),
        }
    }

    pub(crate) fn schema(self) -> &'static str {
        match self {
            Self::System(_) => SYS_SCHEMA,
            Self::Information(_) => crate::schemas::INFORMATION_SCHEMA,
        }
    }

    pub(crate) fn columns(self) -> &'static [CatalogColumn] {
        match self {
            Self::System(view) => view.columns(),
            Self::Information(view) => view.columns(),
        }
    }
}

pub(crate) fn rows(snapshot: &CatalogSnapshot, relation: SqlServerRelation) -> Vec<Vec<Value>> {
    match relation {
        SqlServerRelation::System(view) => system_views::rows(snapshot, view),
        SqlServerRelation::Information(view) => information_schema::rows(snapshot, view),
    }
}

pub(crate) struct CatalogRow {
    columns: &'static [CatalogColumn],
    values: Vec<Value>,
}

impl CatalogRow {
    pub(crate) fn new(columns: &'static [CatalogColumn]) -> Self {
        Self {
            columns,
            values: vec![Value::Null; columns.len()],
        }
    }

    pub(crate) fn set(mut self, name: &str, value: Value) -> Self {
        if let Some(at) = self.columns.iter().position(|column| column.name == name)
            && let Some(slot) = self.values.get_mut(at)
        {
            *slot = value;
        }
        self
    }

    pub(crate) fn done(self) -> Vec<Value> {
        self.values
    }
}

pub(crate) fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

pub(crate) fn int(value: i64) -> Value {
    i32::try_from(value).map_or(Value::Null, Value::Integer)
}

pub(crate) fn small(value: i64) -> Value {
    i16::try_from(value).map_or(Value::Null, Value::SmallInt)
}

pub(crate) fn bit(value: bool) -> Value {
    Value::Boolean(value)
}

pub(crate) fn optional_int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, int)
}

pub(crate) fn optional_small(value: Option<i64>) -> Value {
    value.map_or(Value::Null, small)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Table,
    View,
    Sequence,
    PrimaryKey,
    Unique,
    ForeignKey,
    Check,
    Default,
}

impl ObjectKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Table => "U ",
            Self::View => "V ",
            Self::Sequence => "SO",
            Self::PrimaryKey => "PK",
            Self::Unique => "UQ",
            Self::ForeignKey => "F ",
            Self::Check => "C ",
            Self::Default => "D ",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Table => "USER_TABLE",
            Self::View => "VIEW",
            Self::Sequence => "SEQUENCE_OBJECT",
            Self::PrimaryKey => "PRIMARY_KEY_CONSTRAINT",
            Self::Unique => "UNIQUE_CONSTRAINT",
            Self::ForeignKey => "FOREIGN_KEY_CONSTRAINT",
            Self::Check => "CHECK_CONSTRAINT",
            Self::Default => "DEFAULT_CONSTRAINT",
        }
    }

    fn request_kind(self) -> u8 {
        match self {
            Self::Table => b'U',
            Self::View => b'V',
            Self::Sequence => b'S',
            Self::PrimaryKey => b'P',
            Self::Unique => b'Q',
            Self::ForeignKey => b'F',
            Self::Check => b'C',
            Self::Default => b'D',
        }
    }

    pub(crate) fn is_constraint(self) -> bool {
        matches!(
            self,
            Self::PrimaryKey | Self::Unique | Self::ForeignKey | Self::Check | Self::Default
        )
    }

    pub(crate) fn of_constraint(source: ConstraintSource) -> Self {
        match source {
            ConstraintSource::Primary => Self::PrimaryKey,
            ConstraintSource::Unique(_) => Self::Unique,
            ConstraintSource::Check(_) => Self::Check,
            ConstraintSource::Foreign(_) => Self::ForeignKey,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectSource {
    Relation,
    Constraint(usize),
    Default(usize),
}

pub(crate) struct SqlServerSchema {
    pub(crate) schema_id: i32,
    pub(crate) name: String,
    pub(crate) internal: Option<String>,
}

pub(crate) struct SqlServerObject {
    pub(crate) object_id: i32,
    pub(crate) name: String,
    pub(crate) schema_id: i32,
    pub(crate) parent_object_id: i32,
    pub(crate) kind: ObjectKind,
    pub(crate) relation: usize,
    pub(crate) source: ObjectSource,
}

pub(crate) struct SqlServerIndex {
    pub(crate) index_id: i32,
    pub(crate) source: Option<IndexSource>,
}

impl SqlServerIndex {
    pub(crate) fn is_heap(&self) -> bool {
        self.source.is_none()
    }

    pub(crate) fn is_clustered(&self) -> bool {
        self.source == Some(IndexSource::Primary)
    }
}

pub(crate) struct SqlServerObjects {
    pub(crate) schemas: Vec<SqlServerSchema>,
    pub(crate) objects: Vec<SqlServerObject>,
    by_id: HashMap<i32, usize>,
    by_name: HashMap<(i32, String), usize>,
    by_relation: HashMap<usize, usize>,
    by_default: HashMap<(usize, usize), usize>,
}

struct PendingObject {
    request: OidRequest,
    name: String,
    schema_id: i32,
    parent: Option<usize>,
    kind: ObjectKind,
    relation: usize,
    source: ObjectSource,
}

pub(crate) fn default_constraint_name(table: &str, column: &str) -> String {
    format!("df__{table}__{column}")
}

impl SqlServerObjects {
    pub(crate) fn build(snapshot: &CatalogSnapshot) -> Self {
        let mut schemas = vec![
            SqlServerSchema {
                schema_id: DBO_SCHEMA_ID,
                name: DBO.to_owned(),
                internal: Some(crate::schemas::DEFAULT_SCHEMA.to_owned()),
            },
            SqlServerSchema {
                schema_id: GUEST_SCHEMA_ID,
                name: "guest".to_owned(),
                internal: None,
            },
            SqlServerSchema {
                schema_id: INFORMATION_SCHEMA_ID,
                name: "INFORMATION_SCHEMA".to_owned(),
                internal: None,
            },
            SqlServerSchema {
                schema_id: SYS_SCHEMA_ID,
                name: SYS_SCHEMA.to_owned(),
                internal: None,
            },
        ];
        let mut user_schemas: Vec<&str> = snapshot
            .schemas
            .iter()
            .map(|schema| schema.name.as_str())
            .filter(|name| {
                *name != crate::schemas::DEFAULT_SCHEMA
                    && !HIDDEN_SCHEMAS.contains(name)
                    && !RESERVED_SCHEMAS.contains(name)
            })
            .collect();
        user_schemas.sort_unstable();
        let mut next_id = FIRST_USER_SCHEMA_ID;
        for name in user_schemas {
            schemas.push(SqlServerSchema {
                schema_id: next_id,
                name: name.to_owned(),
                internal: Some(name.to_owned()),
            });
            next_id = next_id.saturating_add(1);
        }
        let schema_id_of = |internal: &str| {
            schemas
                .iter()
                .find(|schema| schema.internal.as_deref() == Some(internal))
                .map(|schema| schema.schema_id)
        };
        let mut pending: Vec<PendingObject> = Vec::new();
        let mut relation_objects: HashMap<usize, usize> = HashMap::new();
        for (at, entry) in snapshot.relations.iter().enumerate() {
            let kind = match entry.kind {
                RelationKind::Table => ObjectKind::Table,
                RelationKind::View => ObjectKind::View,
                RelationKind::Sequence => ObjectKind::Sequence,
                RelationKind::Index | RelationKind::Catalog => continue,
            };
            let Some(schema_id) = schema_id_of(&entry.schema) else {
                continue;
            };
            relation_objects.insert(at, pending.len());
            pending.push(PendingObject {
                request: OidRequest {
                    kind: kind.request_kind(),
                    owner: String::new(),
                    name: entry.internal.clone(),
                },
                name: entry.name.clone(),
                schema_id,
                parent: None,
                kind,
                relation: at,
                source: ObjectSource::Relation,
            });
        }
        for (at, constraint) in snapshot.constraints.iter().enumerate() {
            let Some(&parent) = relation_objects.get(&constraint.table) else {
                continue;
            };
            let Some(owner) = snapshot.relations.get(constraint.table) else {
                continue;
            };
            let kind = ObjectKind::of_constraint(constraint.source);
            let schema_id = pending[parent].schema_id;
            pending.push(PendingObject {
                request: OidRequest {
                    kind: kind.request_kind(),
                    owner: owner.internal.clone(),
                    name: constraint.name.clone(),
                },
                name: constraint.name.clone(),
                schema_id,
                parent: Some(parent),
                kind,
                relation: constraint.table,
                source: ObjectSource::Constraint(at),
            });
        }
        for default in &snapshot.defaults {
            let Some(&parent) = relation_objects.get(&default.table) else {
                continue;
            };
            let Some(entry) = snapshot.relations.get(default.table) else {
                continue;
            };
            let Some(column) = entry
                .table()
                .and_then(|table| table.columns.get(default.column))
            else {
                continue;
            };
            let name = default_constraint_name(&entry.name, &column.name);
            let schema_id = pending[parent].schema_id;
            pending.push(PendingObject {
                request: OidRequest {
                    kind: ObjectKind::Default.request_kind(),
                    owner: entry.internal.clone(),
                    name: column.name.clone(),
                },
                name,
                schema_id,
                parent: Some(parent),
                kind: ObjectKind::Default,
                relation: default.table,
                source: ObjectSource::Default(default.column),
            });
        }
        let requests: Vec<OidRequest> = pending
            .iter()
            .map(|object| OidRequest {
                kind: object.request.kind,
                owner: object.request.owner.clone(),
                name: object.request.name.clone(),
            })
            .collect();
        let ids: Vec<i32> = assign_oids(OidSpace::SqlServerObject, &requests)
            .into_iter()
            .map(|id| i32::try_from(id).unwrap_or(i32::MAX))
            .collect();
        let mut objects: Vec<SqlServerObject> = Vec::with_capacity(pending.len());
        for (object, id) in pending.iter().zip(&ids) {
            objects.push(SqlServerObject {
                object_id: *id,
                name: object.name.clone(),
                schema_id: object.schema_id,
                parent_object_id: object
                    .parent
                    .and_then(|parent| ids.get(parent).copied())
                    .unwrap_or(0),
                kind: object.kind,
                relation: object.relation,
                source: object.source,
            });
        }
        let mut built = Self {
            schemas,
            objects,
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            by_relation: HashMap::new(),
            by_default: HashMap::new(),
        };
        for (at, object) in built.objects.iter().enumerate() {
            built.by_id.insert(object.object_id, at);
            built
                .by_name
                .entry((object.schema_id, object.name.clone()))
                .or_insert(at);
            match object.source {
                ObjectSource::Relation => {
                    built.by_relation.insert(object.relation, at);
                }
                ObjectSource::Constraint(_) => {}
                ObjectSource::Default(column) => {
                    built.by_default.insert((object.relation, column), at);
                }
            }
        }
        built
    }

    pub(crate) fn object(&self, object_id: i32) -> Option<&SqlServerObject> {
        self.by_id
            .get(&object_id)
            .and_then(|&at| self.objects.get(at))
    }

    pub(crate) fn of_relation(&self, relation: usize) -> Option<&SqlServerObject> {
        self.by_relation
            .get(&relation)
            .and_then(|&at| self.objects.get(at))
    }

    pub(crate) fn of_default(&self, relation: usize, column: usize) -> Option<&SqlServerObject> {
        self.by_default
            .get(&(relation, column))
            .and_then(|&at| self.objects.get(at))
    }

    pub(crate) fn schema(&self, schema_id: i32) -> Option<&SqlServerSchema> {
        self.schemas
            .iter()
            .find(|schema| schema.schema_id == schema_id)
    }

    pub(crate) fn schema_named(&self, name: &str) -> Option<&SqlServerSchema> {
        self.schemas
            .iter()
            .find(|schema| schema.name.eq_ignore_ascii_case(name))
    }

    pub(crate) fn schema_name(&self, schema_id: i32) -> &str {
        self.schema(schema_id)
            .map_or("", |schema| schema.name.as_str())
    }

    pub(crate) fn named(&self, schema_id: i32, name: &str) -> Option<&SqlServerObject> {
        self.by_name
            .get(&(schema_id, name.to_owned()))
            .and_then(|&at| self.objects.get(at))
            .or_else(|| {
                self.objects.iter().find(|object| {
                    object.schema_id == schema_id && object.name.eq_ignore_ascii_case(name)
                })
            })
    }

    pub(crate) fn relations(&self) -> impl Iterator<Item = &SqlServerObject> {
        self.objects
            .iter()
            .filter(|object| object.source == ObjectSource::Relation)
    }

    pub(crate) fn indexes(
        &self,
        snapshot: &CatalogSnapshot,
        relation: usize,
    ) -> Vec<SqlServerIndex> {
        let Some(table) = snapshot
            .relations
            .get(relation)
            .and_then(RelationEntry::table)
        else {
            return Vec::new();
        };
        let mut indexes = Vec::with_capacity(table.indexes.len() + 1);
        indexes.push(if table.primary_key.is_empty() {
            SqlServerIndex {
                index_id: 0,
                source: None,
            }
        } else {
            SqlServerIndex {
                index_id: 1,
                source: Some(IndexSource::Primary),
            }
        });
        for position in 0..table.indexes.len() {
            indexes.push(SqlServerIndex {
                index_id: i32::try_from(position).map_or(i32::MAX, |at| at.saturating_add(2)),
                source: Some(IndexSource::Secondary(position)),
            });
        }
        indexes
    }

    pub(crate) fn index_name(
        &self,
        snapshot: &CatalogSnapshot,
        relation: usize,
        index: &SqlServerIndex,
    ) -> Option<String> {
        let table = snapshot.relations.get(relation)?.table()?;
        match index.source? {
            IndexSource::Primary => Some(table.primary_key_constraint_name()),
            IndexSource::Secondary(at) => table.indexes.get(at).map(|found| found.name.clone()),
        }
    }

    pub(crate) fn resolve_name(&self, written: &str) -> Option<&SqlServerObject> {
        let parts = split_multipart_name(written)?;
        let (schema, name) = match parts.as_slice() {
            [name] => (None, name.as_str()),
            [schema, name] | [_, schema, name] | [_, _, schema, name] => {
                (Some(schema.as_str()), name.as_str())
            }
            _ => return None,
        };
        let schema_id = match schema {
            None | Some("") => DBO_SCHEMA_ID,
            Some(schema) => self.schema_named(schema)?.schema_id,
        };
        self.named(schema_id, name)
    }

    pub(crate) fn relation_object(&self, entry_at: usize) -> Option<i32> {
        self.of_relation(entry_at).map(|object| object.object_id)
    }
}

pub(crate) fn split_multipart_name(written: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut characters = written.trim().chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '[' => loop {
                match characters.next()? {
                    ']' if characters.peek() == Some(&']') => {
                        characters.next();
                        current.push(']');
                    }
                    ']' => break,
                    other => current.push(other),
                }
            },
            '"' => loop {
                match characters.next()? {
                    '"' if characters.peek() == Some(&'"') => {
                        characters.next();
                        current.push('"');
                    }
                    '"' => break,
                    other => current.push(other),
                }
            },
            '.' => parts.push(std::mem::take(&mut current)),
            other => current.extend(other.to_lowercase()),
        }
    }
    parts.push(current);
    if parts.len() > 4 || parts.last().is_none_or(String::is_empty) {
        return None;
    }
    Some(parts.into_iter().map(|part| part.to_lowercase()).collect())
}

pub(crate) struct ColumnInfo<'a> {
    pub(crate) name: &'a str,
    pub(crate) column_type: SqlServerColumnType,
    pub(crate) nullable: bool,
    pub(crate) identity: bool,
    pub(crate) has_default: bool,
}

pub(crate) fn relation_columns(entry: &RelationEntry) -> Vec<ColumnInfo<'_>> {
    match &entry.source {
        RelationSource::Table(table) => table
            .columns
            .iter()
            .enumerate()
            .map(|(position, column)| ColumnInfo {
                name: &column.name,
                column_type: SqlServerColumnType::of(column.sql_type),
                nullable: !(column.not_null || table.primary_key.contains(&position)),
                identity: column.auto_increment,
                has_default: column.default.is_some(),
            })
            .collect(),
        RelationSource::View(view) => view
            .columns
            .iter()
            .map(|column| ColumnInfo {
                name: &column.name,
                column_type: SqlServerColumnType::of(
                    column.sql_type.unwrap_or(crate::types::SqlType::Text),
                ),
                nullable: true,
                identity: false,
                has_default: false,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn default_definition(stored: &str) -> String {
    let trimmed = stored.trim();
    let shown = match trimmed {
        "CAST(localtimestamp AS timestamp(3))" => "getdate()".to_owned(),
        "CAST(localtimestamp AS timestamp(0))" => "getdate()".to_owned(),
        "CAST(localtimestamp AS timestamp(6))" => "sysdatetime()".to_owned(),
        "CAST(now() AS timestamp(6) with time zone)" => "sysdatetimeoffset()".to_owned(),
        "gen_random_uuid()" => "newid()".to_owned(),
        "true" => "(1)".to_owned(),
        "false" => "(0)".to_owned(),
        "NULL" => "NULL".to_owned(),
        number if is_number_text(number) => format!("({number})"),
        other => other.to_owned(),
    };
    format!("({shown})")
}

fn is_number_text(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty()
        && digits.bytes().any(|byte| byte.is_ascii_digit())
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.' || byte == b'e' || byte == b'E')
}

pub(crate) fn check_definition(expression: &str) -> String {
    format!("({})", expression.trim())
}

pub(crate) const UNKNOWN_DATE: Value = Value::Timestamp(0);

pub(crate) fn referential_action(action: crate::ast::ReferentialAction) -> (i64, &'static str) {
    match action {
        crate::ast::ReferentialAction::NoAction | crate::ast::ReferentialAction::Restrict => {
            (0, "NO_ACTION")
        }
        crate::ast::ReferentialAction::Cascade => (1, "CASCADE"),
        crate::ast::ReferentialAction::SetNull => (2, "SET_NULL"),
        crate::ast::ReferentialAction::SetDefault => (3, "SET_DEFAULT"),
    }
}

pub(crate) struct ForeignKeyFacts<'a> {
    pub(crate) key: &'a crate::encoding::ForeignKey,
    pub(crate) referenced_relation: Option<usize>,
    pub(crate) referenced_columns: Vec<usize>,
    pub(crate) referenced_index: Option<IndexSource>,
}

pub(crate) fn foreign_key_facts<'a>(
    snapshot: &'a CatalogSnapshot,
    relation: usize,
    key_at: usize,
) -> Option<ForeignKeyFacts<'a>> {
    let table = snapshot.relations.get(relation)?.table()?;
    let key = table.foreign_keys.get(key_at)?;
    let referenced_relation = snapshot.table_named(&key.parent);
    let parent = referenced_relation
        .and_then(|at| snapshot.relations.get(at))
        .and_then(RelationEntry::table);
    let referenced_columns: Vec<usize> = parent.map_or_else(Vec::new, |parent| {
        key.parent_columns
            .iter()
            .filter_map(|name| parent.column_index(name))
            .collect()
    });
    let referenced_index = parent.and_then(|parent| {
        let same = |columns: &[usize]| {
            columns.len() == referenced_columns.len()
                && referenced_columns
                    .iter()
                    .all(|column| columns.contains(column))
        };
        if !parent.primary_key.is_empty() && same(&parent.primary_key) {
            return Some(IndexSource::Primary);
        }
        parent
            .indexes
            .iter()
            .position(|index| index.unique && same(&index.columns))
            .map(IndexSource::Secondary)
    });
    Some(ForeignKeyFacts {
        key,
        referenced_relation,
        referenced_columns,
        referenced_index,
    })
}

pub(crate) fn index_id_of(source: IndexSource) -> i64 {
    match source {
        IndexSource::Primary => 1,
        IndexSource::Secondary(at) => i64::try_from(at).map_or(i64::MAX, |at| at + 2),
    }
}
