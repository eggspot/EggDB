use super::mysql_events::quoted;
use super::{CopyIo, QueryResult, Session};
use crate::ast::{EventTriggerChange, EventTriggerCommand, Name, Statement};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::event_triggers::{self, EventTrigger, FiredStatement};
use crate::types::Value;
use std::sync::Arc;

pub(crate) const EVENT_TRIGGERS_TABLE: &str = "eggdb_internal.pg_event_triggers";
const CREATE_EVENT_TRIGGERS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
    CREATE TABLE IF NOT EXISTS eggdb_internal.pg_event_triggers (\
    name text PRIMARY KEY, event text NOT NULL, tags text NOT NULL, function text NOT NULL, \
    enabled text NOT NULL, owner text NOT NULL)";
const EVENT_TRIGGER_COLUMNS: &str = "name, event, tags, function, enabled, owner";

fn text_at(row: &[Value], at: usize) -> Option<String> {
    row.get(at).and_then(Value::character_text).map(str::to_owned)
}

fn trigger_of_row(row: &[Value]) -> Option<EventTrigger> {
    let tags = text_at(row, 2)?;
    Some(EventTrigger {
        name: text_at(row, 0)?,
        event: text_at(row, 1)?,
        tags: tags
            .split(',')
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .collect(),
        function: text_at(row, 3)?,
        enabled: text_at(row, 4)?.chars().next().unwrap_or('O'),
    })
}

fn missing(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("event trigger \"{name}\" does not exist"),
    )
}

