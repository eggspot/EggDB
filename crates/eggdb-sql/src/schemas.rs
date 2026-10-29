use crate::ast::Name;
use crate::dialect::Dialect;
use crate::encoding::{
    decode_schema, encode_schema, prefix_end, schema_comment_key, schema_key, sequence_key,
    table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use std::cell::RefCell;
use std::sync::{Arc, Weak};

pub(crate) const QUALIFIER: char = '\0';
pub(crate) const DEFAULT_SCHEMA: &str = "public";
pub(crate) const CATALOG_SCHEMA: &str = "pg_catalog";
pub(crate) const INFORMATION_SCHEMA: &str = "information_schema";
const DROP_BATCH: usize = 256;

pub(crate) fn is_qualified(name: &str) -> bool {
    name.contains(QUALIFIER)
}

pub(crate) fn is_well_formed(name: &str) -> bool {
    match name.split_once(QUALIFIER) {
        None => true,
        Some((schema, relation)) => {
            !schema.is_empty()
                && !relation.is_empty()
                && !relation.contains(QUALIFIER)
                && schema != DEFAULT_SCHEMA
        }
    }
}

pub(crate) fn written_qualified(schema: &str, name: &str) -> String {
    format!("{schema}{QUALIFIER}{name}")
}

pub(crate) fn normalized(name: &str) -> &str {
    name.strip_prefix(DEFAULT_SCHEMA)
        .and_then(|rest| rest.strip_prefix(QUALIFIER))
        .unwrap_or(name)
}

pub(crate) fn qualified(schema: &str, name: &str) -> String {
    if schema == DEFAULT_SCHEMA {
        name.to_owned()
    } else {
        format!("{schema}{QUALIFIER}{name}")
    }
}

pub(crate) fn schema_of(internal: &str) -> &str {
    internal
        .split_once(QUALIFIER)
        .map_or(DEFAULT_SCHEMA, |(schema, _)| schema)
}

pub(crate) fn base_name(internal: &str) -> &str {
    internal
        .split_once(QUALIFIER)
        .map_or(internal, |(_, name)| name)
}

pub(crate) fn display(internal: &str) -> String {
    let mut shown = String::with_capacity(internal.len());
    let mut previous: Option<char> = None;
    for character in internal.chars() {
        match character {
            QUALIFIER if previous.is_none_or(|before| !is_name_character(before)) => {}
            QUALIFIER => shown.push('.'),
            other => shown.push(other),
        }
        previous = Some(character);
    }
    shown
}

fn is_name_character(character: char) -> bool {
    !matches!(character, ' ' | '"' | '\'' | '(' | ',' | '\n')
}

pub(crate) fn unreachable_name(name: &str) -> String {
    format!("{QUALIFIER}{name}")
}

pub(crate) fn schema_named(text: &str, position: usize) -> SqlResult<Name> {
    if text.contains(QUALIFIER) || text.contains(crate::temp_tables::TEMP_TABLE_MARKER) {
        return Err(SqlError::new(
            SqlState::INVALID_NAME,
            format!("invalid name \"{}\"", text.escape_default()),
        )
        .at(position));
    }
    Ok(Name {
        text: text.to_owned(),
        position,
    })
}

type SharedPath = Arc<[String]>;

struct PathState {
    settings: Option<Arc<Settings>>,
    dialect: Dialect,
    resolved: Option<SharedPath>,
    relations: Option<SharedPath>,
    known: Option<(u64, SharedPath)>,
    computed: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PathCache {
    settings: Weak<Settings>,
    dialect: Dialect,
    version: u64,
    path: Option<SharedPath>,
}

thread_local! {
    static PATH: RefCell<Option<PathState>> = const { RefCell::new(None) };
}

pub(crate) struct EnteredPath {
    previous: Option<PathState>,
}

impl EnteredPath {
    pub(crate) fn remember(&self, cache: &mut PathCache) {
        let computed = PATH.with(|path| {
            let path = path.try_borrow().ok()?;
            let state = path.as_ref()?;
            let version = state.computed?;
            let settings = state.settings.as_ref()?;
            Some((
                Arc::downgrade(settings),
                state.dialect,
                version,
                state.resolved.clone()?,
            ))
        });
        if let Some((settings, dialect, version, found)) = computed {
            *cache = PathCache {
                settings,
                dialect,
                version,
                path: Some(found),
            };
        }
    }
}

impl Drop for EnteredPath {
    fn drop(&mut self) {
        let previous = self.previous.take();
        PATH.with(|path| {
            if let Ok(mut path) = path.try_borrow_mut() {
                *path = previous;
            }
        });
    }
}

pub(crate) fn enter(settings: &Arc<Settings>, dialect: Dialect, cache: &PathCache) -> EnteredPath {
    let known = match &cache.path {
        Some(path)
            if cache.dialect == dialect
                && std::ptr::eq(cache.settings.as_ptr(), Arc::as_ptr(settings)) =>
        {
            Some((cache.version, Arc::clone(path)))
        }
        _ => None,
    };
    let state = PathState {
        settings: matches!(dialect, Dialect::PostgreSql | Dialect::Oracle)
            .then(|| Arc::clone(settings)),
        dialect,
        resolved: None,
        relations: None,
        known,
        computed: None,
    };
    let previous = PATH.with(|path| {
        path.try_borrow_mut()
            .ok()
            .and_then(|mut path| path.replace(state))
    });
    EnteredPath { previous }
}

pub(crate) fn path_entries(setting: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for part in split_list(setting) {
        let part = part.trim();
        let name = if let Some(quoted) = part
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
        {
            quoted.replace("\"\"", "\"")
        } else {
            part.to_ascii_lowercase()
        };
        if !name.is_empty() && entries.len() < crate::encoding::MAX_SEARCH_PATH {
            entries.push(name);
        }
    }
    entries
}

fn split_list(setting: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (at, character) in setting.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&setting[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&setting[start..]);
    parts
}

pub(crate) fn schema_exists(reader: &dyn KeyValueRead, name: &str) -> SqlResult<bool> {
    if name == DEFAULT_SCHEMA {
        return Ok(true);
    }
    if name.is_empty() || name.contains(QUALIFIER) {
        return Ok(false);
    }
    match reader.get(&schema_key(name))? {
        Some(bytes) => decode_schema(name, &bytes).map(|()| true),
        None => Ok(false),
    }
}

fn is_system_schema(name: &str) -> bool {
    name == CATALOG_SCHEMA || name == INFORMATION_SCHEMA
}

fn default_path() -> SharedPath {
    Arc::from([DEFAULT_SCHEMA.to_owned()])
}

fn compute_path(
    reader: &dyn KeyValueRead,
    dialect: Dialect,
    settings: Option<&Settings>,
    known: Option<&(u64, SharedPath)>,
) -> SqlResult<SharedPath> {
    let Some(settings) = settings else {
        return Ok(default_path());
    };
    if let (Some(version), Some((known_at, known))) = (reader.committed_version(), known)
        && version == *known_at
    {
        return Ok(Arc::clone(known));
    }
    let user = settings.get("session_authorization").unwrap_or_default();
    let mut existing: Vec<String> = Vec::new();
    for entry in path_entries(&settings.get("search_path").unwrap_or_default()) {
        let name = if entry == "$user" && dialect == Dialect::Oracle {
            DEFAULT_SCHEMA.to_owned()
        } else if entry == "$user" {
            user.clone()
        } else {
            entry
        };
        if existing.contains(&name) {
            continue;
        }
        if is_system_schema(&name) || schema_exists(reader, &name)? {
            existing.push(name);
        }
    }
    Ok(existing.into())
}

fn shared_path(reader: &dyn KeyValueRead) -> SqlResult<SharedPath> {
    let pending = PATH.with(|path| {
        let path = path.try_borrow().ok()?;
        let state = path.as_ref()?;
        Some(match &state.resolved {
            Some(resolved) => Ok(Arc::clone(resolved)),
            None => Err((state.settings.clone(), state.dialect, state.known.clone())),
        })
    });
    let (settings, dialect, known) = match pending {
        None => return Ok(default_path()),
        Some(Ok(resolved)) => return Ok(resolved),
        Some(Err(inputs)) => inputs,
    };
    let resolved = if settings.is_none() {
        default_path()
    } else {
        compute_path(reader, dialect, settings.as_deref(), known.as_ref())?
    };
    let relations: SharedPath = if resolved.iter().any(|schema| is_system_schema(schema)) {
        resolved
            .iter()
            .filter(|schema| !is_system_schema(schema))
            .cloned()
            .collect::<Vec<_>>()
            .into()
    } else {
        Arc::clone(&resolved)
    };
    let version = reader.committed_version();
    PATH.with(|path| {
        if let Ok(mut path) = path.try_borrow_mut()
            && let Some(state) = path.as_mut()
        {
            state.resolved = Some(Arc::clone(&resolved));
            state.relations = Some(relations);
            state.computed = version;
        }
    });
    Ok(resolved)
}

pub(crate) fn effective_path(reader: &dyn KeyValueRead) -> SqlResult<Vec<String>> {
    Ok(shared_path(reader)?.to_vec())
}

pub(crate) fn shared_relation_path(reader: &dyn KeyValueRead) -> SqlResult<SharedPath> {
    let resolved = shared_path(reader)?;
    let relations = PATH.with(|path| {
        path.try_borrow()
            .ok()
            .and_then(|path| path.as_ref().and_then(|state| state.relations.clone()))
    });
    Ok(relations.unwrap_or(resolved))
}

pub(crate) fn cached_relation_path() -> Vec<String> {
    PATH.with(|path| {
        path.try_borrow()
            .ok()
            .and_then(|path| path.as_ref().and_then(|state| state.relations.clone()))
    })
    .map_or_else(|| vec![DEFAULT_SCHEMA.to_owned()], |path| path.to_vec())
}

pub(crate) fn relation_path(reader: &dyn KeyValueRead) -> SqlResult<Vec<String>> {
    Ok(shared_relation_path(reader)?.to_vec())
}

pub(crate) fn is_default_path(path: &[String]) -> bool {
    path.len() == 1 && path[0] == DEFAULT_SCHEMA
}

fn relation_stored(reader: &dyn KeyValueRead, internal: &str) -> SqlResult<bool> {
    Ok(reader.get(&table_key(internal))?.is_some()
        || reader.get(&sequence_key(internal))?.is_some())
}

pub(crate) fn resolve_relation(reader: &dyn KeyValueRead, name: &str) -> SqlResult<String> {
    if is_qualified(name) || crate::temp_tables::is_temp_table(name) {
        return Ok(normalized(name).to_owned());
    }
    let path = relation_path(reader)?;
    if path.len() == 1 && path[0] == DEFAULT_SCHEMA {
        return Ok(name.to_owned());
    }
    for schema in &path {
        let candidate = qualified(schema, name);
        if relation_stored(reader, &candidate)? {
            return Ok(candidate);
        }
    }
    Ok(name.to_owned())
}

pub(crate) fn current_schema(reader: &dyn KeyValueRead) -> SqlResult<Option<String>> {
    Ok(effective_path(reader)?.into_iter().next())
}

pub(crate) fn current_schemas(reader: &dyn KeyValueRead, implicit: bool) -> SqlResult<String> {
    let mut schemas = effective_path(reader)?;
    if implicit && !schemas.iter().any(|schema| schema == CATALOG_SCHEMA) {
        schemas.insert(0, CATALOG_SCHEMA.to_owned());
    }
    let shown: Vec<String> = schemas
        .iter()
        .map(|schema| {
            if schema.is_empty()
                || schema
                    .chars()
                    .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | ' '))
            {
                format!("\"{}\"", schema.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                schema.clone()
            }
        })
        .collect();
    Ok(format!("{{{}}}", shown.join(",")))
}

pub(crate) fn missing_schema(name: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Unknown database '{name}'"),
        )
        .with_vendor_code(1049),
        _ => SqlError::new(
            SqlState::INVALID_SCHEMA_NAME,
            format!("schema \"{name}\" does not exist"),
        ),
    }
}

