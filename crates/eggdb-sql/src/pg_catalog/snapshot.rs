use super::CatalogRelation;
use super::oids::{self, OidRequest, Space};
use super::types::{RegKind, RegValue, split_object_name};
use crate::comments::StoredComment;
use crate::encoding::{
    SequenceDefinition, TableDefinition, ViewDefinition, prefix_end, schema_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};
use crate::pg_keywords::quote_ident;
use crate::routines::{NamedRoutines, ReturnType, RoutineDefinition, RoutineReturns};
use crate::settings::Settings;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::Value;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

const SNAPSHOT_FUNCTIONS: [&str; 22] = [
    "pg_get_serial_sequence",
    "pg_get_functiondef",
    "pg_get_function_arguments",
    "pg_get_function_identity_arguments",
    "pg_get_function_result",
    "pg_function_is_visible",
    "to_regclass",
    "pg_table_is_visible",
    "pg_relation_is_publishable",
    "pg_type_is_visible",
    "pg_get_expr",
    "pg_get_constraintdef",
    "pg_get_indexdef",
    "pg_get_viewdef",
    "pg_get_triggerdef",
    "pg_get_partkeydef",
    "obj_description",
    "col_description",
    "regtype",
    "regnamespace",
    "regrole",
    "regproc",
];

const SEQUENCE_FUNCTIONS: [&str; 3] = ["nextval", "currval", "setval"];

const CATALOG_TEXT_HINTS: [&str; 3] = ["reg", "pg_", "description"];

pub(crate) fn text_needs_catalog(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    CATALOG_TEXT_HINTS.iter().any(|hint| lower.contains(hint))
        && crate::lexer::tokenize(text).is_ok_and(|tokens| mentions_catalog(&tokens))
}

pub(crate) fn mentions_catalog(tokens: &[Token<'_>]) -> bool {
    let mut depth: usize = 0;
    let mut sequence_call_depths: Vec<usize> = Vec::new();
    let mut previous_sequence_function = false;
    for token in tokens {
        match &token.kind {
            TokenKind::LeftParen => {
                depth += 1;
                if previous_sequence_function {
                    sequence_call_depths.push(depth);
                }
            }
            TokenKind::RightParen => {
                if sequence_call_depths.last() == Some(&depth) {
                    sequence_call_depths.pop();
                }
                depth = depth.saturating_sub(1);
            }
            TokenKind::QuotedIdentifier(word)
                if matches!(
                    super::cast_type_named(word),
                    Some(crate::types::SqlType::Catalog(super::CatalogType::Reg(_)))
                ) && sequence_call_depths.is_empty() =>
            {
                return true;
            }
            TokenKind::Word(word) => {
                let named = |name: &&str| word.eq_ignore_ascii_case(name);
                if SNAPSHOT_FUNCTIONS.iter().any(named) {
                    return true;
                }
                if word.eq_ignore_ascii_case("regclass") && sequence_call_depths.is_empty() {
                    return true;
                }
                previous_sequence_function = SEQUENCE_FUNCTIONS.iter().any(named);
                continue;
            }
            _ => {}
        }
        previous_sequence_function = false;
    }
    false
}

struct Frame {
    wanted: bool,
    user: String,
    database: String,
    settings: Arc<Settings>,
    snapshot: Option<Rc<CatalogSnapshot>>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
thread_local! {
    pub(crate) static LOADS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    pub(crate) static BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(crate) struct EnteredStatement {
    depth: usize,
}

impl Drop for EnteredStatement {
    fn drop(&mut self) {
        FRAMES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter_statement(wanted: bool, settings: &Arc<Settings>) -> EnteredStatement {
    let frame = Frame {
        wanted,
        user: settings.get("session_authorization").unwrap_or_default(),
        database: settings.database.clone(),
        settings: Arc::clone(settings),
        snapshot: None,
    };
    let depth = FRAMES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let depth = frames.len();
            frames.push(frame);
            depth
        }
        Err(_) => usize::MAX,
    });
    EnteredStatement { depth }
}

pub(crate) fn statement_settings() -> Arc<Settings> {
    FRAMES
        .with(|frames| {
            frames
                .try_borrow()
                .ok()
                .and_then(|frames| frames.last().map(|frame| Arc::clone(&frame.settings)))
        })
        .unwrap_or_default()
}

pub(crate) fn load_if_wanted(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let wanted = FRAMES.with(|frames| {
        frames.try_borrow().ok().is_some_and(|frames| {
            frames
                .last()
                .is_some_and(|frame| frame.wanted && frame.snapshot.is_none())
        })
    });
    if wanted {
        snapshot_for(reader)?;
    }
    Ok(())
}

pub(crate) fn load_for_stored_expressions(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let missing = FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .is_some_and(|frames| frames.last().is_some_and(|frame| frame.snapshot.is_none()))
    });
    if missing {
        snapshot_for(reader)?;
    }
    Ok(())
}

