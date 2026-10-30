use crate::ast::{
    AlterRoutine, AlterRoutineAction, CreateRoutine, DropRoutine, DropTrigger, Name,
    RoutineTarget, Statement, TriggerName,
};
use crate::dialect::Dialect;
use crate::encoding::{decode_routines, encode_routines, prefix_end, routine_key, routines_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::tokenize_in;
use crate::parser::Parser;
use crate::schemas::{base_name, display, is_qualified, normalized, qualified, schema_of};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::triggers::TriggerAction;
use crate::types::SqlType;
use std::collections::HashMap;
use std::sync::Arc;

mod calls;
mod oracle;
mod sqlserver;

pub(crate) use calls::{
    DEFAULT_ARGUMENT, Prefetch, ROUTINE_CALL, call_in_row, call_procedure, call_procedure_outputs,
    called_name, default_argument, default_value, evaluate, exprs_call_marked_routine,
    insert_calls_marked_routine, is_default_argument, marked_call,
    mentions_routine_call, prepare_delete, prepare_insert, prepare_query, prepare_select,
    prepare_update, query_calls_marked_routine, run_do, select_calls_marked_routine,
    select_calls_volatile, select_may_write,
};

pub(crate) const MAX_ROUTINE_TEXT: usize = 1 << 20;
pub(crate) const MAX_OVERLOADS: usize = 256;
pub(crate) const MAX_PARAMETERS: usize = 100;
const CACHED_NAMES: usize = 512;
const CACHED_PROGRAMS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineKind {
    Function,
    Procedure,
}

impl RoutineKind {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Function => "FUNCTION",
            Self::Procedure => "PROCEDURE",
        }
    }

    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Procedure => "procedure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineLanguage {
    Sql,
    PlPgSql,
    Tsql,
    PlSql,
}

impl RoutineLanguage {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::PlPgSql => "plpgsql",
            Self::Tsql => "tsql",
            Self::PlSql => "plsql",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParameterMode {
    In,
    Out,
    InOut,
    Variadic,
}

impl ParameterMode {
    pub(crate) fn is_input(self) -> bool {
        self != Self::Out
    }

    pub(crate) fn is_output(self) -> bool {
        matches!(self, Self::Out | Self::InOut)
    }