pub(crate) fn creation_name(reader: &dyn KeyValueRead, name: &Name) -> SqlResult<String> {
    let text = normalized(&name.text);
    if let Some((schema, _)) = text.split_once(QUALIFIER) {
        if !schema_exists(reader, schema)? {
            return Err(missing_schema(schema, Dialect::PostgreSql).at(name.position));
        }
        return Ok(text.to_owned());
    }
    if text.len() != name.text.len() {
        return Ok(text.to_owned());
    }
    let path = relation_path(reader)?;
    match path.first() {
        Some(schema) => Ok(qualified(schema, &name.text)),
        None => Err(SqlError::new(
            SqlState::INVALID_SCHEMA_NAME,
            "no schema has been selected to create in",
        )
        .at(name.position)),
    }
}

fn check_new_schema_name(name: &Name, dialect: Dialect) -> SqlResult<()> {
    crate::constraints::check_identifier(name)?;
    if dialect == Dialect::PostgreSql
        && (name.text.starts_with("pg_") || name.text == INFORMATION_SCHEMA)
    {
        return Err(SqlError::new(
            SqlState::RESERVED_NAME,
            format!("unacceptable schema name \"{}\"", name.text),
        )
        .with_detail(
            "The prefix \"pg_\" and the name information_schema are reserved for system schemas.",
        )
        .at(name.position));
    }
    Ok(())
}

