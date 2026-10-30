use super::oids::{self, BuiltinType};
use super::snapshot::{
    CatalogSnapshot, ConstraintSource, IndexSource, RelationKind, RelationSource,
};
use super::{CatalogRelation, deparse};
use crate::arrays::{ArrayElement, ArrayValue};
use crate::ast::ReferentialAction;
use crate::encoding::{
    CommentKind, IdentityGeneration, IndexMethod, SortOptions, TableDefinition,
};
use crate::error::SqlResult;
use crate::memory::WorkMemory;
use crate::session::OutputColumn;
use crate::set_returning::GeneratedRows;
use crate::store::KeyValueRead;
use crate::types::{SqlType, Value};
use std::collections::HashSet;

const PG_CLASS: u32 = 1259;
const PG_CONSTRAINT: u32 = 2606;
const PG_NAMESPACE: u32 = 2615;
const HASH: u32 = 405;
const INDOPTION_DESC: i64 = 1;
const INDOPTION_NULLS_FIRST: i64 = 2;
const INTERNAL_LANGUAGE: u32 = 12;

pub(crate) fn generated_rows(
    reader: &dyn KeyValueRead,
    relation: CatalogRelation,
    columns_only: bool,
) -> SqlResult<GeneratedRows> {
    let columns: Vec<OutputColumn> = relation
        .columns()
        .iter()
        .map(|column| OutputColumn {
            name: column.name.to_owned(),
            sql_type: Some(column.sql_type),
        })
        .collect();
    if columns_only {
        return Ok(GeneratedRows::empty(columns));
    }
    let snapshot = super::snapshot::snapshot_for(reader)?;
    let built = snapshot.rows(relation)?;
    let mut charge = WorkMemory::new();
    let mut rows = Vec::with_capacity(built.len());
    for row in built.iter() {
        charge.charge_row(row)?;
        rows.push(row.clone());
    }
    Ok(GeneratedRows {
        columns,
        rows,
        charge,
    })
}

struct Row {
    relation: CatalogRelation,
    values: Vec<Value>,
}

impl Row {
    fn new(relation: CatalogRelation) -> Self {
        Self {
            relation,
            values: vec![Value::Null; relation.columns().len()],
        }
    }