    pub(crate) fn keyword(self) -> Option<&'static str> {
        match self {
            Self::In => None,
            Self::Out => Some("OUT"),
            Self::InOut => Some("INOUT"),
            Self::Variadic => Some("VARIADIC"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoutineParameter {
    pub(crate) name: Option<String>,
    pub(crate) mode: ParameterMode,
    pub(crate) sql_type: SqlType,
    pub(crate) default: Option<String>,
    pub(crate) declared: Option<crate::sqlserver_types::SqlServerType>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReturnType {
    Void,
    Trigger,
    Record,
    Type(SqlType),
    Relation(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RoutineReturns {
    Nothing,
    One(ReturnType),
    Set(ReturnType),
    Table(Vec<(String, SqlType)>),
}

impl RoutineReturns {
    pub(crate) fn is_set(&self) -> bool {
        matches!(self, Self::Set(_) | Self::Table(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

impl Volatility {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Immutable => "i",
            Self::Stable => "s",
            Self::Volatile => "v",
        }
    }

    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Immutable => "IMMUTABLE",
            Self::Stable => "STABLE",
            Self::Volatile => "VOLATILE",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RoutineDefinition {
    pub(crate) name: String,
    pub(crate) kind: RoutineKind,
    pub(crate) dialect: Dialect,
    pub(crate) language: RoutineLanguage,
    pub(crate) parameters: Vec<RoutineParameter>,
    pub(crate) returns: RoutineReturns,
    pub(crate) volatility: Volatility,
    pub(crate) strict: bool,
    pub(crate) security_definer: bool,
    pub(crate) settings: Vec<(String, String)>,
    pub(crate) body: String,
    pub(crate) text: String,
    pub(crate) tsql: Option<Box<crate::tsql::TsqlRoutine>>,
    pub(crate) plsql: Option<Box<crate::plsql::PlsqlRoutine>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredRoutine {
    pub(crate) dialect: Dialect,
    pub(crate) kind: RoutineKind,
    pub(crate) identity: String,
    pub(crate) text: String,
}

pub(crate) fn identity_type_name(sql_type: SqlType) -> String {
    let named = sql_type.name();
    let mut shown = String::with_capacity(named.len());
    let mut depth = 0usize;
    for character in named.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            other if depth == 0 => shown.push(other),
            _ => {}
        }
    }
    shown.trim().to_owned()
}

pub(crate) fn identity_of(types: &[SqlType]) -> String {
    types
        .iter()
        .map(|sql_type| identity_type_name(*sql_type))
        .collect::<Vec<_>>()
        .join(", ")
}

impl RoutineDefinition {
    pub(crate) fn input_types(&self) -> Vec<SqlType> {
        self.parameters
            .iter()
            .filter(|parameter| parameter.mode.is_input())
            .map(|parameter| parameter.sql_type)
            .collect()
    }

    pub(crate) fn identity(&self) -> String {
        match &self.plsql {
            Some(plsql) => plsql.identity(&self.parameters),
            None => identity_of(&self.input_types()),
        }
    }

    pub(crate) fn signature(&self) -> String {
        format!("{}({})", display(&self.name), self.identity())
    }

    pub(crate) fn required_inputs(&self) -> usize {
        self.parameters
            .iter()
            .filter(|parameter| parameter.mode.is_input() && parameter.default.is_none())
            .count()
    }

    pub(crate) fn stored(&self) -> StoredRoutine {
        StoredRoutine {
            dialect: self.dialect,
            kind: self.kind,
            identity: self.identity(),
            text: self.text.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NamedRoutines {
    pub(crate) name: String,
    pub(crate) routines: Vec<Arc<RoutineDefinition>>,
}

#[derive(Default)]
pub(crate) struct RoutineCache {
    entries: HashMap<String, CachedRoutines>,
    programs: Vec<(Arc<RoutineDefinition>, Arc<crate::plpgsql::Program>)>,
    tsql_programs: Vec<(Arc<RoutineDefinition>, Arc<crate::tsql::Program>)>,
}

struct CachedRoutines {
    bytes: Option<Vec<u8>>,
    checked_at: Option<u64>,
    routines: Option<Arc<NamedRoutines>>,
}

pub(crate) fn parse_definition(text: &str, dialect: Dialect) -> SqlResult<RoutineDefinition> {
    let tokens = tokenize_in(text, dialect)?;
    match Parser::new(text, &tokens).in_dialect(dialect).statement()? {
        Statement::CreateRoutine(create) => Ok(create.definition),
        _ => Err(crate::encoding::corrupt("routine definitions")),
    }
}

fn definitions(name: &str, stored: Vec<StoredRoutine>) -> SqlResult<NamedRoutines> {
    let mut routines = Vec::with_capacity(stored.len());
    for routine in stored {
        let mut definition = parse_definition(&routine.text, routine.dialect)
            .map_err(|_| crate::encoding::corrupt("routine definitions"))?;
        if definition.kind != routine.kind || definition.identity() != routine.identity {
            return Err(crate::encoding::corrupt("routine definitions"));
        }
        definition.name = name.to_owned();
        routines.push(Arc::new(definition));
    }
    Ok(NamedRoutines {
        name: name.to_owned(),
        routines,
    })
}

pub(crate) fn of_name(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<Arc<NamedRoutines>>> {
    let version = reader.committed_version();
    let cache = &mut tables.routines;
    if let Some(cached) = cache.entries.get(name)
        && version.is_some()
        && cached.checked_at == version
    {
        return Ok(cached.routines.clone());
    }
    let bytes = reader.get(&routine_key(name))?;
    if let Some(cached) = cache.entries.get_mut(name)
        && cached.bytes == bytes
    {
        cached.checked_at = version;
        return Ok(cached.routines.clone());
    }
    let routines = match &bytes {
        Some(bytes) => Some(Arc::new(definitions(name, decode_routines(name, bytes)?)?)),
        None => None,
    };
    if cache.entries.len() >= CACHED_NAMES {
        cache.entries.clear();
    }
    cache.entries.insert(
        name.to_owned(),
        CachedRoutines {
            bytes,
            checked_at: version,
            routines: routines.clone(),
        },
    );
    Ok(routines)
}

pub(crate) fn compiled_program(
    tables: &mut TableCache,
    definition: &Arc<RoutineDefinition>,
) -> SqlResult<Arc<crate::plpgsql::Program>> {
    if let Some(program) = crate::plpgsql::local_program(definition) {
        return Ok(program);
    }
    let programs = &mut tables.routines.programs;
    if let Some((_, program)) = programs
        .iter()
        .find(|(known, _)| Arc::ptr_eq(known, definition))
    {
        return Ok(Arc::clone(program));
    }
    let program = Arc::new(crate::plpgsql::compile_for_run(definition)?);
    if programs.len() >= CACHED_PROGRAMS {
        programs.clear();
    }
    programs.push((Arc::clone(definition), Arc::clone(&program)));
    Ok(program)
}

pub(crate) fn compiled_tsql_program(
    tables: &mut TableCache,
    definition: &Arc<RoutineDefinition>,
    compile: impl FnOnce() -> SqlResult<crate::tsql::Program>,
) -> SqlResult<Arc<crate::tsql::Program>> {
    let programs = &mut tables.routines.tsql_programs;
    if let Some((_, program)) = programs
        .iter()
        .find(|(known, _)| Arc::ptr_eq(known, definition))
    {
        return Ok(Arc::clone(program));
    }
    let program = Arc::new(compile()?);
    if programs.len() >= CACHED_PROGRAMS {
        programs.clear();
    }
    programs.push((Arc::clone(definition), Arc::clone(&program)));
    Ok(program)
}

pub(crate) fn resolve(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<Arc<NamedRoutines>>> {
    if let Some(local) = crate::plpgsql::local_routines(name) {
        return Ok(Some(local));
    }
    let plain = normalized(name);
    if is_qualified(plain) || plain.len() != name.len() {
        return of_name(reader, tables, plain);
    }
    for schema in crate::schemas::relation_path(reader)? {
        if let Some(found) = of_name(reader, tables, &qualified(&schema, name))? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub(crate) fn resolve_trigger_function(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    written: &str,
) -> SqlResult<Option<Arc<RoutineDefinition>>> {
    let internal = match written.split_once('.') {
        Some((schema, name)) => qualified(schema, name),
        None => written.to_owned(),
    };
    let Some(named) = resolve(reader, tables, &internal)? else {
        return Ok(None);
    };
    Ok(named
        .routines
        .iter()
        .find(|routine| {
            routine.kind == RoutineKind::Function
                && routine.input_types().is_empty()
                && routine.returns == RoutineReturns::One(ReturnType::Trigger)
        })
        .cloned())
}

pub(crate) fn every_routine(reader: &dyn KeyValueRead) -> SqlResult<Vec<NamedRoutines>> {
    let prefix = routines_prefix();
    let mut found = Vec::new();
    let mut failure = None;
    let mut memory = crate::memory::WorkMemory::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            memory.charge(key.len() + value.len() + 64)?;
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("routine definitions"))
                .and_then(|name| definitions(name, decode_routines(name, value)?));
            match decoded {
                Ok(routines) => found.push(routines),
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

pub(crate) fn has_routines(reader: &dyn KeyValueRead, name: &str) -> SqlResult<bool> {
    Ok(reader.get(&routine_key(name))?.is_some())
}

fn stored(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Vec<StoredRoutine>> {
    match reader.get(&routine_key(name))? {
        Some(bytes) => decode_routines(name, &bytes),
        None => Ok(Vec::new()),
    }
}

fn save(writer: &mut dyn KeyValueWrite, name: &str, routines: &[StoredRoutine]) -> SqlResult<()> {
    let key = routine_key(name);
    if routines.is_empty() {
        writer.delete(&key)?;
    } else {
        writer.put(&key, &encode_routines(routines))?;
    }
    Ok(())
}

fn signature_of(name: &str, identity: &str) -> String {
    format!("{}({identity})", display(name))
}

fn duplicate(kind: RoutineKind, name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_FUNCTION,
        format!(
            "{} \"{}\" already exists with same argument types",
            kind.noun(),
            base_name(name)
        ),
    )
    .at(position)
}

fn missing(kind: Option<RoutineKind>, signature: &str, position: usize) -> SqlError {
    let noun = kind.map_or("function", RoutineKind::noun);
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("{noun} {signature} does not exist"),
    )
    .at(position)
}

fn not_unique(kind: Option<RoutineKind>, name: &str, position: usize) -> SqlError {
    let noun = kind.map_or("function", RoutineKind::noun);
    SqlError::new(
        SqlState::AMBIGUOUS_FUNCTION,
        format!("{noun} name \"{}\" is not unique", display(name)),
    )
    .with_hint(format!(
        "Specify the argument list to select the {noun} unambiguously."
    ))
    .at(position)
}

fn wrong_kind(signature: &str, wanted: RoutineKind, position: usize) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("{signature} is not a {}", wanted.noun()),
    )
    .at(position)
}

fn return_type_changed(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        "cannot change return type of existing function",
    )
    .with_hint("Use DROP FUNCTION first.")
    .at(position)
}

fn check_returns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    definition: &RoutineDefinition,
) -> SqlResult<()> {
    let relation = match &definition.returns {
        RoutineReturns::One(ReturnType::Relation(name))
        | RoutineReturns::Set(ReturnType::Relation(name)) => name,
        RoutineReturns::One(ReturnType::Trigger) if !definition.input_types().is_empty() => {
            return Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "trigger functions cannot have declared arguments",
            )
            .with_hint("The arguments of the trigger can be accessed through TG_NARGS and TG_ARGV instead."));
        }
        RoutineReturns::One(ReturnType::Trigger) if definition.language == RoutineLanguage::Sql => {
            return Err(SqlError::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "SQL functions cannot return type trigger",
            ));
        }
        _ => return Ok(()),
    };
    match tables.relation(reader, relation)? {
        Some(_) => Ok(()),
        None => Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("type \"{}\" does not exist", display(relation)),
        )),
    }
}

fn check_body(definition: &RoutineDefinition, body_position: Option<usize>) -> SqlResult<()> {
    match definition.language {
        RoutineLanguage::Sql => tokenize_in(&definition.body, definition.dialect).map(drop),
        RoutineLanguage::PlPgSql => crate::plpgsql::check(definition, body_position),
        RoutineLanguage::Tsql | RoutineLanguage::PlSql => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyCheck {
    Checked,
    Skipped,
}

impl BodyCheck {
    pub(crate) fn of_setting(value: Option<&str>) -> Self {
        let off = value.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "off" | "of" | "false" | "fa" | "fal" | "fals" | "f" | "no" | "n" | "0"
            )
        });
        if off { Self::Skipped } else { Self::Checked }
    }
}

pub(crate) fn create_routine(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateRoutine,
    body_check: BodyCheck,
) -> SqlResult<QueryResult> {
    let mut definition = create.definition.clone();
    crate::constraints::check_identifier(&Name {
        text: base_name(&create.name.text).to_owned(),
        position: create.name.position,
    })?;
    definition.name = crate::schemas::creation_name(&*writer, &create.name)?;
    if definition.text.len() > MAX_ROUTINE_TEXT {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("a routine's definition is at most {MAX_ROUTINE_TEXT} bytes"),
        )
        .at(create.name.position));
    }
    match definition.dialect {
        Dialect::SqlServer => return sqlserver::create_routine(writer, tables, create, definition),
        Dialect::Oracle => return oracle::create_routine(writer, tables, create, definition),
        _ => {}
    }
    check_returns(&*writer, tables, &definition)?;
    if body_check == BodyCheck::Checked {
        check_body(&definition, create.body_position)?;
    }
    let mut routines = stored(&*writer, &definition.name)?;
    let identity = definition.identity();
    let tag = format!("CREATE {}", definition.kind.keyword());
    match routines.iter().position(|known| known.identity == identity) {
        Some(at) => {
            if !create.replace {
                return Err(duplicate(definition.kind, &definition.name, create.name.position));
            }
            let existing = parse_definition(&routines[at].text, routines[at].dialect)?;
            if existing.kind != definition.kind {
                return Err(wrong_kind(
                    &signature_of(&definition.name, &identity),
                    definition.kind,
                    create.name.position,
                ));
            }
            if existing.returns != definition.returns {
                return Err(return_type_changed(create.name.position));
            }
            routines[at] = definition.stored();
        }
        None => {
            if routines.len() >= MAX_OVERLOADS {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "\"{}\" has {MAX_OVERLOADS} overloads, the most EggDB keeps under one name",
                        display(&definition.name)
                    ),
                )
                .at(create.name.position));
            }
            routines.push(definition.stored());
        }
    }
    save(writer, &definition.name, &routines)?;
    Ok(QueryResult::command(tag))
}

struct Located {
    name: String,
    identity: String,
    kind: RoutineKind,
}

fn locate(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    kind: Option<RoutineKind>,
    target: &RoutineTarget,
) -> SqlResult<Result<Located, SqlError>> {
    let wanted_identity = target.arguments.as_deref().map(identity_of);
    let shown = signature_of(
        &target.name.text,
        wanted_identity.as_deref().unwrap_or_default(),
    );
    let position = target.name.position;
    let Some(named) = resolve(reader, tables, &target.name.text)? else {
        return Ok(Err(missing(kind, &shown, position)));
    };
    let matching: Vec<&Arc<RoutineDefinition>> = named
        .routines
        .iter()
        .filter(|routine| {
            wanted_identity
                .as_ref()
                .is_none_or(|identity| routine.identity() == *identity)
        })
        .filter(|routine| kind.is_none_or(|kind| routine.kind == kind) || wanted_identity.is_some())
        .collect();
    let routine = match matching.as_slice() {
        [] => return Ok(Err(missing(kind, &shown, position))),
        [one] => *one,
        _ => return Err(not_unique(kind, &named.name, position)),
    };
    if let Some(kind) = kind
        && routine.kind != kind
    {
        return Err(wrong_kind(&routine.signature(), kind, position));
    }
    Ok(Ok(Located {
        name: named.name.clone(),
        identity: routine.identity(),
        kind: routine.kind,
    }))
}

fn names_routine(written: &str, internal: &str) -> bool {
    match written.split_once('.') {
        Some((schema, name)) => qualified(schema, name) == internal,
        None => base_name(internal) == written,
    }
}

fn dependent_triggers(
    reader: &dyn KeyValueRead,
    located: &Located,
) -> SqlResult<Vec<(String, String)>> {
    if located.kind != RoutineKind::Function || !located.identity.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for table in crate::triggers::every_trigger(reader)? {
        for trigger in &table.triggers {
            if let TriggerAction::Function { name, .. } = &trigger.action
                && names_routine(name, &located.name)
            {
                found.push((table.table.clone(), trigger.name.clone()));
            }
        }
    }
    Ok(found)
}

pub(crate) fn drop_routines(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    drop: &DropRoutine,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    match dialect {
        Dialect::SqlServer => return sqlserver::drop_routines(writer, tables, drop),
        Dialect::Oracle => return oracle::drop_routines(writer, tables, drop),
        _ => {}
    }
    let keyword = drop.kind.map_or("ROUTINE", RoutineKind::keyword);
    let mut result = QueryResult::command(format!("DROP {keyword}"));
    for target in &drop.targets {
        let located = match locate(&*writer, tables, drop.kind, target)? {
            Ok(located) => located,
            Err(error) if drop.if_exists => {
                result = result.with_notice(format!("{}, skipping", error.message()));
                continue;
            }
            Err(error) => return Err(error),
        };
        let dependents = dependent_triggers(&*writer, &located)?;
        if !dependents.is_empty() && !drop.cascade {
            let (table, trigger) = &dependents[0];
            return Err(SqlError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot drop {} {} because other objects depend on it",
                    located.kind.noun(),
                    signature_of(&located.name, &located.identity)
                ),
            )
            .with_detail(format!(
                "trigger {trigger} on table {} depends on {} {}",
                display(table),
                located.kind.noun(),
                signature_of(&located.name, &located.identity)
            ))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
            .at(target.name.position));
        }
        for (table, trigger) in dependents {
            let drop_trigger = DropTrigger {
                names: vec![TriggerName {
                    schema: None,
                    name: Name {
                        text: trigger.clone(),
                        position: target.name.position,
                    },
                }],
                table: Some(Name {
                    text: table.clone(),
                    position: target.name.position,
                }),
                if_exists: true,
            };
            crate::triggers::drop_triggers(writer, tables, &drop_trigger, dialect)?;
            result = result.with_notice(format!(
                "drop cascades to trigger {trigger} on table {}",
                display(&table)
            ));
        }
        let mut routines = stored(&*writer, &located.name)?;
        routines.retain(|routine| routine.identity != located.identity);
        save(writer, &located.name, &routines)?;
    }
    Ok(result)
}

