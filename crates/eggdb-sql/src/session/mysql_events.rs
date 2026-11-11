use super::{OutputColumn, QueryResult, Session};
use crate::ast::{AlterEvent, EventDefinition, EventSchedule, Expr, Name};
use crate::datetime;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::mysql_date_functions::{
    add_interval, epoch_seconds_of, epoch_seconds_of_micros, micros_of_epoch_seconds,
};
use crate::types::{SqlType, Value};

pub(crate) const EVENTS_TABLE: &str = "eggdb_internal.mysql_events";
pub(crate) const SETTINGS_TABLE: &str = "eggdb_internal.mysql_settings";
const CREATE_SETTINGS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
    CREATE TABLE IF NOT EXISTS eggdb_internal.mysql_settings (name text PRIMARY KEY, value text NOT NULL)";
const CREATE_EVENTS_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
    CREATE TABLE IF NOT EXISTS eggdb_internal.mysql_events (\
    event_key text PRIMARY KEY, db text NOT NULL, name text NOT NULL, body text NOT NULL, \
    execute_at bigint, interval_value text, interval_unit text, starts bigint, ends bigint, \
    next_run bigint, preserve boolean NOT NULL, enabled boolean NOT NULL, comment text NOT NULL, \
    created bigint NOT NULL, last_altered bigint NOT NULL, last_executed bigint)";
const EVENT_COLUMNS: &str = "db, name, body, execute_at, interval_value, interval_unit, starts, \
    ends, next_run, preserve, enabled, comment, created";
const MAX_CATCH_UP_STEPS: usize = 100_000;

pub(crate) fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

pub(crate) fn number_or_null(number: Option<i64>) -> String {
    number.map_or_else(|| "NULL".to_owned(), |number| number.to_string())
}

pub(crate) fn event_key(database: &str, name: &str) -> String {
    format!("{}.{}", database.to_lowercase(), name.to_lowercase())
}

/// One stored MySQL event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StoredEvent {
    pub(crate) database: String,
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) execute_at: Option<i64>,
    pub(crate) interval_value: Option<String>,
    pub(crate) interval_unit: Option<String>,
    pub(crate) starts: Option<i64>,
    pub(crate) ends: Option<i64>,
    pub(crate) next_run: Option<i64>,
    pub(crate) preserve: bool,
    pub(crate) enabled: bool,
    pub(crate) comment: String,
    pub(crate) created: i64,
}

fn text_at(row: &[Value], at: usize) -> Option<String> {
    row.get(at).and_then(Value::character_text).map(str::to_owned)
}

fn number_at(row: &[Value], at: usize) -> Option<i64> {
    row.get(at).and_then(Value::as_integer)
}

fn flag_at(row: &[Value], at: usize) -> bool {
    matches!(row.get(at), Some(Value::Boolean(true)))
}

impl StoredEvent {
    pub(crate) fn of_row(row: &[Value]) -> Option<Self> {
        Some(Self {
            database: text_at(row, 0)?,
            name: text_at(row, 1)?,
            body: text_at(row, 2)?,
            execute_at: number_at(row, 3),
            interval_value: text_at(row, 4),
            interval_unit: text_at(row, 5),
            starts: number_at(row, 6),
            ends: number_at(row, 7),
            next_run: number_at(row, 8),
            preserve: flag_at(row, 9),
            enabled: flag_at(row, 10),
            comment: text_at(row, 11).unwrap_or_default(),
            created: number_at(row, 12).unwrap_or_default(),
        })
    }

    fn values_sql(&self, last_altered: i64) -> String {
        format!(
            "{}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, NULL",
            quoted(&event_key(&self.database, &self.name)),
            quoted(&self.database),
            quoted(&self.name),
            quoted(&self.body),
            number_or_null(self.execute_at),
            self.interval_value
                .as_deref()
                .map_or_else(|| "NULL".to_owned(), quoted),
            self.interval_unit
                .as_deref()
                .map_or_else(|| "NULL".to_owned(), quoted),
            number_or_null(self.starts),
            number_or_null(self.ends),
            number_or_null(self.next_run),
            self.preserve,
            self.enabled,
            quoted(&self.comment),
            self.created,
            last_altered,
        )
    }

    fn recurring(&self) -> bool {
        self.execute_at.is_none()
    }

