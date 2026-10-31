//! Scheduled jobs (D200): cron schedules as pg_cron reads them, the `cron` schema that
//! `CREATE EXTENSION pg_cron` installs, and running the jobs due in a minute, which the server
//! does once a minute.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::session::Session;
use crate::store::KeyValueStore;
use crate::types::Value;

const FIELD_RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MAX_MESSAGE_CHARS: usize = 1_000;

/// The statements `CREATE EXTENSION pg_cron` runs: the `cron` schema with pg_cron's `job` and
/// `job_run_details` tables and its `schedule` and `unschedule` functions.
pub(crate) const PG_CRON_SCRIPT: &str = r"
CREATE SCHEMA cron;
CREATE TABLE cron.job (
    jobid bigserial PRIMARY KEY,
    schedule text NOT NULL,
    command text NOT NULL,
    nodename text NOT NULL DEFAULT 'localhost',
    nodeport integer NOT NULL DEFAULT 5432,
    database text NOT NULL DEFAULT current_database(),
    username text NOT NULL DEFAULT current_user,
    active boolean NOT NULL DEFAULT true,
    jobname text UNIQUE
);
CREATE TABLE cron.job_run_details (
    jobid bigint,
    runid bigserial PRIMARY KEY,
    job_pid integer,
    database text,
    username text,
    command text,
    status text,
    return_message text,
    start_time timestamptz,
    end_time timestamptz
);
CREATE FUNCTION cron.schedule(job_schedule text, job_command text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    id bigint;
BEGIN
    INSERT INTO cron.job (schedule, command) VALUES (job_schedule, job_command) RETURNING jobid INTO id;
    RETURN id;
END
$$;
CREATE FUNCTION cron.schedule(job_name text, job_schedule text, job_command text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    id bigint;
BEGIN
    UPDATE cron.job SET schedule = job_schedule, command = job_command, active = true
        WHERE jobname = job_name RETURNING jobid INTO id;
    IF id IS NULL THEN
        INSERT INTO cron.job (schedule, command, jobname) VALUES (job_schedule, job_command, job_name) RETURNING jobid INTO id;
    END IF;
    RETURN id;
END
$$;
CREATE FUNCTION cron.unschedule(job_id bigint) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM cron.job WHERE jobid = job_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'could not find valid entry for job %', job_id;
    END IF;
    RETURN true;
END
$$;
CREATE FUNCTION cron.unschedule(job_name text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM cron.job WHERE jobname = job_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'could not find valid entry for job ''%''', job_name;
    END IF;
    RETURN true;
END
$$;
";

/// A cron schedule: the minutes, hours, days of the month, months and days of the week it
/// fires in (UTC), as standard cron and pg_cron read its five fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    fields: [u64; 5],
    any_day_of_month: bool,
    any_day_of_week: bool,
}

/// A calendar minute in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minute {
    /// 0 to 59.
    pub minute: u32,
    /// 0 to 23.
    pub hour: u32,
    /// 1 to 31.
    pub day: u32,
    /// 1 to 12.
    pub month: u32,
    /// 0 (Sunday) to 6.
    pub weekday: u32,
}

impl Minute {
    /// The minute holding `epoch_seconds` (seconds since 1970-01-01 UTC).
    pub fn of_epoch(epoch_seconds: i64) -> Self {
        let days = epoch_seconds.div_euclid(86_400);
        let seconds = epoch_seconds.rem_euclid(86_400);
        let (_, month, day) = civil_from_days(days);
        Self {
            minute: ((seconds / 60) % 60) as u32,
            hour: (seconds / 3_600) as u32,
            day,
            month,
            weekday: (days + 4).rem_euclid(7) as u32,
        }
    }
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn invalid(schedule: &str) -> SqlError {
    SqlError::new(crate::error::SqlState::INVALID_PARAMETER_VALUE, format!("invalid schedule: {schedule}"))
}

fn value_of(text: &str, field: usize) -> Option<u32> {
    if let Ok(number) = text.parse::<u32>() {
        return Some(number);
    }
    let lower = text.to_ascii_lowercase();
    match field {
        3 => MONTH_NAMES.iter().position(|name| *name == lower).map(|at| at as u32 + 1),
        4 => DAY_NAMES.iter().position(|name| *name == lower).map(|at| at as u32),
        _ => None,
    }
}

fn parse_field(text: &str, field: usize) -> Option<u64> {
    let (low, high) = FIELD_RANGES[field];
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok().filter(|step| *step > 0)?),
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((from, to)) = range.split_once('-') {
            (value_of(from, field)?, value_of(to, field)?)
        } else {
            let single = value_of(range, field)?;
            (single, if part.contains('/') { high } else { single })
        };
        if start < low || end > high || start > end {
            return None;
        }
        let mut value = start;
        while value <= end {
            bits |= 1 << value;
            value += step;
        }
    }
    if field == 4 && bits & (1 << 7) != 0 {
        bits = (bits | 1) & !(1 << 7);
    }
    Some(bits)
}