pub(crate) fn snapshot_for(reader: &dyn KeyValueRead) -> SqlResult<Rc<CatalogSnapshot>> {
    if let Some(found) = current() {
        return Ok(found);
    }
    let identity = FRAMES.with(|frames| {
        frames.try_borrow().ok().and_then(|frames| {
            frames.last().map(|frame| {
                (
                    frame.user.clone(),
                    frame.database.clone(),
                    Arc::clone(&frame.settings),
                )
            })
        })
    });
    let (user, database, settings) = identity.unwrap_or_else(|| {
        (
            "eggdb".to_owned(),
            "eggdb".to_owned(),
            Arc::new(Settings::default()),
        )
    });
    let loaded = Rc::new(CatalogSnapshot::load(reader, user, database, settings)?);
    FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(frame) = frames.last_mut()
        {
            frame.snapshot = Some(Rc::clone(&loaded));
        }
    });
    Ok(loaded)
}

pub(crate) fn session_user() -> Option<String> {
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().map(|frame| frame.user.clone()))
    })
}

pub(crate) fn current() -> Option<Rc<CatalogSnapshot>> {
    FRAMES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().and_then(|frame| frame.snapshot.clone()))
    })
}

pub(crate) fn require() -> SqlResult<Rc<CatalogSnapshot>> {
    current().ok_or_else(|| {
        SqlError::not_supported("the PostgreSQL catalog is not available in this statement")
            .with_hint("Name a catalog relation or function in the statement itself.")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelationKind {
    Table,
    View,
    Sequence,
    Index,
    Catalog,
}

impl RelationKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Table | Self::Catalog => "r",
            Self::View => "v",
            Self::Sequence => "S",
            Self::Index => "i",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IndexSource {
    Primary,
    Secondary(usize),
}

pub(crate) enum RelationSource {
    Table(Arc<TableDefinition>),
    View(Arc<ViewDefinition>),
    Sequence(Box<SequenceDefinition>),
    Index { table: usize, index: IndexSource },
    Catalog(CatalogRelation),
}

pub(crate) struct RelationEntry {
    pub(crate) oid: u32,
    pub(crate) namespace: u32,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) internal: String,
    pub(crate) kind: RelationKind,
    pub(crate) source: RelationSource,
}

impl RelationEntry {
    pub(crate) fn table(&self) -> Option<&TableDefinition> {
        match &self.source {
            RelationSource::Table(table) => Some(table),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintSource {
    Primary,
    Unique(usize),
    Check(usize),
    Foreign(usize),
}

impl ConstraintSource {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Primary => "p",
            Self::Unique(_) => "u",
            Self::Check(_) => "c",
            Self::Foreign(_) => "f",
        }
    }
}

pub(crate) struct ConstraintEntry {
    pub(crate) oid: u32,
    pub(crate) name: String,
    pub(crate) table: usize,
    pub(crate) index_relation: Option<usize>,
    pub(crate) source: ConstraintSource,
}

pub(crate) struct DefaultEntry {
    pub(crate) oid: u32,
    pub(crate) table: usize,
    pub(crate) column: usize,
}

pub(crate) struct SchemaEntry {
    pub(crate) oid: u32,
    pub(crate) name: String,
    pub(crate) owner: u32,
}

pub(crate) struct TriggerEntry {
    pub(crate) oid: u32,
    pub(crate) relation: usize,
    pub(crate) definition: crate::triggers::TriggerDefinition,
}

pub(crate) struct RoutineEntry {
    pub(crate) oid: u32,
    pub(crate) namespace: u32,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) definition: Arc<RoutineDefinition>,
}

pub(crate) struct PackageEntry {
    pub(crate) oid: u32,
    pub(crate) body_oid: u32,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) stored: crate::plsql::StoredPackage,
}

pub(crate) struct CatalogSnapshot {
    pub(crate) user: String,
    pub(crate) database: String,
    pub(crate) settings: Arc<Settings>,
    pub(crate) path: Vec<String>,
    pub(crate) schemas: Vec<SchemaEntry>,
    pub(crate) relations: Vec<RelationEntry>,
    pub(crate) constraints: Vec<ConstraintEntry>,
    pub(crate) defaults: Vec<DefaultEntry>,
    pub(crate) comments: Vec<StoredComment>,
    pub(crate) triggers: Vec<TriggerEntry>,
    pub(crate) routines: Vec<RoutineEntry>,
    pub(crate) packages: Vec<PackageEntry>,
    relation_oids: HashMap<u32, usize>,
    relation_names: HashMap<(String, String), usize>,
    constraint_oids: HashMap<u32, usize>,
    schema_names: HashMap<String, usize>,
    tables: HashMap<String, usize>,
    index_entries: HashMap<(usize, IndexSource), usize>,
    rows: RefCell<HashMap<CatalogRelation, Rc<Vec<Vec<Value>>>>>,
    sqlserver: OnceCell<crate::sqlserver_catalog::SqlServerObjects>,
    pub(crate) user_types: Arc<crate::user_types::Registry>,
    pub(crate) partitions: crate::partitions::PartitionCatalog,
    pub(crate) row_counts: std::collections::HashMap<String, u64>,
    pub(crate) stored_roles: Vec<crate::roles::StoredRole>,
}

fn user_schema_names(reader: &dyn KeyValueRead) -> SqlResult<Vec<String>> {
    let prefix = schema_key("");
    let mut names = Vec::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, _| {
            if let Some(name) = key
                .get(prefix.len()..)
                .and_then(|name| std::str::from_utf8(name).ok())
                && !name.is_empty()
            {
                names.push(name.to_owned());
            }
            Ok(true)
        },
    )?;
    Ok(names)
}