pub(crate) fn duplicate_schema(name: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Can't create database '{name}'; database exists"),
        )
        .with_vendor_code(1007),
        _ => SqlError::new(
            SqlState::DUPLICATE_SCHEMA,
            format!("schema \"{name}\" already exists"),
        ),
    }
}

pub(crate) fn create_schema(
    writer: &mut dyn KeyValueWrite,
    name: &Name,
    if_not_exists: bool,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let tag = command_tag(dialect, "CREATE");
    check_new_schema_name(name, dialect)?;
    let owner_schema = dialect == Dialect::SqlServer && name.text == "dbo";
    if schema_exists(writer, &name.text)? || is_system_schema(&name.text) || owner_schema {
        if if_not_exists {
            return Ok(QueryResult::command(tag).with_notice(format!(
                "NOTICE:  schema \"{}\" already exists, skipping",
                name.text
            )));
        }
        return Err(duplicate_schema(&name.text, dialect).at(name.position));
    }
    writer.delete(&schema_comment_key(&name.text))?;
    writer.put(&schema_key(&name.text), &encode_schema())?;
    Ok(QueryResult::command(tag))
}

fn command_tag(dialect: Dialect, verb: &str) -> String {
    match dialect {
        Dialect::MySql => format!("{verb} DATABASE"),
        _ => format!("{verb} SCHEMA"),
    }
}

