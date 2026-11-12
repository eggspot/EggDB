use crate::ast::{AlterSequence, CreateSequence, Expr, ExprKind, Literal, Name, SequenceOptions};
use crate::dialect::Dialect;
use crate::encoding::{
    SequenceDefinition, TableDefinition, integer_type_range, prefix_end, sequence_key,
    sequences_prefix, table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::lexer::{swap_uniform_case, tokenize};
use crate::memory::WorkMemory;
use crate::parser::Parser;
use crate::session::{OutputColumn, QueryResult};
use crate::set_returning::GeneratedRows;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{SqlType, Value};
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionSequences {
    currval: HashMap<String, i64>,
    lastval: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SequenceAdvance {
    pub(crate) name: String,
    pub(crate) sequence: SequenceDefinition,
    pub(crate) before: (i64, bool),
}

struct StatementSequences {
    dialect: Dialect,
    loaded: HashMap<String, Option<SequenceDefinition>>,
    all_loaded: bool,
    wanted: bool,
    changed: Vec<String>,
    first_changed_by: &'static str,
    before: HashMap<String, (i64, bool)>,
    session: SessionSequences,
    memory: WorkMemory,
    missing: Option<String>,
    identities: Option<HashMap<String, IdentityCounter>>,
}

struct IdentityCounter {
    next: Option<i64>,
    increment: i64,
}

/// The name PostgreSQL gives the sequence behind `table`'s identity or serial `column`.
pub(crate) fn implicit_sequence_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_seq")
}

const MAX_IDENTIFIER_BYTES: usize = 63;
const NEXTVAL: &str = "nextval()";

thread_local! {
    static STATEMENT: RefCell<Vec<StatementSequences>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn enter(session: SessionSequences, dialect: Dialect, wanted: bool) {
    STATEMENT.with(|state| {
        state.borrow_mut().push(StatementSequences {
            dialect,
            loaded: HashMap::new(),
            all_loaded: false,
            wanted,
            changed: Vec::new(),
            first_changed_by: NEXTVAL,
            before: HashMap::new(),
            session,
            memory: WorkMemory::new(),
            missing: None,
            identities: None,
        });
    });
}

pub(crate) fn leave() -> (SessionSequences, Vec<SequenceAdvance>) {
    let Some(state) = STATEMENT.with(|state| state.borrow_mut().pop()) else {
        return (SessionSequences::default(), Vec::new());
    };
    let StatementSequences {
        mut loaded,
        changed,
        before,
        session,
        ..
    } = state;
    let advances = changed
        .into_iter()
        .filter_map(|name| {
            let sequence = loaded.remove(&name).flatten()?;
            let before = before.get(&name).copied()?;
            Some(SequenceAdvance {
                name,
                sequence,
                before,
            })
        })
        .collect();
    (session, advances)
}

fn with_state<T>(body: impl FnOnce(&mut StatementSequences) -> SqlResult<T>) -> SqlResult<T> {
    STATEMENT.with(|state| match state.borrow_mut().last_mut() {
        Some(state) => body(state),
        None => Err(SqlError::not_supported(
            "sequence functions outside a statement of a session",
        )),
    })
}

pub(crate) fn load_for_sequence_calls(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let wanted = STATEMENT.with(|state| state.borrow().last().is_some_and(|state| state.wanted));
    if !wanted {
        return Ok(());
    }
    load_all(reader)?;
    load_identity_counters(reader)
}

fn load_identity_counters(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let needed = STATEMENT.with(|state| {
        state
            .borrow()
            .last()
            .is_some_and(|state| state.identities.is_none())
    });
    if !needed {
        return Ok(());
    }
    let mut tables = crate::indexes::every_table(reader)?;
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let catalog = crate::partitions::load(reader)?;
    let mut found = Vec::new();
    for table in &tables {
        let schema = crate::schemas::schema_of(&table.name);
        let base = crate::schemas::base_name(&table.name);
        for (index, column) in table.columns.iter().enumerate() {
            if table.identity_of(index).is_none() {
                continue;
            }
            let name =
                crate::schemas::qualified(schema, &implicit_sequence_name(base, &column.name));
            let next = if table.auto_increment_column() == Some(index) {
                crate::partition_dml::next_identity_value(reader, &catalog, table)?
            } else {
                None
            };
            found.push((
                name,
                IdentityCounter {
                    next,
                    increment: table.identity_seed().increment,
                },
            ));
        }
    }
    with_state(|state| {
        let mut identities = HashMap::new();
        for (name, counter) in found {
            if !state.loaded.contains_key(&name) && !identities.contains_key(&name) {
                identities.insert(name, counter);
            }
        }
        state.identities = Some(identities);
        Ok(())
    })
}

fn identity_counter<'s>(state: &'s StatementSequences, name: &str) -> Option<&'s IdentityCounter> {
    state
        .identities
        .as_ref()
        .and_then(|identities| identities.get(name))
}

fn drawn_by_inserts(name: &str) -> SqlError {
    SqlError::not_supported(format!(
        "nextval and currval of \"{name}\", the sequence an identity column owns"
    ))
    .with_hint("Insert a row to take the column's next value; setval may set it to the value it already has.")
}

