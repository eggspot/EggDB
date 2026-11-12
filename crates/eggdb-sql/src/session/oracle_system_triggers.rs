use super::{Block, CopyIo, QueryResult, Session};
use crate::ast::{AlterTrigger, AlterTriggerAction, Name, Statement};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_system_triggers::{
    CREATE_TRIGGERS_TABLE, CreateSystemTrigger, SystemEvent, SystemScope, TRIGGERS_TABLE, covers, ddl_event,
    object_type,
};
use crate::sqlserver_security::literal;
use crate::types::Value;

struct StoredSystemTrigger {
    timing: String,
    events: Vec<String>,
    owner: Option<String>,
    body: String,
}

const STATEMENT_MARK: &str = "\u{0}system trigger";

fn text_of(value: &Value) -> Option<String> {
    value.character_text().map(str::to_owned)
}

impl Session<'_> {
    fn system_trigger_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        self.in_trigger_dialect(Dialect::PostgreSql, |session| session.run_quietly(text))
    }

    pub(super) fn system_triggers_stored(&mut self) -> bool {
        self.read(|reader, _| Ok(reader.get(&crate::encoding::table_key(TRIGGERS_TABLE))?.is_some()))
            .unwrap_or(false)
    }

    pub(super) fn is_system_trigger(&mut self, name: &str) -> bool {
        if !self.system_triggers_stored() {
            return false;
        }
        self.system_trigger_sql(&format!(
            "SELECT 1 FROM {TRIGGERS_TABLE} WHERE upper(name) = upper({})",
            literal(name)
        ))
        .is_ok_and(|results| results.iter().any(|result| !result.rows.is_empty()))
    }

    fn enabled_system_triggers(&mut self) -> SqlResult<Vec<StoredSystemTrigger>> {
        let results = self.system_trigger_sql(&format!(
            "SELECT timing, events, owner, body FROM {TRIGGERS_TABLE} WHERE enabled ORDER BY created, name"
        ))?;
        Ok(results
            .into_iter()
            .flat_map(|result| result.rows)
            .filter(|row| row.len() == 4)
            .map(|row| StoredSystemTrigger {
                timing: text_of(&row[0]).unwrap_or_default(),
                events: text_of(&row[1])
                    .unwrap_or_default()
                    .split(',')
                    .filter(|event| !event.is_empty())
                    .map(str::to_owned)
                    .collect(),
                owner: text_of(&row[2]),
                body: text_of(&row[3]).unwrap_or_default(),
            })
            .collect())
    }

    fn oracle_login_user(&self) -> String {
        self.settings.get("session_authorization").unwrap_or_default().to_uppercase()
    }

    pub(super) fn create_system_trigger(&mut self, create: &CreateSystemTrigger) -> SqlResult<QueryResult> {
        if self.is_system_trigger(&create.name.text) && !create.replace {
            return Err(SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                "ORA-04081: trigger already exists",
            )
            .with_oracle_code(4081));
        }
        let owner = match &create.scope {
            SystemScope::Database => "NULL".to_owned(),
            SystemScope::Schema(Some(owner)) => literal(owner),
            SystemScope::Schema(None) => literal(&self.oracle_login_user()),
        };
        self.system_trigger_sql(CREATE_TRIGGERS_TABLE)?;
        self.system_trigger_sql(&format!(
            "DELETE FROM {TRIGGERS_TABLE} WHERE upper(name) = upper({name}); \
             INSERT INTO {TRIGGERS_TABLE} VALUES ({name}, {timing}, {events}, {owner}, {body}, {enabled}, localtimestamp)",
            name = literal(&create.name.text.to_uppercase()),
            timing = literal(create.timing.code()),
            events = literal(&create.events.join(",")),
            body = literal(&create.body),
            enabled = create.enabled,
        ))?;
        Ok(QueryResult::command("CREATE TRIGGER"))
    }

    pub(super) fn drop_system_trigger(&mut self, name: &str) -> SqlResult<QueryResult> {
        self.system_trigger_sql(&format!(
            "DELETE FROM {TRIGGERS_TABLE} WHERE upper(name) = upper({})",
            literal(name)
        ))?;
        Ok(QueryResult::command("DROP TRIGGER"))
    }

    pub(super) fn alter_system_trigger(&mut self, alter: &AlterTrigger) -> SqlResult<QueryResult> {
        let assignment = match &alter.action {
            AlterTriggerAction::Enable => "enabled = true".to_owned(),
            AlterTriggerAction::Disable => "enabled = false".to_owned(),
            AlterTriggerAction::Compile => return Ok(QueryResult::command("ALTER TRIGGER")),
            AlterTriggerAction::RenameTo(to) => {
                if self.is_system_trigger(&to.text) {
                    return Err(SqlError::new(SqlState::DUPLICATE_OBJECT, "ORA-04081: trigger already exists")
                        .with_oracle_code(4081));
                }
                format!("name = {}", literal(&to.text.to_uppercase()))
            }
        };
        self.system_trigger_sql(&format!(
            "UPDATE {TRIGGERS_TABLE} SET {assignment} WHERE upper(name) = upper({})",
            literal(&alter.name.name.text)
        ))?;
        Ok(QueryResult::command("ALTER TRIGGER"))
    }

    fn triggers_for(&mut self, timing: &str, event: &str) -> SqlResult<Vec<StoredSystemTrigger>> {
        let user = self.oracle_login_user();
        Ok(self
            .enabled_system_triggers()?
            .into_iter()
            .filter(|trigger| {
                trigger.timing == timing
                    && covers(&trigger.events, event)
                    && trigger.owner.as_ref().is_none_or(|owner| owner.eq_ignore_ascii_case(&user))
            })
            .collect())
    }

    fn run_system_trigger_bodies(&mut self, triggers: &[StoredSystemTrigger], event: SystemEvent) -> SqlResult<()> {
        let _firing = crate::oracle_system_triggers::enter(event);
        for trigger in triggers {
            self.run_quietly(&trigger.body)?;
        }
        Ok(())
    }

    fn system_event_of(&self, statement: &Statement, event: &str) -> SystemEvent {
        let (object_name, object_owner, kind) = match crate::event_triggers::object_of(statement) {
            Some((kind, identity, schema)) => (
                Some(identity.rsplit('.').next().unwrap_or(&identity).to_uppercase()),
                schema,
                object_type(kind),
            ),
            None => match crate::event_triggers::dropped_objects(statement).into_iter().next() {
                Some(dropped) => (Some(dropped.name.to_uppercase()), dropped.schema, object_type(dropped.object_type)),
                None => (None, None, None),
            },
        };
        let login_user = self.oracle_login_user();
        let object_owner = object_owner.map(|schema| {
            if schema == crate::schemas::DEFAULT_SCHEMA {
                login_user.clone()
            } else {
                schema.to_uppercase()
            }
        });
        SystemEvent {
            sysevent: event.to_uppercase(),
            object_name,
            object_owner,
            object_type: kind.map(str::to_owned),
            login_user,
            server_error: None,
        }
    }

    fn statement_ddl_event(statement: &Statement) -> Option<&'static str> {
        if matches!(statement, Statement::Truncate(_)) {
            return Some("truncate");
        }
        crate::event_triggers::ddl_tag(statement).and_then(ddl_event)
    }

    /// Runs an outermost Oracle statement with the system triggers that cover it: `BEFORE`
    /// triggers, the statement, then `AFTER` triggers, undone together when one fails; and
    /// `AFTER SERVERERROR` triggers when the statement fails.
    pub(super) fn run_with_system_triggers(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
    ) -> SqlResult<QueryResult> {
        let outcome = match Self::statement_ddl_event(statement) {
            Some(event) => self.run_ddl_with_system_triggers(statement, io, event),
            None => self.run_statement(statement, io),
        };
        if let Err(error) = &outcome {
            self.fire_server_error_triggers(error);
        }
        outcome
    }

    fn run_ddl_with_system_triggers(
        &mut self,
        statement: &mut Statement,
        io: &mut CopyIo<'_, '_>,
        event: &'static str,
    ) -> SqlResult<QueryResult> {
        let before = self.triggers_for("BEFORE", event)?;
        let after = self.triggers_for("AFTER", event)?;
        if before.is_empty() && after.is_empty() {
            return self.run_statement(statement, io);
        }
        let data = self.system_event_of(statement, event);
        let mark = Name {
            text: STATEMENT_MARK.to_owned(),
            position: 0,
        };
        let opened = matches!(self.block, Block::None);
        if opened {
            self.in_trigger_dialect(Dialect::PostgreSql, |session| session.run_quietly("BEGIN"))?;
        } else {
            self.set_savepoint(&mark)?;
        }
        let outcome = self
            .run_system_trigger_bodies(&before, data.clone())
            .and_then(|()| self.run_statement(statement, io))
            .and_then(|result| self.run_system_trigger_bodies(&after, data).map(|()| result));
        let finish = match (&outcome, opened) {
            (Ok(_), true) => "COMMIT",
            (Err(_), true) => "ROLLBACK",
            (Ok(_), false) => {
                self.release_savepoint(&mark)?;
                return outcome;
            }
            (Err(_), false) => {
                let _ = self.rollback_to_savepoint(&mark);
                let _ = self.release_savepoint(&mark);
                return outcome;
            }
        };
        if self.in_block() {
            self.in_trigger_dialect(Dialect::PostgreSql, |session| session.run_quietly(finish))?;
        }
        outcome
    }

    fn fire_server_error_triggers(&mut self, error: &SqlError) {
        let Ok(triggers) = self.triggers_for("AFTER", "servererror") else {
            return;
        };
        if triggers.is_empty() {
            return;
        }
        let code = error.vendor_code().map_or(0, i64::from);
        let message = if code == 0 {
            error.message().to_owned()
        } else {
            format!("ORA-{code:05}: {}", error.message())
        };
        let event = SystemEvent {
            sysevent: "SERVERERROR".to_owned(),
            login_user: self.oracle_login_user(),
            server_error: Some((code, message)),
            ..SystemEvent::default()
        };
        let _ = self.run_system_trigger_bodies(&triggers, event);
    }

    /// Fires `AFTER LOGON` triggers once an Oracle-dialect connection has authenticated; an
    /// error from one fails the login, as Oracle's do for users without `ADMINISTER DATABASE
    /// TRIGGER`.
    pub(super) fn fire_logon_triggers(&mut self) -> SqlResult<()> {
        if !self.system_triggers_stored() {
            return Ok(());
        }
        let triggers = self.triggers_for("AFTER", "logon")?;
        if triggers.is_empty() {
            return Ok(());
        }
        let event = SystemEvent {
            sysevent: "LOGON".to_owned(),
            login_user: self.oracle_login_user(),
            ..SystemEvent::default()
        };
        self.run_system_trigger_bodies(&triggers, event)
    }
}