    fn set(mut self, name: &str, value: Value) -> Self {
        if let Some(at) = self.relation.column_named(name)
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

fn oid(value: u32) -> Value {
    Value::Oid(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn flag(value: bool) -> Value {
    Value::Boolean(value)
}

fn small(value: i64) -> Value {
    Value::SmallInt(i16::try_from(value).unwrap_or(i16::MAX))
}

fn int(value: i64) -> Value {
    Value::Integer(i32::try_from(value).unwrap_or(i32::MAX))
}

fn vector(element: ArrayElement, items: Vec<Value>) -> Value {
    Value::Array(Box::new(ArrayValue {
        element,
        lower_bound: 0,
        items,
    }))
}

fn smallints(items: &[usize]) -> Value {
    Value::Array(Box::new(ArrayValue::new(
        ArrayElement::SmallInt,
        items.iter().map(|at| small(*at as i64 + 1)).collect(),
    )))
}

fn regproc_none() -> Value {
    regproc(0, "-")
}

fn regproc(function: u32, name: &str) -> Value {
    Value::Reg(Box::new(super::RegValue {
        kind: super::RegKind::Proc,
        oid: function,
        name: name.to_owned(),
    }))
}

struct TypeFacts {
    oid: u32,
    modifier: i32,
    length: i16,
    by_value: bool,
    align: char,
    storage: char,
    collation: u32,
}

fn facts(sql_type: SqlType) -> TypeFacts {
    let type_oid = oids::type_oid(sql_type);
    let builtin: Option<&BuiltinType> = oids::builtin_type(type_oid);
    TypeFacts {
        oid: type_oid,
        modifier: oids::type_modifier(sql_type),
        length: builtin.map_or(-1, |found| found.length),
        by_value: builtin.is_some_and(|found| found.by_value),
        align: builtin.map_or('i', |found| found.align),
        storage: builtin.map_or('x', |found| found.storage),
        collation: builtin.map_or(0, |found| found.collation),
    }
}

pub(crate) fn build(
    snapshot: &CatalogSnapshot,
    relation: CatalogRelation,
) -> SqlResult<Vec<Vec<Value>>> {
    Ok(match relation {
        CatalogRelation::Class => class_rows(snapshot),
        CatalogRelation::Attribute => attribute_rows(snapshot),
        CatalogRelation::Type => type_rows(),
        CatalogRelation::Namespace => namespace_rows(snapshot),
        CatalogRelation::Index => index_rows(snapshot),
        CatalogRelation::Constraint => constraint_rows(snapshot),
        CatalogRelation::AttrDef => attrdef_rows(snapshot),
        CatalogRelation::Description => description_rows(snapshot),
        CatalogRelation::Database => vec![database_row(snapshot)],
        CatalogRelation::Roles => role_rows(snapshot),
        CatalogRelation::Am => am_rows(),
        CatalogRelation::Collation => collation_rows(),
        CatalogRelation::Sequence => sequence_rows(snapshot),
        CatalogRelation::Depend => depend_rows(snapshot),
        CatalogRelation::Tablespace => tablespace_rows(),
        CatalogRelation::Inherits
        | CatalogRelation::Policy
        | CatalogRelation::StatisticExt
        | CatalogRelation::Publication
        | CatalogRelation::PublicationRel
        | CatalogRelation::PublicationNamespace
        | CatalogRelation::Trigger
        | CatalogRelation::Rewrite
        | CatalogRelation::AuthMembers
        | CatalogRelation::Opclass
        | CatalogRelation::Enum
        | CatalogRelation::Range => Vec::new(),
        CatalogRelation::Proc => proc_rows(),
        CatalogRelation::Settings => setting_rows(snapshot),
        CatalogRelation::Information(relation) => {
            crate::information_schema::rows(snapshot, relation)
        }
        CatalogRelation::SqlServer(relation) => crate::sqlserver_catalog::rows(snapshot, relation),
        CatalogRelation::Oracle(relation) => crate::oracle_dictionary::rows(snapshot, relation),
    })
}

fn referenced_tables(snapshot: &CatalogSnapshot) -> HashSet<String> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| entry.table())
        .flat_map(|table| table.foreign_keys.iter().map(|key| key.parent.clone()))
        .collect()
}

pub(crate) fn index_columns(table: &TableDefinition, index: IndexSource) -> &[usize] {
    match index {
        IndexSource::Primary => &table.primary_key,
        IndexSource::Secondary(at) => table
            .indexes
            .get(at)
            .map_or(&[][..], |found| found.columns.as_slice()),
    }
}

pub(crate) fn index_is_unique(table: &TableDefinition, index: IndexSource) -> bool {
    match index {
        IndexSource::Primary => true,
        IndexSource::Secondary(at) => table.indexes.get(at).is_some_and(|found| found.unique),
    }
}

pub(crate) fn index_method(table: &TableDefinition, index: IndexSource) -> IndexMethod {
    match index {
        IndexSource::Primary => IndexMethod::BTree,
        IndexSource::Secondary(at) => table
            .indexes
            .get(at)
            .map_or(IndexMethod::BTree, |found| found.method),
    }
}

pub(crate) fn index_sort_options(table: &TableDefinition, index: IndexSource) -> SortOptions {
    match index {
        IndexSource::Primary => SortOptions::default(),
        IndexSource::Secondary(at) => table
            .indexes
            .get(at)
            .map_or(SortOptions::default(), |found| found.sort_options),
    }
}

fn access_method_oid(method: IndexMethod) -> u32 {
    match method {
        IndexMethod::BTree => oids::BTREE,
        IndexMethod::Hash => HASH,
    }
}

fn identity_code(generation: Option<IdentityGeneration>) -> &'static str {
    match generation {
        Some(IdentityGeneration::Always) => "a",
        Some(IdentityGeneration::ByDefault) => "d",
        None => "",
    }
}

fn class_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let referenced = referenced_tables(snapshot);
    let mut rows = Vec::with_capacity(snapshot.relations.len());
    for entry in &snapshot.relations {
        let mut kind = entry.kind.code();
        let natts;
        let mut has_index = false;
        let mut checks = 0;
        let mut has_triggers = false;
        let mut has_rules = false;
        let mut am = 0;
        let mut replident = "n";
        let mut tuples: f32 = -1.0;
        let mut pages = 0;
        let mut file_node = entry.oid;
        match &entry.source {
            RelationSource::Table(table) => {
                natts = table.columns.len();
                has_index = !table.primary_key.is_empty() || !table.indexes.is_empty();
                checks = table.checks.len();
                has_triggers = !table.foreign_keys.is_empty()
                    || table.referenced
                    || referenced.contains(&table.name);
                am = oids::HEAP;
                replident = "d";
            }
            RelationSource::View(view) => {
                natts = view.columns.len();
                has_rules = true;
                file_node = 0;
            }
            RelationSource::Sequence(_) => {
                natts = 3;
                tuples = 1.0;
                pages = 1;
            }
            RelationSource::Index { table, index } => {
                let owner = snapshot
                    .relations
                    .get(*table)
                    .and_then(|owner| owner.table());
                natts = owner.map_or(0, |owner| index_columns(owner, *index).len());
                am = access_method_oid(
                    owner.map_or(IndexMethod::BTree, |owner| index_method(owner, *index)),
                );
                tuples = 0.0;
                pages = 1;
            }
            RelationSource::Catalog(relation) => {
                natts = relation.columns().len();
                if relation.is_view() {
                    kind = "v";
                    file_node = 0;
                } else {
                    am = oids::HEAP;
                    replident = "d";
                }
            }
        }
        rows.push(
            Row::new(CatalogRelation::Class)
                .set("oid", oid(entry.oid))
                .set("relname", text(&entry.name))
                .set("relnamespace", oid(entry.namespace))
                .set("reltype", oid(0))
                .set("reloftype", oid(0))
                .set("relowner", oid(oids::SESSION_USER))
                .set("relam", oid(am))
                .set("relfilenode", oid(file_node))
                .set("reltablespace", oid(0))
                .set("relpages", int(pages))
                .set("reltuples", Value::Real(tuples))
                .set("relallvisible", int(0))
                .set("reltoastrelid", oid(0))
                .set("relhasindex", flag(has_index))
                .set("relisshared", flag(false))
                .set("relpersistence", text("p"))
                .set("relkind", text(kind))
                .set("relnatts", small(natts as i64))
                .set("relchecks", small(checks as i64))
                .set("relhasrules", flag(has_rules))
                .set("relhastriggers", flag(has_triggers))
                .set("relhassubclass", flag(false))
                .set("relrowsecurity", flag(false))
                .set("relforcerowsecurity", flag(false))
                .set("relispopulated", flag(true))
                .set("relreplident", text(replident))
                .set("relispartition", flag(false))
                .set("relrewrite", oid(0))
                .set("relfrozenxid", oid(0))
                .set("relminmxid", oid(0))
                .done(),
        );
    }
    rows
}