pub(crate) fn load_all(reader: &dyn KeyValueRead) -> SqlResult<()> {
    let needed =
        STATEMENT.with(|state| state.borrow().last().is_some_and(|state| !state.all_loaded));
    if !needed {
        return Ok(());
    }
    let prefix = sequences_prefix();
    let mut found: Vec<(String, SequenceDefinition)> = Vec::new();
    let mut memory = WorkMemory::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let name = String::from_utf8_lossy(&key[prefix.len()..]).into_owned();
            memory.charge(key.len() + value.len() + 64)?;
            match SequenceDefinition::decode(value) {
                Ok(sequence) => found.push((name, sequence)),
                Err(error) => {
                    failure = Some(error);
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    with_state(|state| {
        for (name, sequence) in found {
            state.memory.charge(name.len() + 128)?;
            if !state.loaded.contains_key(&name) {
                state
                    .before
                    .insert(name.clone(), (sequence.last_value, sequence.is_called));
                state.loaded.insert(name, Some(sequence));
            }
        }
        state.all_loaded = true;
        Ok(())
    })
}

pub(crate) fn save_changed(writer: &mut dyn KeyValueWrite) -> SqlResult<()> {
    let changed: Vec<(String, SequenceDefinition)> = STATEMENT.with(|state| {
        state.borrow().last().map_or_else(Vec::new, |state| {
            state
                .changed
                .iter()
                .filter_map(|name| {
                    state
                        .loaded
                        .get(name)
                        .and_then(Option::as_ref)
                        .map(|sequence| (name.clone(), sequence.clone()))
                })
                .collect()
        })
    });
    for (name, sequence) in changed {
        if writer.get(&sequence_key(&name))?.is_some() {
            writer.put(&sequence_key(&name), &sequence.encode())?;
        }
    }
    Ok(())
}

pub(crate) fn first_changing_function() -> &'static str {
    STATEMENT.with(|state| {
        state
            .borrow()
            .last()
            .map_or(NEXTVAL, |state| state.first_changed_by)
    })
}

pub(crate) fn forget_changes() {
    STATEMENT.with(|state| {
        if let Some(state) = state.borrow_mut().last_mut() {
            state.changed.clear();
        }
    });
}

pub(crate) fn mark_changes_committed() {
    STATEMENT.with(|state| {
        if let Some(state) = state.borrow_mut().last_mut() {
            state.changed.clear();
            for (name, sequence) in &state.loaded {
                if let Some(sequence) = sequence {
                    state
                        .before
                        .insert(name.clone(), (sequence.last_value, sequence.is_called));
                }
            }
        }
    });
}

pub(crate) fn discard_session_values() {
    STATEMENT.with(|state| {
        if let Some(state) = state.borrow_mut().last_mut() {
            state.session = SessionSequences::default();
        }
    });
}

pub(crate) fn forget_loaded() {
    STATEMENT.with(|state| {
        if let Some(state) = state.borrow_mut().last_mut() {
            state.loaded.retain(|name, _| state.changed.contains(name));
            state.all_loaded = false;
        }
    });
}

pub(crate) fn mentions_sequence_call(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["nextval", "currval", "setval", "lastval"]
        .iter()
        .any(|name| lower.contains(name))
        || mentions_next_value_for(&lower)
}

fn mentions_next_value_for(lower: &str) -> bool {
    lower.match_indices("next").any(|(at, word)| {
        lower
            .get(at + word.len()..)
            .is_some_and(|rest| rest.trim_start().starts_with("value"))
    })
}

pub(crate) fn mentions_nextval(table: &TableDefinition) -> bool {
    table
        .columns
        .iter()
        .filter_map(|column| column.default.as_deref())
        .chain(table.checks.iter().map(|check| check.expression.as_str()))
        .any(mentions_sequence_call)
}

pub(crate) fn read(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<SequenceDefinition>> {
    reader
        .get(&sequence_key(name))?
        .map(|bytes| SequenceDefinition::decode(&bytes))
        .transpose()
}

pub(crate) fn every_sequence(
    reader: &dyn KeyValueRead,
) -> SqlResult<Vec<(String, SequenceDefinition)>> {
    let prefix = sequences_prefix();
    let mut found = Vec::new();
    let mut memory = WorkMemory::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            memory.charge(key.len() + value.len() + 64)?;
            let name = String::from_utf8_lossy(&key[prefix.len()..]).into_owned();
            match SequenceDefinition::decode(value) {
                Ok(sequence) => found.push((name, sequence)),
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

const RELATION_COLUMNS: [(&str, SqlType); 3] = [
    ("last_value", SqlType::BigInt),
    ("log_cnt", SqlType::BigInt),
    ("is_called", SqlType::Boolean),
];

const MARIADB_RELATION_COLUMNS: [(&str, SqlType); 8] = [
    ("next_not_cached_value", SqlType::BigInt),
    ("minimum_value", SqlType::BigInt),
    ("maximum_value", SqlType::BigInt),
    ("start_value", SqlType::BigInt),
    ("increment", SqlType::BigInt),
    ("cache_size", SqlType::BigInt),
    ("cycle_option", SqlType::SmallInt),
    ("cycle_count", SqlType::BigInt),
];

fn mariadb_relation_rows(
    reader: &dyn KeyValueRead,
    name: &str,
    columns_only: bool,
) -> SqlResult<Option<GeneratedRows>> {
    let columns: Vec<OutputColumn> = MARIADB_RELATION_COLUMNS
        .iter()
        .map(|(column, sql_type)| OutputColumn {
            name: (*column).to_owned(),
            sql_type: Some(*sql_type),
        })
        .collect();
    let Some(sequence) = read(reader, name)? else {
        return Ok(None);
    };
    if columns_only {
        return Ok(Some(GeneratedRows::empty(columns)));
    }
    let next = if sequence.is_called {
        sequence.last_value.saturating_add(sequence.increment)
    } else {
        sequence.last_value
    };
    let row = vec![
        Value::BigInt(next),
        Value::BigInt(sequence.min),
        Value::BigInt(sequence.max),
        Value::BigInt(sequence.start),
        Value::BigInt(sequence.increment),
        Value::BigInt(sequence.cache),
        Value::SmallInt(i16::from(sequence.cycle)),
        Value::BigInt(0),
    ];
    let mut charge = WorkMemory::new();
    charge.charge_row(&row)?;
    Ok(Some(GeneratedRows {
        columns,
        rows: vec![row],
        charge,
    }))
}

/// Whether `name` is a sequence that a query can read as PostgreSQL's one-row relation:
/// a stored sequence or the implicit one behind an identity column.
pub(crate) fn readable_as_relation(reader: &dyn KeyValueRead, name: &str) -> SqlResult<bool> {
    Ok(relation_state(reader, name)?.is_some())
}

/// The one row PostgreSQL shows for `SELECT * FROM <sequence>`: `last_value`, `log_cnt`
/// and `is_called`, which `pg_dump` reads to restore the sequence with `setval`.
pub(crate) fn relation_rows(
    reader: &dyn KeyValueRead,
    name: &str,
    columns_only: bool,
) -> SqlResult<GeneratedRows> {
    if crate::constraints::current_dialect() == Dialect::MySql
        && let Some(rows) = mariadb_relation_rows(reader, name, columns_only)?
    {
        return Ok(rows);
    }
    let columns: Vec<OutputColumn> = RELATION_COLUMNS
        .iter()
        .map(|(column, sql_type)| OutputColumn {
            name: (*column).to_owned(),
            sql_type: Some(*sql_type),
        })
        .collect();
    if columns_only {
        return Ok(GeneratedRows::empty(columns));
    }
    let Some((last_value, is_called)) = relation_state(reader, name)? else {
        return Err(missing_sequence(name, Dialect::PostgreSql));
    };
    let row = vec![
        Value::BigInt(last_value),
        Value::BigInt(0),
        Value::Boolean(is_called),
    ];
    let mut charge = WorkMemory::new();
    charge.charge_row(&row)?;
    Ok(GeneratedRows {
        columns,
        rows: vec![row],
        charge,
    })
}

fn relation_state(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<(i64, bool)>> {
    if let Some(sequence) = read(reader, name)? {
        return Ok(Some((sequence.last_value, sequence.is_called)));
    }
    identity_relation_state(reader, name)
}

fn identity_relation_state(
    reader: &dyn KeyValueRead,
    name: &str,
) -> SqlResult<Option<(i64, bool)>> {
    let wanted_schema = crate::schemas::schema_of(name);
    let catalog = crate::partitions::load(reader)?;
    for table in crate::indexes::every_table(reader)? {
        let schema = crate::schemas::schema_of(&table.name);
        if schema != wanted_schema {
            continue;
        }
        let base = crate::schemas::base_name(&table.name);
        for (index, column) in table.columns.iter().enumerate() {
            if table.identity_of(index).is_none()
                || crate::schemas::qualified(schema, &implicit_sequence_name(base, &column.name))
                    != name
            {
                continue;
            }
            let seed = table.identity_seed();
            let next = if table.auto_increment_column() == Some(index) {
                crate::partition_dml::next_identity_value(reader, &catalog, &table)?
            } else {
                None
            };
            let state = match next.filter(|next| *next != seed.seed) {
                Some(next) => next
                    .checked_sub(seed.increment)
                    .map_or((seed.seed, false), |last| (last, true)),
                None => (seed.seed, false),
            };
            return Ok(Some(state));
        }
    }
    Ok(None)
}

fn shown(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::Oracle => swap_uniform_case(name),
        _ => name.to_owned(),
    }
}

#[cold]
#[inline(never)]
fn missing_sequence(name: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::Oracle => SqlError::new(SqlState::UNDEFINED_TABLE, "sequence does not exist")
            .with_oracle_code(2289),
        _ => SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        ),
    }
}

#[cold]
#[inline(never)]
fn invalid(message: String, oracle: Option<(u16, &str)>, dialect: Dialect) -> SqlError {
    match (dialect, oracle) {
        (Dialect::Oracle, Some((code, text))) => {
            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, text).with_oracle_code(code)
        }
        _ => SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message),
    }
}