impl Session<'_> {
    fn event_trigger_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn event_triggers_stored(&mut self) -> SqlResult<bool> {
        self.read(|reader, _| {
            Ok(reader
                .get(&crate::encoding::table_key(EVENT_TRIGGERS_TABLE))?
                .is_some())
        })
    }

    pub(super) fn stored_event_triggers(&mut self) -> SqlResult<Vec<EventTrigger>> {
        if !self.event_triggers_stored()? {
            return Ok(Vec::new());
        }
        let results = self.event_trigger_sql(&format!(
            "SELECT {EVENT_TRIGGER_COLUMNS} FROM {EVENT_TRIGGERS_TABLE} ORDER BY name"
        ))?;
        Ok(results
            .into_iter()
            .flat_map(|result| result.rows)
            .filter_map(|row| trigger_of_row(&row))
            .collect())
    }

    fn require_event_trigger_owner(&self, what: &str, name: &Name) -> SqlResult<()> {
        if self.settings.checked_role.is_some() {
            return Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to {what} event trigger \"{}\"", name.text),
            )
            .with_hint(format!("Must be superuser to {what} an event trigger.")));
        }
        Ok(())
    }

    pub(super) fn event_trigger(&mut self, command: &EventTriggerCommand) -> SqlResult<QueryResult> {
        match command {
            EventTriggerCommand::Create {
                name,
                event,
                tags,
                function,
            } => {
                self.require_event_trigger_owner("create", name)?;
                if !event_triggers::is_known_event(&event.text) {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized event name \"{}\"", event.text),
                    )
                    .at(event.position));
                }
                if event.text == event_triggers::LOGIN && !tags.is_empty() {
                    return Err(SqlError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "tag filtering is not supported for login event triggers",
                    ));
                }
                if event.text != event_triggers::TABLE_REWRITE
                    && let Some(refused) =
                        tags.iter().find_map(|tag| event_triggers::filter_tag_error(tag))
                {
                    return Err(refused);
                }
                let found = self.read(|reader, tables| {
                    crate::routines::resolve_event_trigger_function(reader, tables, function)
                })?;
                if found.is_none() {
                    let any = self.read(|reader, tables| {
                        crate::routines::resolve(reader, tables, function)
                    })?;
                    return Err(match any {
                        Some(_) => SqlError::new(
                            SqlState::INVALID_OBJECT_DEFINITION,
                            format!("function {function} must return type event_trigger"),
                        ),
                        None => SqlError::new(
                            SqlState::UNDEFINED_FUNCTION,
                            format!("function {function}() does not exist"),
                        ),
                    });
                }
                if self
                    .stored_event_triggers()?
                    .iter()
                    .any(|stored| stored.name == name.text)
                {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("event trigger \"{}\" already exists", name.text),
                    )
                    .at(name.position));
                }
                let owner = self
                    .settings
                    .get("session_authorization")
                    .unwrap_or_default();
                self.event_trigger_sql(CREATE_EVENT_TRIGGERS_TABLE)?;
                self.event_trigger_sql(&format!(
                    "INSERT INTO {EVENT_TRIGGERS_TABLE} ({EVENT_TRIGGER_COLUMNS}) \
                     VALUES ({}, {}, {}, {}, 'O', {})",
                    quoted(&name.text),
                    quoted(&event.text),
                    quoted(&tags.join(",")),
                    quoted(function),
                    quoted(&owner),
                ))?;
                Ok(QueryResult::command("CREATE EVENT TRIGGER"))
            }
            EventTriggerCommand::Drop { name, if_exists } => {
                self.require_event_trigger_owner("drop", name)?;
                let exists = self
                    .stored_event_triggers()?
                    .iter()
                    .any(|stored| stored.name == name.text);
                if !exists {
                    if *if_exists {
                        return Ok(QueryResult::command("DROP EVENT TRIGGER").with_notice(
                            format!("event trigger \"{}\" does not exist, skipping", name.text),
                        ));
                    }
                    return Err(missing(&name.text).at(name.position));
                }
                self.event_trigger_sql(&format!(
                    "DELETE FROM {EVENT_TRIGGERS_TABLE} WHERE name = {}",
                    quoted(&name.text)
                ))?;
                Ok(QueryResult::command("DROP EVENT TRIGGER"))
            }
            EventTriggerCommand::Alter { name, change } => {
                self.require_event_trigger_owner("alter", name)?;
                let stored = self.stored_event_triggers()?;
                if !stored.iter().any(|found| found.name == name.text) {
                    return Err(missing(&name.text).at(name.position));
                }
                let assignment = match change {
                    EventTriggerChange::Enable(mode) => format!("enabled = '{mode}'"),
                    EventTriggerChange::Disable => "enabled = 'D'".to_owned(),
                    EventTriggerChange::Rename(to) => {
                        if stored.iter().any(|found| found.name == to.text) {
                            return Err(SqlError::new(
                                SqlState::DUPLICATE_OBJECT,
                                format!("event trigger \"{}\" already exists", to.text),
                            )
                            .at(to.position));
                        }
                        format!("name = {}", quoted(&to.text))
                    }
                    EventTriggerChange::Owner(owner) => {
                        format!("owner = {}", quoted(&owner.text))
                    }
                };
                self.event_trigger_sql(&format!(
                    "UPDATE {EVENT_TRIGGERS_TABLE} SET {assignment} WHERE name = {}",
                    quoted(&name.text)
                ))?;
                Ok(QueryResult::command("ALTER EVENT TRIGGER"))
            }
        }
    }

    fn fire_event_triggers(
        &mut self,
        triggers: &[EventTrigger],
        event: &str,
        fired: &FiredStatement<'_>,
    ) -> SqlResult<()> {
        if !triggers
            .iter()
            .any(|trigger| trigger.fires_on(event, fired.tag))
        {
            return Ok(());
        }
        let settings = Arc::clone(&self.settings);
        self.write("SELECT", |writer, tables| {
            event_triggers::fire(writer, tables, &settings, triggers, event, fired)?;
            Ok(QueryResult::command("SELECT"))
        })
        .map(drop)
    }

    /// Fires PostgreSQL's `login` event triggers once a connection has authenticated, as
    /// PostgreSQL 17 does: in name order, in one transaction. An error from one of them fails
    /// the login; a listener reports it and closes the connection. An Oracle-dialect session
    /// fires its `AFTER LOGON` triggers instead, and a SQL Server-dialect one its logon
    /// triggers.
    pub fn fire_login_event_triggers(&mut self) -> SqlResult<()> {
        if self.dialect == Dialect::Oracle {
            return self.fire_logon_triggers();
        }
        if self.dialect == Dialect::SqlServer {
            return self.fire_sqlserver_logon_triggers();
        }
        if self.dialect != Dialect::PostgreSql {
            return Ok(());
        }
        let checked_role = Arc::make_mut(&mut self.settings).checked_role.take();
        let stored = self.stored_event_triggers();
        Arc::make_mut(&mut self.settings).checked_role = checked_role;
        let triggers: Vec<EventTrigger> = stored?
            .into_iter()
            .filter(|trigger| trigger.fires_on(event_triggers::LOGIN, event_triggers::LOGIN))
            .collect();
        if triggers.is_empty() {
            return Ok(());
        }
        let fired = FiredStatement {
            tag: event_triggers::LOGIN,
            object: None,
            dropped: &[],
            rewrite: None,
        };
        self.atomically(|session| session.fire_event_triggers(&triggers, event_triggers::LOGIN, &fired))
    }

    /// Runs a statement, firing PostgreSQL's event triggers around it when it is a DDL command
    /// and any are enabled: `ddl_command_start` before it, then `sql_drop` and
    /// `ddl_command_end` after it, all in one transaction so a failing trigger undoes the
    /// command.
    pub(super) fn run_with_event_triggers(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        let (_depth, outermost) = event_triggers::enter_statement();
        if outermost
            && self.dialect == Dialect::SqlServer
            && let Some(tag) = event_triggers::ddl_tag(statement)
        {
            return self.run_with_ddl_triggers(statement, io, tag);
        }
        if outermost && self.dialect == Dialect::Oracle && self.system_triggers_stored() {
            return self.run_with_system_triggers(statement, io);
        }
        let tag = if outermost && self.dialect == Dialect::PostgreSql {
            event_triggers::ddl_tag(statement)
        } else {
            None
        };
        let Some(tag) = tag else {
            return self.run_statement(statement, io);
        };
        let triggers: Vec<EventTrigger> = self
            .stored_event_triggers()?
            .into_iter()
            .filter(|trigger| matches!(trigger.enabled, 'O' | 'A'))
            .collect();
        if triggers.is_empty() {
            return self.run_statement(statement, io);
        }
        let dropped = event_triggers::dropped_objects(statement);
        let object = event_triggers::object_of(statement);
        let rewritten = event_triggers::rewritten_table(statement).filter(|_| {
            triggers
                .iter()
                .any(|trigger| trigger.fires_on(event_triggers::TABLE_REWRITE, tag))
        });
        let fired = FiredStatement {
            tag,
            object,
            dropped: &dropped,
            rewrite: None,
        };
        let mut outcome = None;
        self.atomically(|session| {
            session.fire_event_triggers(&triggers, event_triggers::DDL_COMMAND_START, &fired)?;
            if let Some((table, reason)) = &rewritten {
                let oid = session.read(|reader, _| {
                    let snapshot = crate::pg_catalog::snapshot_for(reader)?;
                    let at = if table.contains('.') {
                        snapshot.find_internal(table)
                    } else {
                        snapshot.find_relation(None, table)
                    };
                    Ok(at.and_then(|at| snapshot.relations.get(at)).map_or(0, |entry| entry.oid))
                })?;
                let rewriting = FiredStatement {
                    tag,
                    object: fired.object.clone(),
                    dropped: fired.dropped,
                    rewrite: Some((oid, *reason)),
                };
                session.fire_event_triggers(&triggers, event_triggers::TABLE_REWRITE, &rewriting)?;
            }
            let result = session.run_statement(statement, io)?;
            if !dropped.is_empty() {
                session.fire_event_triggers(&triggers, event_triggers::SQL_DROP, &fired)?;
            }
            session.fire_event_triggers(&triggers, event_triggers::DDL_COMMAND_END, &fired)?;
            outcome = Some(result);
            Ok(())
        })?;
        outcome.ok_or_else(|| {
            SqlError::new(
                SqlState::INTERNAL_ERROR,
                "the command ran without a result",
            )
        })
    }
}