struct AttributeRow<'a> {
    relation: u32,
    name: &'a str,
    number: usize,
    sql_type: SqlType,
    not_null: bool,
    has_default: bool,
    identity: &'a str,
}

fn attribute_row(attribute: AttributeRow<'_>) -> Vec<Value> {
    let facts = facts(attribute.sql_type);
    Row::new(CatalogRelation::Attribute)
        .set("attrelid", oid(attribute.relation))
        .set("attname", text(attribute.name))
        .set("atttypid", oid(facts.oid))
        .set("attlen", small(i64::from(facts.length)))
        .set("attnum", small(attribute.number as i64))
        .set("attcacheoff", int(-1))
        .set("atttypmod", int(i64::from(facts.modifier)))
        .set("attndims", small(0))
        .set("attbyval", flag(facts.by_value))
        .set("attalign", Value::Text(facts.align.to_string()))
        .set("attstorage", Value::Text(facts.storage.to_string()))
        .set("attcompression", text(""))
        .set("attnotnull", flag(attribute.not_null))
        .set("atthasdef", flag(attribute.has_default))
        .set("atthasmissing", flag(false))
        .set("attidentity", text(attribute.identity))
        .set("attgenerated", text(""))
        .set("attisdropped", flag(false))
        .set("attislocal", flag(true))
        .set("attinhcount", small(0))
        .set("attstattarget", small(-1))
        .set("attcollation", oid(facts.collation))
        .done()
}

