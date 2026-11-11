use super::{AgentProcedure, Arguments, agent_error, quoted};
use crate::error::SqlResult;
use crate::session::{QueryResult, Session};
use crate::types::Value;
use std::cell::Cell;

const OPERATORS_TABLE: &str = "eggdb_internal.sqlserver_agent_operators";
const ALERTS_TABLE: &str = "eggdb_internal.sqlserver_agent_alerts";
const NOTIFICATIONS_TABLE: &str = "eggdb_internal.sqlserver_agent_alert_notifications";
const SENT_TABLE: &str = "eggdb_internal.sqlserver_agent_notifications_sent";
const HIGHEST_SEVERITY: i64 = 25;
const NOTIFY_BY_EVERY_METHOD: i64 = 7;
pub(super) const CREATE_ALERT_TABLES: &str = "CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_operators (name text PRIMARY KEY, enabled boolean NOT NULL, email_address text, pager_address text, netsend_address text); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_alerts (name text PRIMARY KEY, message_id integer NOT NULL, severity integer NOT NULL, enabled boolean NOT NULL, delay_between_responses integer NOT NULL, notification_message text, database_name text, event_description_keyword text, job_name text, occurrence_count integer NOT NULL DEFAULT 0, last_occurrence timestamptz, last_response timestamptz); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_alert_notifications (alert_name text NOT NULL, operator_name text NOT NULL, notification_method integer NOT NULL, PRIMARY KEY (alert_name, operator_name)); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_notifications_sent (operator_name text NOT NULL, alert_name text, subject text, body text, sent_at timestamptz NOT NULL DEFAULT now()); \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysoperators AS SELECT (row_number() OVER (ORDER BY name))::integer AS id, name, CASE WHEN enabled THEN 1 ELSE 0 END AS enabled, email_address, pager_address, netsend_address, 0 AS last_email_date, 0 AS last_email_time, 0 AS category_id FROM eggdb_internal.sqlserver_agent_operators; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysalerts AS SELECT (row_number() OVER (ORDER BY a.name))::integer AS id, a.name, 'MSSQLSERVER' AS event_source, a.message_id, a.severity, CASE WHEN a.enabled THEN 1 ELSE 0 END AS enabled, a.delay_between_responses, coalesce(to_char(a.last_occurrence, 'YYYYMMDD')::integer, 0) AS last_occurrence_date, coalesce(to_char(a.last_occurrence, 'HH24MISS')::integer, 0) AS last_occurrence_time, coalesce(to_char(a.last_response, 'YYYYMMDD')::integer, 0) AS last_response_date, coalesce(to_char(a.last_response, 'HH24MISS')::integer, 0) AS last_response_time, a.notification_message, a.database_name, a.event_description_keyword, a.occurrence_count, CASE WHEN a.job_name IS NULL THEN '00000000-0000-0000-0000-000000000000'::uuid ELSE md5(a.job_name)::uuid END AS job_id, (SELECT count(*)::integer FROM eggdb_internal.sqlserver_agent_alert_notifications n WHERE n.alert_name = a.name) AS has_notification, 0 AS category_id FROM eggdb_internal.sqlserver_agent_alerts a; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysnotifications AS SELECT a.id AS alert_id, o.id AS operator_id, n.notification_method FROM eggdb_internal.sqlserver_agent_alert_notifications n JOIN eggdb_internal.sqlserver_agent_sysalerts a ON a.name = n.alert_name JOIN eggdb_internal.sqlserver_agent_sysoperators o ON o.name = n.operator_name";

thread_local! {
    static FIRING: Cell<bool> = const { Cell::new(false) };
}

struct Firing;

impl Firing {
    fn start() -> Option<Self> {
        (!FIRING.with(|firing| firing.replace(true))).then_some(Self)
    }
}

impl Drop for Firing {
    fn drop(&mut self) {
        FIRING.with(|firing| firing.set(false));
    }
}