fn relation_request(kind: RelationKind, owner: &str, internal: &str) -> OidRequest {
    OidRequest {
        kind: kind.code().as_bytes()[0],
        owner: owner.to_owned(),
        name: internal.to_owned(),
    }
}

struct PendingRelation {
    schema: String,
    name: String,
    internal: String,
    kind: RelationKind,
    owner: String,
    source: RelationSource,
}

impl CatalogSnapshot {
    fn load(
        reader: &dyn KeyValueRead,
        user: String,
        database: String,
        settings: Arc<Settings>,
    ) -> SqlResult<Self> {
        #[cfg(test)]
        LOADS.with(|loads| loads.set(loads.get() + 1));
        let path = crate::schemas::relation_path(reader)?;
        crate::user_types::ensure_loaded(reader)?;
        let user_types = crate::user_types::every_type(reader)?;
        let mut schema_names: Vec<String> = user_schema_names(reader)?;
        schema_names.sort();
        let schema_oids = oids::assign(
            Space::Namespace,
            &schema_names
                .iter()
                .map(|name| OidRequest {
                    kind: b'n',
                    owner: String::new(),
                    name: name.clone(),
                })
                .collect::<Vec<_>>(),
        );
        let mut schemas: Vec<SchemaEntry> = oids::SYSTEM_SCHEMAS
            .iter()
            .map(|(oid, name, owner)| SchemaEntry {
                oid: *oid,
                name: (*name).to_owned(),
                owner: *owner,
            })
            .collect();
        for (name, oid) in schema_names.into_iter().zip(schema_oids) {
            if !schemas.iter().any(|known| known.name == name) {
                schemas.push(SchemaEntry {
                    oid,
                    name,
                    owner: oids::SESSION_USER,
                });
            }
        }
        let schema_of = |internal: &str| crate::schemas::schema_of(internal).to_owned();
        let known = |schema: &str| schemas.iter().any(|known| known.name == schema);
        let mut pending: Vec<PendingRelation> = Vec::new();
        let mut tables = crate::indexes::every_table(reader)?;
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        for table in tables {
            let table = Arc::new(table);
            let schema = schema_of(&table.name);
            if !known(&schema) {
                continue;
            }
            let table_at = pending.len();
            let internal = table.name.clone();
            pending.push(PendingRelation {
                schema: schema.clone(),
                name: crate::schemas::base_name(&table.name).to_owned(),
                internal: internal.clone(),
                kind: RelationKind::Table,
                owner: String::new(),
                source: RelationSource::Table(Arc::clone(&table)),
            });
            if !table.primary_key.is_empty() {
                let name = table.primary_key_constraint_name();
                pending.push(PendingRelation {
                    schema: schema.clone(),
                    internal: crate::schemas::qualified(&schema, &name),
                    name,
                    kind: RelationKind::Index,
                    owner: internal.clone(),
                    source: RelationSource::Index {
                        table: table_at,
                        index: IndexSource::Primary,
                    },
                });
            }
            for (position, index) in table.indexes.iter().enumerate() {
                pending.push(PendingRelation {
                    schema: schema.clone(),
                    internal: crate::schemas::qualified(&schema, &index.name),
                    name: index.name.clone(),
                    kind: RelationKind::Index,
                    owner: internal.clone(),
                    source: RelationSource::Index {
                        table: table_at,
                        index: IndexSource::Secondary(position),
                    },
                });
            }
        }
        for view in crate::views::every_view(reader)? {
            if !known(crate::schemas::schema_of(&view.name)) {
                continue;
            }
            pending.push(PendingRelation {
                schema: schema_of(&view.name),
                name: crate::schemas::base_name(&view.name).to_owned(),
                internal: view.name.clone(),
                kind: RelationKind::View,
                owner: String::new(),
                source: RelationSource::View(Arc::new(view)),
            });
        }
        for (name, sequence) in crate::sequences::every_sequence(reader)? {
            if !known(crate::schemas::schema_of(&name)) {
                continue;
            }
            pending.push(PendingRelation {
                schema: schema_of(&name),
                name: crate::schemas::base_name(&name).to_owned(),
                internal: name,
                kind: RelationKind::Sequence,
                owner: String::new(),
                source: RelationSource::Sequence(Box::new(sequence)),
            });
        }
        let assigned = oids::assign(
            Space::Relation,
            &pending
                .iter()
                .map(|entry| relation_request(entry.kind, &entry.owner, &entry.internal))
                .collect::<Vec<_>>(),
        );
        let namespace_of = |schema: &str| {
            schemas
                .iter()
                .find(|known| known.name == schema)
                .map_or(oids::PUBLIC, |known| known.oid)
        };
        let mut relations: Vec<RelationEntry> = pending
            .into_iter()
            .zip(assigned)
            .map(|(entry, oid)| RelationEntry {
                oid,
                namespace: namespace_of(&entry.schema),
                schema: entry.schema,
                name: entry.name,
                internal: entry.internal,
                kind: entry.kind,
                source: entry.source,
            })
            .collect();
        for relation in CatalogRelation::every() {
            let schema = relation.schema();
            relations.push(RelationEntry {
                oid: relation.oid(),
                namespace: namespace_of(schema),
                schema: schema.to_owned(),
                name: relation.name().to_owned(),
                internal: crate::schemas::qualified(schema, relation.name()),
                kind: RelationKind::Catalog,
                source: RelationSource::Catalog(relation),
            });
        }
        let mut snapshot = Self {
            user,
            database,
            settings,
            path,
            schemas,
            relations,
            constraints: Vec::new(),
            defaults: Vec::new(),
            comments: crate::comments::every_comment(reader)?,
            triggers: Vec::new(),
            routines: Vec::new(),
            packages: Vec::new(),
            relation_oids: HashMap::new(),
            relation_names: HashMap::new(),
            constraint_oids: HashMap::new(),
            schema_names: HashMap::new(),
            tables: HashMap::new(),
            index_entries: HashMap::new(),
            rows: RefCell::new(HashMap::new()),
            sqlserver: OnceCell::new(),
            user_types,
            partitions: crate::partitions::load(reader)?,
            row_counts: crate::statistics::every_row_count(reader)?,
            stored_roles: crate::roles::stored_roles(reader)?,
        };
        snapshot.index_relations();
        snapshot.add_constraints_and_defaults();
        snapshot.add_triggers(crate::triggers::every_trigger(reader)?);
        snapshot.add_routines(crate::routines::every_routine(reader)?);
        snapshot.add_packages(crate::plsql::every_package(reader)?);
        Ok(snapshot)
    }

