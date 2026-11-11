use super::{QueryResult, Session};
use crate::ast::ProcedureCall;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::session::mysql_events::quoted;
use crate::types::Value;

pub(crate) const JOBS_TABLE: &str = "eggdb_internal.sqlserver_agent_jobs";
pub(crate) const STEPS_TABLE: &str = "eggdb_internal.sqlserver_agent_steps";
pub(crate) const SCHEDULES_TABLE: &str = "eggdb_internal.sqlserver_agent_schedules";
pub(crate) const ATTACHED_TABLE: &str = "eggdb_internal.sqlserver_agent_job_schedules";
pub(crate) const HISTORY_TABLE: &str = "eggdb_internal.sqlserver_agent_history";
const FLOW_TABLE: &str = "eggdb_internal.sqlserver_agent_step_flow";
const QUIT_WITH_SUCCESS: i64 = 1;
const QUIT_WITH_FAILURE: i64 = 2;
const NEXT_STEP: i64 = 3;
const GO_TO_STEP: i64 = 4;
const RETRY_STATUS: i64 = 2;
const MOST_STEPS_RUN: usize = 1_000;
const CREATE_TABLES: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_jobs (job_name text PRIMARY KEY, enabled boolean NOT NULL, description text); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_steps (job_name text NOT NULL, step_id integer NOT NULL, step_name text NOT NULL, database_name text, command text NOT NULL, PRIMARY KEY (job_name, step_id)); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_schedules (schedule_name text PRIMARY KEY, enabled boolean NOT NULL, freq_type integer NOT NULL, freq_interval integer NOT NULL, freq_subday_type integer NOT NULL, freq_subday_interval integer NOT NULL, freq_recurrence_factor integer NOT NULL, active_start_date integer NOT NULL, active_end_date integer NOT NULL, active_start_time integer NOT NULL, active_end_time integer NOT NULL); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_job_schedules (job_name text NOT NULL, schedule_name text NOT NULL, PRIMARY KEY (job_name, schedule_name)); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_history (job_name text NOT NULL, step_id integer NOT NULL, run_status integer NOT NULL, message text, run_time timestamptz NOT NULL DEFAULT now()); \
    CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_agent_step_flow (job_name text NOT NULL, step_id integer NOT NULL, on_success_action integer NOT NULL, on_success_step_id integer NOT NULL, on_fail_action integer NOT NULL, on_fail_step_id integer NOT NULL, retry_attempts integer NOT NULL, retry_interval integer NOT NULL, PRIMARY KEY (job_name, step_id)); \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysjobs AS SELECT md5(job_name)::uuid AS job_id, job_name AS name, CASE WHEN enabled THEN 1 ELSE 0 END AS enabled, description, 1 AS start_step_id, 0 AS category_id, 0 AS notify_level_eventlog FROM eggdb_internal.sqlserver_agent_jobs; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysjobsteps AS SELECT md5(s.job_name)::uuid AS job_id, s.step_id, s.step_name, 'TSQL' AS subsystem, s.command, s.database_name, coalesce(f.on_success_action, 3) AS on_success_action, coalesce(f.on_success_step_id, 0) AS on_success_step_id, coalesce(f.on_fail_action, 2) AS on_fail_action, coalesce(f.on_fail_step_id, 0) AS on_fail_step_id, coalesce(f.retry_attempts, 0) AS retry_attempts, coalesce(f.retry_interval, 0) AS retry_interval FROM eggdb_internal.sqlserver_agent_steps s LEFT JOIN eggdb_internal.sqlserver_agent_step_flow f ON f.job_name = s.job_name AND f.step_id = s.step_id; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysschedules AS SELECT (row_number() OVER (ORDER BY schedule_name))::integer AS schedule_id, md5(schedule_name)::uuid AS schedule_uid, schedule_name AS name, CASE WHEN enabled THEN 1 ELSE 0 END AS enabled, freq_type, freq_interval, freq_subday_type, freq_subday_interval, freq_recurrence_factor, active_start_date, active_end_date, active_start_time, active_end_time FROM eggdb_internal.sqlserver_agent_schedules; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysjobschedules AS SELECT s.schedule_id, md5(a.job_name)::uuid AS job_id, 0 AS next_run_date, 0 AS next_run_time FROM eggdb_internal.sqlserver_agent_job_schedules a JOIN eggdb_internal.sqlserver_agent_sysschedules s ON s.name = a.schedule_name; \
    CREATE OR REPLACE VIEW eggdb_internal.sqlserver_agent_sysjobhistory AS SELECT (row_number() OVER (ORDER BY h.run_time, h.job_name, h.step_id, h.run_status DESC))::integer AS instance_id, md5(h.job_name)::uuid AS job_id, h.step_id, coalesce(st.step_name, '(Job outcome)') AS step_name, h.message, h.run_status, to_char(h.run_time, 'YYYYMMDD')::integer AS run_date, to_char(h.run_time, 'HH24MISS')::integer AS run_time, 0 AS run_duration, 0 AS sql_message_id, 0 AS sql_severity FROM eggdb_internal.sqlserver_agent_history h LEFT JOIN eggdb_internal.sqlserver_agent_steps st ON st.job_name = h.job_name AND st.step_id = h.step_id";

