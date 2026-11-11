use super::super::frame::RoutineFrame;
use super::{Flow, Run, parsed};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

const PACKAGE: &str = "dbms_scheduler";
const OWNER: &str = "sys";
const UNIX_EPOCH_FROM_2000_SECONDS: i64 = 946_684_800;
use crate::jobs::ORACLE_SCHEDULER_TABLE as SCHEDULER_TABLE;
const CREATE_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal";
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_jobs (job_name text PRIMARY KEY, job_type text NOT NULL, job_action text NOT NULL, start_epoch bigint NOT NULL, repeat_interval text, end_epoch bigint, enabled boolean NOT NULL, auto_drop boolean NOT NULL, last_run bigint)";
const PROGRAMS_TABLE: &str = "eggdb_internal.oracle_scheduler_programs";
const SCHEDULES_TABLE: &str = "eggdb_internal.oracle_scheduler_schedules";
const CREATE_OBJECT_TABLES: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_programs (program_name text PRIMARY KEY, program_type text NOT NULL, program_action text NOT NULL, enabled boolean NOT NULL)",
    "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_schedules (schedule_name text PRIMARY KEY, start_epoch bigint NOT NULL, repeat_interval text, end_epoch bigint)",
];
const JOB_TYPES: [&str; 2] = ["PLSQL_BLOCK", "STORED_PROCEDURE"];

struct SchedulerCall {
    member: String,
    arguments: String,
}

impl SchedulerCall {
    fn of(text: &str) -> Option<Self> {
        let text = text.trim();
        if !text.get(..4)?.eq_ignore_ascii_case("call") {
            return None;
        }
        let rest = text.get(4..)?;
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        let name = rest.get(..open)?.trim().replace('"', "").to_ascii_lowercase();
        let parts: Vec<&str> = name.split('.').map(str::trim).collect();
        let member = match parts.as_slice() {
            [PACKAGE, member] | [OWNER, PACKAGE, member] => (*member).to_owned(),
            _ => return None,
        };
        Some(Self {
            member,
            arguments: rest.get(open + 1..close)?.to_owned(),
        })
    }
}