fn attribute_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for entry in &snapshot.relations {
        let relation = entry.oid;
        match &entry.source {
            RelationSource::Table(table) => {
                for (at, column) in table.columns.iter().enumerate() {
                    rows.push(attribute_row(AttributeRow {
                        relation,
                        name: &column.name,
                        number: at + 1,
                        sql_type: column.sql_type,
                        not_null: column.not_null || table.primary_key.contains(&at),
                        has_default: column.default.is_some() && table.identity_of(at).is_none(),
                        identity: identity_code(table.identity_of(at)),
                    }));
                }
            }
            RelationSource::View(view) => {
                for (at, column) in view.columns.iter().enumerate() {
                    rows.push(attribute_row(AttributeRow {
                        relation,
                        name: &column.name,
                        number: at + 1,
                        sql_type: column.sql_type.unwrap_or(SqlType::Text),
                        not_null: false,
                        has_default: false,
                        identity: "",
                    }));
                }
            }
            RelationSource::Sequence(_) => {
                for (at, (name, sql_type)) in [
                    ("last_value", SqlType::BigInt),
                    ("log_cnt", SqlType::BigInt),
                    ("is_called", SqlType::Boolean),
                ]
                .into_iter()
                .enumerate()
                {
                    rows.push(attribute_row(AttributeRow {
                        relation,
                        name,
                        number: at + 1,
                        sql_type,
                        not_null: true,
                        has_default: false,
                        identity: "",
                    }));
                }
            }
            RelationSource::Index { table, index } => {
                let Some(owner) = snapshot
                    .relations
                    .get(*table)
                    .and_then(|owner| owner.table())
                else {
                    continue;
                };
                let hashed = index_method(owner, *index) == IndexMethod::Hash;
                for (at, column_at) in index_columns(owner, *index).iter().enumerate() {
                    let Some(column) = owner.columns.get(*column_at) else {
                        continue;
                    };
                    rows.push(attribute_row(AttributeRow {
                        relation,
                        name: &column.name,
                        number: at + 1,
                        sql_type: if hashed {
                            SqlType::Integer
                        } else {
                            column.sql_type
                        },
                        not_null: false,
                        has_default: false,
                        identity: "",
                    }));
                }
            }
            RelationSource::Catalog(catalog) => {
                for (at, column) in catalog.columns().iter().enumerate() {
                    rows.push(attribute_row(AttributeRow {
                        relation,
                        name: column.name,
                        number: at + 1,
                        sql_type: column.sql_type,
                        not_null: false,
                        has_default: false,
                        identity: "",
                    }));
                }
            }
        }
    }
    rows
}

fn type_rows() -> Vec<Vec<Value>> {
    oids::BUILTIN_TYPES
        .iter()
        .map(|found| {
            let mut row = Row::new(CatalogRelation::Type)
                .set("oid", oid(found.oid))
                .set("typname", text(found.typname))
                .set("typnamespace", oid(oids::PG_CATALOG))
                .set("typowner", oid(oids::SESSION_USER))
                .set("typlen", small(i64::from(found.length)))
                .set("typbyval", flag(found.by_value))
                .set("typtype", Value::Text(found.kind.to_string()))
                .set("typcategory", Value::Text(found.category.to_string()))
                .set("typispreferred", flag(found.preferred))
                .set("typisdefined", flag(true))
                .set("typdelim", text(","))
                .set("typrelid", oid(0))
                .set("typelem", oid(found.element))
                .set("typarray", oid(found.array))
                .set("typalign", Value::Text(found.align.to_string()))
                .set("typstorage", Value::Text(found.storage.to_string()))
                .set("typnotnull", flag(false))
                .set("typbasetype", oid(0))
                .set("typtypmod", int(-1))
                .set("typndims", int(0))
                .set("typcollation", oid(found.collation));
            for name in [
                "typsubscript",
                "typinput",
                "typoutput",
                "typreceive",
                "typsend",
                "typmodin",
                "typmodout",
                "typanalyze",
            ] {
                row = row.set(name, regproc_none());
            }
            if found.category == 'A' && found.element != 0 && !matches!(found.oid, 22 | 30) {
                for (column, function) in [
                    ("typinput", oids::ARRAY_IN),
                    ("typoutput", oids::ARRAY_OUT),
                    ("typreceive", oids::ARRAY_RECV),
                    ("typsend", oids::ARRAY_SEND),
                ] {
                    let name = oids::BUILTIN_FUNCTIONS
                        .iter()
                        .find(|builtin| builtin.oid == function)
                        .map_or("-", |builtin| builtin.name);
                    row = row.set(column, regproc(function, name));
                }
            }
            row.done()
        })
        .collect()
}