/// The `msdb.dbo` views SQL Server Agent's tools read, each kept as a view over the Agent tables.
pub(crate) const AGENT_VIEWS: [&str; 5] = ["sysjobs", "sysjobsteps", "sysschedules", "sysjobschedules", "sysjobhistory"];
const MAX_MESSAGE_CHARS: usize = 1_000;
const SUPPORTED_FREQUENCIES: [i64; 4] = [1, 4, 8, 16];

/// The SQL Server Agent procedures of `msdb` that EggDB answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentProcedure {
    AddJob,
    UpdateJob,
    DeleteJob,
    AddJobStep,
    AddSchedule,
    AttachSchedule,
    DetachSchedule,
    DeleteSchedule,
    AddJobSchedule,
    AddJobServer,
    StartJob,
}

impl AgentProcedure {
    pub(crate) fn named(name: &str) -> Option<Self> {
        let base = name.rsplit('.').next().unwrap_or(name).trim_matches(['[', ']']).to_ascii_lowercase();
        Some(match base.as_str() {
            "sp_add_job" => Self::AddJob,
            "sp_update_job" => Self::UpdateJob,
            "sp_delete_job" => Self::DeleteJob,
            "sp_add_jobstep" => Self::AddJobStep,
            "sp_add_schedule" => Self::AddSchedule,
            "sp_attach_schedule" => Self::AttachSchedule,
            "sp_detach_schedule" => Self::DetachSchedule,
            "sp_delete_schedule" => Self::DeleteSchedule,
            "sp_add_jobschedule" => Self::AddJobSchedule,
            "sp_add_jobserver" => Self::AddJobServer,
            "sp_start_job" => Self::StartJob,
            _ => return None,
        })
    }