fn apply_options(
    sequence: &mut SequenceDefinition,
    options: &SequenceOptions,
    creating: bool,
    dialect: Dialect,
) -> SqlResult<()> {
    let old_range = integer_type_range(sequence.sql_type).unwrap_or((i64::MIN, i64::MAX));
    if let Some((sql_type, position)) = options.sql_type {
        if integer_type_range(sql_type).is_none() {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "sequence type must be smallint, integer, or bigint",
            )
            .at(position));
        }
        sequence.sql_type = sql_type;
    }
    let (low, high) = integer_type_range(sequence.sql_type).unwrap_or((i64::MIN, i64::MAX));
    if let Some(increment) = options.increment {
        if increment == 0 {
            return Err(invalid(
                "INCREMENT must not be zero".to_owned(),
                Some((4002, "INCREMENT must be a nonzero integer")),
                dialect,
            ));
        }
        sequence.increment = increment;
    }
    let ascending = sequence.increment > 0;
    match options.max {
        Some(Some(max)) => sequence.max = max,
        Some(None) => sequence.max = if ascending { high } else { -1 },
        None if creating || (options.sql_type.is_some() && sequence.max == old_range.1) => {
            sequence.max = if ascending { high } else { -1 }
        }
        None => {}
    }
    match options.min {
        Some(Some(min)) => sequence.min = min,
        Some(None) => sequence.min = if ascending { 1 } else { low },
        None if creating || (options.sql_type.is_some() && sequence.min == old_range.0) => {
            sequence.min = if ascending { 1 } else { low }
        }
        None => {}
    }
    let type_name = sequence.sql_type.name();
    if sequence.min < low || sequence.min > high {
        return Err(invalid(
            format!(
                "MINVALUE ({}) is out of range for sequence data type {type_name}",
                sequence.min
            ),
            None,
            dialect,
        ));
    }
    if sequence.max < low || sequence.max > high {
        return Err(invalid(
            format!(
                "MAXVALUE ({}) is out of range for sequence data type {type_name}",
                sequence.max
            ),
            None,
            dialect,
        ));
    }
    if sequence.min >= sequence.max {
        return Err(invalid(
            format!(
                "MINVALUE ({}) must be less than MAXVALUE ({})",
                sequence.min, sequence.max
            ),
            Some((4004, "MINVALUE must be less than MAXVALUE")),
            dialect,
        ));
    }
    match options.start {
        Some(start) => sequence.start = start,
        None if creating => {
            sequence.start = if ascending {
                sequence.min
            } else {
                sequence.max
            }
        }
        None => {}
    }
    if sequence.start < sequence.min {
        return Err(invalid(
            format!(
                "START value ({}) cannot be less than MINVALUE ({})",
                sequence.start, sequence.min
            ),
            Some((4006, "START WITH cannot be less than MINVALUE")),
            dialect,
        ));
    }
    if sequence.start > sequence.max {
        return Err(invalid(
            format!(
                "START value ({}) cannot be greater than MAXVALUE ({})",
                sequence.start, sequence.max
            ),
            Some((4008, "START WITH cannot be more than MAXVALUE")),
            dialect,
        ));
    }
    if creating {
        sequence.last_value = sequence.start;
        sequence.is_called = false;
    }
    if let Some(restart) = options.restart {
        let value = restart.unwrap_or(sequence.start);
        if value < sequence.min {
            return Err(invalid(
                format!(
                    "RESTART value ({value}) cannot be less than MINVALUE ({})",
                    sequence.min
                ),
                None,
                dialect,
            ));
        }
        if value > sequence.max {
            return Err(invalid(
                format!(
                    "RESTART value ({value}) cannot be greater than MAXVALUE ({})",
                    sequence.max
                ),
                None,
                dialect,
            ));
        }
        sequence.last_value = value;
        sequence.is_called = false;
    }
    if sequence.last_value < sequence.min {
        return Err(invalid(
            format!(
                "RESTART value ({}) cannot be less than MINVALUE ({})",
                sequence.last_value, sequence.min
            ),
            Some((4006, "START WITH cannot be less than MINVALUE")),
            dialect,
        ));
    }
    if sequence.last_value > sequence.max {
        return Err(invalid(
            format!(
                "RESTART value ({}) cannot be greater than MAXVALUE ({})",
                sequence.last_value, sequence.max
            ),
            Some((4008, "START WITH cannot be more than MAXVALUE")),
            dialect,
        ));
    }
    if let Some(cache) = options.cache {
        if cache < 1 {
            return Err(invalid(
                format!("CACHE ({cache}) must be greater than zero"),
                Some((4010, "the number of values to CACHE must be greater than 1")),
                dialect,
            ));
        }
        sequence.cache = cache;
    }
    if let Some(cycle) = options.cycle {
        sequence.cycle = cycle;
    }
    Ok(())
}