fn proc_rows() -> Vec<Vec<Value>> {
    oids::BUILTIN_FUNCTIONS
        .iter()
        .map(|function| {
            let arguments = function.argument_types.len() as i64;
            Row::new(CatalogRelation::Proc)
                .set("oid", oid(function.oid))
                .set("proname", text(function.name))
                .set("pronamespace", oid(oids::PG_CATALOG))
                .set("proowner", oid(oids::SESSION_USER))
                .set("prolang", oid(INTERNAL_LANGUAGE))
                .set("procost", Value::Real(1.0))
                .set("prorows", Value::Real(0.0))
                .set("provariadic", oid(0))
                .set("prosupport", regproc_none())
                .set("prokind", text("f"))
                .set("prosecdef", flag(false))
                .set("proleakproof", flag(false))
                .set("proisstrict", flag(true))
                .set("proretset", flag(false))
                .set("provolatile", text("s"))
                .set("proparallel", text("s"))
                .set("pronargs", small(arguments))
                .set("pronargdefaults", small(0))
                .set("prorettype", oid(function.result_type))
                .set(
                    "proargtypes",
                    vector(
                        ArrayElement::OidVector,
                        function.argument_types.iter().map(|&found| oid(found)).collect(),
                    ),
                )
                .set("prosrc", text(function.name))
                .done()
        })
        .collect()
}

fn namespace_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .schemas
        .iter()
        .map(|schema| {
            Row::new(CatalogRelation::Namespace)
                .set("oid", oid(schema.oid))
                .set("nspname", text(&schema.name))
                .set("nspowner", oid(schema.owner))
                .done()
        })
        .collect()
}

fn collation_of(table: &TableDefinition, column: usize) -> Value {
    oid(table
        .columns
        .get(column)
        .map_or(0, |column| facts(column.sql_type).collation))
}

fn index_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for entry in &snapshot.relations {
        let RelationSource::Index { table, index } = &entry.source else {
            continue;
        };
        let Some(owner_entry) = snapshot.relations.get(*table) else {
            continue;
        };
        let Some(owner) = owner_entry.table() else {
            continue;
        };
        let columns = index_columns(owner, *index);
        let count = columns.len() as i64;
        let sort_options = index_sort_options(owner, *index);
        rows.push(
            Row::new(CatalogRelation::Index)
                .set("indexrelid", oid(entry.oid))
                .set("indrelid", oid(owner_entry.oid))
                .set("indnatts", small(count))
                .set("indnkeyatts", small(count))
                .set("indisunique", flag(index_is_unique(owner, *index)))
                .set("indnullsnotdistinct", flag(false))
                .set("indisprimary", flag(*index == IndexSource::Primary))
                .set("indisexclusion", flag(false))
                .set("indimmediate", flag(true))
                .set("indisclustered", flag(false))
                .set("indisvalid", flag(true))
                .set("indcheckxmin", flag(false))
                .set("indisready", flag(true))
                .set("indislive", flag(true))
                .set("indisreplident", flag(false))
                .set(
                    "indkey",
                    vector(
                        ArrayElement::Int2Vector,
                        columns.iter().map(|at| small(*at as i64 + 1)).collect(),
                    ),
                )
                .set(
                    "indcollation",
                    vector(
                        ArrayElement::OidVector,
                        columns.iter().map(|at| collation_of(owner, *at)).collect(),
                    ),
                )
                .set(
                    "indclass",
                    vector(
                        ArrayElement::OidVector,
                        columns.iter().map(|_| oid(0)).collect(),
                    ),
                )
                .set(
                    "indoption",
                    vector(
                        ArrayElement::Int2Vector,
                        (0..columns.len())
                            .map(|position| {
                                let mut option = 0;
                                if sort_options.is_descending(position) {
                                    option |= INDOPTION_DESC;
                                }
                                if sort_options.has_nulls_first(position) {
                                    option |= INDOPTION_NULLS_FIRST;
                                }
                                small(option)
                            })
                            .collect(),
                    ),
                )
                .done(),
        );
    }
    rows
}

