use crate::ast::{MaterializedViewCommand, Name, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::plpgsql::{self, Call};
use crate::settings::Settings;
use crate::store::KeyValueWrite;
use crate::types::{SqlType, Value};
use std::cell::{Cell, RefCell};

pub(crate) const EVENTS: [&str; 4] = [
    DDL_COMMAND_START,
    DDL_COMMAND_END,
    SQL_DROP,
    TABLE_REWRITE,
];
pub(crate) const DDL_COMMAND_START: &str = "ddl_command_start";
pub(crate) const DDL_COMMAND_END: &str = "ddl_command_end";
pub(crate) const SQL_DROP: &str = "sql_drop";
pub(crate) const TABLE_REWRITE: &str = "table_rewrite";

pub(crate) const FIRED_TAGS: [&str; 34] = [
    "ALTER DOMAIN",
    "ALTER FUNCTION",
    "ALTER INDEX",
    "ALTER MATERIALIZED VIEW",
    "ALTER PROCEDURE",
    "ALTER SCHEMA",
    "ALTER SEQUENCE",
    "ALTER TABLE",
    "ALTER TRIGGER",
    "ALTER TYPE",
    "ALTER VIEW",
    "COMMENT",
    "CREATE DOMAIN",
    "CREATE FUNCTION",
    "CREATE INDEX",
    "CREATE MATERIALIZED VIEW",
    "CREATE POLICY",
    "CREATE PROCEDURE",
    "CREATE SCHEMA",
    "CREATE SEQUENCE",
    "CREATE TABLE",
    "CREATE TABLE AS",
    "CREATE TRIGGER",
    "CREATE TYPE",
    "CREATE VIEW",
    "DROP DOMAIN",
    "DROP FUNCTION",
    "DROP INDEX",
    "DROP MATERIALIZED VIEW",
    "DROP POLICY",
    "DROP PROCEDURE",
    "DROP SCHEMA",
    "DROP SEQUENCE",
    "DROP TABLE",
];

const MORE_FIRED_TAGS: [&str; 7] = [
    "DROP TRIGGER",
    "DROP TYPE",
    "DROP VIEW",
    "GRANT",
    "REVOKE",
    "REFRESH MATERIALIZED VIEW",
    "SELECT INTO",
];

const NEVER_FIRED_TAGS: [&str; 10] = [
    "CREATE DATABASE",
    "DROP DATABASE",
    "ALTER DATABASE",
    "CREATE ROLE",
    "DROP ROLE",
    "ALTER ROLE",
    "CREATE TABLESPACE",
    "CREATE EVENT TRIGGER",
    "ALTER EVENT TRIGGER",
    "DROP EVENT TRIGGER",
];

pub(crate) fn is_known_event(event: &str) -> bool {
    EVENTS.contains(&event)
}

pub(crate) fn filter_tag_error(tag: &str) -> Option<SqlError> {
    if FIRED_TAGS.contains(&tag) || MORE_FIRED_TAGS.contains(&tag) {
        return None;
    }
    if NEVER_FIRED_TAGS.contains(&tag) {
        return Some(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("event triggers are not supported for {tag}"),
        ));
    }
    Some(SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("filter value \"{tag}\" not recognized for filter variable \"tag\""),
    ))
}