fn split_arguments(text: &str) -> Vec<(Option<String>, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    for character in text.chars() {
        match character {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
        .into_iter()
        .map(|part| {
            let named = part.find("=>").filter(|at| !part[..*at].contains('\''));
            match named {
                Some(at) => (
                    Some(part[..at].trim().trim_matches('"').to_ascii_lowercase()),
                    part[at + 2..].trim().to_owned(),
                ),
                None => (None, part.trim().to_owned()),
            }
        })
        .collect()
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn scheduler_error(code: u16, message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("ORA-{code:05}: {message}")).with_oracle_code(code)
}

fn epoch_of(value: &Value) -> Option<i64> {
    match value {
        Value::Timestamp(micros) | Value::TimestampTz(micros) => {
            Some(micros.div_euclid(1_000_000) + UNIX_EPOCH_FROM_2000_SECONDS)
        }
        _ => None,
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

impl Run<'_> {
    fn scheduler_sql(&mut self, text: &str) -> SqlResult<crate::session::QueryResult> {
        let statement = parsed(text, &self.settings, &RoutineFrame::default(), Dialect::PostgreSql)?;
        self.execute(statement)
    }

    fn scheduler_arguments(&mut self, text: &str, names: &[&str], member: &str) -> SqlResult<Vec<(String, Value)>> {
        let mut bound = Vec::new();
        for (index, (name, expression)) in split_arguments(text).into_iter().enumerate() {
            let name = match name {
                Some(name) if names.contains(&name.as_str()) => name,
                Some(name) => {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("PLS-00306: wrong number or types of arguments in call to '{}' ({name})", member.to_ascii_uppercase()),
                    ));
                }
                None => names.get(index).map(|name| (*name).to_owned()).ok_or_else(|| {
                    SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("PLS-00306: wrong number or types of arguments in call to '{}'", member.to_ascii_uppercase()),
                    )
                })?,
            };
            let value = self.text_value(&expression)?;
            bound.push((name, value));
        }
        Ok(bound)
    }

    fn job_exists(&mut self, job: &str) -> SqlResult<bool> {
        let result = self.scheduler_sql(&format!(
            "SELECT 1 FROM {SCHEDULER_TABLE} WHERE job_name = {}",
            quoted(job)
        ))?;
        Ok(!result.rows.is_empty())
    }

    pub(super) fn dbms_scheduler_call(&mut self, text: &str) -> SqlResult<Option<Flow>> {
        let Some(call) = SchedulerCall::of(text) else {
            return Ok(None);
        };
        let names: &[&str] = match call.member.as_str() {
            "create_job" => &[
                "job_name",
                "job_type",
                "job_action",
                "number_of_arguments",
                "start_date",
                "repeat_interval",
                "end_date",
                "job_class",
                "enabled",
                "auto_drop",
                "comments",
                "program_name",
                "schedule_name",
            ],
            "create_program" => &[
                "program_name",
                "program_type",
                "program_action",
                "number_of_arguments",
                "enabled",
                "comments",
            ],
            "drop_program" => &["program_name", "force"],
            "create_schedule" => &["schedule_name", "start_date", "repeat_interval", "end_date", "comments"],
            "drop_schedule" => &["schedule_name", "force"],
            "run_job" => &["job_name", "use_current_session"],
            "drop_job" => &["job_name", "force", "defer"],
            "enable" => &["name", "commit_semantics"],
            "disable" => &["name", "force", "commit_semantics"],
            "set_attribute" => &["name", "attribute", "value"],
            other => {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("PLS-00302: component '{}' must be declared", other.to_ascii_uppercase()),
                ));
            }
        };
        let arguments = self.scheduler_arguments(&call.arguments, names, &call.member)?;
        let value = |name: &str| {
            arguments
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
                .filter(|value| *value != Value::Null)
        };
        let text_of = |name: &str| value(name).and_then(|value| value.to_text());
        let job = text_of("job_name")
            .or_else(|| text_of("name"))
            .or_else(|| text_of("program_name"))
            .or_else(|| text_of("schedule_name"))
            .map(|name| name.trim().to_ascii_uppercase())
            .ok_or_else(|| scheduler_error(27_452, "invalid parameter value for the job name".to_owned()))?;
        self.scheduler_sql(CREATE_SCHEMA)?;
        self.scheduler_sql(CREATE_TABLE)?;
        for statement in CREATE_OBJECT_TABLES {
            self.scheduler_sql(statement)?;
        }
        let validated_interval = |interval: &str| {
            crate::jobs::CalendarRule::parse(interval)
                .map(|_| ())
                .map_err(|message| scheduler_error(27_465, format!("invalid value {interval} for attribute REPEAT_INTERVAL: {message}")))
        };
        match call.member.as_str() {
            "create_program" => {
                let program_type = text_of("program_type").unwrap_or_else(|| "PLSQL_BLOCK".to_owned()).to_ascii_uppercase();
                if !JOB_TYPES.contains(&program_type.as_str()) {
                    return Err(SqlError::not_supported(&format!("DBMS_SCHEDULER programs of type {program_type}; EggDB runs PLSQL_BLOCK and STORED_PROCEDURE programs")));
                }
                let action = text_of("program_action").ok_or_else(|| scheduler_error(27_452, "program_action is required".to_owned()))?;
                let found = self.scheduler_sql(&format!("SELECT 1 FROM {PROGRAMS_TABLE} WHERE program_name = {}", quoted(&job)))?;
                if !found.rows.is_empty() {
                    return Err(scheduler_error(27_477, format!("\"{job}\" already exists")));
                }
                let enabled = matches!(value("enabled"), Some(Value::Boolean(true)));
                self.scheduler_sql(&format!(
                    "INSERT INTO {PROGRAMS_TABLE} VALUES ({}, {}, {}, {enabled})",
                    quoted(&job),
                    quoted(&program_type),
                    quoted(&action)
                ))?;
            }
            "create_schedule" => {
                let found = self.scheduler_sql(&format!("SELECT 1 FROM {SCHEDULES_TABLE} WHERE schedule_name = {}", quoted(&job)))?;
                if !found.rows.is_empty() {
                    return Err(scheduler_error(27_477, format!("\"{job}\" already exists")));
                }
                let interval = text_of("repeat_interval");
                if let Some(interval) = &interval {
                    validated_interval(interval)?;
                }
                let start = value("start_date").and_then(|value| epoch_of(&value)).unwrap_or_else(now_epoch);
                let end = value("end_date").and_then(|value| epoch_of(&value));
                self.scheduler_sql(&format!(
                    "INSERT INTO {SCHEDULES_TABLE} VALUES ({}, {start}, {}, {})",
                    quoted(&job),
                    interval.as_deref().map_or_else(|| "NULL".to_owned(), quoted),
                    end.map_or_else(|| "NULL".to_owned(), |end| end.to_string()),
                ))?;
            }
            "drop_program" | "drop_schedule" => {
                let (table, column) = if call.member == "drop_program" {
                    (PROGRAMS_TABLE, "program_name")
                } else {
                    (SCHEDULES_TABLE, "schedule_name")
                };
                let removed = self.scheduler_sql(&format!(
                    "DELETE FROM {table} WHERE {column} = {} RETURNING 1",
                    quoted(&job)
                ))?;
                if removed.rows.is_empty() {
                    return Err(scheduler_error(27_476, format!("\"{job}\" does not exist")));
                }
            }
            "create_job" => {
                if self.job_exists(&job)? {
                    return Err(scheduler_error(27_477, format!("\"{job}\" already exists")));
                }
                let program = match text_of("program_name") {
                    Some(program) => {
                        let program = program.trim().to_ascii_uppercase();
                        let found = self.scheduler_sql(&format!(
                            "SELECT program_type, program_action FROM {PROGRAMS_TABLE} WHERE program_name = {}",
                            quoted(&program)
                        ))?;
                        let Some(row) = found.rows.first() else {
                            return Err(scheduler_error(27_476, format!("\"{program}\" does not exist")));
                        };
                        Some((
                            row.first().and_then(Value::to_text).unwrap_or_default(),
                            row.get(1).and_then(Value::to_text).unwrap_or_default(),
                        ))
                    }
                    None => None,
                };
                let schedule = match text_of("schedule_name") {
                    Some(schedule) => {
                        let schedule = schedule.trim().to_ascii_uppercase();
                        let found = self.scheduler_sql(&format!(
                            "SELECT start_epoch, repeat_interval, end_epoch FROM {SCHEDULES_TABLE} WHERE schedule_name = {}",
                            quoted(&schedule)
                        ))?;
                        let Some(row) = found.rows.first() else {
                            return Err(scheduler_error(27_476, format!("\"{schedule}\" does not exist")));
                        };
                        Some((
                            row.first().and_then(Value::as_integer),
                            row.get(1).and_then(Value::to_text),
                            row.get(2).and_then(Value::as_integer),
                        ))
                    }
                    None => None,
                };
                let job_type = text_of("job_type")
                    .or_else(|| program.as_ref().map(|(kind, _)| kind.clone()))
                    .unwrap_or_else(|| "PLSQL_BLOCK".to_owned())
                    .to_ascii_uppercase();
                if !JOB_TYPES.contains(&job_type.as_str()) {
                    return Err(SqlError::not_supported(&format!("DBMS_SCHEDULER jobs of type {job_type}; EggDB runs PLSQL_BLOCK and STORED_PROCEDURE jobs")));
                }
                let action = text_of("job_action")
                    .or_else(|| program.as_ref().map(|(_, action)| action.clone()))
                    .ok_or_else(|| scheduler_error(27_452, "job_action is required".to_owned()))?;
                let interval = text_of("repeat_interval").or_else(|| schedule.as_ref().and_then(|(_, interval, _)| interval.clone()));
                if let Some(interval) = &interval {
                    validated_interval(interval)?;
                }
                let start = value("start_date")
                    .and_then(|value| epoch_of(&value))
                    .or_else(|| schedule.as_ref().and_then(|(start, _, _)| *start))
                    .unwrap_or_else(now_epoch);
                let end = value("end_date")
                    .and_then(|value| epoch_of(&value))
                    .or_else(|| schedule.as_ref().and_then(|(_, _, end)| *end));
                let enabled = matches!(value("enabled"), Some(Value::Boolean(true)));
                let auto_drop = !matches!(value("auto_drop"), Some(Value::Boolean(false)));
                self.scheduler_sql(&format!(
                    "INSERT INTO {SCHEDULER_TABLE} VALUES ({}, {}, {}, {start}, {}, {}, {enabled}, {auto_drop}, NULL)",
                    quoted(&job),
                    quoted(&job_type),
                    quoted(&action),
                    interval.as_deref().map_or_else(|| "NULL".to_owned(), quoted),
                    end.map_or_else(|| "NULL".to_owned(), |end| end.to_string()),
                ))?;
            }
            "run_job" => {
                let found = self.scheduler_sql(&format!(
                    "SELECT job_type, job_action FROM {SCHEDULER_TABLE} WHERE job_name = {}",
                    quoted(&job)
                ))?;
                let Some(row) = found.rows.first() else {
                    return Err(scheduler_error(27_475, format!("\"{job}\" must be a job")));
                };
                let job_type = row.first().and_then(Value::to_text).unwrap_or_default();
                let action = row.get(1).and_then(Value::to_text).unwrap_or_default();
                let block = crate::jobs::scheduler_block(&job_type, &action);
                let statement = parsed(&block, &self.settings, &RoutineFrame::default(), Dialect::Oracle)?;
                self.execute(statement)?;
                self.scheduler_sql(&format!(
                    "UPDATE {SCHEDULER_TABLE} SET last_run = {} WHERE job_name = {}",
                    now_epoch(),
                    quoted(&job)
                ))?;
            }
            "drop_job" | "enable" | "disable" | "set_attribute" => {
                if !self.job_exists(&job)? {
                    return Err(scheduler_error(27_475, format!("\"{job}\" must be a job")));
                }
                let change = match call.member.as_str() {
                    "drop_job" => format!("DELETE FROM {SCHEDULER_TABLE} WHERE job_name = {}", quoted(&job)),
                    "enable" | "disable" => format!(
                        "UPDATE {SCHEDULER_TABLE} SET enabled = {} WHERE job_name = {}",
                        call.member == "enable",
                        quoted(&job)
                    ),
                    _ => {
                        let attribute = text_of("attribute").unwrap_or_default().to_ascii_lowercase();
                        let new_value = value("value");
                        let assignment = match attribute.as_str() {
                            "repeat_interval" => {
                                let interval = new_value.as_ref().and_then(Value::to_text);
                                if let Some(interval) = &interval {
                                    crate::jobs::CalendarRule::parse(interval).map_err(|message| {
                                        scheduler_error(27_465, format!("invalid value {interval} for attribute REPEAT_INTERVAL: {message}"))
                                    })?;
                                }
                                format!("repeat_interval = {}", interval.as_deref().map_or_else(|| "NULL".to_owned(), quoted))
                            }
                            "job_action" => format!(
                                "job_action = {}",
                                quoted(&new_value.as_ref().and_then(Value::to_text).unwrap_or_default())
                            ),
                            "start_date" => format!(
                                "start_epoch = {}",
                                new_value.as_ref().and_then(epoch_of).unwrap_or_else(now_epoch)
                            ),
                            "end_date" => format!(
                                "end_epoch = {}",
                                new_value.as_ref().and_then(epoch_of).map_or_else(|| "NULL".to_owned(), |end| end.to_string())
                            ),
                            "auto_drop" => format!("auto_drop = {}", matches!(new_value, Some(Value::Boolean(true)))),
                            other => {
                                return Err(SqlError::not_supported(&format!("DBMS_SCHEDULER.SET_ATTRIBUTE of {other}")));
                            }
                        };
                        format!("UPDATE {SCHEDULER_TABLE} SET {assignment} WHERE job_name = {}", quoted(&job))
                    }
                };
                self.scheduler_sql(&change)?;
            }
            _ => {}
        }
        Ok(Some(Flow::Next))
    }
}