    pub(crate) fn create_text(&self) -> String {
        let schedule = match (&self.execute_at, &self.interval_value, &self.interval_unit) {
            (Some(at), _, _) => format!("AT '{}'", local_text(*at)),
            (None, Some(value), Some(unit)) => {
                let mut text = format!("EVERY '{value}' {}", unit.to_ascii_uppercase());
                if let Some(starts) = self.starts {
                    text.push_str(&format!(" STARTS '{}'", local_text(starts)));
                }
                if let Some(ends) = self.ends {
                    text.push_str(&format!(" ENDS '{}'", local_text(ends)));
                }
                text
            }
            _ => String::new(),
        };
        let mut text = format!(
            "CREATE DEFINER=`root`@`localhost` EVENT `{}` ON SCHEDULE {schedule} ON COMPLETION {}PRESERVE {}",
            self.name.replace('`', "``"),
            if self.preserve { "" } else { "NOT " },
            if self.enabled { "ENABLE" } else { "DISABLE" },
        );
        if !self.comment.is_empty() {
            text.push_str(&format!(" COMMENT {}", quoted(&self.comment)));
        }
        text.push_str(" DO ");
        text.push_str(&self.body);
        text
    }

    /// The first run after `now` (seconds since 1970 UTC), skipping the runs a stopped server
    /// missed; `None` once the event has no run left.
    pub(crate) fn following_run(&self, now: i64) -> Option<i64> {
        let (Some(value), Some(unit), Some(mut next)) = (
            self.interval_value.as_deref(),
            self.interval_unit.as_deref(),
            self.next_run,
        ) else {
            return None;
        };
        let amount = Value::Text(value.to_owned());
        for _ in 0..MAX_CATCH_UP_STEPS {
            if next > now {
                break;
            }
            let micros = micros_of_epoch_seconds(next)?;
            let moved = epoch_seconds_of_micros(add_interval(micros, &amount, unit)?);
            if moved <= next {
                return None;
            }
            next = moved;
        }
        if next <= now || self.ends.is_some_and(|ends| next > ends) {
            return None;
        }
        Some(next)
    }
}

pub(crate) fn local_text(seconds: i64) -> String {
    micros_of_epoch_seconds(seconds)
        .and_then(|utc| datetime::utc_to_local(utc).ok())
        .map_or_else(String::new, datetime::format_timestamp)
}

fn unknown_event(name: &str) -> SqlError {
    SqlError::new(SqlState::MYSQL_GENERAL, format!("Unknown event '{name}'")).with_vendor_code(1539)
}

fn event_exists(name: &str) -> SqlError {
    SqlError::new(
        SqlState::MYSQL_GENERAL,
        format!("Event '{name}' already exists"),
    )
    .with_vendor_code(1537)
}

fn text_column(name: &str) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        sql_type: Some(SqlType::Text),
    }
}

pub(super) fn show_events_columns() -> Vec<OutputColumn> {
    [
        "Db",
        "Name",
        "Definer",
        "Time zone",
        "Type",
        "Execute at",
        "Interval value",
        "Interval field",
        "Starts",
        "Ends",
        "Status",
        "Originator",
        "character_set_client",
        "collation_connection",
        "Database Collation",
    ]
    .into_iter()
    .map(text_column)
    .collect()
}

pub(super) fn show_create_event_columns() -> Vec<OutputColumn> {
    [
        "Event",
        "sql_mode",
        "time_zone",
        "Create Event",
        "character_set_client",
        "collation_connection",
        "Database Collation",
    ]
    .into_iter()
    .map(text_column)
    .collect()
}

fn rows_result(columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult::rows(format!("SELECT {}", rows.len()), columns, rows)
}

fn shown_time(seconds: Option<i64>) -> Value {
    seconds.map_or(Value::Null, |seconds| Value::Text(local_text(seconds)))
}