fn action_code(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "a",
        ReferentialAction::Restrict => "r",
        ReferentialAction::Cascade => "c",
        ReferentialAction::SetNull => "n",
        ReferentialAction::SetDefault => "d",
    }
}

pub(crate) fn referenced_index(
    snapshot: &CatalogSnapshot,
    parent: usize,
    columns: &[String],
) -> Option<usize> {
    let table = snapshot.relations.get(parent)?.table()?;
    let positions: Vec<usize> = columns
        .iter()
        .map(|name| table.column_index(name))
        .collect::<Option<_>>()?;
    let wanted = if table.primary_key == positions {
        IndexSource::Primary
    } else {
        IndexSource::Secondary(
            table
                .indexes
                .iter()
                .position(|index| index.unique && index.columns == positions)?,
        )
    };
    snapshot.index_relation(parent, wanted)
}

fn constraint_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::with_capacity(snapshot.constraints.len());
    for constraint in &snapshot.constraints {
        let Some(entry) = snapshot.relations.get(constraint.table) else {
            continue;
        };
        let Some(table) = entry.table() else {
            continue;
        };
        let mut row = Row::new(CatalogRelation::Constraint)
            .set("oid", oid(constraint.oid))
            .set("conname", text(&constraint.name))
            .set("connamespace", oid(entry.namespace))
            .set("contype", text(constraint.source.code()))
            .set("condeferrable", flag(false))
            .set("condeferred", flag(false))
            .set("convalidated", flag(true))
            .set("conrelid", oid(entry.oid))
            .set("contypid", oid(0))
            .set(
                "conindid",
                oid(constraint
                    .index_relation
                    .and_then(|at| snapshot.relations.get(at))
                    .map_or(0, |index| index.oid)),
            )
            .set("conparentid", oid(0))
            .set("confrelid", oid(0))
            .set("confupdtype", text(" "))
            .set("confdeltype", text(" "))
            .set("confmatchtype", text(" "))
            .set("conislocal", flag(true))
            .set("coninhcount", small(0))
            .set("connoinherit", flag(true));
        match constraint.source {
            ConstraintSource::Primary => {
                row = row.set("conkey", smallints(&table.primary_key));
            }
            ConstraintSource::Unique(at) => {
                if let Some(index) = table.indexes.get(at) {
                    row = row.set("conkey", smallints(&index.columns));
                }
            }
            ConstraintSource::Check(at) => {
                row = row.set("connoinherit", flag(false)).set(
                    "conbin",
                    Value::Text(deparse::check_text(snapshot, constraint.table, at, true)),
                );
            }
            ConstraintSource::Foreign(at) => {
                if let Some(key) = table.foreign_keys.get(at) {
                    let parent = snapshot.table_named(&key.parent);
                    let parent_positions: Vec<usize> = parent
                        .and_then(|parent| snapshot.relations.get(parent))
                        .and_then(|parent| parent.table())
                        .map(|parent| {
                            key.parent_columns
                                .iter()
                                .filter_map(|name| parent.column_index(name))
                                .collect()
                        })
                        .unwrap_or_default();
                    let index = parent
                        .and_then(|parent| referenced_index(snapshot, parent, &key.parent_columns));
                    row = row
                        .set("condeferrable", flag(key.deferrable))
                        .set(
                            "conindid",
                            oid(index
                                .and_then(|at| snapshot.relations.get(at))
                                .map_or(0, |index| index.oid)),
                        )
                        .set(
                            "confrelid",
                            oid(parent
                                .and_then(|at| snapshot.relations.get(at))
                                .map_or(0, |parent| parent.oid)),
                        )
                        .set("confupdtype", text(action_code(key.on_update)))
                        .set("confdeltype", text(action_code(key.on_delete)))
                        .set("confmatchtype", text("s"))
                        .set("conkey", smallints(&key.columns))
                        .set("confkey", smallints(&parent_positions));
                }
            }
        }
        rows.push(row.done());
    }
    rows
}