impl CronSchedule {
    /// Reads a schedule: five fields (`*`, numbers, `a-b` ranges, `,` lists, `/n` steps, and
    /// month and weekday names), or `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`,
    /// `@midnight` or `@hourly`.
    pub fn parse(schedule: &str) -> SqlResult<Self> {
        let expanded = match schedule.trim() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let parts: Vec<&str> = expanded.split_ascii_whitespace().collect();
        let [minute, hour, day, month, weekday] = parts.as_slice() else {
            return Err(invalid(schedule));
        };
        let mut fields = [0u64; 5];
        for (at, text) in [minute, hour, day, month, weekday].into_iter().enumerate() {
            fields[at] = parse_field(text, at).ok_or_else(|| invalid(schedule))?;
        }
        Ok(Self { fields, any_day_of_month: *day == "*", any_day_of_week: *weekday == "*" })
    }

    /// Whether the schedule fires in `minute`. When both the day of the month and the day of
    /// the week are restricted, either one matching is enough, as cron has always done.
    pub fn fires_in(&self, minute: Minute) -> bool {
        let has = |field: usize, value: u32| self.fields[field] & (1 << value) != 0;
        let day_of_month = has(2, minute.day);
        let day_of_week = has(4, minute.weekday);
        let day = match (self.any_day_of_month, self.any_day_of_week) {
            (false, false) => day_of_month || day_of_week,
            _ => day_of_month && day_of_week,
        };
        has(0, minute.minute) && has(1, minute.hour) && has(3, minute.month) && day
    }
}

/// What running the due jobs did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobsRun {
    /// Jobs that ran and succeeded.
    pub succeeded: u64,
    /// Jobs that ran and failed.
    pub failed: u64,
    /// Jobs whose schedule does not read.
    pub invalid_schedules: u64,
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Runs, in the PostgreSQL dialect, every active job of `cron.job` whose schedule fires in the
/// minute holding `epoch_seconds`, each in its own transaction, and records each run in
/// `cron.job_run_details`. A file without the `cron` schema has no jobs.
pub fn run_due_jobs<S: KeyValueStore>(store: &S, epoch_seconds: i64) -> SqlResult<JobsRun> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute("SELECT jobid, schedule, command FROM cron.job WHERE active ORDER BY jobid", &mut |result| {
        rows = result.rows;
    });
    if let Err(error) = listed {
        return if error.state() == crate::error::SqlState::UNDEFINED_TABLE || error.state() == crate::error::SqlState::INVALID_SCHEMA_NAME {
            Ok(JobsRun::default())
        } else {
            Err(error)
        };
    }
    let minute = Minute::of_epoch(epoch_seconds);
    let mut outcome = JobsRun::default();
    for row in rows {
        let (Some(Value::BigInt(id)), Some(schedule), Some(command)) =
            (row.first().cloned(), row.get(1).and_then(text_of), row.get(2).and_then(text_of))
        else {
            continue;
        };
        let Ok(parsed) = CronSchedule::parse(&schedule) else {
            outcome.invalid_schedules += 1;
            continue;
        };
        if !parsed.fires_in(minute) {
            continue;
        }
        let mut job = Session::with_dialect(store, Dialect::PostgreSql);
        let mut tag = String::new();
        let ran = job.execute(&command, &mut |result| tag = result.tag);
        let _ = job.close();
        let (status, message) = match ran {
            Ok(()) => {
                outcome.succeeded += 1;
                ("succeeded", tag)
            }
            Err(error) => {
                outcome.failed += 1;
                ("failed", error.message().chars().take(MAX_MESSAGE_CHARS).collect())
            }
        };
        session.execute(
            &format!(
                "INSERT INTO cron.job_run_details (jobid, database, username, command, status, return_message, start_time, end_time) \
                 VALUES ({id}, current_database(), current_user, {}, '{status}', {}, to_timestamp({epoch_seconds}), now())",
                quoted(&command),
                quoted(&message)
            ),
            &mut |_| {},
        )?;
    }
    session.close()?;
    Ok(outcome)
}

#[cfg(test)]
mod tests;