fn owner_of(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    owned_by: &Option<(Name, Name)>,
) -> SqlResult<Option<(String, String)>> {
    let Some((table_name, column)) = owned_by else {
        return Ok(None);
    };
    let resolved = crate::schemas::resolve_relation(reader, &table_name.text)?;
    let table = tables.table(reader, &resolved)?.ok_or_else(|| {
        SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{}\" does not exist", table_name.text),
        )
        .at(table_name.position)
    })?;
    if table.column_index(&column.text).is_none() {
        return Err(SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!(
                "column \"{}\" of relation \"{}\" does not exist",
                column.text, table.name
            ),
        )
        .at(column.position));
    }
    Ok(Some((table.name.clone(), column.text.clone())))
}

pub(crate) fn relation_exists(reader: &dyn KeyValueRead, name: &str) -> SqlResult<bool> {
    Ok(reader.get(&table_key(name))?.is_some() || reader.get(&sequence_key(name))?.is_some())
}

pub(crate) fn create_sequence(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateSequence,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let name = &create.name.text;
    crate::constraints::check_identifier(&create.name)?;
    if relation_exists(writer, name)? {
        if create.if_not_exists {
            return Ok(QueryResult::command("CREATE SEQUENCE").with_notice(format!(
                "NOTICE:  relation \"{name}\" already exists, skipping"
            )));
        }
        return Err(match dialect {
            Dialect::Oracle => SqlError::new(
                SqlState::DUPLICATE_TABLE,
                "name is already used by an existing object",
            )
            .with_oracle_code(955),
            _ => SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ),
        }
        .at(create.name.position));
    }
    let mut sequence = SequenceDefinition {
        sql_type: SqlType::BigInt,
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        cache: 1,
        cycle: false,
        last_value: 1,
        is_called: false,
        owned_by: None,
    };
    apply_options(&mut sequence, &create.options, true, dialect)?;
    if let Some(owned_by) = &create.options.owned_by {
        sequence.owned_by = owner_of(writer, tables, owned_by)?;
    }
    crate::comments::forget_owner(writer, name)?;
    writer.put(&sequence_key(name), &sequence.encode())?;
    forget_loaded();
    if let Some((owner, _)) = &sequence.owned_by {
        crate::constraints::refresh_flags(writer, tables, owner)?;
    }
    Ok(QueryResult::command("CREATE SEQUENCE"))
}