fn attrdef_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .defaults
        .iter()
        .filter_map(|default| {
            let entry = snapshot.relations.get(default.table)?;
            Some(
                Row::new(CatalogRelation::AttrDef)
                    .set("oid", oid(default.oid))
                    .set("adrelid", oid(entry.oid))
                    .set("adnum", small(default.column as i64 + 1))
                    .set(
                        "adbin",
                        Value::Text(deparse::default_text(
                            snapshot,
                            default.table,
                            default.column,
                        )),
                    )
                    .done(),
            )
        })
        .collect()
}

fn description(object: u32, class: u32, sub: i64, comment: &str) -> Vec<Value> {
    Row::new(CatalogRelation::Description)
        .set("objoid", oid(object))
        .set("classoid", oid(class))
        .set("objsubid", int(sub))
        .set("description", text(comment))
        .done()
}

fn column_number(snapshot: &CatalogSnapshot, at: usize, column: &str) -> Option<usize> {
    let entry = snapshot.relations.get(at)?;
    match &entry.source {
        RelationSource::Table(table) => table.column_index(column),
        RelationSource::View(view) => view.columns.iter().position(|found| found.name == column),
        _ => None,
    }
    .map(|position| position + 1)
}

fn description_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for comment in &snapshot.comments {
        let row = match comment.kind {
            CommentKind::Schema => snapshot
                .schema_named(&comment.part)
                .map(|schema| description(schema.oid, PG_NAMESPACE, 0, &comment.text)),
            CommentKind::Relation => snapshot
                .find_internal(&comment.owner)
                .and_then(|at| snapshot.relations.get(at))
                .map(|entry| description(entry.oid, PG_CLASS, 0, &comment.text)),
            CommentKind::Column => snapshot.find_internal(&comment.owner).and_then(|at| {
                let number = column_number(snapshot, at, &comment.part)?;
                let entry = snapshot.relations.get(at)?;
                Some(description(
                    entry.oid,
                    PG_CLASS,
                    number as i64,
                    &comment.text,
                ))
            }),
            CommentKind::Index => {
                let schema = crate::schemas::schema_of(&comment.owner);
                snapshot
                    .find_relation(Some(schema), &comment.part)
                    .and_then(|at| snapshot.relations.get(at))
                    .filter(|entry| entry.kind == RelationKind::Index)
                    .map(|entry| description(entry.oid, PG_CLASS, 0, &comment.text))
            }
            CommentKind::Constraint => snapshot.find_internal(&comment.owner).and_then(|at| {
                snapshot
                    .constraints
                    .iter()
                    .find(|constraint| constraint.table == at && constraint.name == comment.part)
                    .map(|constraint| description(constraint.oid, PG_CONSTRAINT, 0, &comment.text))
            }),
        };
        rows.extend(row);
    }
    rows
}

fn database_row(snapshot: &CatalogSnapshot) -> Vec<Value> {
    Row::new(CatalogRelation::Database)
        .set("oid", oid(oids::DATABASE))
        .set("datname", text(&snapshot.database))
        .set("datdba", oid(oids::SESSION_USER))
        .set("encoding", int(i64::from(oids::UTF8_ENCODING)))
        .set("datlocprovider", text("c"))
        .set("datistemplate", flag(false))
        .set("datallowconn", flag(true))
        .set("datconnlimit", int(-1))
        .set("datfrozenxid", oid(0))
        .set("datminmxid", oid(0))
        .set("dattablespace", oid(oids::DEFAULT_TABLESPACE))
        .set("datcollate", text("C"))
        .set("datctype", text("C"))
        .done()
}

fn role_row(name: &str, role: u32, everything: bool) -> Vec<Value> {
    Row::new(CatalogRelation::Roles)
        .set("rolname", text(name))
        .set("rolsuper", flag(everything))
        .set("rolinherit", flag(true))
        .set("rolcreaterole", flag(everything))
        .set("rolcreatedb", flag(everything))
        .set("rolcanlogin", flag(everything))
        .set("rolreplication", flag(everything))
        .set("rolconnlimit", int(-1))
        .set("rolpassword", text("********"))
        .set("rolbypassrls", flag(everything))
        .set("oid", oid(role))
        .done()
}

fn role_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    let mut rows = vec![role_row(&snapshot.user, oids::SESSION_USER, true)];
    for (role, name) in oids::PREDEFINED_ROLES {
        if name != snapshot.user {
            rows.push(role_row(name, role, false));
        }
    }
    rows
}