pub(crate) struct SchemaContents {
    pub(crate) relations: Vec<String>,
    pub(crate) sequences: Vec<String>,
}

pub(crate) fn contents(
    reader: &dyn KeyValueRead,
    schema: &str,
    most: usize,
) -> SqlResult<SchemaContents> {
    let mut found = SchemaContents {
        relations: Vec::new(),
        sequences: Vec::new(),
    };
    let schema_prefix = format!("{schema}{QUALIFIER}");
    for (prefix, into) in [
        (table_key(&schema_prefix), 0),
        (sequence_key(&schema_prefix), 1),
    ] {
        let start = prefix.len() - schema_prefix.len();
        let mut failure = None;
        reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |key, _| {
                match std::str::from_utf8(&key[start..]) {
                    Ok(name) if is_well_formed(name) => {
                        let list = if into == 0 {
                            &mut found.relations
                        } else {
                            &mut found.sequences
                        };
                        list.push(name.to_owned());
                    }
                    _ => {
                        failure = Some(crate::encoding::corrupt("schema object name"));
                        return Ok(false);
                    }
                }
                Ok(found.relations.len() + found.sequences.len() < most)
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
    }
    Ok(found)
}

pub(crate) fn drop_schemas(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command(command_tag(dialect, "DROP"));
    for name in names {
        if name.text == DEFAULT_SCHEMA || is_system_schema(&name.text) {
            return Err(SqlError::not_supported(format!(
                "dropping schema \"{}\" is not supported",
                name.text
            ))
            .with_hint("EggDB keeps the public schema and the system schemas.")
            .at(name.position));
        }
        if !schema_exists(writer, &name.text)? {
            if if_exists {
                result = result.with_notice(match dialect {
                    Dialect::MySql => format!(
                        "NOTICE:  Can't drop database '{}'; database doesn't exist",
                        name.text
                    ),
                    _ => format!("NOTICE:  schema \"{}\" does not exist, skipping", name.text),
                });
                continue;
            }
            return Err(match dialect {
                Dialect::MySql => SqlError::new(
                    SqlState::MYSQL_GENERAL,
                    format!(
                        "Can't drop database '{}'; database doesn't exist",
                        name.text
                    ),
                )
                .with_vendor_code(1008),
                _ => missing_schema(&name.text, dialect),
            }
            .at(name.position));
        }
        let cascade = cascade || dialect == Dialect::MySql;
        loop {
            let found = contents(writer, &name.text, DROP_BATCH)?;
            if found.relations.is_empty() && found.sequences.is_empty() {
                break;
            }
            if !cascade {
                let first = found
                    .relations
                    .first()
                    .or(found.sequences.first())
                    .map(|object| display(object))
                    .unwrap_or_default();
                return Err(SqlError::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop schema {} because other objects depend on it",
                        name.text
                    ),
                )
                .with_detail(format!("{first} depends on schema {}", name.text))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
                .at(name.position));
            }
            result = drop_contents(writer, tables, found, dialect, result)?;
        }
        writer.delete(&schema_comment_key(&name.text))?;
        writer.delete(&schema_key(&name.text))?;
    }
    Ok(result)
}