    fn parameters(self) -> &'static [&'static str] {
        const SCHEDULE: [&str; 11] = [
            "@enabled",
            "@freq_type",
            "@freq_interval",
            "@freq_subday_type",
            "@freq_subday_interval",
            "@freq_relative_interval",
            "@freq_recurrence_factor",
            "@active_start_date",
            "@active_end_date",
            "@active_start_time",
            "@active_end_time",
        ];
        match self {
            Self::AddJob => &["@job_name", "@enabled", "@description"],
            Self::UpdateJob => &["@job_id", "@job_name", "@new_name", "@enabled", "@description"],
            Self::DeleteJob => &["@job_id", "@job_name"],
            Self::AddJobStep => &[
                "@job_id",
                "@job_name",
                "@step_id",
                "@step_name",
                "@subsystem",
                "@command",
                "@database_name",
                "@on_success_action",
                "@on_success_step_id",
                "@on_fail_action",
                "@on_fail_step_id",
                "@retry_attempts",
                "@retry_interval",
            ],
            Self::AddSchedule => &[
                "@schedule_name",
                SCHEDULE[0],
                SCHEDULE[1],
                SCHEDULE[2],
                SCHEDULE[3],
                SCHEDULE[4],
                SCHEDULE[5],
                SCHEDULE[6],
                SCHEDULE[7],
                SCHEDULE[8],
                SCHEDULE[9],
                SCHEDULE[10],
            ],
            Self::AttachSchedule | Self::DetachSchedule => &["@job_id", "@job_name", "@schedule_id", "@schedule_name"],
            Self::DeleteSchedule => &["@schedule_id", "@schedule_name"],
            Self::AddJobSchedule => &[
                "@job_id",
                "@job_name",
                "@name",
                SCHEDULE[0],
                SCHEDULE[1],
                SCHEDULE[2],
                SCHEDULE[3],
                SCHEDULE[4],
                SCHEDULE[5],
                SCHEDULE[6],
                SCHEDULE[7],
                SCHEDULE[8],
                SCHEDULE[9],
                SCHEDULE[10],
            ],
            Self::AddJobServer => &["@job_id", "@job_name", "@server_name"],
            Self::StartJob => &["@job_name", "@job_id", "@server_name", "@step_name"],
        }
    }
}

struct Arguments(Vec<(String, Value)>);

impl Arguments {
    fn value(&self, name: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value)
            .filter(|value| **value != Value::Null)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.value(name).and_then(Value::to_text)
    }

    fn required(&self, name: &str, procedure: &str) -> SqlResult<String> {
        self.text(name).ok_or_else(|| {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("Procedure or function '{procedure}' expects parameter '{name}', which was not supplied."),
            )
            .with_sqlserver_code(201)
        })
    }

    fn integer(&self, name: &str, default: i64) -> i64 {
        self.value(name).and_then(Value::as_integer).unwrap_or(default)
    }
}

fn agent_error(code: u16, message: String) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message).with_sqlserver_code(code)
}

fn today_date() -> i64 {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64 / 86_400);
    let (year, month, day) = crate::jobs::civil_from_days(days);
    year * 10_000 + i64::from(month) * 100 + i64::from(day)
}