/// The command tag PostgreSQL gives a DDL statement that fires event triggers, or `None` for
/// a statement that fires none.
pub(crate) fn ddl_tag(statement: &Statement) -> Option<&'static str> {
    use crate::routines::RoutineKind;
    Some(match statement {
        Statement::CreateTable(_) | Statement::CreateTableLike(_) => "CREATE TABLE",
        Statement::CreateTableAs(_) => "CREATE TABLE AS",
        Statement::DropTable { .. } => "DROP TABLE",
        Statement::AlterTable(_) => "ALTER TABLE",
        Statement::CreateIndex(_) => "CREATE INDEX",
        Statement::DropIndex(_) => "DROP INDEX",
        Statement::CreateSequence(_) => "CREATE SEQUENCE",
        Statement::AlterSequence(_) => "ALTER SEQUENCE",
        Statement::DropSequence { .. } => "DROP SEQUENCE",
        Statement::CreateSchema { .. } => "CREATE SCHEMA",
        Statement::DropSchema { .. } => "DROP SCHEMA",
        Statement::RenameSchema { .. } => "ALTER SCHEMA",
        Statement::CreateView(create) if !create.temporary => "CREATE VIEW",
        Statement::DropView { .. } => "DROP VIEW",
        Statement::AlterView(_) => "ALTER VIEW",
        Statement::Comment(_) => "COMMENT",
        Statement::CreateType(_) => "CREATE TYPE",
        Statement::CreateDomain(_) => "CREATE DOMAIN",
        Statement::DropType { domain: true, .. } => "DROP DOMAIN",
        Statement::DropType { .. } => "DROP TYPE",
        Statement::AlterType(_) => "ALTER TYPE",
        Statement::AlterDomain(_) => "ALTER DOMAIN",
        Statement::CreateTrigger(_) => "CREATE TRIGGER",
        Statement::DropTrigger(_) => "DROP TRIGGER",
        Statement::AlterTrigger(_) | Statement::EnableTrigger(_) => "ALTER TRIGGER",
        Statement::CreateRoutine(create) => match create.definition.kind {
            RoutineKind::Function => "CREATE FUNCTION",
            RoutineKind::Procedure => "CREATE PROCEDURE",
        },
        Statement::DropRoutine(drop) => match drop.kind {
            Some(RoutineKind::Procedure) => "DROP PROCEDURE",
            _ => "DROP FUNCTION",
        },
        Statement::AlterRoutine(_) => "ALTER FUNCTION",
        Statement::MaterializedView(MaterializedViewCommand::Create { .. }) => {
            "CREATE MATERIALIZED VIEW"
        }
        Statement::MaterializedView(MaterializedViewCommand::Refresh { .. }) => {
            "REFRESH MATERIALIZED VIEW"
        }
        Statement::MaterializedView(MaterializedViewCommand::Drop { .. }) => {
            "DROP MATERIALIZED VIEW"
        }
        Statement::RowSecurity(crate::row_security::RowSecurityStatement::CreatePolicy {
            ..
        }) => "CREATE POLICY",
        Statement::RowSecurity(crate::row_security::RowSecurityStatement::DropPolicy { .. }) => {
            "DROP POLICY"
        }
        Statement::RowSecurity(_) => "ALTER TABLE",
        Statement::Role(crate::roles::RoleStatement::Privileges { granting, .. }) => {
            if *granting {
                "GRANT"
            } else {
                "REVOKE"
            }
        }
        _ => return None,
    })
}

/// One object a statement drops, as `pg_event_trigger_dropped_objects()` lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DroppedObject {
    pub(crate) object_type: &'static str,
    pub(crate) schema: Option<String>,
    pub(crate) name: String,
}

impl DroppedObject {
    fn of(object_type: &'static str, name: &Name) -> Self {
        let schema = crate::schemas::schema_of(&name.text);
        Self {
            object_type,
            schema: Some(if schema.is_empty() {
                crate::schemas::DEFAULT_SCHEMA.to_owned()
            } else {
                schema.to_owned()
            }),
            name: crate::schemas::base_name(&name.text).to_owned(),
        }
    }

