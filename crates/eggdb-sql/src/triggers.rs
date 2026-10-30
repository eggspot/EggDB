use crate::ast::{CreateTrigger, DropTrigger, EnableTrigger, Name, TriggerPlacement};
use crate::dialect::Dialect;
use crate::encoding::{
    TableDefinition, ViewDefinition, decode_triggers, encode_triggers, prefix_end, trigger_key,
    triggers_prefix,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::schemas::{base_name, display, schema_of};
use crate::session::{OutputColumn, QueryResult};
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

mod body;
mod errors;
mod functions;
mod views;

pub(crate) use body::{BodyStep, ConditionalBranch, SignalStep, StepTarget, TriggerFrame};
pub(crate) use functions::run_statement_function;
pub(crate) use views::{delete_instead_of_view, insert_instead_of_view, update_instead_of_view};

#[cfg(test)]
mod function_tests;
#[cfg(test)]
mod tests;

pub(crate) const MAX_TRIGGER_TEXT: usize = 1 << 20;
pub(crate) const MAX_TRIGGERS_PER_TABLE: usize = 1024;
pub(crate) const MAX_TRIGGER_DEPTH: usize = 32;
pub(crate) const RAISE_FUNCTION: &str = "trigger_raise";
pub(crate) const INSERTED_TABLE: &str = "inserted";
pub(crate) const DELETED_TABLE: &str = "deleted";
const CACHED_TABLES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerEvent {
    Insert,
    Update,
    Delete,
    Truncate,
}

impl TriggerEvent {
    fn bit(self) -> u8 {
        match self {
            Self::Insert => 1,
            Self::Update => 2,
            Self::Delete => 4,
            Self::Truncate => 8,
        }
    }

    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TriggerEvents(u8);

impl TriggerEvents {
    const KNOWN: u8 = 0b1111;

    pub(crate) fn with(self, event: TriggerEvent) -> Self {
        Self(self.0 | event.bit())
    }

    pub(crate) fn contains(self, event: TriggerEvent) -> bool {
        self.0 & event.bit() != 0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn bits(self) -> u8 {
        self.0
    }

    pub(crate) fn from_bits(bits: u8) -> Option<Self> {
        (bits != 0 && bits & !Self::KNOWN == 0).then_some(Self(bits))
    }

    pub(crate) fn listed(self) -> Vec<TriggerEvent> {
        [
            TriggerEvent::Insert,
            TriggerEvent::Update,
            TriggerEvent::Delete,
            TriggerEvent::Truncate,
        ]
        .into_iter()
        .filter(|event| self.contains(*event))
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TriggerLevel {
    Row,
    Statement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TriggerAction {
    Statements(String),
    Batch(String),
    Function { name: String, arguments: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerDefinition {
    pub(crate) name: String,
    pub(crate) dialect: Dialect,
    pub(crate) timing: TriggerTiming,
    pub(crate) events: TriggerEvents,
    pub(crate) columns: Vec<String>,
    pub(crate) level: TriggerLevel,
    pub(crate) condition: Option<String>,
    pub(crate) old_table: Option<String>,
    pub(crate) new_table: Option<String>,
    pub(crate) action: TriggerAction,
    pub(crate) enabled: bool,
    pub(crate) text: String,
}

impl TriggerDefinition {
    pub(crate) fn is_consistent(&self) -> bool {
        let action_fits = matches!(
            (&self.action, self.dialect),
            (
                TriggerAction::Statements(_),
                Dialect::Sqlite | Dialect::MySql
            ) | (TriggerAction::Batch(_), Dialect::SqlServer)
                | (TriggerAction::Function { .. }, Dialect::PostgreSql)
        );
        let level_fits = match self.dialect {
            Dialect::SqlServer => {
                self.level == TriggerLevel::Statement && self.timing != TriggerTiming::Before
            }
            Dialect::Sqlite | Dialect::MySql => self.level == TriggerLevel::Row,
            Dialect::PostgreSql => {
                !(self.timing == TriggerTiming::InsteadOf && self.level == TriggerLevel::Statement)
            }
            Dialect::Oracle => false,
        };
        let transition_tables_fit = (self.old_table.is_none() && self.new_table.is_none())
            || self.timing == TriggerTiming::After;
        action_fits
            && level_fits
            && transition_tables_fit
            && !self.events.is_empty()
            && (self.columns.is_empty() || self.events.contains(TriggerEvent::Update))
            && (!self.events.contains(TriggerEvent::Truncate)
                || self.level == TriggerLevel::Statement)
            && !(self.dialect == Dialect::SqlServer && self.condition.is_some())
            && !self.text.is_empty()
    }

    fn fires_on(&self, event: TriggerEvent, updated: &[&str]) -> bool {
        self.enabled
            && self.events.contains(event)
            && (event != TriggerEvent::Update
                || self.columns.is_empty()
                || self.columns.iter().any(|column| {
                    updated
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(column))
                }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableTriggers {
    pub(crate) table: String,
    pub(crate) triggers: Vec<TriggerDefinition>,
}

#[derive(Default)]
pub(crate) struct TriggerCache {
    entries: HashMap<String, CachedTriggers>,
}

struct CachedTriggers {
    bytes: Option<Vec<u8>>,
    checked_at: Option<u64>,
    triggers: Option<Arc<TableTriggers>>,
}

pub(crate) fn of_table(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &str,
) -> SqlResult<Option<Arc<TableTriggers>>> {
    let version = reader.committed_version();
    let cache = &mut tables.triggers;
    if let Some(cached) = cache.entries.get(table)
        && version.is_some()
        && cached.checked_at == version
    {
        return Ok(cached.triggers.clone());
    }
    let bytes = reader.get(&trigger_key(table))?;
    if let Some(cached) = cache.entries.get_mut(table)
        && cached.bytes == bytes
    {
        cached.checked_at = version;
        return Ok(cached.triggers.clone());
    }
    let triggers = match &bytes {
        Some(bytes) => Some(Arc::new(decode_triggers(table, bytes)?)),
        None => None,
    };
    if cache.entries.len() >= CACHED_TABLES {
        cache.entries.clear();
    }
    cache.entries.insert(
        table.to_owned(),
        CachedTriggers {
            bytes,
            checked_at: version,
            triggers: triggers.clone(),
        },
    );
    Ok(triggers)
}

pub(crate) fn every_trigger(reader: &dyn KeyValueRead) -> SqlResult<Vec<TableTriggers>> {
    let prefix = triggers_prefix();
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
                .map_err(|_| crate::encoding::corrupt("trigger definitions"))
                .and_then(|table| decode_triggers(table, value));
            match decoded {
                Ok(triggers) => found.push(triggers),
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

fn save(writer: &mut dyn KeyValueWrite, triggers: &TableTriggers) -> SqlResult<()> {
    let key = trigger_key(&triggers.table);
    if triggers.triggers.is_empty() {
        writer.delete(&key)?;
    } else {
        writer.put(&key, &encode_triggers(triggers))?;
    }
    Ok(())
}

fn stored(reader: &dyn KeyValueRead, table: &str) -> SqlResult<TableTriggers> {
    Ok(match reader.get(&trigger_key(table))? {
        Some(bytes) => decode_triggers(table, &bytes)?,
        None => TableTriggers {
            table: table.to_owned(),
            triggers: Vec::new(),
        },
    })
}

pub(crate) fn remove_table_triggers(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&trigger_key(table))?;
    Ok(())
}

pub(crate) fn move_table_triggers(
    writer: &mut dyn KeyValueWrite,
    from: &str,
    to: &str,
) -> SqlResult<()> {
    let Some(bytes) = writer.get(&trigger_key(from))? else {
        return Ok(());
    };
    let mut triggers = decode_triggers(from, &bytes)?;
    writer.delete(&trigger_key(from))?;
    triggers.table = to.to_owned();
    save(writer, &triggers)
}

fn relation_columns(relation: &Relation) -> Vec<OutputColumn> {
    match relation {
        Relation::Table(table) => table_columns(table),
        Relation::View(view) => view_columns(view),
    }
}

pub(crate) fn table_columns(table: &TableDefinition) -> Vec<OutputColumn> {
    table
        .columns
        .iter()
        .map(|column| OutputColumn {
            name: column.name.clone(),
            sql_type: Some(column.sql_type),
        })
        .collect()
}

pub(crate) fn view_columns(view: &ViewDefinition) -> Vec<OutputColumn> {
    view.columns
        .iter()
        .map(|column| OutputColumn {
            name: column.name.clone(),
            sql_type: column.sql_type,
        })
        .collect()
}

struct Located {
    table: String,
    index: usize,
}

fn find_by_name(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &Name,
    schema: Option<&str>,
    table: Option<&Name>,
) -> SqlResult<Option<Located>> {
    let wanted = base_name(&name.text);
    if let Some(table) = table {
        let Some(found) = of_table(reader, tables, &table.text)? else {
            return Ok(None);
        };
        return Ok(found
            .triggers
            .iter()
            .position(|trigger| trigger.name.eq_ignore_ascii_case(wanted))
            .map(|index| Located {
                table: table.text.clone(),
                index,
            }));
    }
    let mut candidates = Vec::new();
    for listed in every_trigger(reader)? {
        if let Some(index) = listed
            .triggers
            .iter()
            .position(|trigger| trigger.name.eq_ignore_ascii_case(wanted))
        {
            candidates.push(Located {
                table: listed.table,
                index,
            });
        }
    }
    if let Some(schema) = schema
        && let Some(at) = candidates
            .iter()
            .position(|found| schema_of(&found.table).eq_ignore_ascii_case(schema))
    {
        return Ok(Some(candidates.swap_remove(at)));
    }
    Ok(candidates.into_iter().next())
}

fn schema_wide_names(dialect: Dialect) -> bool {
    dialect != Dialect::PostgreSql
}

fn already_named(
    reader: &dyn KeyValueRead,
    table: &str,
    name: &str,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    if !schema_wide_names(dialect) {
        let triggers = stored(reader, table)?;
        return Ok(triggers
            .triggers
            .iter()
            .any(|trigger| trigger.name.eq_ignore_ascii_case(name))
            .then(|| table.to_owned()));
    }
    let schema = schema_of(table);
    Ok(every_trigger(reader)?.into_iter().find_map(|listed| {
        (schema_of(&listed.table) == schema
            && listed
                .triggers
                .iter()
                .any(|trigger| trigger.name.eq_ignore_ascii_case(name)))
        .then_some(listed.table)
    }))
}

fn check_target(
    relation: &Relation,
    definition: &TriggerDefinition,
    table: &Name,
) -> SqlResult<()> {
    let is_view = matches!(relation, Relation::View(_));
    let instead = definition.timing == TriggerTiming::InsteadOf;
    match (is_view, instead, definition.dialect) {
        (true, false, Dialect::SqlServer) | (true, _, Dialect::MySql) => {
            Err(errors::not_a_base_table(&table.text, definition.dialect).at(table.position))
        }
        (true, false, dialect) => {
            Err(errors::view_needs_instead_of(&table.text, definition.timing, dialect)
                .at(table.position))
        }
        (false, true, Dialect::SqlServer) => Ok(()),
        (false, true, dialect) => {
            Err(errors::table_cannot_be_instead_of(&table.text, dialect).at(table.position))
        }
        _ => Ok(()),
    }
}

fn check_update_columns(
    relation: &Relation,
    definition: &TriggerDefinition,
    table: &Name,
) -> SqlResult<()> {
    if definition.dialect != Dialect::PostgreSql {
        return Ok(());
    }
    let columns = relation_columns(relation);
    for column in &definition.columns {
        if !columns.iter().any(|known| &known.name == column) {
            return Err(SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{column}\" of relation \"{}\" does not exist",
                    base_name(&table.text)
                ),
            ));
        }
    }
    Ok(())
}

fn placed(
    triggers: &mut Vec<TriggerDefinition>,
    definition: TriggerDefinition,
    placement: Option<&TriggerPlacement>,
) -> SqlResult<()> {
    let Some(placement) = placement else {
        triggers.push(definition);
        return Ok(());
    };
    let other = triggers.iter().position(|trigger| {
        trigger.name.eq_ignore_ascii_case(&placement.other.text)
            && trigger.timing == definition.timing
            && trigger.events == definition.events
    });
    let Some(other) = other else {
        return Err(errors::missing_referenced_trigger(&placement.other.text)
            .at(placement.other.position));
    };
    let at = if placement.follows { other + 1 } else { other };
    triggers.insert(at, definition);
    Ok(())
}

pub(crate) fn create_trigger(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateTrigger,
) -> SqlResult<QueryResult> {
    let definition = &create.definition;
    let dialect = definition.dialect;
    let relation = tables
        .relation(&*writer, &create.table.text)
        .map_err(|error| error.at_if_unplaced(create.table.position))?;
    let Some(relation) = relation else {
        return Err(errors::missing_table(&create.table, dialect));
    };
    check_target(&relation, definition, &create.table)?;
    check_update_columns(&relation, definition, &create.table)?;
    if let TriggerAction::Function { name, .. } = &definition.action {
        functions::check_trigger_function(&*writer, tables, name)
            .map_err(|error| error.at(create.name.position))?;
    }
    let mut result = QueryResult::command("CREATE TRIGGER");
    let mut triggers = stored(&*writer, &create.table.text)?;
    if let Some(owner) = already_named(&*writer, &create.table.text, &definition.name, dialect)? {
        if create.if_not_exists {
            if dialect == Dialect::MySql {
                result.warnings.push(errors::exists_note());
            }
            return Ok(result);
        }
        if !create.replace {
            return Err(errors::duplicate(&definition.name, &owner, dialect).at(create.name.position));
        }
        if owner != create.table.text {
            if dialect == Dialect::SqlServer {
                let mut other = stored(&*writer, &owner)?;
                other
                    .triggers
                    .retain(|trigger| !trigger.name.eq_ignore_ascii_case(&definition.name));
                save(writer, &other)?;
            } else {
                return Err(
                    errors::duplicate(&definition.name, &owner, dialect).at(create.name.position)
                );
            }
        }
    }
    let replaced = triggers
        .triggers
        .iter()
        .position(|trigger| trigger.name.eq_ignore_ascii_case(&definition.name));
    match replaced {
        Some(at) => triggers.triggers[at] = definition.clone(),
        None => {
            if triggers.triggers.len() >= MAX_TRIGGERS_PER_TABLE {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "table \"{}\" has {MAX_TRIGGERS_PER_TABLE} triggers, the most EggDB keeps on one table",
                        display(&create.table.text)
                    ),
                ));
            }
            placed(
                &mut triggers.triggers,
                definition.clone(),
                create.placement.as_ref(),
            )?;
        }
    }
    save(writer, &triggers)?;
    Ok(result)
}

pub(crate) fn drop_triggers(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    drop: &DropTrigger,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP TRIGGER");
    for target in &drop.names {
        let found = find_by_name(
            &*writer,
            tables,
            &target.name,
            target.schema.as_deref(),
            drop.table.as_ref(),
        )?;
        let Some(found) = found else {
            if drop.if_exists {
                match dialect {
                    Dialect::PostgreSql => {
                        let table = drop
                            .table
                            .as_ref()
                            .map_or_else(String::new, |table| display(&table.text));
                        result = result.with_notice(format!(
                            "NOTICE:  trigger \"{}\" for relation \"{table}\" does not exist, skipping",
                            target.name.text
                        ));
                    }
                    Dialect::MySql => result.warnings.push(errors::missing_note()),
                    _ => {}
                }
                continue;
            }
            return Err(errors::missing_trigger(&target.name, drop.table.as_ref(), dialect));
        };
        let mut triggers = stored(&*writer, &found.table)?;
        if found.index < triggers.triggers.len() {
            triggers.triggers.remove(found.index);
        }
        save(writer, &triggers)?;
    }
    Ok(result)
}

pub(crate) fn enable_triggers(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    enable: &EnableTrigger,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let tag = match dialect {
        Dialect::PostgreSql => "ALTER TABLE",
        _ if enable.enable => "ENABLE TRIGGER",
        _ => "DISABLE TRIGGER",
    };
    if tables.relation(&*writer, &enable.table.text)?.is_none() {
        return Err(errors::missing_table(&enable.table, dialect));
    }
    let mut triggers = stored(&*writer, &enable.table.text)?;
    match &enable.names {
        None => {
            for trigger in &mut triggers.triggers {
                trigger.enabled = enable.enable;
            }
        }
        Some(names) => {
            for name in names {
                let wanted = base_name(&name.text);
                let Some(trigger) = triggers
                    .triggers
                    .iter_mut()
                    .find(|trigger| trigger.name.eq_ignore_ascii_case(wanted))
                else {
                    return Err(errors::missing_trigger(name, Some(&enable.table), dialect));
                };
                trigger.enabled = enable.enable;
            }
        }
    }
    if !triggers.triggers.is_empty() {
        save(writer, &triggers)?;
    }
    Ok(QueryResult::command(tag))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatementTriggerCall {
    pub(crate) trigger: TriggerDefinition,
    pub(crate) table: String,
    pub(crate) columns: Arc<Vec<OutputColumn>>,
    pub(crate) event: TriggerEvent,
    pub(crate) updated: Vec<String>,
    pub(crate) old_rows: Vec<Vec<Value>>,
    pub(crate) new_rows: Vec<Vec<Value>>,
}

struct StatementFrame {
    settings: Arc<Settings>,
    pending: Vec<StatementTriggerCall>,
}

struct ActiveTrigger {
    table: String,
    name: String,
    dialect: Dialect,
}

#[derive(Default)]
struct State {
    frames: Vec<StatementFrame>,
    active: Vec<ActiveTrigger>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub(crate) struct StatementScope {
    depth: usize,
}

impl StatementScope {
    pub(crate) fn take_pending(&self) -> Vec<StatementTriggerCall> {
        STATE.with(|state| {
            state
                .try_borrow_mut()
                .ok()
                .and_then(|mut state| {
                    state
                        .frames
                        .get_mut(self.depth)
                        .map(|frame| std::mem::take(&mut frame.pending))
                })
                .unwrap_or_default()
        })
    }
}

impl Drop for StatementScope {
    fn drop(&mut self) {
        STATE.with(|state| {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.frames.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter_statement(settings: Arc<Settings>) -> StatementScope {
    let depth = STATE.with(|state| match state.try_borrow_mut() {
        Ok(mut state) => {
            let depth = state.frames.len();
            state.frames.push(StatementFrame {
                settings,
                pending: Vec::new(),
            });
            depth
        }
        Err(_) => usize::MAX,
    });
    StatementScope { depth }
}

pub(crate) fn enter_nested_statement() -> StatementScope {
    enter_statement(current_settings())
}

pub(crate) fn finish_nested_statement(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    scope: StatementScope,
) -> SqlResult<()> {
    let pending = scope.take_pending();
    drop(scope);
    let (calls, others): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .partition(|call| matches!(call.trigger.action, TriggerAction::Function { .. }));
    queue_statement_calls(others);
    for call in &calls {
        functions::run_statement_function(writer, tables, settings, call)?;
    }
    Ok(())
}

pub(crate) fn statement_triggers_pending() -> bool {
    STATE.with(|state| {
        state.try_borrow().is_ok_and(|state| {
            state
                .frames
                .last()
                .is_some_and(|frame| !frame.pending.is_empty())
        })
    })
}

fn current_settings() -> Arc<Settings> {
    STATE.with(|state| {
        state
            .try_borrow()
            .ok()
            .and_then(|state| state.frames.last().map(|frame| Arc::clone(&frame.settings)))
            .unwrap_or_default()
    })
}

fn queue_statement_calls(calls: Vec<StatementTriggerCall>) {
    if calls.is_empty() {
        return;
    }
    STATE.with(|state| {
        if let Ok(mut state) = state.try_borrow_mut()
            && let Some(frame) = state.frames.last_mut()
        {
            frame.pending.extend(calls);
        }
    });
}

pub(crate) struct ActiveScope {
    depth: usize,
}

impl Drop for ActiveScope {
    fn drop(&mut self) {
        STATE.with(|state| {
            if let Ok(mut state) = state.try_borrow_mut() {
                state.active.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter_active(table: &str, trigger: &TriggerDefinition) -> SqlResult<ActiveScope> {
    STATE.with(|state| {
        let mut state = state
            .try_borrow_mut()
            .map_err(|_| SqlError::new(SqlState::INTERNAL_ERROR, "trigger state in use"))?;
        let depth = state.active.len();
        if depth >= MAX_TRIGGER_DEPTH {
            return Err(errors::too_deep(trigger.dialect));
        }
        state.active.push(ActiveTrigger {
            table: table.to_owned(),
            name: trigger.name.clone(),
            dialect: trigger.dialect,
        });
        Ok(ActiveScope { depth })
    })
}

fn is_active(table: &str, name: &str) -> bool {
    STATE.with(|state| {
        state.try_borrow().is_ok_and(|state| {
            state
                .active
                .iter()
                .any(|active| active.table == table && active.name == name)
        })
    })
}

fn modified_by_invoking_mysql_trigger(table: &str) -> bool {
    STATE.with(|state| {
        state.try_borrow().is_ok_and(|state| {
            state
                .active
                .iter()
                .any(|active| active.dialect == Dialect::MySql && active.table == table)
        })
    })
}

pub(crate) fn trigger_depth() -> usize {
    STATE.with(|state| state.try_borrow().map_or(0, |state| state.active.len()))
}

fn recursion_allowed(dialect: Dialect) -> bool {
    dialect == Dialect::PostgreSql
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowOutcome {
    Write,
    Skip,
}

pub(crate) struct Firing {
    table: String,
    columns: Arc<Vec<OutputColumn>>,
    before: Vec<TriggerDefinition>,
    after: Vec<TriggerDefinition>,
    instead: Vec<TriggerDefinition>,
    calls: Vec<StatementTriggerCall>,
    replaces_writes: bool,
}

impl Firing {
    pub(crate) fn for_table(
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        table: &TableDefinition,
        event: TriggerEvent,
        updated: &[usize],
    ) -> SqlResult<Option<Self>> {
        let Some(listed) = of_table(&*writer, tables, &table.name)? else {
            return Ok(None);
        };
        let updated: Vec<&str> = updated
            .iter()
            .filter_map(|&index| table.columns.get(index).map(|column| column.name.as_str()))
            .collect();
        let firing = Self::build(&table.name, &listed, event, &updated, || {
            table_columns(table)
        })?;
        Self::fire_before_statement(writer, tables, firing)
    }

    pub(crate) fn for_view(
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        view: &ViewDefinition,
        event: TriggerEvent,
        updated: &[&str],
    ) -> SqlResult<Option<Self>> {
        let Some(listed) = of_table(&*writer, tables, &view.name)? else {
            return Ok(None);
        };
        let firing = Self::build(&view.name, &listed, event, updated, || view_columns(view))?;
        let firing = firing.filter(|firing| {
            !firing.instead.is_empty() || firing.replaces_writes
        });
        Self::fire_before_statement(writer, tables, firing)
    }

    fn fire_before_statement(
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        firing: Option<Self>,
    ) -> SqlResult<Option<Self>> {
        let Some(mut firing) = firing else {
            return Ok(None);
        };
        let (before, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut firing.calls)
            .into_iter()
            .partition(|call| {
                call.trigger.timing == TriggerTiming::Before
                    && matches!(call.trigger.action, TriggerAction::Function { .. })
            });
        firing.calls = rest;
        if !before.is_empty() {
            let settings = current_settings();
            for call in &before {
                functions::run_statement_function(writer, tables, &settings, call)?;
            }
        }
        Ok(Some(firing))
    }

    fn build(
        table: &str,
        listed: &TableTriggers,
        event: TriggerEvent,
        updated: &[&str],
        columns: impl FnOnce() -> Vec<OutputColumn>,
    ) -> SqlResult<Option<Self>> {
        let firing: Vec<&TriggerDefinition> = listed
            .triggers
            .iter()
            .filter(|trigger| trigger.fires_on(event, updated))
            .filter(|trigger| {
                recursion_allowed(trigger.dialect) || !is_active(table, &trigger.name)
            })
            .collect();
        if firing.is_empty() {
            return Ok(None);
        }
        if modified_by_invoking_mysql_trigger(table) {
            return Err(errors::table_used_by_invoking_statement(table));
        }
        let mut ordered = firing;
        if ordered
            .iter()
            .all(|trigger| trigger.dialect == Dialect::PostgreSql)
        {
            ordered.sort_by(|left, right| left.name.cmp(&right.name));
        }
        let columns = Arc::new(columns());
        let mut built = Self {
            table: table.to_owned(),
            columns: Arc::clone(&columns),
            before: Vec::new(),
            after: Vec::new(),
            instead: Vec::new(),
            calls: Vec::new(),
            replaces_writes: false,
        };
        for trigger in ordered {
            match (trigger.level, trigger.timing) {
                (TriggerLevel::Row, TriggerTiming::Before) => built.before.push(trigger.clone()),
                (TriggerLevel::Row, TriggerTiming::After) => built.after.push(trigger.clone()),
                (TriggerLevel::Row, TriggerTiming::InsteadOf) => {
                    built.instead.push(trigger.clone());
                }
                (TriggerLevel::Statement, timing) => {
                    built.replaces_writes |= timing == TriggerTiming::InsteadOf;
                    built.calls.push(StatementTriggerCall {
                        trigger: trigger.clone(),
                        table: table.to_owned(),
                        columns: Arc::clone(&columns),
                        event,
                        updated: match event {
                            TriggerEvent::Insert => {
                                columns.iter().map(|column| column.name.clone()).collect()
                            }
                            _ => updated.iter().map(|name| (*name).to_owned()).collect(),
                        },
                        old_rows: Vec::new(),
                        new_rows: Vec::new(),
                    });
                }
            }
        }
        Ok(Some(built))
    }

    pub(crate) fn has_before(&self) -> bool {
        !self.before.is_empty() || self.replaces_writes
    }

    fn record(&mut self, timing: TriggerTiming, old: Option<&[Value]>, new: Option<&[Value]>) {
        for call in &mut self.calls {
            let records = match timing {
                TriggerTiming::InsteadOf => call.trigger.timing == TriggerTiming::InsteadOf,
                _ => call.trigger.timing == timing,
            };
            if !records {
                continue;
            }
            if let Some(old) = old {
                call.old_rows.push(old.to_vec());
            }
            if let Some(new) = new {
                call.new_rows.push(new.to_vec());
            }
        }
    }

    pub(crate) fn before_row(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        old: Option<&[Value]>,
        mut new: Option<&mut Vec<Value>>,
    ) -> SqlResult<RowOutcome> {
        for trigger in &self.before {
            let end = body::run_row_trigger(
                writer,
                tables,
                &self.table,
                trigger,
                &self.columns,
                old,
                new.as_deref_mut(),
            )?;
            if end == body::BodyEnd::Ignored {
                return Ok(RowOutcome::Skip);
            }
        }
        if self.replaces_writes {
            let new = new.as_deref().map(Vec::as_slice);
            self.record(TriggerTiming::InsteadOf, old, new);
            return Ok(RowOutcome::Skip);
        }
        Ok(RowOutcome::Write)
    }

    pub(crate) fn instead_row(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> SqlResult<()> {
        self.record(TriggerTiming::InsteadOf, old, new);
        let mut scratch = new.map(<[Value]>::to_vec);
        for trigger in &self.instead {
            let end = body::run_row_trigger(
                writer,
                tables,
                &self.table,
                trigger,
                &self.columns,
                old,
                scratch.as_mut(),
            )?;
            if end == body::BodyEnd::Ignored {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn after_row(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        tables: &mut TableCache,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> SqlResult<()> {
        self.record(TriggerTiming::After, old, new);
        let mut scratch = new.map(<[Value]>::to_vec);
        for trigger in &self.after {
            let end = body::run_row_trigger(
                writer,
                tables,
                &self.table,
                trigger,
                &self.columns,
                old,
                scratch.as_mut(),
            )?;
            if end == body::BodyEnd::Ignored {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn runs_row_triggers(&self) -> bool {
        !self.before.is_empty() || !self.after.is_empty() || !self.instead.is_empty()
    }

    pub(crate) fn finish(self) {
        queue_statement_calls(self.calls);
    }
}

pub(crate) fn is_ignored_row(error: &SqlError) -> bool {
    errors::is_ignored_row(error)
}

pub(crate) fn raise(
    evaluator: &crate::eval::Evaluator<'_>,
    arguments: &[crate::ast::Expr],
) -> SqlResult<Value> {
    if trigger_depth() == 0 {
        return Err(errors::raise_outside_trigger());
    }
    let kind = match arguments.first().map(|argument| &argument.kind) {
        Some(crate::ast::ExprKind::Literal(crate::ast::Literal::String(kind))) => kind.as_str(),
        _ => return Err(errors::raise_outside_trigger()),
    };
    if kind == "ignore" {
        return Err(errors::ignored_row());
    }
    let message = match arguments.get(1) {
        Some(argument) => evaluator
            .eval(argument, None)?
            .to_text()
            .unwrap_or_default(),
        None => String::new(),
    };
    Err(errors::raised(&message))
}