pub(crate) fn alter_sequence(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    alter: &AlterSequence,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let name = &alter.name.text;
    let Some(mut sequence) = read(writer, name)? else {
        if alter.if_exists {
            return Ok(QueryResult::command("ALTER SEQUENCE").with_notice(format!(
                "NOTICE:  relation \"{name}\" does not exist, skipping"
            )));
        }
        return Err(missing_sequence(name, dialect).at(alter.name.position));
    };
    if let Some(to) = &alter.rename {
        crate::constraints::check_identifier(to)?;
        if relation_exists(writer, &to.text)? {
            return Err(SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists", to.text),
            )
            .at(to.position));
        }
        if !every_default_referring(writer, name)?.is_empty() {
            return Err(
                SqlError::not_supported("renaming a sequence that a column default uses")
                    .at(alter.name.position),
            );
        }
        writer.delete(&sequence_key(name))?;
        writer.put(&sequence_key(&to.text), &sequence.encode())?;
        crate::comments::move_owner(writer, name, &to.text)?;
        forget_loaded();
        return Ok(QueryResult::command("ALTER SEQUENCE"));
    }
    apply_options(&mut sequence, &alter.options, false, dialect)?;
    let old_owner = sequence.owned_by.clone();
    if let Some(owned_by) = &alter.options.owned_by {
        sequence.owned_by = owner_of(writer, tables, owned_by)?;
    }
    writer.put(&sequence_key(name), &sequence.encode())?;
    forget_loaded();
    for (owner, _) in old_owner.iter().chain(sequence.owned_by.iter()) {
        crate::constraints::refresh_flags(writer, tables, owner)?;
    }
    Ok(QueryResult::command("ALTER SEQUENCE"))
}

pub(crate) fn check_default_sequences(reader: &dyn KeyValueRead, text: &str) -> SqlResult<()> {
    if !mentions_sequence_call(text) {
        return Ok(());
    }
    for name in default_sequence_names(text) {
        let name = sequence_on_path(reader, name)?;
        if read(reader, &name)?.is_none() {
            return Err(missing_or_not_a_sequence(reader, &name)?);
        }
    }
    Ok(())
}

fn not_a_sequence(name: &str) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("\"{}\" is not a sequence", crate::schemas::base_name(name)),
    )
}

fn missing_or_not_a_sequence(reader: &dyn KeyValueRead, name: &str) -> SqlResult<SqlError> {
    let relation = crate::schemas::resolve_relation(reader, name)?;
    Ok(
        if reader.get(&table_key(&relation))?.is_some()
            || crate::pg_catalog::names_non_sequence_relation(reader, name)
        {
            not_a_sequence(name)
        } else {
            missing_sequence(name, Dialect::PostgreSql)
        },
    )
}

/// The error for an INSERT, UPDATE or DELETE whose target table was not found (`error`):
/// PostgreSQL's "cannot change sequence" (42809) when `target` names a sequence.
pub(crate) fn refuse_changing_sequence(
    reader: &dyn KeyValueRead,
    target: &Name,
    error: SqlError,
) -> SqlError {
    if error.state() != SqlState::UNDEFINED_TABLE || target.text.is_empty() {
        return error;
    }
    let Ok(name) = sequence_on_path(reader, target.text.clone()) else {
        return error;
    };
    match read(reader, &name) {
        Ok(Some(_)) => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!(
                "cannot change sequence \"{}\"",
                crate::schemas::base_name(&name)
            ),
        )
        .at(target.position),
        _ => error,
    }
}

pub(crate) fn explain_missing(reader: &dyn KeyValueRead, error: SqlError) -> SqlError {
    let missing = STATEMENT.with(|state| {
        state
            .try_borrow_mut()
            .ok()
            .and_then(|mut state| state.last_mut().and_then(|state| state.missing.take()))
    });
    let Some(name) = missing else {
        return error;
    };
    if error.state() != SqlState::UNDEFINED_TABLE
        || error.message() != format!("relation \"{name}\" does not exist")
    {
        return error;
    }
    match missing_or_not_a_sequence(reader, &name) {
        Ok(refined) if refined.state() == SqlState::WRONG_OBJECT_TYPE => refined,
        _ => error,
    }
}

pub(crate) fn name_argument<'e>(function: &str, position: usize, argument: &'e Expr) -> &'e Expr {
    if position == 0
        && matches!(function, "nextval" | "currval" | "setval")
        && let ExprKind::Function { name, arguments } = &argument.kind
        && name == "regclass_cast"
        && let [inner] = arguments.as_slice()
    {
        return inner;
    }
    argument
}

pub(crate) fn default_sequence_names(text: &str) -> Vec<String> {
    let Ok(tokens) = tokenize(text) else {
        return Vec::new();
    };
    let mut parser = Parser::new(text, &tokens);
    let Ok(expr) = parser.expression() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect_sequence_names(&expr, &mut names);
    names
}

fn collect_sequence_names(expr: &Expr, names: &mut Vec<String>) {
    if let ExprKind::Function { name, arguments } = &expr.kind
        && matches!(name.as_str(), "nextval" | "currval" | "setval")
        && let Some(first) = arguments.first()
        && let Some(text) = constant_name(first)
        && let Ok(resolved) = regclass_name(&text)
    {
        names.push(resolved);
    }
    for child in crate::eval::children(expr) {
        collect_sequence_names(child, names);
    }
}

fn constant_name(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::String(text)) => Some(text.clone()),
        ExprKind::Cast { expr, .. } => constant_name(expr),
        ExprKind::Function { name, arguments } if name == "regclass_cast" => {
            arguments.first().and_then(constant_name)
        }
        _ => None,
    }
}