    fn add_packages(&mut self, stored: Vec<(String, crate::plsql::StoredPackage)>) {
        let requests: Vec<OidRequest> = stored
            .iter()
            .flat_map(|(internal, _)| {
                [b'P', b'B'].map(|kind| OidRequest {
                    kind,
                    owner: String::new(),
                    name: internal.clone(),
                })
            })
            .collect();
        let mut assigned = oids::assign(Space::Package, &requests).into_iter();
        for (internal, stored) in stored {
            let oid = assigned.next().unwrap_or(0);
            let body_oid = assigned.next().unwrap_or(0);
            let schema = crate::schemas::schema_of(&internal).to_owned();
            if self.schema_named(&schema).is_none() {
                continue;
            }
            self.packages.push(PackageEntry {
                oid,
                body_oid,
                schema,
                name: crate::schemas::base_name(&internal).to_owned(),
                stored,
            });
        }
    }

    fn add_routines(&mut self, named: Vec<NamedRoutines>) {
        let mut assigned = oids::assign(Space::Routine, &routine_requests(&named)).into_iter();
        for listed in named {
            let schema = crate::schemas::schema_of(&listed.name).to_owned();
            let namespace = self.schema_named(&schema).map(|entry| entry.oid);
            for definition in listed.routines {
                let oid = assigned.next().unwrap_or(0);
                let Some(namespace) = namespace else {
                    continue;
                };
                self.routines.push(RoutineEntry {
                    oid,
                    namespace,
                    schema: schema.clone(),
                    name: crate::schemas::base_name(&listed.name).to_owned(),
                    definition,
                });
            }
        }
    }