    fn identity(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{schema}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

pub(crate) fn dropped_objects(statement: &Statement) -> Vec<DroppedObject> {
    let each = |object_type: &'static str, names: &[Name]| {
        names
            .iter()
            .map(|name| DroppedObject::of(object_type, name))
            .collect::<Vec<_>>()
    };
    match statement {
        Statement::DropTable { names, .. } => each("table", names),
        Statement::DropIndex(drop) => each("index", &drop.names),
        Statement::DropSequence { names, .. } => each("sequence", names),
        Statement::DropView { names, .. } => each("view", names),
        Statement::DropType {
            names,
            domain: true,
            ..
        } => each("domain", names),
        Statement::DropType { names, .. } => each("type", names),
        Statement::MaterializedView(MaterializedViewCommand::Drop { names, .. }) => {
            each("materialized view", names)
        }
        Statement::DropSchema { names, .. } => names
            .iter()
            .map(|name| DroppedObject {
                object_type: "schema",
                schema: None,
                name: name.text.clone(),
            })
            .collect(),
        Statement::DropRoutine(drop) => drop
            .targets
            .iter()
            .map(|target| {
                DroppedObject::of(
                    match drop.kind {
                        Some(crate::routines::RoutineKind::Procedure) => "procedure",
                        _ => "function",
                    },
                    &target.name,
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// One stored event trigger.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventTrigger {
    pub(crate) name: String,
    pub(crate) event: String,
    pub(crate) tags: Vec<String>,
    pub(crate) function: String,
    pub(crate) enabled: char,
}

impl EventTrigger {
    pub(crate) fn fires_on(&self, event: &str, tag: &str) -> bool {
        self.event == event
            && matches!(self.enabled, 'O' | 'A')
            && (self.tags.is_empty() || self.tags.iter().any(|wanted| wanted == tag))
    }
}

#[derive(Default)]
struct Firing {
    event: String,
    tag: String,
    object_type: &'static str,
    object_identity: String,
    schema: Option<String>,
    dropped: Vec<DroppedObject>,
}

thread_local! {
    static FIRING: RefCell<Option<Firing>> = const { RefCell::new(None) };
    static STATEMENT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

pub(crate) struct StatementDepth;

impl Drop for StatementDepth {
    fn drop(&mut self) {
        STATEMENT_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Counts statements run inside other statements on this thread; only the outermost one fires
/// event triggers, so statements EggDB runs for itself never do.
pub(crate) fn enter_statement() -> (StatementDepth, bool) {
    let outermost = STATEMENT_DEPTH.with(|depth| {
        let entered = depth.get() + 1;
        depth.set(entered);
        entered == 1
    });
    (StatementDepth, outermost && !firing())
}

pub(crate) fn firing() -> bool {
    FIRING.with(|firing| firing.borrow().is_some())
}

pub(crate) fn current_event() -> Option<(String, String)> {
    FIRING.with(|firing| {
        firing
            .borrow()
            .as_ref()
            .map(|firing| (firing.event.clone(), firing.tag.clone()))
    })
}

struct FiringGuard;

impl Drop for FiringGuard {
    fn drop(&mut self) {
        FIRING.with(|firing| *firing.borrow_mut() = None);
    }
}

/// What the statement that fires the triggers did, for `pg_event_trigger_ddl_commands()` and
/// `pg_event_trigger_dropped_objects()`.
pub(crate) struct FiredStatement<'s> {
    pub(crate) tag: &'static str,
    pub(crate) object: Option<(&'static str, String, Option<String>)>,
    pub(crate) dropped: &'s [DroppedObject],
}

pub(crate) fn object_of(statement: &Statement) -> Option<(&'static str, String, Option<String>)> {
    let named = |object_type: &'static str, name: &Name| {
        let schema = crate::schemas::schema_of(&name.text);
        let schema = if schema.is_empty() {
            crate::schemas::DEFAULT_SCHEMA.to_owned()
        } else {
            schema.to_owned()
        };
        let base = crate::schemas::base_name(&name.text);
        (object_type, format!("{schema}.{base}"), Some(schema))
    };
    Some(match statement {
        Statement::CreateTable(create) => named("table", &create.name),
        Statement::CreateTableAs(create) => named("table", &create.name),
        Statement::AlterTable(alter) => named("table", &alter.name),
        Statement::CreateIndex(create) => match &create.index.name {
            Some(name) => named("index", name),
            None => named("table", &create.table),
        },
        Statement::CreateSequence(create) => named("sequence", &create.name),
        Statement::AlterSequence(alter) => named("sequence", &alter.name),
        Statement::CreateView(create) => named("view", &create.name),
        Statement::AlterView(alter) => named("view", &alter.name),
        Statement::CreateType(create) => named("type", &create.name),
        Statement::CreateDomain(create) => named("domain", &create.name),
        Statement::CreateTrigger(create) => named("trigger", &create.name),
        Statement::CreateRoutine(create) => named(
            match create.definition.kind {
                crate::routines::RoutineKind::Function => "function",
                crate::routines::RoutineKind::Procedure => "procedure",
            },
            &create.name,
        ),
        Statement::MaterializedView(MaterializedViewCommand::Create { name, .. }) => {
            named("materialized view", name)
        }
        Statement::CreateSchema { name, .. } => ("schema", name.text.clone(), None),
        Statement::RenameSchema { to, .. } => ("schema", to.text.clone(), None),
        _ => return None,
    })
}

/// Runs every enabled event trigger on `event` whose tag filter matches, in name order, as
/// PostgreSQL does; the first failure fails the statement.
pub(crate) fn fire(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    triggers: &[EventTrigger],
    event: &str,
    fired: &FiredStatement<'_>,
) -> SqlResult<()> {
    let mut chosen: Vec<&EventTrigger> = triggers
        .iter()
        .filter(|trigger| trigger.fires_on(event, fired.tag))
        .collect();
    chosen.sort_by(|left, right| left.name.cmp(&right.name));
    for trigger in chosen {
        let Some(definition) =
            crate::routines::resolve_event_trigger_function(&*writer, tables, &trigger.function)?
        else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {}() does not exist", trigger.function),
            ));
        };
        let (object_type, object_identity, schema) = fired
            .object
            .clone()
            .unwrap_or(("", String::new(), None));
        FIRING.with(|firing| {
            *firing.borrow_mut() = Some(Firing {
                event: event.to_owned(),
                tag: fired.tag.to_owned(),
                object_type,
                object_identity,
                schema,
                dropped: fired.dropped.to_vec(),
            });
        });
        let _guard = FiringGuard;
        plpgsql::invoke(
            writer,
            tables,
            settings,
            Call {
                definition: &definition,
                arguments: Vec::new(),
                trigger: None,
            },
        )?;
    }
    Ok(())
}

pub(crate) const DDL_COMMANDS: &str = "pg_event_trigger_ddl_commands";
pub(crate) const DROPPED_OBJECTS: &str = "pg_event_trigger_dropped_objects";

pub(crate) const DDL_COMMANDS_COLUMNS: [(&str, SqlType); 9] = [
    ("classid", SqlType::Catalog(crate::pg_catalog::CatalogType::Oid)),
    ("objid", SqlType::Catalog(crate::pg_catalog::CatalogType::Oid)),
    ("objsubid", SqlType::Integer),
    ("command_tag", SqlType::Text),
    ("object_type", SqlType::Text),
    ("schema_name", SqlType::Text),
    ("object_identity", SqlType::Text),
    ("in_extension", SqlType::Boolean),
    ("command", SqlType::Catalog(crate::pg_catalog::CatalogType::Oid)),
];

pub(crate) const DROPPED_OBJECTS_COLUMNS: [(&str, SqlType); 12] = [
    ("classid", SqlType::Catalog(crate::pg_catalog::CatalogType::Oid)),
    ("objid", SqlType::Catalog(crate::pg_catalog::CatalogType::Oid)),
    ("objsubid", SqlType::Integer),
    ("original", SqlType::Boolean),
    ("normal", SqlType::Boolean),
    ("is_temporary", SqlType::Boolean),
    ("object_type", SqlType::Text),
    ("schema_name", SqlType::Text),
    ("object_name", SqlType::Text),
    ("object_identity", SqlType::Text),
    ("address_names", SqlType::Array(crate::arrays::ArrayElement::Text)),
    ("address_args", SqlType::Array(crate::arrays::ArrayElement::Text)),
];

fn outside_event(function: &str, event: &str) -> SqlError {
    SqlError::new(
        SqlState::EVENT_TRIGGER_PROTOCOL_VIOLATED,
        format!("{function} can only be called in a {event} event trigger function"),
    )
}

pub(crate) fn record_type(name: &str, types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    if types.is_empty() {
        Ok(SqlType::Defined(crate::types::DefinedType::RECORD))
    } else {
        Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name}() takes no arguments"),
        ))
    }
}

fn texts(items: Vec<String>) -> Value {
    Value::Array(Box::new(crate::arrays::ArrayValue::new(
        crate::arrays::ArrayElement::Text,
        items.into_iter().map(Value::Text).collect(),
    )))
}

pub(crate) fn ddl_command_records(
    _: &[Value],
    sink: &mut crate::json_functions::RecordSink<'_>,
) -> SqlResult<()> {
    let row = FIRING.with(|firing| {
        let firing = firing.borrow();
        match firing.as_ref() {
            Some(firing) if firing.event == DDL_COMMAND_END => Ok(Some(vec![
                Value::Oid(0),
                Value::Oid(0),
                Value::Integer(0),
                Value::Text(firing.tag.clone()),
                Value::Text(firing.object_type.to_owned()),
                firing.schema.clone().map_or(Value::Null, Value::Text),
                Value::Text(firing.object_identity.clone()),
                Value::Boolean(false),
                Value::Null,
            ])),
            _ => Err(outside_event(DDL_COMMANDS, DDL_COMMAND_END)),
        }
    })?;
    match row {
        Some(row) if !row[4].to_text().unwrap_or_default().is_empty() => sink(row),
        _ => Ok(()),
    }
}

pub(crate) fn dropped_object_records(
    _: &[Value],
    sink: &mut crate::json_functions::RecordSink<'_>,
) -> SqlResult<()> {
    let dropped = FIRING.with(|firing| {
        let firing = firing.borrow();
        match firing.as_ref() {
            Some(firing) if firing.event == SQL_DROP => Ok(firing.dropped.clone()),
            _ => Err(outside_event(DROPPED_OBJECTS, SQL_DROP)),
        }
    })?;
    for object in dropped {
        let mut names = Vec::new();
        if let Some(schema) = &object.schema {
            names.push(schema.clone());
        }
        names.push(object.name.clone());
        sink(vec![
            Value::Oid(0),
            Value::Oid(0),
            Value::Integer(0),
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Text(object.object_type.to_owned()),
            object.schema.clone().map_or(Value::Null, Value::Text),
            Value::Text(object.name.clone()),
            Value::Text(object.identity()),
            texts(names),
            texts(Vec::new()),
        ])?;
    }
    Ok(())
}

pub(crate) fn record_rows(
    name: &str,
    values: &[Value],
    sink: &mut (dyn FnMut(Value) -> SqlResult<()> + '_),
) -> SqlResult<()> {
    let columns: Vec<String> = if name == DDL_COMMANDS {
        DDL_COMMANDS_COLUMNS
            .iter()
            .map(|(column, _)| (*column).to_owned())
            .collect()
    } else {
        DROPPED_OBJECTS_COLUMNS
            .iter()
            .map(|(column, _)| (*column).to_owned())
            .collect()
    };
    let mut each = |fields: Vec<Value>| {
        sink(crate::records::record_value(
            crate::records::RECORD_OID,
            fields,
            columns.clone(),
        ))
    };
    if name == DDL_COMMANDS {
        ddl_command_records(values, &mut each)
    } else {
        dropped_object_records(values, &mut each)
    }
}

#[cfg(test)]
#[path = "event_triggers_tests.rs"]
mod tests;