pub(crate) fn every_default_referring(
    reader: &dyn KeyValueRead,
    sequence: &str,
) -> SqlResult<Vec<(String, String)>> {
    let mut found = Vec::new();
    for table in crate::indexes::every_table(reader)? {
        for column in &table.columns {
            if let Some(text) = &column.default
                && text.to_ascii_lowercase().contains("nextval")
                && default_sequence_names(text)
                    .iter()
                    .any(|name| name == sequence)
            {
                found.push((table.name.clone(), column.name.clone()));
            }
        }
    }
    Ok(found)
}

pub(crate) fn drop_sequences(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP SEQUENCE");
    for name in names {
        if read(writer, &name.text)?.is_none() {
            if if_exists {
                result = result.with_notice(format!(
                    "NOTICE:  sequence \"{}\" does not exist, skipping",
                    name.text
                ));
                continue;
            }
            return Err(missing_sequence(&name.text, dialect).at(name.position));
        }
        let users = every_default_referring(writer, &name.text)?;
        if !users.is_empty() && dialect != Dialect::Oracle {
            if !cascade {
                let details: Vec<String> = users
                    .iter()
                    .map(|(table, column)| {
                        format!(
                            "default value for column {column} of table {table} depends on sequence {}",
                            name.text
                        )
                    })
                    .collect();
                return Err(SqlError::new(
                    SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop sequence {} because other objects depend on it",
                        name.text
                    ),
                )
                .with_detail(details.join("\n"))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
            }
            for (table_name, column) in &users {
                let Some(table) = tables.table(writer, table_name)? else {
                    continue;
                };
                let mut table = (*table).clone();
                if let Some(index) = table.column_index(column) {
                    table.columns[index].default = None;
                }
                writer.put(&table_key(&table.name), &table.encode())?;
                result = result.with_notice(format!(
                    "NOTICE:  drop cascades to default value for column {column} of table {table_name}"
                ));
            }
        }
        let owner = read(writer, &name.text)?.and_then(|sequence| sequence.owned_by);
        writer.delete(&sequence_key(&name.text))?;
        crate::comments::forget_owner(writer, &name.text)?;
        if let Some((owner, _)) = owner {
            crate::constraints::refresh_flags(writer, tables, &owner)?;
        }
    }
    forget_loaded();
    Ok(result)
}

pub(crate) fn drop_owned_by(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &str,
    column: Option<&str>,
) -> SqlResult<()> {
    for (name, sequence) in every_sequence(writer)? {
        if let Some((owner, owner_column)) = &sequence.owned_by
            && owner == table
            && column.is_none_or(|column| column == owner_column)
        {
            writer.delete(&sequence_key(&name))?;
        }
    }
    forget_loaded();
    crate::constraints::refresh_flags(writer, tables, table)
}

pub(crate) fn rename_owner(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    column: Option<(&str, &str)>,
    new_table: &str,
) -> SqlResult<()> {
    for (name, mut sequence) in every_sequence(writer)? {
        let Some((owner, owner_column)) = &mut sequence.owned_by else {
            continue;
        };
        if owner != table {
            continue;
        }
        match column {
            Some((from, to)) if owner_column == from => *owner_column = to.to_owned(),
            Some(_) => continue,
            None => *owner = new_table.to_owned(),
        }
        writer.put(&sequence_key(&name), &sequence.encode())?;
    }
    forget_loaded();
    Ok(())
}

pub(crate) fn move_sequence(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    let Some(bytes) = writer.get(&sequence_key(from))? else {
        return Ok(());
    };
    writer.delete(&sequence_key(from))?;
    writer.put(&sequence_key(to), &bytes)?;
    crate::comments::move_owner(writer, from, to)?;
    forget_loaded();
    Ok(())
}

pub(crate) fn regclass_name(text: &str) -> SqlResult<String> {
    regclass_target(text).map(|(name, _)| name)
}

fn regclass_target(text: &str) -> SqlResult<(String, bool)> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            "invalid name syntax".to_owned(),
        )
    };
    let mut parts = Vec::new();
    let mut chars = text.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        part.push('"');
                    }
                    Some('"') => break,
                    Some(c) => part.push(c),
                    None => return Err(invalid()),
                }
            }
            if part.is_empty() {
                return Err(invalid());
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                if c == '"' || c.is_whitespace() {
                    return Err(invalid());
                }
                part.push(c.to_ascii_lowercase());
                chars.next();
            }
            if part.is_empty() {
                return Err(invalid());
            }
        }
        parts.push(part);
        match chars.next() {
            None => break,
            Some('.') => {}
            Some(_) => return Err(invalid()),
        }
        if parts.len() > 3 {
            return Err(invalid());
        }
    }
    for part in &mut parts {
        if part.len() > MAX_IDENTIFIER_BYTES {
            let mut end = MAX_IDENTIFIER_BYTES;
            while !part.is_char_boundary(end) {
                end -= 1;
            }
            part.truncate(end);
        }
    }
    if parts
        .iter()
        .any(|part| part.contains(crate::schemas::QUALIFIER))
    {
        return Err(invalid());
    }
    match parts.as_slice() {
        [name] => Ok((name.clone(), false)),
        [schema, name] if schema == "public" || schema == "pg_catalog" => Ok((name.clone(), true)),
        [schema, name] => Ok((crate::schemas::qualified(schema, name), true)),
        _ => Err(SqlError::not_supported(
            "cross-database references are not implemented",
        )),
    }
}