    fn add_triggers(&mut self, stored: Vec<crate::triggers::TableTriggers>) {
        let owners: HashMap<&str, usize> = self
            .relations
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry.kind, RelationKind::Table | RelationKind::View))
            .map(|(at, entry)| (entry.internal.as_str(), at))
            .collect();
        let mut pending = Vec::new();
        for listed in stored {
            let Some(&relation) = owners.get(listed.table.as_str()) else {
                continue;
            };
            for definition in listed.triggers {
                pending.push((relation, definition));
            }
        }
        let requests: Vec<OidRequest> = pending
            .iter()
            .map(|(relation, definition)| OidRequest {
                kind: b'T',
                owner: self.relations[*relation].internal.clone(),
                name: definition.name.clone(),
            })
            .collect();
        let assigned = oids::assign(Space::Trigger, &requests);
        self.triggers = pending
            .into_iter()
            .zip(assigned)
            .map(|((relation, definition), oid)| TriggerEntry {
                oid,
                relation,
                definition,
            })
            .collect();
    }

    fn index_relations(&mut self) {
        for (at, entry) in self.relations.iter().enumerate() {
            self.relation_oids.insert(entry.oid, at);
            self.relation_names
                .entry((entry.schema.clone(), entry.name.clone()))
                .or_insert(at);
            if entry.kind == RelationKind::Table {
                self.tables.insert(entry.internal.clone(), at);
            }
            if let RelationSource::Index { table, index } = entry.source {
                self.index_entries.insert((table, index), at);
            }
        }
        for (at, schema) in self.schemas.iter().enumerate() {
            self.schema_names.insert(schema.name.clone(), at);
        }
    }

    fn add_constraints_and_defaults(&mut self) {
        let mut pending: Vec<(OidRequest, String, usize, Option<usize>, ConstraintSource)> =
            Vec::new();
        let mut defaults: Vec<(OidRequest, usize, usize)> = Vec::new();
        for (at, entry) in self.relations.iter().enumerate() {
            let Some(table) = entry.table() else {
                continue;
            };
            let index_relation = |wanted: IndexSource| self.index_relation(at, wanted);
            let mut add = |kind: u8, name: String, index: Option<usize>, source| {
                pending.push((
                    OidRequest {
                        kind,
                        owner: entry.internal.clone(),
                        name: name.clone(),
                    },
                    name,
                    at,
                    index,
                    source,
                ));
            };
            if !table.primary_key.is_empty() {
                add(
                    b'p',
                    table.primary_key_constraint_name(),
                    index_relation(IndexSource::Primary),
                    ConstraintSource::Primary,
                );
            }
            for (position, index) in table.indexes.iter().enumerate() {
                if index.unique && index.backs_unique_constraint {
                    add(
                        b'u',
                        index.name.clone(),
                        index_relation(IndexSource::Secondary(position)),
                        ConstraintSource::Unique(position),
                    );
                }
            }
            for (position, check) in table.checks.iter().enumerate() {
                add(
                    b'c',
                    check.name.clone(),
                    None,
                    ConstraintSource::Check(position),
                );
            }
            for (position, key) in table.foreign_keys.iter().enumerate() {
                add(
                    b'f',
                    key.name.clone(),
                    None,
                    ConstraintSource::Foreign(position),
                );
            }
            for (column_at, column) in table.columns.iter().enumerate() {
                if column.default.is_some() && table.identity_of(column_at).is_none() {
                    defaults.push((
                        OidRequest {
                            kind: b'd',
                            owner: entry.internal.clone(),
                            name: column.name.clone(),
                        },
                        at,
                        column_at,
                    ));
                }
            }
        }
        let requests: Vec<OidRequest> = pending
            .iter()
            .map(|(request, ..)| OidRequest {
                kind: request.kind,
                owner: request.owner.clone(),
                name: request.name.clone(),
            })
            .collect();
        let constraint_oids = oids::assign(Space::Constraint, &requests);
        for ((_, name, table, index_relation, source), oid) in
            pending.into_iter().zip(constraint_oids)
        {
            self.constraint_oids.insert(oid, self.constraints.len());
            self.constraints.push(ConstraintEntry {
                oid,
                name,
                table,
                index_relation,
                source,
            });
        }
        let default_requests: Vec<OidRequest> = defaults
            .iter()
            .map(|(request, ..)| OidRequest {
                kind: request.kind,
                owner: request.owner.clone(),
                name: request.name.clone(),
            })
            .collect();
        let default_oids = oids::assign(Space::AttrDef, &default_requests);
        self.defaults = defaults
            .into_iter()
            .zip(default_oids)
            .map(|((_, table, column), oid)| DefaultEntry { oid, table, column })
            .collect();
    }

    pub(crate) fn relation(&self, oid: u32) -> Option<&RelationEntry> {
        self.relation_oids
            .get(&oid)
            .and_then(|&at| self.relations.get(at))
    }

    pub(crate) fn index_relation(&self, table: usize, index: IndexSource) -> Option<usize> {
        self.index_entries.get(&(table, index)).copied()
    }

    pub(crate) fn relation_index(&self, oid: u32) -> Option<usize> {
        self.relation_oids.get(&oid).copied()
    }

    pub(crate) fn trigger(&self, oid: u32) -> Option<&TriggerEntry> {
        self.triggers.iter().find(|trigger| trigger.oid == oid)
    }

    pub(crate) fn routine(&self, oid: u32) -> Option<&RoutineEntry> {
        self.routines.iter().find(|routine| routine.oid == oid)
    }

    pub(crate) fn routine_is_visible(&self, routine: &RoutineEntry) -> bool {
        self.search_order()
            .find(|schema| {
                self.routines
                    .iter()
                    .any(|other| other.name == routine.name && other.schema == *schema)
            })
            .is_some_and(|schema| schema == routine.schema)
    }

    pub(crate) fn routines_named(&self, schema: Option<&str>, name: &str) -> Vec<&RoutineEntry> {
        match schema {
            Some(schema) => self.routines_in(schema, name),
            None => self
                .search_order()
                .map(|schema| self.routines_in(schema, name))
                .find(|found| !found.is_empty())
                .unwrap_or_default(),
        }
    }

    fn routines_in(&self, schema: &str, name: &str) -> Vec<&RoutineEntry> {
        self.routines
            .iter()
            .filter(|routine| routine.name == name && routine.schema == schema)
            .collect()
    }

    pub(crate) fn trigger_function_oid(&self, written: &str) -> u32 {
        let (schema, name) = match written.split_once('.') {
            Some((schema, name)) => (Some(schema), name),
            None => (None, written),
        };
        self.routines_named(schema, name)
            .into_iter()
            .find(|routine| {
                routine.definition.input_types().is_empty()
                    && routine.definition.returns == RoutineReturns::One(ReturnType::Trigger)
            })
            .map_or(0, |routine| routine.oid)
    }

    pub(crate) fn has_triggers(&self, relation: usize) -> bool {
        self.triggers
            .iter()
            .any(|trigger| trigger.relation == relation)
    }

    pub(crate) fn partition_key_definition(&self, oid: u32) -> Option<String> {
        let table = self.relation(oid)?.table()?;
        let key = self.partitions.key_of(&table.name)?;
        Some(crate::partitions::key_definition(key))
    }

    pub(crate) fn partition_bound_definition(&self, table: &TableDefinition) -> Option<String> {
        let (parent, bound) = self.partitions.parent_of(&table.name)?;
        let parent = self
            .table_named(parent)
            .and_then(|at| self.relations.get(at))
            .and_then(RelationEntry::table)?;
        let key = self.partitions.key_of(&parent.name)?;
        let numeric = crate::partitions::numeric_key_columns(parent, key);
        Some(crate::partitions::bound_definition(bound, &numeric))
    }

    pub(crate) fn table_named(&self, internal: &str) -> Option<usize> {
        self.tables.get(internal).copied()
    }

    pub(crate) fn constraint(&self, oid: u32) -> Option<&ConstraintEntry> {
        self.constraint_oids
            .get(&oid)
            .and_then(|&at| self.constraints.get(at))
    }

    pub(crate) fn schema(&self, oid: u32) -> Option<&SchemaEntry> {
        self.schemas.iter().find(|schema| schema.oid == oid)
    }

    pub(crate) fn schema_named(&self, name: &str) -> Option<&SchemaEntry> {
        self.schema_names
            .get(name)
            .and_then(|&at| self.schemas.get(at))
    }

    fn search_order(&self) -> impl Iterator<Item = &str> {
        std::iter::once(crate::schemas::CATALOG_SCHEMA).chain(self.path.iter().map(String::as_str))
    }

    pub(crate) fn find_relation(&self, schema: Option<&str>, name: &str) -> Option<usize> {
        match schema {
            Some(schema) => self
                .relation_names
                .get(&(schema.to_owned(), name.to_owned()))
                .copied(),
            None => self.search_order().find_map(|schema| {
                self.relation_names
                    .get(&(schema.to_owned(), name.to_owned()))
                    .copied()
            }),
        }
    }

    pub(crate) fn is_visible(&self, at: usize) -> bool {
        let Some(entry) = self.relations.get(at) else {
            return false;
        };
        self.find_relation(None, &entry.name) == Some(at)
    }

    pub(crate) fn display_name(&self, at: usize) -> String {
        let Some(entry) = self.relations.get(at) else {
            return String::new();
        };
        if self.is_visible(at) {
            quote_ident(&entry.name)
        } else {
            format!(
                "{}.{}",
                quote_ident(&entry.schema),
                quote_ident(&entry.name)
            )
        }
    }

    pub(crate) fn qualified_name(&self, at: usize) -> String {
        self.relations.get(at).map_or_else(String::new, |entry| {
            format!(
                "{}.{}",
                quote_ident(&entry.schema),
                quote_ident(&entry.name)
            )
        })
    }

    pub(crate) fn display_internal(&self, internal: &str) -> String {
        match self.find_internal(internal) {
            Some(at) => self.display_name(at),
            None => crate::schemas::display(internal),
        }
    }

    pub(crate) fn find_internal(&self, internal: &str) -> Option<usize> {
        let schema = crate::schemas::schema_of(internal);
        let name = crate::schemas::base_name(internal);
        self.find_relation(Some(schema), name)
    }

    pub(crate) fn role_name(&self, oid: u32) -> Option<String> {
        if oid == oids::SESSION_USER {
            return Some(self.user.clone());
        }
        oids::PREDEFINED_ROLES
            .iter()
            .find(|(found, _)| *found == oid)
            .map(|(_, name)| (*name).to_owned())
    }

    pub(crate) fn sqlserver_objects(&self) -> &crate::sqlserver_catalog::SqlServerObjects {
        self.sqlserver
            .get_or_init(|| crate::sqlserver_catalog::SqlServerObjects::build(self))
    }

    pub(crate) fn rows(&self, relation: CatalogRelation) -> SqlResult<Rc<Vec<Vec<Value>>>> {
        if let Some(found) = self
            .rows
            .try_borrow()
            .ok()
            .and_then(|rows| rows.get(&relation).cloned())
        {
            return Ok(found);
        }
        #[cfg(test)]
        BUILDS.with(|builds| builds.set(builds.get() + 1));
        let built = Rc::new(super::relations::build(self, relation)?);
        if let Ok(mut rows) = self.rows.try_borrow_mut() {
            rows.insert(relation, Rc::clone(&built));
        }
        Ok(built)
    }
}

