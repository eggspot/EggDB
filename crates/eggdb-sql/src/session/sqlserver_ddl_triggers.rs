use super::{Block, CopyIo, QueryResult, Session};
use crate::ast::Statement;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::sqlserver_ddl_triggers::{
    CREATE_TRIGGERS_TABLE, DdlTriggerCommand, DdlTriggerScope, EventData, LOGON_EVENT, TRIGGERS_TABLE,
    event_matches, event_type, is_known_event, object_type,
};
use crate::sqlserver_security::literal;
use crate::types::Value;

struct StoredDdlTrigger {
    name: String,
    events: Vec<String>,
    body: String,
    enabled: bool,
}

fn missing_trigger(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("Cannot drop the trigger '{name}', because it does not exist or you do not have permission."),
    )
    .with_sqlserver_code(3701)
}

fn transaction_ended_in_trigger() -> SqlError {
    SqlError::new(
        SqlState::TRANSACTION_ROLLBACK,
        "The transaction ended in the trigger. The batch has been aborted.",
    )
    .with_sqlserver_code(3609)
}

fn text_of(value: &Value) -> String {
    value.character_text().map(str::to_owned).unwrap_or_default()
}

impl Session<'_> {
    fn ddl_trigger_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn ddl_triggers_stored(&mut self) -> SqlResult<bool> {
        self.read(|reader, _| Ok(reader.get(&crate::encoding::table_key(TRIGGERS_TABLE))?.is_some()))
    }

    fn stored_ddl_triggers(&mut self) -> SqlResult<Vec<StoredDdlTrigger>> {
        if !self.ddl_triggers_stored()? {
            return Ok(Vec::new());
        }
        let results = self.ddl_trigger_sql(&format!(
            "SELECT name, events, body, enabled FROM {TRIGGERS_TABLE} ORDER BY created, name"
        ))?;
        Ok(results
            .into_iter()
            .flat_map(|result| result.rows)
            .filter(|row| row.len() == 4)
            .map(|row| StoredDdlTrigger {
                name: text_of(&row[0]),
                events: text_of(&row[1]).split(',').filter(|event| !event.is_empty()).map(str::to_owned).collect(),
                body: text_of(&row[2]),
                enabled: row[3] == Value::Boolean(true),
            })
            .collect())
    }

    fn require_ddl_trigger_owner(&self) -> SqlResult<()> {
        if self.settings.checked_role.is_some() {
            return Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "Cannot create, alter or drop a DDL trigger: only a server administrator can.",
            )
            .with_sqlserver_code(1088));
        }
        Ok(())
    }

    pub(super) fn run_ddl_trigger_command(&mut self, command: &DdlTriggerCommand) -> SqlResult<QueryResult> {
        self.require_ddl_trigger_owner()?;
        match command {
            DdlTriggerCommand::Create {
                name,
                scope,
                events,
                body,
                replace,
                alter,
            } => {
                if let Some(unknown) = events.iter().find(|event| !is_known_event(&event.text)) {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("'{}' is not a recognized trigger event type.", unknown.text.to_ascii_uppercase()),
                    )
                    .with_sqlserver_code(1084)
                    .at(unknown.position));
                }
                let logon = events.iter().any(|event| event.text.eq_ignore_ascii_case(LOGON_EVENT));
                if logon && (*scope != DdlTriggerScope::Server || events.len() > 1) {
                    return Err(SqlError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "The specified event type(s) is/are not valid on the specified target object.",
                    )
                    .with_sqlserver_code(1098));
                }
                let exists = self.stored_ddl_triggers()?.iter().any(|stored| stored.name == name.text);
                if exists && !*replace && !*alter {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("There is already an object named '{}' in the database.", name.text),
                    )
                    .with_sqlserver_code(2714));
                }
                if !exists && *alter {
                    return Err(missing_trigger(&name.text));
                }
                let events: Vec<String> = events.iter().map(|event| event.text.to_ascii_uppercase()).collect();
                self.ddl_trigger_sql(CREATE_TRIGGERS_TABLE)?;
                self.ddl_trigger_sql(&format!(
                    "DELETE FROM {TRIGGERS_TABLE} WHERE name = {name}; \
                     INSERT INTO {TRIGGERS_TABLE} VALUES ({name}, {scope}, {events}, {body}, true, localtimestamp)",
                    name = literal(&name.text),
                    scope = literal(scope.code()),
                    events = literal(&events.join(",")),
                    body = literal(body),
                ))?;
                Ok(QueryResult::command(command.tag()))
            }
            DdlTriggerCommand::Drop {
                names,
                scope,
                if_exists,
            } => {
                let stored = self.stored_ddl_triggers()?;
                for name in names {
                    if !stored.iter().any(|found| found.name == name.text) {
                        if *if_exists {
                            continue;
                        }
                        return Err(missing_trigger(&name.text));
                    }
                    self.ddl_trigger_sql(&format!(
                        "DELETE FROM {TRIGGERS_TABLE} WHERE name = {} AND scope = {}",
                        literal(&name.text),
                        literal(scope.code())
                    ))?;
                }
                Ok(QueryResult::command(command.tag()))
            }
            DdlTriggerCommand::Enable { names, scope, enable } => {
                let stored = self.stored_ddl_triggers()?;
                let filter = match names {
                    None => String::new(),
                    Some(names) => {
                        if let Some(name) = names.iter().find(|name| !stored.iter().any(|found| found.name == name.text)) {
                            return Err(SqlError::new(
                                SqlState::UNDEFINED_OBJECT,
                                format!("Cannot find the object \"{}\" because it does not exist or you do not have permissions.", name.text),
                            )
                            .with_sqlserver_code(1088));
                        }
                        let listed: Vec<String> = names.iter().map(|name| literal(&name.text)).collect();
                        format!(" AND name IN ({})", listed.join(", "))
                    }
                };
                if self.ddl_triggers_stored()? {
                    self.ddl_trigger_sql(&format!(
                        "UPDATE {TRIGGERS_TABLE} SET enabled = {enable} WHERE scope = {}{filter}",
                        literal(scope.code())
                    ))?;
                }
                Ok(QueryResult::command(command.tag()))
            }
        }
    }

    fn ddl_event_data(&mut self, statement: &Statement, event_type: &'static str) -> EventData {
        let named = crate::event_triggers::object_of(statement);
        let (object, schema, kind) = match &named {
            Some((kind, identity, schema)) => (
                Some(identity.rsplit('.').next().unwrap_or(identity).to_owned()),
                schema.clone(),
                object_type(kind),
            ),
            None => match crate::event_triggers::dropped_objects(statement).first() {
                Some(dropped) => (Some(dropped.name.clone()), dropped.schema.clone(), object_type(dropped.object_type)),
                None => (None, None, None),
            },
        };
        let post_time = Value::Timestamp(crate::datetime::statement_now())
            .to_text()
            .unwrap_or_default()
            .replacen(' ', "T", 1);
        EventData {
            event_type,
            post_time,
            spid: self.settings.sqlserver.connection_id,
            login: self.settings.get("session_authorization").unwrap_or_default(),
            database: self.settings.sqlserver.database.clone(),
            schema: schema.map(|schema| if schema == crate::schemas::DEFAULT_SCHEMA { crate::sqlserver_catalog::DBO.to_owned() } else { schema }),
            object,
            object_type: kind,
            command_text: self.ddl_command_text.take().unwrap_or_default(),
        }
    }

    /// Runs every enabled logon trigger (`ON ALL SERVER FOR LOGON`) once a SQL Server-dialect
    /// connection has signed in, in creation order, in one transaction: an error from one, or a
    /// `ROLLBACK` in one, refuses the login, as SQL Server's do.
    pub(super) fn fire_sqlserver_logon_triggers(&mut self) -> SqlResult<()> {
        if !self.ddl_triggers_stored()? {
            return Ok(());
        }
        let triggers: Vec<StoredDdlTrigger> = self
            .stored_ddl_triggers()?
            .into_iter()
            .filter(|trigger| trigger.enabled && trigger.events.iter().any(|event| event.eq_ignore_ascii_case(LOGON_EVENT)))
            .collect();
        if triggers.is_empty() {
            return Ok(());
        }
        let post_time = Value::Timestamp(crate::datetime::statement_now())
            .to_text()
            .unwrap_or_default()
            .replacen(' ', "T", 1);
        let data = EventData {
            event_type: LOGON_EVENT,
            post_time,
            spid: self.settings.sqlserver.connection_id,
            login: self.settings.get("session_authorization").unwrap_or_default(),
            database: self.settings.sqlserver.database.clone(),
            schema: None,
            object: None,
            object_type: None,
            command_text: String::new(),
        };
        let opened = self.autocommit() && matches!(self.block, Block::None);
        if opened {
            self.run_quietly("BEGIN TRANSACTION")?;
        }
        let outcome = {
            let _firing = crate::sqlserver_ddl_triggers::enter(&data);
            triggers.iter().try_for_each(|trigger| {
                self.run_ddl_trigger_body(&trigger.body)?;
                if self.in_block() {
                    Ok(())
                } else {
                    Err(transaction_ended_in_trigger())
                }
            })
        };
        match outcome {
            Ok(()) => {
                if opened && self.in_block() {
                    self.run_quietly("COMMIT TRANSACTION")?;
                }
                Ok(())
            }
            Err(error) => {
                if self.in_block() {
                    let _ = self.run_quietly("ROLLBACK TRANSACTION");
                }
                Err(error)
            }
        }
    }

    /// Runs a DDL statement in the SQL Server dialect and then every enabled DDL trigger whose
    /// events cover it, in the order they were created, in one transaction: a trigger that
    /// fails, or ends the transaction with `ROLLBACK`, undoes the statement.
    pub(super) fn run_with_ddl_triggers(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
        tag: &'static str,
    ) -> SqlResult<QueryResult> {
        let Some(event) = event_type(tag) else {
            return self.run_statement(statement, io);
        };
        let triggers: Vec<StoredDdlTrigger> = self
            .stored_ddl_triggers()?
            .into_iter()
            .filter(|trigger| trigger.enabled && trigger.events.iter().any(|wanted| event_matches(wanted, event)))
            .collect();
        if triggers.is_empty() {
            return self.run_statement(statement, io);
        }
        let data = self.ddl_event_data(statement, event);
        let opened = self.autocommit() && matches!(self.block, Block::None);
        if opened {
            self.run_quietly("BEGIN TRANSACTION")?;
        }
        let outcome = self.run_statement(statement, io).and_then(|result| {
            let _firing = crate::sqlserver_ddl_triggers::enter(&data);
            for trigger in &triggers {
                self.run_ddl_trigger_body(&trigger.body)?;
                if !self.in_block() {
                    return Err(transaction_ended_in_trigger());
                }
            }
            Ok(result)
        });
        match outcome {
            Ok(result) => {
                if opened && self.in_block() {
                    self.run_quietly("COMMIT TRANSACTION")?;
                }
                Ok(result)
            }
            Err(error) => {
                if self.in_block() {
                    if opened {
                        let _ = self.run_quietly("ROLLBACK TRANSACTION");
                    } else {
                        self.abandon_transaction()?;
                    }
                }
                Err(error)
            }
        }
    }
}