fn on_path(name: String) -> String {
    if crate::schemas::is_qualified(&name) {
        return name;
    }
    let path = crate::schemas::cached_relation_path();
    if path.len() == 1 && path[0] == crate::schemas::DEFAULT_SCHEMA {
        return name;
    }
    let found = STATEMENT.with(|state| {
        let state = state.try_borrow().ok()?;
        let state = state.last()?;
        path.iter()
            .map(|schema| crate::schemas::qualified(schema, &name))
            .find(|candidate| matches!(state.loaded.get(candidate), Some(Some(_))))
    });
    match found {
        Some(candidate) => candidate,
        None if path
            .iter()
            .any(|schema| schema == crate::schemas::DEFAULT_SCHEMA) =>
        {
            name
        }
        None => crate::schemas::unreachable_name(&name),
    }
}

fn sequence_on_path(reader: &dyn KeyValueRead, name: String) -> SqlResult<String> {
    if crate::schemas::is_qualified(&name) {
        return Ok(name);
    }
    for schema in crate::schemas::relation_path(reader)? {
        let candidate = crate::schemas::qualified(&schema, &name);
        if reader.get(&sequence_key(&candidate))?.is_some() {
            return Ok(candidate);
        }
    }
    Ok(name)
}

fn argument_name(value: &Value) -> SqlResult<String> {
    match value {
        Value::Text(text) => {
            regclass_target(text).map(
                |(name, schema_given)| {
                    if schema_given { name } else { on_path(name) }
                },
            )
        }
        Value::BigInt(_) | Value::Integer(_) | Value::SmallInt(_) => Err(SqlError::not_supported(
            "sequence object identifiers (OIDs) are not supported yet",
        )),
        other => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function nextval({}) does not exist", other.type_name()),
        )),
    }
}

fn loaded<'s>(
    state: &'s mut StatementSequences,
    name: &str,
) -> SqlResult<&'s mut SequenceDefinition> {
    let dialect = state.dialect;
    let found = state.loaded.get(name).map(Option::is_some);
    if found == Some(false) || (found.is_none() && state.all_loaded) {
        state.missing = Some(name.to_owned());
        return Err(missing_sequence(name, dialect));
    }
    match state.loaded.get_mut(name) {
        Some(Some(sequence)) => Ok(sequence),
        Some(None) => Err(missing_sequence(name, dialect)),
        None if state.all_loaded => Err(missing_sequence(name, dialect)),
        None => Err(SqlError::not_supported(format!(
            "sequence \"{name}\" used where this statement does not read sequences"
        ))),
    }
}

fn mark_changed(state: &mut StatementSequences, name: &str, function: &'static str) {
    if state.changed.is_empty() {
        state.first_changed_by = function;
    }
    if !state.changed.iter().any(|known| known == name) {
        state.changed.push(name.to_owned());
    }
}

fn next_value(name: &str, sequence: &mut SequenceDefinition, dialect: Dialect) -> SqlResult<i64> {
    let ascending = sequence.increment > 0;
    let candidate = if sequence.is_called {
        sequence.last_value.checked_add(sequence.increment)
    } else {
        Some(sequence.last_value)
    };
    let next = candidate.filter(|&next| next >= sequence.min && next <= sequence.max);
    let value = match next {
        Some(next) => next,
        None if sequence.cycle => {
            if ascending {
                sequence.min
            } else {
                sequence.max
            }
        }
        None => {
            let shown = shown(name, dialect);
            return Err(match dialect {
                Dialect::Oracle => SqlError::new(
                    SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                    format!(
                        "sequence {shown}.NEXTVAL {} and cannot be instantiated",
                        if ascending {
                            "exceeds MAXVALUE"
                        } else {
                            "goes below MINVALUE"
                        }
                    ),
                )
                .with_oracle_code(8004),
                _ => SqlError::new(
                    SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                    if ascending {
                        format!(
                            "nextval: reached maximum value of sequence \"{name}\" ({})",
                            sequence.max
                        )
                    } else {
                        format!(
                            "nextval: reached minimum value of sequence \"{name}\" ({})",
                            sequence.min
                        )
                    },
                ),
            });
        }
    };
    sequence.last_value = value;
    sequence.is_called = true;
    Ok(value)
}

fn nextval(name: &str) -> SqlResult<Value> {
    with_state(|state| {
        if identity_counter(state, name).is_some() {
            return Err(drawn_by_inserts(name));
        }
        let dialect = state.dialect;
        let sequence = loaded(state, name)?;
        let value = next_value(name, sequence, dialect)?;
        mark_changed(state, name, NEXTVAL);
        state.session.currval.insert(name.to_owned(), value);
        state.session.lastval = Some(name.to_owned());
        Ok(Value::BigInt(value))
    })
}

fn currval(name: &str) -> SqlResult<Value> {
    with_state(|state| {
        if identity_counter(state, name).is_some() {
            return Err(drawn_by_inserts(name));
        }
        let dialect = state.dialect;
        loaded(state, name)?;
        match state.session.currval.get(name) {
            Some(&value) => Ok(Value::BigInt(value)),
            None => Err(match dialect {
                Dialect::Oracle => SqlError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "sequence {}.CURRVAL is not yet defined in this session",
                        shown(name, dialect)
                    ),
                )
                .with_oracle_code(8002),
                _ => SqlError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("currval of sequence \"{name}\" is not yet defined in this session"),
                ),
            }),
        }
    })
}

fn lastval() -> SqlResult<Value> {
    with_state(|state| {
        let undefined = || {
            SqlError::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "lastval is not yet defined in this session",
            )
        };
        let name = state.session.lastval.clone().ok_or_else(undefined)?;
        let exists = match state.loaded.get(&name) {
            Some(found) => found.is_some(),
            None => !state.all_loaded,
        };
        if !exists {
            return Err(undefined());
        }
        state
            .session
            .currval
            .get(&name)
            .map(|&value| Value::BigInt(value))
            .ok_or_else(undefined)
    })
}