impl Session<'_> {
    fn agent_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn agent_rows(&mut self, text: &str) -> SqlResult<Vec<Vec<Value>>> {
        Ok(self
            .agent_sql(text)?
            .into_iter()
            .rev()
            .find(|result| result.columns.is_some())
            .map(|result| result.rows)
            .unwrap_or_default())
    }

    fn agent_job_exists(&mut self, job: &str) -> SqlResult<bool> {
        Ok(!self
            .agent_rows(&format!("SELECT 1 FROM {JOBS_TABLE} WHERE job_name = {}", quoted(job)))?
            .is_empty())
    }

    fn agent_schedule_exists(&mut self, schedule: &str) -> SqlResult<bool> {
        Ok(!self
            .agent_rows(&format!(
                "SELECT 1 FROM {SCHEDULES_TABLE} WHERE schedule_name = {}",
                quoted(schedule)
            ))?
            .is_empty())
    }

    fn missing_job(job: &str) -> SqlError {
        agent_error(14262, format!("The specified @job_name ('{job}') does not exist."))
    }

    fn add_agent_schedule(&mut self, name: &str, arguments: &Arguments) -> SqlResult<()> {
        if self.agent_schedule_exists(name)? {
            return Err(agent_error(14261, format!("The specified @schedule_name ('{name}') already exists.")));
        }
        let frequency = arguments.integer("@freq_type", 0);
        if !SUPPORTED_FREQUENCIES.contains(&frequency) {
            return Err(SqlError::not_supported(&format!(
                "@freq_type {frequency}: EggDB runs once (1), daily (4), weekly (8) and monthly (16) schedules"
            ))
            .with_sqlserver_code(14266));
        }
        let subday = arguments.integer("@freq_subday_type", 1).max(1);
        if subday == 2 {
            return Err(SqlError::not_supported("schedules every few seconds (@freq_subday_type 2)").with_sqlserver_code(14266));
        }
        self.agent_sql(&format!(
            "INSERT INTO {SCHEDULES_TABLE} VALUES ({}, {}, {frequency}, {}, {subday}, {}, {}, {}, {}, {}, {})",
            quoted(name),
            arguments.integer("@enabled", 1) != 0,
            arguments.integer("@freq_interval", 1),
            arguments.integer("@freq_subday_interval", 0),
            arguments.integer("@freq_recurrence_factor", 1).max(1),
            arguments.integer("@active_start_date", today_date()),
            arguments.integer("@active_end_date", 99_991_231),
            arguments.integer("@active_start_time", 0),
            arguments.integer("@active_end_time", 235_959),
        ))?;
        Ok(())
    }

    fn attach_agent_schedule(&mut self, job: &str, schedule: &str) -> SqlResult<()> {
        if !self.agent_job_exists(job)? {
            return Err(Self::missing_job(job));
        }
        if !self.agent_schedule_exists(schedule)? {
            return Err(agent_error(14262, format!("The specified @schedule_name ('{schedule}') does not exist.")));
        }
        self.agent_sql(&format!(
            "INSERT INTO {ATTACHED_TABLE} VALUES ({}, {}) ON CONFLICT DO NOTHING",
            quoted(job),
            quoted(schedule)
        ))?;
        Ok(())
    }

    pub(super) fn agent_procedure(&mut self, procedure: AgentProcedure, call: &ProcedureCall) -> SqlResult<Vec<QueryResult>> {
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        let names = procedure.parameters();
        let mut bound = Vec::new();
        for (index, argument) in call.arguments.iter().enumerate() {
            let name = match &argument.name {
                Some(name) => {
                    let name = name.to_ascii_lowercase();
                    let name = if name.starts_with('@') { name } else { format!("@{name}") };
                    if !names.contains(&name.as_str()) {
                        return Err(agent_error(
                            8145,
                            format!("{name} is not a parameter for procedure {}.", call.procedure.text),
                        ));
                    }
                    name
                }
                None => names
                    .get(index)
                    .map(|name| (*name).to_owned())
                    .ok_or_else(|| agent_error(8144, format!("Procedure or function {} has too many arguments specified.", call.procedure.text)))?,
            };
            let value = match &argument.value {
                Some(expr) => evaluator.eval(expr, None)?,
                None => Value::Null,
            };
            bound.push((name, value));
        }
        let arguments = Arguments(bound);
        if arguments.value("@job_id").is_some() || arguments.value("@schedule_id").is_some() {
            return Err(SqlError::not_supported("SQL Server Agent jobs and schedules named by id; name them with @job_name and @schedule_name"));
        }
        let procedure_name = call.procedure.text.clone();
        self.agent_sql(CREATE_TABLES)?;
        match procedure {
            AgentProcedure::AddJob => {
                let job = arguments.required("@job_name", &procedure_name)?;
                if self.agent_job_exists(&job)? {
                    return Err(agent_error(14261, format!("The specified @job_name ('{job}') already exists.")));
                }
                self.agent_sql(&format!(
                    "INSERT INTO {JOBS_TABLE} VALUES ({}, {}, {})",
                    quoted(&job),
                    arguments.integer("@enabled", 1) != 0,
                    arguments.text("@description").map_or_else(|| "NULL".to_owned(), |text| quoted(&text))
                ))?;
            }
            AgentProcedure::UpdateJob => {
                let job = arguments.required("@job_name", &procedure_name)?;
                if !self.agent_job_exists(&job)? {
                    return Err(Self::missing_job(&job));
                }
                if let Some(enabled) = arguments.value("@enabled").and_then(Value::as_integer) {
                    self.agent_sql(&format!(
                        "UPDATE {JOBS_TABLE} SET enabled = {} WHERE job_name = {}",
                        enabled != 0,
                        quoted(&job)
                    ))?;
                }
                if let Some(new_name) = arguments.text("@new_name") {
                    for table in [JOBS_TABLE, STEPS_TABLE, ATTACHED_TABLE, HISTORY_TABLE] {
                        self.agent_sql(&format!(
                            "UPDATE {table} SET job_name = {} WHERE job_name = {}",
                            quoted(&new_name),
                            quoted(&job)
                        ))?;
                    }
                }
            }
            AgentProcedure::DeleteJob => {
                let job = arguments.required("@job_name", &procedure_name)?;
                if !self.agent_job_exists(&job)? {
                    return Err(Self::missing_job(&job));
                }
                for table in [JOBS_TABLE, STEPS_TABLE, ATTACHED_TABLE] {
                    self.agent_sql(&format!("DELETE FROM {table} WHERE job_name = {}", quoted(&job)))?;
                }
            }
            AgentProcedure::AddJobStep => {
                let job = arguments.required("@job_name", &procedure_name)?;
                if !self.agent_job_exists(&job)? {
                    return Err(Self::missing_job(&job));
                }
                let subsystem = arguments.text("@subsystem").unwrap_or_else(|| "TSQL".to_owned());
                if !subsystem.eq_ignore_ascii_case("tsql") {
                    return Err(SqlError::not_supported(&format!(
                        "job steps of the {subsystem} subsystem; EggDB runs TSQL steps"
                    )));
                }
                let command = arguments.required("@command", &procedure_name)?;
                let next = self
                    .agent_rows(&format!(
                        "SELECT coalesce(max(step_id), 0) + 1 FROM {STEPS_TABLE} WHERE job_name = {}",
                        quoted(&job)
                    ))?
                    .first()
                    .and_then(|row| row.first())
                    .and_then(Value::as_integer)
                    .unwrap_or(1);
                let step_id = arguments.integer("@step_id", next);
                let step_name = arguments.text("@step_name").unwrap_or_else(|| format!("step {step_id}"));
                let database = arguments.text("@database_name").unwrap_or_else(|| self.settings.database.clone());
                self.agent_sql(&format!(
                    "INSERT INTO {STEPS_TABLE} VALUES ({}, {step_id}, {}, {}, {})",
                    quoted(&job),
                    quoted(&step_name),
                    quoted(&database),
                    quoted(&command)
                ))?;
                let flow = [
                    arguments.integer("@on_success_action", NEXT_STEP),
                    arguments.integer("@on_success_step_id", 0),
                    arguments.integer("@on_fail_action", QUIT_WITH_FAILURE),
                    arguments.integer("@on_fail_step_id", 0),
                    arguments.integer("@retry_attempts", 0),
                    arguments.integer("@retry_interval", 0),
                ];
                if !(QUIT_WITH_SUCCESS..=GO_TO_STEP).contains(&flow[0])
                    || !(QUIT_WITH_SUCCESS..=GO_TO_STEP).contains(&flow[2])
                {
                    return Err(agent_error(14266, "The specified @on_success_action or @on_fail_action is invalid (valid values are: 1, 2, 3, 4).".to_owned()));
                }
                if flow[4] < 0 || flow[5] < 0 {
                    return Err(agent_error(14266, "The specified @retry_attempts or @retry_interval is invalid.".to_owned()));
                }
                self.agent_sql(&format!(
                    "INSERT INTO {FLOW_TABLE} VALUES ({}, {step_id}, {}, {}, {}, {}, {}, {})",
                    quoted(&job),
                    flow[0],
                    flow[1],
                    flow[2],
                    flow[3],
                    flow[4],
                    flow[5]
                ))?;
            }
            AgentProcedure::AddSchedule => {
                let name = arguments.required("@schedule_name", &procedure_name)?;
                self.add_agent_schedule(&name, &arguments)?;
            }
            AgentProcedure::AttachSchedule => {
                let job = arguments.required("@job_name", &procedure_name)?;
                let schedule = arguments.required("@schedule_name", &procedure_name)?;
                self.attach_agent_schedule(&job, &schedule)?;
            }
            AgentProcedure::DetachSchedule => {
                let job = arguments.required("@job_name", &procedure_name)?;
                let schedule = arguments.required("@schedule_name", &procedure_name)?;
                self.agent_sql(&format!(
                    "DELETE FROM {ATTACHED_TABLE} WHERE job_name = {} AND schedule_name = {}",
                    quoted(&job),
                    quoted(&schedule)
                ))?;
            }
            AgentProcedure::DeleteSchedule => {
                let schedule = arguments.required("@schedule_name", &procedure_name)?;
                if !self.agent_schedule_exists(&schedule)? {
                    return Err(agent_error(14262, format!("The specified @schedule_name ('{schedule}') does not exist.")));
                }
                for table in [SCHEDULES_TABLE, ATTACHED_TABLE] {
                    self.agent_sql(&format!("DELETE FROM {table} WHERE schedule_name = {}", quoted(&schedule)))?;
                }
            }
            AgentProcedure::AddJobSchedule => {
                let job = arguments.required("@job_name", &procedure_name)?;
                let name = arguments.required("@name", &procedure_name)?;
                if !self.agent_job_exists(&job)? {
                    return Err(Self::missing_job(&job));
                }
                self.add_agent_schedule(&name, &arguments)?;
                self.attach_agent_schedule(&job, &name)?;
            }
            AgentProcedure::AddJobServer => {}
            AgentProcedure::StartJob => {
                let job = arguments.required("@job_name", &procedure_name)?;
                if !self.agent_job_exists(&job)? {
                    return Err(Self::missing_job(&job));
                }
                self.run_agent_job(&job)?;
                let mut result = QueryResult::command("EXECUTE");
                let text = format!("Job '{job}' started successfully.");
                result.notices.push(text.clone());
                result.info_messages.push(crate::session::InfoMessage {
                    number: 0,
                    severity: 0,
                    state: 1,
                    text,
                });
                return Ok(vec![result]);
            }
        }
        Ok(Vec::new())
    }

    /// Runs a SQL Server Agent job's steps in order, each in its database, stopping at the
    /// first that fails, and records each step's outcome in the job history. Returns whether
    /// every step succeeded.
    pub(crate) fn run_agent_job(&mut self, job: &str) -> SqlResult<bool> {
        self.agent_sql(CREATE_TABLES)?;
        let steps = self.agent_rows(&format!(
            "SELECT s.step_id, s.database_name, s.command, coalesce(f.on_success_action, {NEXT_STEP}), \
             coalesce(f.on_success_step_id, 0), coalesce(f.on_fail_action, {QUIT_WITH_FAILURE}), \
             coalesce(f.on_fail_step_id, 0), coalesce(f.retry_attempts, 0) \
             FROM {STEPS_TABLE} s LEFT JOIN {FLOW_TABLE} f ON f.job_name = s.job_name AND f.step_id = s.step_id \
             WHERE s.job_name = {} ORDER BY s.step_id",
            quoted(job)
        ))?;
        let number = |step: &[Value], at: usize| step.get(at).and_then(Value::as_integer).unwrap_or(0);
        let mut at = 0usize;
        let mut succeeded = true;
        let mut runs = 0usize;
        while let Some(step) = steps.get(at) {
            runs += 1;
            if runs > MOST_STEPS_RUN {
                return Err(agent_error(14266, format!("job '{job}' ran more than {MOST_STEPS_RUN} steps; its step flow loops")));
            }
            let mut attempts_left = number(step, 7).max(0);
            let ran = loop {
                let ran = self.run_agent_step(job, step)?;
                if ran || attempts_left == 0 {
                    break ran;
                }
                attempts_left -= 1;
                self.agent_sql(&format!(
                    "INSERT INTO {HISTORY_TABLE} (job_name, step_id, run_status, message) VALUES ({}, {}, {RETRY_STATUS}, 'The step is being retried.')",
                    quoted(job),
                    number(step, 0)
                ))?;
            };
            succeeded = ran;
            let (action, target) = if ran { (number(step, 3), number(step, 4)) } else { (number(step, 5), number(step, 6)) };
            match action {
                QUIT_WITH_SUCCESS => return Ok(true),
                QUIT_WITH_FAILURE => return Ok(false),
                GO_TO_STEP => match steps.iter().position(|other| number(other, 0) == target) {
                    Some(found) => at = found,
                    None => return Ok(succeeded),
                },
                _ => at += 1,
            }
        }
        Ok(succeeded)
    }

    fn run_agent_step(&mut self, job: &str, step: &[Value]) -> SqlResult<bool> {
        let home = self.settings.database.clone();
        let dialect = self.dialect;
        {
            let step_id = step.first().and_then(Value::as_integer).unwrap_or(0);
            let database = step.get(1).and_then(Value::to_text).unwrap_or_else(|| home.clone());
            let command = step.get(2).and_then(Value::to_text).unwrap_or_default();
            self.dialect = Dialect::SqlServer;
            let switched = if database.eq_ignore_ascii_case(&self.settings.database) {
                Ok(Vec::new())
            } else {
                self.run_quietly(&format!("USE [{}]", database.replace(']', "]]")))
            };
            let ran = switched.and_then(|_| self.run_quietly(&command));
            if !self.settings.database.eq_ignore_ascii_case(&home) {
                let _ = self.run_quietly(&format!("USE [{}]", home.replace(']', "]]")));
            }
            self.dialect = dialect;
            let (status, message) = match &ran {
                Ok(_) => (1, "The step succeeded.".to_owned()),
                Err(error) => (0, error.message().chars().take(MAX_MESSAGE_CHARS).collect::<String>()),
            };
            self.agent_sql(&format!(
                "INSERT INTO {HISTORY_TABLE} (job_name, step_id, run_status, message) VALUES ({}, {step_id}, {status}, {})",
                quoted(job),
                quoted(&message)
            ))?;
            Ok(ran.is_ok())
        }
    }
}