fn text_or_null(text: Option<String>) -> String {
    text.map_or_else(|| "NULL".to_owned(), |text| quoted(&text))
}

impl Session<'_> {
    fn agent_name_exists(&mut self, table: &str, name: &str) -> SqlResult<bool> {
        Ok(!self
            .agent_rows(&format!("SELECT 1 FROM {table} WHERE name = {}", quoted(name)))?
            .is_empty())
    }

    fn missing_name(kind: &str, name: &str) -> crate::error::SqlError {
        agent_error(14262, format!("The specified {kind} ('{name}') does not exist."))
    }

    pub(super) fn agent_alert_procedure(
        &mut self,
        procedure: AgentProcedure,
        arguments: &Arguments,
        procedure_name: &str,
    ) -> SqlResult<Vec<QueryResult>> {
        self.agent_sql(CREATE_ALERT_TABLES)?;
        match procedure {
            AgentProcedure::AddOperator => {
                let name = arguments.required("@name", procedure_name)?;
                if self.agent_name_exists(OPERATORS_TABLE, &name)? {
                    return Err(agent_error(14261, format!("The specified @name ('{name}') already exists.")));
                }
                self.agent_sql(&format!(
                    "INSERT INTO {OPERATORS_TABLE} VALUES ({}, {}, {}, {}, {})",
                    quoted(&name),
                    arguments.integer("@enabled", 1) != 0,
                    text_or_null(arguments.text("@email_address")),
                    text_or_null(arguments.text("@pager_address")),
                    text_or_null(arguments.text("@netsend_address")),
                ))?;
            }
            AgentProcedure::UpdateOperator => {
                let name = arguments.required("@name", procedure_name)?;
                if !self.agent_name_exists(OPERATORS_TABLE, &name)? {
                    return Err(Self::missing_name("@name", &name));
                }
                let mut changes = Vec::new();
                if let Some(enabled) = arguments.value("@enabled").and_then(Value::as_integer) {
                    changes.push(format!("enabled = {}", enabled != 0));
                }
                for column in ["email_address", "pager_address", "netsend_address"] {
                    if let Some(text) = arguments.text(&format!("@{column}")) {
                        changes.push(format!("{column} = {}", quoted(&text)));
                    }
                }
                if !changes.is_empty() {
                    self.agent_sql(&format!(
                        "UPDATE {OPERATORS_TABLE} SET {} WHERE name = {}",
                        changes.join(", "),
                        quoted(&name)
                    ))?;
                }
                if let Some(new_name) = arguments.text("@new_name") {
                    self.agent_sql(&format!(
                        "UPDATE {OPERATORS_TABLE} SET name = {new} WHERE name = {old}; UPDATE {NOTIFICATIONS_TABLE} SET operator_name = {new} WHERE operator_name = {old}",
                        new = quoted(&new_name),
                        old = quoted(&name)
                    ))?;
                }
            }
            AgentProcedure::DeleteOperator => {
                let name = arguments.required("@name", procedure_name)?;
                if !self.agent_name_exists(OPERATORS_TABLE, &name)? {
                    return Err(Self::missing_name("@name", &name));
                }
                self.agent_sql(&format!(
                    "DELETE FROM {NOTIFICATIONS_TABLE} WHERE operator_name = {name}; DELETE FROM {OPERATORS_TABLE} WHERE name = {name}",
                    name = quoted(&name)
                ))?;
            }
            AgentProcedure::AddAlert | AgentProcedure::UpdateAlert => {
                let name = arguments.required("@name", procedure_name)?;
                let exists = self.agent_name_exists(ALERTS_TABLE, &name)?;
                let adding = procedure == AgentProcedure::AddAlert;
                if adding && exists {
                    return Err(agent_error(14261, format!("The specified @name ('{name}') already exists.")));
                }
                if !adding && !exists {
                    return Err(Self::missing_name("@name", &name));
                }
                if arguments.text("@performance_condition").is_some() || arguments.text("@wmi_query").is_some() {
                    return Err(crate::error::SqlError::not_supported(
                        "performance condition and WMI alerts; EggDB raises SQL Server event alerts",
                    ));
                }
                let job = arguments.text("@job_name");
                if let Some(job) = &job
                    && !self.agent_job_exists(job)?
                {
                    return Err(Self::missing_name("@job_name", job));
                }
                let message_id = arguments.integer("@message_id", 0);
                let severity = arguments.integer("@severity", 0);
                if adding && ((message_id == 0) == (severity == 0)) {
                    return Err(agent_error(
                        14500,
                        "Supply either a non-zero message ID or a non-zero severity, but not both.".to_owned(),
                    ));
                }
                if !(0..=HIGHEST_SEVERITY).contains(&severity) {
                    return Err(agent_error(14266, format!("The specified @severity ('{severity}') is invalid (valid values are: 1-25).")));
                }
                if adding {
                    self.agent_sql(&format!(
                        "INSERT INTO {ALERTS_TABLE} (name, message_id, severity, enabled, delay_between_responses, notification_message, database_name, event_description_keyword, job_name) VALUES ({}, {message_id}, {severity}, {}, {}, {}, {}, {}, {})",
                        quoted(&name),
                        arguments.integer("@enabled", 1) != 0,
                        arguments.integer("@delay_between_responses", 0).max(0),
                        text_or_null(arguments.text("@notification_message")),
                        text_or_null(arguments.text("@database_name")),
                        text_or_null(arguments.text("@event_description_keyword")),
                        text_or_null(job),
                    ))?;
                } else {
                    let mut changes = Vec::new();
                    if let Some(enabled) = arguments.value("@enabled").and_then(Value::as_integer) {
                        changes.push(format!("enabled = {}", enabled != 0));
                    }
                    if arguments.value("@message_id").is_some() || arguments.value("@severity").is_some() {
                        changes.push(format!("message_id = {message_id}, severity = {severity}"));
                    }
                    if let Some(delay) = arguments.value("@delay_between_responses").and_then(Value::as_integer) {
                        changes.push(format!("delay_between_responses = {}", delay.max(0)));
                    }
                    for column in ["notification_message", "database_name", "event_description_keyword", "job_name"] {
                        if let Some(text) = arguments.text(&format!("@{column}")) {
                            changes.push(format!("{column} = {}", quoted(&text)));
                        }
                    }
                    if arguments.value("@occurrence_count").and_then(Value::as_integer) == Some(0) {
                        changes.push("occurrence_count = 0".to_owned());
                    }
                    if !changes.is_empty() {
                        self.agent_sql(&format!(
                            "UPDATE {ALERTS_TABLE} SET {} WHERE name = {}",
                            changes.join(", "),
                            quoted(&name)
                        ))?;
                    }
                }
            }
            AgentProcedure::DeleteAlert => {
                let name = arguments.required("@name", procedure_name)?;
                if !self.agent_name_exists(ALERTS_TABLE, &name)? {
                    return Err(Self::missing_name("@name", &name));
                }
                self.agent_sql(&format!(
                    "DELETE FROM {NOTIFICATIONS_TABLE} WHERE alert_name = {name}; DELETE FROM {ALERTS_TABLE} WHERE name = {name}",
                    name = quoted(&name)
                ))?;
            }
            AgentProcedure::AddNotification | AgentProcedure::DeleteNotification => {
                let alert = arguments.required("@alert_name", procedure_name)?;
                let operator = arguments.required("@operator_name", procedure_name)?;
                if !self.agent_name_exists(ALERTS_TABLE, &alert)? {
                    return Err(Self::missing_name("@alert_name", &alert));
                }
                if !self.agent_name_exists(OPERATORS_TABLE, &operator)? {
                    return Err(Self::missing_name("@operator_name", &operator));
                }
                if procedure == AgentProcedure::AddNotification {
                    let method = arguments.integer("@notification_method", 1);
                    if !(1..=NOTIFY_BY_EVERY_METHOD).contains(&method) {
                        return Err(agent_error(14266, format!("The specified @notification_method ('{method}') is invalid (valid values are: 1, 2, 4, 7).")));
                    }
                    self.agent_sql(&format!(
                        "INSERT INTO {NOTIFICATIONS_TABLE} VALUES ({}, {}, {method}) ON CONFLICT (alert_name, operator_name) DO UPDATE SET notification_method = excluded.notification_method",
                        quoted(&alert),
                        quoted(&operator)
                    ))?;
                } else {
                    self.agent_sql(&format!(
                        "DELETE FROM {NOTIFICATIONS_TABLE} WHERE alert_name = {} AND operator_name = {}",
                        quoted(&alert),
                        quoted(&operator)
                    ))?;
                }
            }
            AgentProcedure::NotifyOperator => {
                let name = arguments.required("@name", procedure_name)?;
                if !self.agent_name_exists(OPERATORS_TABLE, &name)? {
                    return Err(Self::missing_name("@name", &name));
                }
                self.agent_sql(&format!(
                    "INSERT INTO {SENT_TABLE} (operator_name, subject, body) VALUES ({}, {}, {})",
                    quoted(&name),
                    text_or_null(arguments.text("@subject")),
                    text_or_null(arguments.text("@body")),
                ))?;
            }
            _ => {}
        }
        Ok(Vec::new())
    }

    /// Counts the event against every enabled SQL Server Agent alert it matches, and for each
    /// one whose delay between responses has passed, runs its job and records a notification for
    /// each of its operators. A file without alerts has nothing to fire.
    pub(crate) fn fire_agent_alerts(&mut self, number: u32, severity: u8, text: &str) -> SqlResult<()> {
        let Some(_firing) = Firing::start() else {
            return Ok(());
        };
        let database = self.settings.database.clone();
        let Ok(matched) = self.agent_rows(&format!(
            "SELECT name FROM {ALERTS_TABLE} WHERE enabled \
             AND (message_id = {number} OR (message_id = 0 AND severity = {severity})) \
             AND (event_description_keyword IS NULL OR strpos({text}, event_description_keyword) > 0) \
             AND (database_name IS NULL OR lower(database_name) = lower({database})) ORDER BY name",
            text = quoted(text),
            database = quoted(&database)
        )) else {
            return Ok(());
        };
        for row in matched {
            let Some(alert) = row.first().and_then(Value::to_text) else {
                continue;
            };
            let responded = self.agent_rows(&format!(
                "UPDATE {ALERTS_TABLE} SET occurrence_count = occurrence_count + 1, last_occurrence = now() WHERE name = {alert}; \
                 UPDATE {ALERTS_TABLE} SET last_response = now() WHERE name = {alert} \
                 AND (last_response IS NULL OR last_response + make_interval(secs => delay_between_responses) <= now()) \
                 RETURNING job_name, notification_message",
                alert = quoted(&alert)
            ))?;
            let Some(response) = responded.first() else {
                continue;
            };
            let message = response.get(1).and_then(Value::to_text);
            let body = match message {
                Some(message) => format!("{text}\n{message}"),
                None => text.to_owned(),
            };
            self.agent_sql(&format!(
                "INSERT INTO {SENT_TABLE} (operator_name, alert_name, subject, body) \
                 SELECT operator_name, alert_name, {subject}, {body} FROM {NOTIFICATIONS_TABLE} WHERE alert_name = {alert}",
                subject = quoted(&format!("SQL Server Alert System: '{alert}' occurred")),
                body = quoted(&body),
                alert = quoted(&alert)
            ))?;
            if let Some(job) = response.first().and_then(Value::to_text) {
                self.run_agent_job(&job)?;
            }
        }
        Ok(())
    }
}