fn routine_requests(named: &[NamedRoutines]) -> Vec<OidRequest> {
    named
        .iter()
        .flat_map(|listed| {
            listed.routines.iter().map(|routine| OidRequest {
                kind: b'P',
                owner: listed.name.clone(),
                name: routine.identity(),
            })
        })
        .collect()
}

pub(crate) fn routine_oid(
    reader: &dyn KeyValueRead,
    definition: &RoutineDefinition,
) -> SqlResult<u32> {
    let named = crate::routines::every_routine(reader)?;
    let assigned = oids::assign(Space::Routine, &routine_requests(&named));
    let identity = definition.identity();
    Ok(named
        .iter()
        .flat_map(|listed| listed.routines.iter().map(move |routine| (listed, routine)))
        .zip(assigned)
        .find(|((listed, routine), _)| {
            listed.name == definition.name && routine.identity() == identity
        })
        .map_or(0, |(_, oid)| oid))
}

fn missing(state: SqlState, what: &str, text: &str) -> SqlError {
    SqlError::new(state, format!("{what} \"{text}\" does not exist"))
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn names_non_sequence_relation(reader: &dyn KeyValueRead, name: &str) -> bool {
    let Ok(snapshot) = snapshot_for(reader) else {
        return false;
    };
    let found = if crate::schemas::is_qualified(name) {
        snapshot.find_internal(name)
    } else {
        snapshot.find_relation(None, name)
    };
    found
        .and_then(|at| snapshot.relations.get(at))
        .is_some_and(|entry| entry.kind != RelationKind::Sequence)
}

pub(crate) fn reg_from_text(kind: RegKind, text: &str) -> SqlResult<RegValue> {
    if is_number(text) {
        let oid = super::types::parse_identifier(text, "oid")?;
        return Ok(reg_from_oid(kind, oid));
    }
    if text.trim() == "-" {
        return Ok(reg_from_oid(kind, 0));
    }
    let found = |oid: u32| RegValue {
        kind,
        oid,
        name: String::new(),
    };
    let reg = match kind {
        RegKind::Type => found(super::types::regtype_oid(text)?),
        RegKind::Proc => {
            let (schema, name) = split_object_name(text, kind)?;
            let known = schema
                .as_deref()
                .is_none_or(|schema| schema == crate::schemas::CATALOG_SCHEMA);
            let builtin = oids::BUILTIN_FUNCTIONS
                .iter()
                .find(|function| known && function.name == name)
                .map(|function| function.oid);
            match builtin {
                Some(oid) => found(oid),
                None => found(user_routine_oid(schema.as_deref(), &name, text)?),
            }
        }
        RegKind::Class | RegKind::Namespace | RegKind::Role => {
            let (schema, name) = split_object_name(text, kind)?;
            let snapshot = require()?;
            match kind {
                RegKind::Class => {
                    let at = snapshot
                        .find_relation(schema.as_deref(), &name)
                        .ok_or_else(|| {
                            let shown = match &schema {
                                Some(schema) => format!("{schema}.{name}"),
                                None => name.clone(),
                            };
                            missing(SqlState::UNDEFINED_TABLE, "relation", &shown)
                        })?;
                    found(snapshot.relations[at].oid)
                }
                RegKind::Namespace => {
                    let schema_entry = snapshot
                        .schema_named(&name)
                        .filter(|_| schema.is_none())
                        .ok_or_else(|| missing(SqlState::INVALID_SCHEMA_NAME, "schema", &name))?;
                    found(schema_entry.oid)
                }
                _ => {
                    let oid = if schema.is_none() && name == snapshot.user {
                        oids::SESSION_USER
                    } else {
                        oids::PREDEFINED_ROLES
                            .iter()
                            .find(|(_, role)| schema.is_none() && *role == name)
                            .map(|(oid, _)| *oid)
                            .ok_or_else(|| missing(SqlState::UNDEFINED_OBJECT, "role", &name))?
                    };
                    found(oid)
                }
            }
        }
    };
    Ok(reg_from_oid(kind, reg.oid))
}

fn user_routine_oid(schema: Option<&str>, name: &str, text: &str) -> SqlResult<u32> {
    let snapshot = current();
    let named = snapshot
        .as_ref()
        .map(|snapshot| snapshot.routines_named(schema, name))
        .unwrap_or_default();
    match named.as_slice() {
        [only] => Ok(only.oid),
        [] => Err(missing(SqlState::UNDEFINED_FUNCTION, "function", text)),
        _ => Err(SqlError::new(
            SqlState::AMBIGUOUS_FUNCTION,
            format!("more than one function named \"{text}\""),
        )),
    }
}

fn routine_display_name(snapshot: &CatalogSnapshot, oid: u32) -> Option<String> {
    let routine = snapshot.routine(oid)?;
    Some(if snapshot.routine_is_visible(routine) {
        quote_ident(&routine.name)
    } else {
        format!("{}.{}", quote_ident(&routine.schema), quote_ident(&routine.name))
    })
}

pub(crate) fn reg_from_oid(kind: RegKind, oid: u32) -> RegValue {
    if oid == 0 {
        return RegValue {
            kind,
            oid,
            name: "-".to_owned(),
        };
    }
    let snapshot = current();
    let name = match kind {
        RegKind::Class => snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .relation_index(oid)
                .map(|at| snapshot.display_name(at))
        }),
        RegKind::Type => oids::builtin_type(oid).map(|_| oids::format_type(oid, None)),
        RegKind::Namespace => oids::SYSTEM_SCHEMAS
            .iter()
            .find(|(found, ..)| *found == oid)
            .map(|(_, name, _)| (*name).to_owned())
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.schema(oid).map(|schema| schema.name.clone()))
            })
            .map(|name| quote_ident(&name)),
        RegKind::Role => snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.role_name(oid))
            .or_else(|| (oid == oids::SESSION_USER).then(session_user).flatten())
            .or_else(|| {
                oids::PREDEFINED_ROLES
                    .iter()
                    .find(|(found, _)| *found == oid)
                    .map(|(_, name)| (*name).to_owned())
            })
            .map(|name| quote_ident(&name)),
        RegKind::Proc => oids::BUILTIN_FUNCTIONS
            .iter()
            .find(|function| function.oid == oid)
            .map(|function| function.name.to_owned())
            .or_else(|| {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| routine_display_name(snapshot, oid))
            }),
    };
    RegValue {
        kind,
        oid,
        name: name.unwrap_or_else(|| oid.to_string()),
    }
}