/// Whether a SQL Server Agent schedule fires in the minute starting `epoch_seconds` (UTC).
pub(crate) fn schedule_fires(schedule: &[Value], epoch_seconds: i64) -> bool {
    let number = |at: usize| schedule.get(at).and_then(Value::as_integer).unwrap_or(0);
    let [frequency, interval, subday, subday_interval, recurrence, start_date, end_date, start_time, end_time] =
        [0, 1, 2, 3, 4, 5, 6, 7, 8].map(number);
    let days = epoch_seconds.div_euclid(86_400);
    let (year, month, day) = crate::jobs::civil_from_days(days);
    let date = year * 10_000 + i64::from(month) * 100 + i64::from(day);
    if date < start_date || date > end_date {
        return false;
    }
    let seconds = epoch_seconds.rem_euclid(86_400);
    let time = (seconds / 3600) * 10_000 + (seconds / 60 % 60) * 100;
    let start_days = {
        let (y, m, d) = (start_date / 10_000, start_date / 100 % 100, start_date % 100);
        crate::jobs::days_from_civil(y, m, d)
    };
    let day_matches = match frequency {
        1 => date == start_date,
        4 => (days - start_days).rem_euclid(interval.max(1)) == 0,
        8 => {
            let weekday = (days + 4).rem_euclid(7);
            let week = (days - start_days).div_euclid(7);
            interval & (1 << weekday) != 0 && week.rem_euclid(recurrence.max(1)) == 0
        }
        16 => i64::from(day) == interval,
        _ => false,
    };
    if !day_matches || time < start_time / 100 * 100 || time > end_time {
        return false;
    }
    let minutes_since_start = {
        let start = start_time / 10_000 * 60 + start_time / 100 % 100;
        seconds / 60 - start
    };
    match subday {
        4 => minutes_since_start.rem_euclid(subday_interval.max(1)) == 0,
        8 => minutes_since_start.rem_euclid(subday_interval.max(1) * 60) == 0,
        _ => minutes_since_start == 0,
    }
}