fn drop_contents(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    found: SchemaContents,
    dialect: Dialect,
    mut result: QueryResult,
) -> SqlResult<QueryResult> {
    for relation in &found.relations {
        let Some(bytes) = writer.get(&table_key(relation))? else {
            continue;
        };
        let name = vec![Name {
            text: relation.clone(),
            position: 0,
        }];
        let (kind, dropped) = if crate::encoding::is_view_record(&bytes) {
            (
                "view",
                crate::views::drop_views(writer, tables, &name, true, true, dialect)?,
            )
        } else {
            (
                "table",
                crate::execute::drop_tables(writer, tables, &name, true, true, dialect)?,
            )
        };
        result.notices.push(format!(
            "NOTICE:  drop cascades to {kind} {}",
            display(relation)
        ));
        result.notices.extend(dropped.notices);
    }
    for sequence in &found.sequences {
        if crate::sequences::read(writer, sequence)?.is_none() {
            continue;
        }
        let name = vec![Name {
            text: sequence.clone(),
            position: 0,
        }];
        let dropped = crate::sequences::drop_sequences(writer, tables, &name, true, true, dialect)?;
        result.notices.push(format!(
            "NOTICE:  drop cascades to sequence {}",
            display(sequence)
        ));
        result.notices.extend(dropped.notices);
    }
    Ok(result)
}

pub(crate) fn rename_schema(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    from: &Name,
    to: &Name,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    if from.text == DEFAULT_SCHEMA || is_system_schema(&from.text) {
        return Err(SqlError::not_supported(format!(
            "renaming schema \"{}\" is not supported",
            from.text
        ))
        .at(from.position));
    }
    if !schema_exists(writer, &from.text)? {
        return Err(missing_schema(&from.text, dialect).at(from.position));
    }
    check_new_schema_name(to, dialect)?;
    if schema_exists(writer, &to.text)? || is_system_schema(&to.text) {
        return Err(duplicate_schema(&to.text, dialect).at(to.position));
    }
    writer.put(&schema_key(&to.text), &encode_schema())?;
    loop {
        let found = contents(writer, &from.text, DROP_BATCH)?;
        if found.relations.is_empty() && found.sequences.is_empty() {
            break;
        }
        for object in found.relations.iter().chain(&found.sequences) {
            if let Some(view) = crate::views::first_dependent(writer, object)? {
                return Err(SqlError::not_supported(format!(
                    "cannot rename schema {} because view {} depends on {}",
                    from.text,
                    display(&view),
                    display(object)
                ))
                .with_hint("Drop the view, rename the schema, then create the view again.")
                .at(from.position));
            }
        }
        for relation in &found.relations {
            let target = Name {
                text: qualified(&to.text, base_name(relation)),
                position: to.position,
            };
            let Some(bytes) = writer.get(&table_key(relation))? else {
                continue;
            };
            if crate::encoding::is_view_record(&bytes) {
                crate::views::move_view(writer, relation, &target.text)?;
            } else {
                crate::alter::move_table(writer, tables, relation, &target, dialect)?;
            }
        }
        for sequence in &found.sequences {
            crate::sequences::move_sequence(
                writer,
                sequence,
                &qualified(&to.text, base_name(sequence)),
            )?;
        }
    }
    crate::comments::move_schema_comment(writer, &from.text, &to.text)?;
    writer.delete(&schema_key(&from.text))?;
    Ok(QueryResult::command(command_tag(dialect, "ALTER")))
}