fn setval(name: &str, value: &Value, is_called: &Value) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let Some(number) = value.as_integer() else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "function setval(regclass, {}) does not exist",
                value.type_name()
            ),
        ));
    };
    let called = match is_called {
        Value::Boolean(flag) => *flag,
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "function setval(regclass, bigint, {}) does not exist",
                    other.type_name()
                ),
            ));
        }
    };
    with_state(|state| {
        if let Some(counter) = identity_counter(state, name) {
            let next = if called {
                number.checked_add(counter.increment)
            } else {
                Some(number)
            };
            if counter.next.is_some() && next == counter.next {
                return Ok(Value::BigInt(number));
            }
            return Err(SqlError::not_supported(format!(
                "setval of \"{name}\", the sequence an identity column owns, to a value other than the one it holds"
            ))
            .with_hint("The column's next value follows its largest value; delete or insert rows to move it."));
        }
        let sequence = loaded(state, name)?;
        if number < sequence.min || number > sequence.max {
            return Err(SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!(
                    "setval: value {number} is out of bounds for sequence \"{name}\" ({}..{})",
                    sequence.min, sequence.max
                ),
            ));
        }
        sequence.last_value = number;
        sequence.is_called = called;
        mark_changed(state, name, "setval()");
        if called {
            state.session.currval.insert(name.to_owned(), number);
        }
        Ok(Value::BigInt(number))
    })
}

/// The internal function `@@DBTS` and `MIN_ACTIVE_ROWVERSION()` read: a sequence's last
/// value drawn, or the given value before its first (or when it does not exist yet).
pub(crate) const LAST_USED_FUNCTION: &str = "eggdb_sequence_last_used";

fn last_used(name: &str, before_first: i64) -> SqlResult<Value> {
    with_state(|state| {
        let known_missing = state
            .loaded
            .get(name)
            .map_or(state.all_loaded, Option::is_none);
        if known_missing {
            return Ok(Value::BigInt(before_first));
        }
        let sequence = loaded(state, name)?;
        Ok(Value::BigInt(if sequence.is_called {
            sequence.last_value
        } else {
            sequence.last_value.saturating_sub(sequence.increment)
        }))
    })
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    Some(match (name, values) {
        (LAST_USED_FUNCTION, [sequence, before_first]) => argument_name(sequence)
            .and_then(|name| last_used(&name, before_first.as_integer().unwrap_or(0))),
        ("nextval" | "currval" | "setval", [Value::Null, ..]) => Ok(Value::Null),
        ("nextval", [argument]) => argument_name(argument).and_then(|name| nextval(&name)),
        ("currval", [argument]) => argument_name(argument).and_then(|name| currval(&name)),
        ("lastval", []) => lastval(),
        ("setval", [argument, value]) => {
            argument_name(argument).and_then(|name| setval(&name, value, &Value::Boolean(true)))
        }
        ("setval", [argument, value, called]) => {
            argument_name(argument).and_then(|name| setval(&name, value, called))
        }
        ("oracle_nextval", [Value::Text(name)]) => nextval(&on_path(name.clone())),
        ("oracle_currval", [Value::Text(name)]) => currval(&on_path(name.clone())),
        ("regclass_cast", [value]) => {
            crate::pg_catalog::reg_value(crate::pg_catalog::RegKind::Class, value.clone())
        }
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    match name {
        "nextval" | "currval" | "lastval" | "setval" | "oracle_nextval" | "oracle_currval" | LAST_USED_FUNCTION => {
            Some(SqlType::BigInt)
        }
        "regclass_cast" => Some(SqlType::Catalog(crate::pg_catalog::CatalogType::Reg(
            crate::pg_catalog::RegKind::Class,
        ))),
        _ => None,
    }
}

pub(crate) fn reapply(
    writer: &mut dyn KeyValueWrite,
    advances: &[SequenceAdvance],
) -> SqlResult<()> {
    for advance in advances {
        let Some(current) = read(writer, &advance.name)? else {
            continue;
        };
        let wanted = &advance.sequence;
        let untouched = current.sql_type == wanted.sql_type
            && current.increment == wanted.increment
            && current.min == wanted.min
            && current.max == wanted.max
            && current.cycle == wanted.cycle
            && (current.last_value, current.is_called) == advance.before;
        if untouched {
            let mut updated = current;
            updated.last_value = wanted.last_value;
            updated.is_called = wanted.is_called;
            writer.put(&sequence_key(&advance.name), &updated.encode())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::{Session, SqlError};

    fn refusal(session: &mut Session<'_>, sql: &str) -> SqlError {
        match session.execute(sql, &mut |_| {}) {
            Ok(()) => panic!("{sql} succeeded"),
            Err(error) => error,
        }
    }

    #[test]
    fn changing_a_sequence_with_insert_update_or_delete_is_refused_like_postgresql() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE SEQUENCE counter; CREATE SCHEMA other; CREATE SEQUENCE other.tick",
                &mut |_| {},
            )
            .unwrap();
        for sql in [
            "INSERT INTO counter VALUES (1, 1, false)",
            "UPDATE counter SET last_value = 5",
            "DELETE FROM counter",
            "DELETE FROM other.tick",
        ] {
            let error = refusal(&mut session, sql);
            assert_eq!(error.state().code(), "42809", "{sql}");
            assert!(
                error.message().starts_with("cannot change sequence \""),
                "{sql}: {}",
                error.message()
            );
        }
        assert_eq!(
            refusal(&mut session, "DELETE FROM missing").state().code(),
            "42P01"
        );
    }
}