fn am_rows() -> Vec<Vec<Value>> {
    [
        (oids::HEAP, "heap", "t"),
        (oids::BTREE, "btree", "i"),
        (HASH, "hash", "i"),
    ]
    .into_iter()
    .map(|(id, name, kind)| {
        Row::new(CatalogRelation::Am)
            .set("oid", oid(id))
            .set("amname", text(name))
            .set("amhandler", regproc_none())
            .set("amtype", text(kind))
            .done()
    })
    .collect()
}

fn collation_rows() -> Vec<Vec<Value>> {
    [
        (oids::DEFAULT_COLLATION, "default", "d", -1, None),
        (oids::C_COLLATION, "C", "c", -1, Some("C")),
        (951, "POSIX", "c", -1, Some("POSIX")),
        (962, "ucs_basic", "c", 6, Some("C")),
    ]
    .into_iter()
    .map(|(id, name, provider, encoding, locale)| {
        let locale = locale.map_or(Value::Null, text);
        Row::new(CatalogRelation::Collation)
            .set("oid", oid(id))
            .set("collname", text(name))
            .set("collnamespace", oid(oids::PG_CATALOG))
            .set("collowner", oid(oids::SESSION_USER))
            .set("collprovider", text(provider))
            .set("collisdeterministic", flag(true))
            .set("collencoding", int(encoding))
            .set("collcollate", locale.clone())
            .set("collctype", locale)
            .done()
    })
    .collect()
}

fn sequence_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| {
            let RelationSource::Sequence(sequence) = &entry.source else {
                return None;
            };
            Some(
                Row::new(CatalogRelation::Sequence)
                    .set("seqrelid", oid(entry.oid))
                    .set("seqtypid", oid(oids::type_oid(sequence.sql_type)))
                    .set("seqstart", Value::BigInt(sequence.start))
                    .set("seqincrement", Value::BigInt(sequence.increment))
                    .set("seqmax", Value::BigInt(sequence.max))
                    .set("seqmin", Value::BigInt(sequence.min))
                    .set("seqcache", Value::BigInt(sequence.cache))
                    .set("seqcycle", flag(sequence.cycle))
                    .done(),
            )
        })
        .collect()
}

fn depend_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .relations
        .iter()
        .filter_map(|entry| {
            let RelationSource::Sequence(sequence) = &entry.source else {
                return None;
            };
            let (table, column) = sequence.owned_by.as_ref()?;
            let owner_at = snapshot.table_named(table)?;
            let owner = snapshot.relations.get(owner_at)?;
            let owner_table = owner.table()?;
            let position = owner_table.column_index(column)?;
            let number = position + 1;
            let dependency = if owner_table.identity_of(position).is_some() {
                "i"
            } else {
                "a"
            };
            Some(
                Row::new(CatalogRelation::Depend)
                    .set("classid", oid(PG_CLASS))
                    .set("objid", oid(entry.oid))
                    .set("objsubid", int(0))
                    .set("refclassid", oid(PG_CLASS))
                    .set("refobjid", oid(owner.oid))
                    .set("refobjsubid", int(number as i64))
                    .set("deptype", text(dependency))
                    .done(),
            )
        })
        .collect()
}

fn tablespace_rows() -> Vec<Vec<Value>> {
    [
        (oids::DEFAULT_TABLESPACE, "pg_default"),
        (1664, "pg_global"),
    ]
    .into_iter()
    .map(|(id, name)| {
        Row::new(CatalogRelation::Tablespace)
            .set("oid", oid(id))
            .set("spcname", text(name))
            .set("spcowner", oid(oids::SESSION_USER))
            .done()
    })
    .collect()
}

fn setting_rows(snapshot: &CatalogSnapshot) -> Vec<Vec<Value>> {
    snapshot
        .settings
        .all()
        .into_iter()
        .map(|(name, setting, description)| {
            let context = if crate::settings::Settings::changeable(&name) {
                "user"
            } else {
                "internal"
            };
            Row::new(CatalogRelation::Settings)
                .set("name", text(&name))
                .set("setting", text(&setting))
                .set("short_desc", text(&description))
                .set("context", text(context))
                .set("vartype", text("string"))
                .set("source", text("default"))
                .set("boot_val", text(&setting))
                .set("reset_val", text(&setting))
                .set("pending_restart", flag(false))
                .done()
        })
        .collect()
}