impl Session<'_> {
    /// Keeps `SET GLOBAL event_scheduler` in the file, where the job scheduler reads it.
    pub(super) fn store_event_scheduler(&mut self, state: &str) -> SqlResult<()> {
        self.internal_sql(CREATE_SETTINGS_TABLE)?;
        self.internal_sql(&format!(
            "DELETE FROM {SETTINGS_TABLE} WHERE name = 'event_scheduler'; \
             INSERT INTO {SETTINGS_TABLE} VALUES ('event_scheduler', {})",
            quoted(state)
        ))?;
        Ok(())
    }

    fn internal_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn event_names(&self, name: &Name) -> (String, String) {
        let schema = crate::schemas::schema_of(&name.text);
        let database = if schema.is_empty() || schema == crate::schemas::DEFAULT_SCHEMA {
            self.settings.database.clone()
        } else {
            schema.to_owned()
        };
        (database, crate::schemas::base_name(&name.text).to_owned())
    }

    fn stored_event(&mut self, database: &str, name: &str) -> SqlResult<Option<StoredEvent>> {
        self.internal_sql(CREATE_EVENTS_TABLE)?;
        let results = self.internal_sql(&format!(
            "SELECT {EVENT_COLUMNS} FROM {EVENTS_TABLE} WHERE event_key = {}",
            quoted(&event_key(database, name))
        ))?;
        Ok(results
            .into_iter()
            .flat_map(|result| result.rows)
            .next()
            .and_then(|row| StoredEvent::of_row(&row)))
    }

    fn evaluated(&self, expr: &Expr) -> SqlResult<Value> {
        Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        }
        .eval(expr, None)
    }

    fn event_time(&self, expr: &Expr, what: &str) -> SqlResult<i64> {
        let value = self.evaluated(expr)?;
        epoch_seconds_of(&value)?.ok_or_else(|| {
            SqlError::new(
                SqlState::MYSQL_GENERAL,
                format!(
                    "Incorrect {what} value: '{}'",
                    value.to_text().unwrap_or_default()
                ),
            )
            .with_vendor_code(1525)
        })
    }

    fn apply_schedule(
        &self,
        event: &mut StoredEvent,
        schedule: &EventSchedule,
        now: i64,
    ) -> SqlResult<()> {
        match schedule {
            EventSchedule::At(at) => {
                let at = self.event_time(at, "AT")?;
                event.execute_at = Some(at);
                event.interval_value = None;
                event.interval_unit = None;
                event.starts = None;
                event.ends = None;
                event.next_run = Some(at);
            }
            EventSchedule::Every {
                amount,
                unit,
                starts,
                ends,
            } => {
                let amount = self.evaluated(amount)?;
                let amount = match amount {
                    Value::Text(text) => text,
                    other => other.to_text().unwrap_or_default(),
                };
                let probe = micros_of_epoch_seconds(now)
                    .and_then(|micros| add_interval(micros, &Value::Text(amount.clone()), unit))
                    .map(epoch_seconds_of_micros);
                if probe.is_none_or(|moved| moved <= now) {
                    return Err(SqlError::new(
                        SqlState::MYSQL_GENERAL,
                        "INTERVAL is either not positive or too big",
                    )
                    .with_vendor_code(1542));
                }
                let starts = match starts {
                    Some(starts) => self.event_time(starts, "STARTS")?,
                    None => now,
                };
                let ends = match ends {
                    Some(ends) => Some(self.event_time(ends, "ENDS")?),
                    None => None,
                };
                if ends.is_some_and(|ends| ends < starts) {
                    return Err(SqlError::new(
                        SqlState::MYSQL_GENERAL,
                        "ENDS is either invalid or before STARTS",
                    )
                    .with_vendor_code(1521));
                }
                event.execute_at = None;
                event.interval_value = Some(amount);
                event.interval_unit = Some(unit.clone());
                event.starts = Some(starts);
                event.ends = ends;
                event.next_run = Some(starts);
            }
        }
        Ok(())
    }

    fn statement_seconds() -> i64 {
        epoch_seconds_of_micros(datetime::statement_now())
    }

    pub(super) fn create_event(&mut self, definition: &EventDefinition) -> SqlResult<QueryResult> {
        let (database, name) = self.event_names(&definition.name);
        let now = Self::statement_seconds();
        if self.stored_event(&database, &name)?.is_some() {
            if definition.if_not_exists {
                return Ok(QueryResult::command("CREATE EVENT")
                    .with_notice(format!("Note 1537 Event '{name}' already exists")));
            }
            return Err(event_exists(&name));
        }
        let mut event = StoredEvent {
            database,
            name,
            body: definition.body.clone(),
            execute_at: None,
            interval_value: None,
            interval_unit: None,
            starts: None,
            ends: None,
            next_run: None,
            preserve: definition.preserve,
            enabled: definition.enabled,
            comment: definition.comment.clone().unwrap_or_default(),
            created: now,
        };
        self.apply_schedule(&mut event, &definition.schedule, now)?;
        if event.execute_at.is_some_and(|at| at < now) {
            if !event.preserve {
                return Ok(QueryResult::command("CREATE EVENT").with_notice(
                    "Note 1588 Event execution time is in the past and ON COMPLETION NOT PRESERVE is set. The event was dropped immediately after creation."
                        .to_owned(),
                ));
            }
            event.enabled = false;
        }
        self.internal_sql(&format!(
            "INSERT INTO {EVENTS_TABLE} VALUES ({})",
            event.values_sql(now)
        ))?;
        Ok(QueryResult::command("CREATE EVENT"))
    }

    pub(super) fn alter_event(&mut self, alter: &AlterEvent) -> SqlResult<QueryResult> {
        let (database, name) = self.event_names(&alter.name);
        let now = Self::statement_seconds();
        let Some(mut event) = self.stored_event(&database, &name)? else {
            return Err(unknown_event(&name));
        };
        let old_key = event_key(&database, &name);
        if let Some(schedule) = &alter.schedule {
            self.apply_schedule(&mut event, schedule, now)?;
        }
        if let Some(preserve) = alter.preserve {
            event.preserve = preserve;
        }
        if let Some(enabled) = alter.enabled {
            event.enabled = enabled;
        }
        if let Some(comment) = &alter.comment {
            event.comment = comment.clone();
        }
        if let Some(body) = &alter.body {
            event.body = body.clone();
        }
        if let Some(rename) = &alter.rename {
            let (new_database, new_name) = self.event_names(rename);
            if event_key(&new_database, &new_name) != old_key
                && self.stored_event(&new_database, &new_name)?.is_some()
            {
                return Err(event_exists(&new_name));
            }
            event.database = new_database;
            event.name = new_name;
        }
        self.internal_sql(&format!(
            "DELETE FROM {EVENTS_TABLE} WHERE event_key = {}; INSERT INTO {EVENTS_TABLE} VALUES ({})",
            quoted(&old_key),
            event.values_sql(now)
        ))?;
        Ok(QueryResult::command("ALTER EVENT"))
    }

    pub(super) fn drop_event(&mut self, name: &Name, if_exists: bool) -> SqlResult<QueryResult> {
        let (database, event) = self.event_names(name);
        if self.stored_event(&database, &event)?.is_none() {
            if if_exists {
                return Ok(QueryResult::command("DROP EVENT")
                    .with_notice(format!("Note 1305 Event {event} does not exist")));
            }
            return Err(unknown_event(&event));
        }
        self.internal_sql(&format!(
            "DELETE FROM {EVENTS_TABLE} WHERE event_key = {}",
            quoted(&event_key(&database, &event))
        ))?;
        Ok(QueryResult::command("DROP EVENT"))
    }

    pub(super) fn show_events(&mut self, like: Option<&str>) -> SqlResult<QueryResult> {
        self.internal_sql(CREATE_EVENTS_TABLE)?;
        let database = self.settings.database.clone();
        let results = self.internal_sql(&format!(
            "SELECT {EVENT_COLUMNS} FROM {EVENTS_TABLE} WHERE lower(db) = lower({}) ORDER BY name",
            quoted(&database)
        ))?;
        let zone = self
            .settings
            .mysql
            .system("time_zone")
            .and_then(|value| value.to_text().ok())
            .unwrap_or_else(|| "SYSTEM".to_owned());
        let mut rows = Vec::new();
        for event in results
            .into_iter()
            .flat_map(|result| result.rows)
            .filter_map(|row| StoredEvent::of_row(&row))
        {
            if let Some(pattern) = like
                && !crate::eval::like(&event.name.to_lowercase(), &pattern.to_lowercase())?
            {
                continue;
            }
            let recurring = event.recurring();
            rows.push(vec![
                Value::Text(event.database.clone()),
                Value::Text(event.name.clone()),
                Value::Text("root@localhost".into()),
                Value::Text(zone.clone()),
                Value::Text(if recurring { "RECURRING" } else { "ONE TIME" }.into()),
                shown_time(event.execute_at),
                event.interval_value.clone().map_or(Value::Null, Value::Text),
                event
                    .interval_unit
                    .as_deref()
                    .map_or(Value::Null, |unit| Value::Text(unit.to_ascii_uppercase())),
                shown_time(event.starts),
                shown_time(event.ends),
                Value::Text(if event.enabled { "ENABLED" } else { "DISABLED" }.into()),
                Value::Text("1".into()),
                Value::Text("utf8mb4".into()),
                Value::Text("utf8mb4_0900_ai_ci".into()),
                Value::Text("utf8mb4_0900_ai_ci".into()),
            ]);
        }
        Ok(rows_result(show_events_columns(), rows))
    }

    pub(super) fn show_create_event(&mut self, name: &Name) -> SqlResult<QueryResult> {
        let (database, event) = self.event_names(name);
        let Some(stored) = self.stored_event(&database, &event)? else {
            return Err(unknown_event(&event));
        };
        Ok(rows_result(
            show_create_event_columns(),
            vec![vec![
                Value::Text(stored.name.clone()),
                Value::Text(String::new()),
                Value::Text("SYSTEM".into()),
                Value::Text(stored.create_text()),
                Value::Text("utf8mb4".into()),
                Value::Text("utf8mb4_0900_ai_ci".into()),
                Value::Text("utf8mb4_0900_ai_ci".into()),
            ]],
        ))
    }

    /// Runs a MySQL event's body (one statement or a `BEGIN ... END` block) in this session, as
    /// the event scheduler does.
    pub(crate) fn run_mysql_event_body(&mut self, body: &str) -> SqlResult<()> {
        let settings = self.settings.clone();
        self.write("EVENT", |writer, tables| {
            crate::plpgsql::run_mysql_inline(writer, tables, &settings, body)?;
            Ok(QueryResult::command("EVENT"))
        })
        .map(drop)
    }
}