pub(crate) fn alter_routine(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    alter: &AlterRoutine,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    if dialect == Dialect::Oracle {
        return oracle::alter_routine(&*writer, tables, alter);
    }
    let keyword = alter.kind.map_or("ROUTINE", RoutineKind::keyword);
    let result = QueryResult::command(format!("ALTER {keyword}"));
    let located = locate(&*writer, tables, alter.kind, &alter.target)??;
    let new_name = match &alter.action {
        AlterRoutineAction::Accepted => return Ok(result),
        AlterRoutineAction::RenameTo(to) => {
            crate::constraints::check_identifier(to)?;
            qualified(schema_of(&located.name), &to.text)
        }
        AlterRoutineAction::SetSchema(schema) => {
            if !crate::schemas::schema_exists(&*writer, &schema.text)? {
                return Err(crate::schemas::missing_schema(&schema.text, Dialect::PostgreSql)
                    .at(schema.position));
            }
            qualified(&schema.text, base_name(&located.name))
        }
    };
    if new_name == located.name {
        return Ok(result);
    }
    let mut from = stored(&*writer, &located.name)?;
    let Some(at) = from
        .iter()
        .position(|routine| routine.identity == located.identity)
    else {
        return Err(missing(
            alter.kind,
            &signature_of(&located.name, &located.identity),
            alter.target.name.position,
        ));
    };
    let mut to = stored(&*writer, &new_name)?;
    if to.iter().any(|routine| routine.identity == located.identity) {
        return Err(SqlError::new(
            SqlState::DUPLICATE_FUNCTION,
            format!(
                "{} {} already exists in schema \"{}\"",
                located.kind.noun(),
                signature_of(base_name(&new_name), &located.identity),
                schema_of(&new_name)
            ),
        )
        .at(alter.target.name.position));
    }
    if to.len() >= MAX_OVERLOADS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "\"{}\" has {MAX_OVERLOADS} overloads, the most EggDB keeps under one name",
                display(&new_name)
            ),
        ));
    }
    to.push(from.remove(at));
    save(writer, &located.name, &from)?;
    save(writer, &new_name, &to)?;
    Ok(result)
}

#[cfg(test)]
mod calls_tests;
#[cfg(test)]
mod sqlserver_tests;
#[cfg(test)]
mod tests;
