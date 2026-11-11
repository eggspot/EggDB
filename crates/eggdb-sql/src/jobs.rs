//! Scheduled jobs (D200): cron schedules as pg_cron reads them, the `cron` schema that
//! `CREATE EXTENSION pg_cron` installs, and running the jobs due in a minute, which the server
//! does once a minute.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::session::Session;
use crate::store::KeyValueStore;
use crate::types::Value;

const FIELD_RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
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
    jobname text UNIQUE,
    timezone text
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
CREATE FUNCTION cron.alter_job(job_id bigint, schedule text DEFAULT NULL, command text DEFAULT NULL, database text DEFAULT NULL, username text DEFAULT NULL, active boolean DEFAULT NULL, timezone text DEFAULT NULL) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    UPDATE cron.job SET
        schedule = coalesce(alter_job.schedule, cron.job.schedule),
        command = coalesce(alter_job.command, cron.job.command),
        database = coalesce(alter_job.database, cron.job.database),
        username = coalesce(alter_job.username, cron.job.username),
        active = coalesce(alter_job.active, cron.job.active),
        timezone = coalesce(alter_job.timezone, cron.job.timezone)
        WHERE jobid = job_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'Job % does not exist', job_id;
    END IF;
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
    last_day_of_month: bool,
    every_seconds: Option<u32>,
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
    /// Whether the day is its month's last.
    pub last_day_of_month: bool,
}

impl Minute {
    /// The minute holding `epoch_seconds` (seconds since 1970-01-01 UTC).
    pub fn of_epoch(epoch_seconds: i64) -> Self {
        let days = epoch_seconds.div_euclid(86_400);
        let seconds = epoch_seconds.rem_euclid(86_400);
        let (_, month, day) = civil_from_days(days);
        let (_, next_month, _) = civil_from_days(days + 1);
        Self {
            minute: ((seconds / 60) % 60) as u32,
            hour: (seconds / 3_600) as u32,
            day,
            month,
            weekday: (days + 4).rem_euclid(7) as u32,
            last_day_of_month: next_month != month,
        }
    }
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn every_seconds(schedule: &str) -> Option<u32> {
    let mut words = schedule.split_ascii_whitespace();
    let count = words.next()?.parse::<u32>().ok()?;
    let unit = words.next()?.to_ascii_lowercase();
    if words.next().is_some() || !matches!(unit.as_str(), "second" | "seconds") {
        return None;
    }
    (1..=59).contains(&count).then_some(count)
}

fn invalid(schedule: &str) -> SqlError {
    SqlError::new(
        crate::error::SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid schedule: {schedule}"),
    )
}

fn value_of(text: &str, field: usize) -> Option<u32> {
    if let Ok(number) = text.parse::<u32>() {
        return Some(number);
    }
    let lower = text.to_ascii_lowercase();
    match field {
        3 => MONTH_NAMES
            .iter()
            .position(|name| *name == lower)
            .map(|at| at as u32 + 1),
        4 => DAY_NAMES
            .iter()
            .position(|name| *name == lower)
            .map(|at| at as u32),
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
            let Some(next) = value.checked_add(step) else {
                break;
            };
            value = next;
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
        if let Some(seconds) = every_seconds(schedule) {
            return Ok(Self {
                fields: [0; 5],
                any_day_of_month: false,
                any_day_of_week: false,
                last_day_of_month: false,
                every_seconds: Some(seconds),
            });
        }
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
        let mut last_day_of_month = false;
        for (at, text) in [minute, hour, day, month, weekday].into_iter().enumerate() {
            if at == 2 && text.split(',').any(|part| part == "$") {
                last_day_of_month = true;
                let rest: Vec<&str> = text.split(',').filter(|part| *part != "$").collect();
                fields[at] = if rest.is_empty() {
                    0
                } else {
                    parse_field(&rest.join(","), at).ok_or_else(|| invalid(schedule))?
                };
                continue;
            }
            fields[at] = parse_field(text, at).ok_or_else(|| invalid(schedule))?;
        }
        Ok(Self {
            fields,
            any_day_of_month: *day == "*",
            any_day_of_week: *weekday == "*",
            last_day_of_month,
            every_seconds: None,
        })
    }

    /// Whether the schedule runs every few seconds (`'10 seconds'`) rather than in minutes.
    pub fn runs_in_seconds(&self) -> bool {
        self.every_seconds.is_some()
    }

    /// Whether a schedule of seconds runs at `epoch_seconds`: every `n` seconds, counted from
    /// the start of each minute.
    pub fn fires_at_second(&self, epoch_seconds: i64) -> bool {
        self.every_seconds
            .is_some_and(|every| epoch_seconds.rem_euclid(60) % i64::from(every) == 0)
    }

    /// Whether the schedule fires in `minute`. When both the day of the month and the day of
    /// the week are restricted, either one matching is enough, as cron has always done.
    pub fn fires_in(&self, minute: Minute) -> bool {
        if self.every_seconds.is_some() {
            return false;
        }
        let has = |field: usize, value: u32| self.fields[field] & (1 << value) != 0;
        let day_of_month = has(2, minute.day) || (self.last_day_of_month && minute.last_day_of_month);
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
    run_jobs(store, epoch_seconds, false)
}

/// Runs the active jobs of `cron.job` whose schedule runs every few seconds (pg_cron's
/// `'n seconds'`) and is due at `epoch_seconds`; the server calls it once a second.
pub fn run_due_second_jobs<S: KeyValueStore>(
    store: &S,
    epoch_seconds: i64,
) -> SqlResult<JobsRun> {
    run_jobs(store, epoch_seconds, true)
}

fn quoted_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The minute a job's schedule reads at `epoch_seconds`: in UTC, or in the job's `timezone`
/// (a region such as `Europe/Paris`, an offset or a POSIX rule); `None` for a zone that names
/// none of these.
fn local_minute(epoch_seconds: i64, timezone: Option<&str>) -> Option<Minute> {
    let zone = match timezone.map(str::trim) {
        None | Some("") => crate::time_zones::TimeZone::UTC,
        Some(name) => crate::time_zones::TimeZone::named(name)?,
    };
    let offset = i64::from(zone.offset_at(epoch_seconds.saturating_mul(1_000_000)));
    Some(Minute::of_epoch(epoch_seconds.saturating_add(offset)))
}

fn run_jobs<S: KeyValueStore>(store: &S, epoch_seconds: i64, seconds: bool) -> SqlResult<JobsRun> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let mut listed = session.execute(
        "SELECT jobid, schedule, command, username, timezone FROM cron.job WHERE active ORDER BY jobid",
        &mut |result| {
            rows = result.rows;
        },
    );
    if listed
        .as_ref()
        .is_err_and(|error| error.state() == crate::error::SqlState::UNDEFINED_COLUMN)
    {
        listed = session.execute(
            "SELECT jobid, schedule, command, username, NULL::text FROM cron.job WHERE active ORDER BY jobid",
            &mut |result| {
                rows = result.rows;
            },
        );
    }
    if let Err(error) = listed {
        return if error.state() == crate::error::SqlState::UNDEFINED_TABLE
            || error.state() == crate::error::SqlState::INVALID_SCHEMA_NAME
        {
            Ok(JobsRun::default())
        } else {
            Err(error)
        };
    }
    let mut outcome = JobsRun::default();
    for row in rows {
        let (Some(Value::BigInt(id)), Some(schedule), Some(command)) = (
            row.first().cloned(),
            row.get(1).and_then(text_of),
            row.get(2).and_then(text_of),
        ) else {
            continue;
        };
        let Ok(parsed) = CronSchedule::parse(&schedule) else {
            if !seconds {
                outcome.invalid_schedules += 1;
            }
            continue;
        };
        let due = if seconds {
            parsed.fires_at_second(epoch_seconds)
        } else {
            match local_minute(epoch_seconds, row.get(4).and_then(text_of).as_deref()) {
                Some(minute) => parsed.fires_in(minute),
                None => {
                    outcome.invalid_schedules += 1;
                    continue;
                }
            }
        };
        if !due {
            continue;
        }
        let mut job = Session::with_dialect(store, Dialect::PostgreSql);
        let mut tag = String::new();
        let became = match row.get(3).and_then(text_of) {
            Some(user) if !user.is_empty() => job.execute(
                &format!("SET SESSION AUTHORIZATION {}", quoted_identifier(&user)),
                &mut |_| {},
            ),
            _ => Ok(()),
        };
        let ran = became.and_then(|()| job.execute(&command, &mut |result| tag = result.tag));
        let _ = job.close();
        let (status, message) = match ran {
            Ok(()) => {
                outcome.succeeded += 1;
                ("succeeded", tag)
            }
            Err(error) => {
                outcome.failed += 1;
                (
                    "failed",
                    error.message().chars().take(MAX_MESSAGE_CHARS).collect(),
                )
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

/// Runs every enabled MySQL event (`CREATE EVENT`) due at `epoch_seconds`, each in its own
/// MySQL session and transaction, then moves a recurring event to its next run, and disables or
/// drops a finished one as its `ON COMPLETION` clause says. A file without events has none due.
pub fn run_due_events<S: KeyValueStore>(store: &S, epoch_seconds: i64) -> SqlResult<JobsRun> {
    use crate::session::mysql_events::{
        EVENTS_TABLE, StoredEvent, event_key, number_or_null, quoted,
    };
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut switched_off = false;
    let _ = session.execute(
        &format!(
            "SELECT value FROM {} WHERE name = 'event_scheduler'",
            crate::session::mysql_events::SETTINGS_TABLE
        ),
        &mut |result| {
            switched_off = result
                .rows
                .first()
                .and_then(|row| row.first())
                .is_some_and(|value| matches!(value, Value::Text(state) if state == "OFF"));
        },
    );
    if switched_off {
        return Ok(JobsRun::default());
    }
    let mut rows = Vec::new();
    let listed = session.execute(
        &format!(
            "SELECT db, name, body, execute_at, interval_value, interval_unit, starts, ends, \
             next_run, preserve, enabled, comment, created FROM {EVENTS_TABLE} \
             WHERE enabled AND next_run <= {epoch_seconds} ORDER BY next_run, event_key"
        ),
        &mut |result| {
            rows = result.rows;
        },
    );
    if let Err(error) = listed {
        return if error.state() == crate::error::SqlState::UNDEFINED_TABLE
            || error.state() == crate::error::SqlState::INVALID_SCHEMA_NAME
        {
            Ok(JobsRun::default())
        } else {
            Err(error)
        };
    }
    let mut outcome = JobsRun::default();
    for event in rows.iter().filter_map(|row| StoredEvent::of_row(row)) {
        let mut job = Session::with_dialect(store, Dialect::MySql);
        let used = job.execute(
            &format!("USE `{}`", event.database.replace('`', "``")),
            &mut |_| {},
        );
        let ran = used.and_then(|()| job.run_mysql_event_body(&event.body));
        let _ = job.close();
        match ran {
            Ok(()) => outcome.succeeded += 1,
            Err(_) => outcome.failed += 1,
        }
        let key = quoted(&event_key(&event.database, &event.name));
        let next = event.following_run(epoch_seconds);
        let change = match next {
            Some(next) => format!(
                "UPDATE {EVENTS_TABLE} SET next_run = {next}, last_executed = {epoch_seconds} WHERE event_key = {key}"
            ),
            None if event.preserve => format!(
                "UPDATE {EVENTS_TABLE} SET enabled = false, next_run = {}, last_executed = {epoch_seconds} WHERE event_key = {key}",
                number_or_null(None)
            ),
            None => format!("DELETE FROM {EVENTS_TABLE} WHERE event_key = {key}"),
        };
        session.execute(&change, &mut |_| {})?;
    }
    session.close()?;
    Ok(outcome)
}

/// The internal table Oracle's `DBMS_SCHEDULER` jobs live in.
pub(crate) const ORACLE_SCHEDULER_TABLE: &str = "eggdb_internal.oracle_scheduler_jobs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frequency {
    Yearly,
    Monthly,
    Weekly,
    Daily,
    Hourly,
    Minutely,
}

/// An Oracle calendaring expression (`FREQ=DAILY; BYHOUR=2; BYMINUTE=0`), as `DBMS_SCHEDULER`'s
/// `repeat_interval` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CalendarRule {
    frequency: Frequency,
    interval: i64,
    by_month: Vec<u32>,
    by_month_day: Vec<i32>,
    by_day: Vec<u32>,
    by_hour: Vec<u32>,
    by_minute: Vec<u32>,
}

fn rule_numbers(value: &str, low: i64, high: i64, names: &[&str]) -> Result<Vec<i64>, String> {
    value
        .split(',')
        .map(|item| {
            let item = item.trim();
            if let Some(at) = names.iter().position(|name| name.eq_ignore_ascii_case(item)) {
                return Ok(at as i64 + low);
            }
            let number: i64 = item.parse().map_err(|_| format!("{item} is not a value here"))?;
            if (low..=high).contains(&number) || (-high..=-1).contains(&number) && low == 1 && high == 31 {
                Ok(number)
            } else {
                Err(format!("{item} is out of range"))
            }
        })
        .collect()
}

impl CalendarRule {
    /// Reads a calendaring expression: `FREQ` with `INTERVAL`, `BYMONTH`, `BYMONTHDAY` (`-1`
    /// for the last day), `BYDAY`, `BYHOUR`, `BYMINUTE` and `BYSECOND=0`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut rule = Self {
            frequency: Frequency::Daily,
            interval: 1,
            by_month: Vec::new(),
            by_month_day: Vec::new(),
            by_day: Vec::new(),
            by_hour: Vec::new(),
            by_minute: Vec::new(),
        };
        let mut frequency_given = false;
        for clause in text.split(';').map(str::trim).filter(|clause| !clause.is_empty()) {
            let (key, value) = clause.split_once('=').ok_or_else(|| format!("{clause} has no ="))?;
            let value = value.trim();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    frequency_given = true;
                    rule.frequency = match value.to_ascii_uppercase().as_str() {
                        "YEARLY" => Frequency::Yearly,
                        "MONTHLY" => Frequency::Monthly,
                        "WEEKLY" => Frequency::Weekly,
                        "DAILY" => Frequency::Daily,
                        "HOURLY" => Frequency::Hourly,
                        "MINUTELY" => Frequency::Minutely,
                        "SECONDLY" => return Err("FREQ=SECONDLY is not supported; the scheduler ticks each minute".to_owned()),
                        other => return Err(format!("FREQ={other} is not a frequency")),
                    };
                }
                "INTERVAL" => {
                    rule.interval = value.parse().ok().filter(|interval| (1..=1000).contains(interval)).ok_or("INTERVAL takes 1 to 1000")?;
                }
                "BYMONTH" => {
                    rule.by_month = rule_numbers(value, 1, 12, &["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"])?
                        .into_iter()
                        .map(|month| month as u32)
                        .collect();
                }
                "BYMONTHDAY" => {
                    rule.by_month_day = rule_numbers(value, 1, 31, &[])?.into_iter().map(|day| day as i32).collect();
                    if rule.by_month_day.iter().any(|day| *day < -1) {
                        return Err("BYMONTHDAY counts back only to -1, the last day".to_owned());
                    }
                }
                "BYDAY" => {
                    rule.by_day = rule_numbers(value, 0, 6, &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"])?
                        .into_iter()
                        .map(|day| day as u32)
                        .collect();
                }
                "BYHOUR" => rule.by_hour = rule_numbers(value, 0, 23, &[])?.into_iter().map(|hour| hour as u32).collect(),
                "BYMINUTE" => rule.by_minute = rule_numbers(value, 0, 59, &[])?.into_iter().map(|minute| minute as u32).collect(),
                "BYSECOND" if value == "0" => {}
                other => return Err(format!("{other} is not supported")),
            }
        }
        if !frequency_given {
            return Err("FREQ is required".to_owned());
        }
        Ok(rule)
    }

    /// Whether the rule fires in the minute starting `epoch_seconds`, for a job that starts at
    /// `start_epoch`: the `BY` lists where given, else the start's minute, hour, weekday and day,
    /// and every `INTERVAL` units of `FREQ` counted from the start.
    pub(crate) fn fires(&self, start_epoch: i64, epoch_seconds: i64) -> bool {
        let start = start_epoch - start_epoch.rem_euclid(60);
        if epoch_seconds < start {
            return false;
        }
        let (from, at) = (Minute::of_epoch(start), Minute::of_epoch(epoch_seconds));
        let (start_days, days) = (start.div_euclid(86_400), epoch_seconds.div_euclid(86_400));
        let (start_year, start_month, _) = civil_from_days(start_days);
        let (year, month, _) = civil_from_days(days);
        let months = (year * 12 + i64::from(month)) - (start_year * 12 + i64::from(start_month));
        let every = |count: i64| count.rem_euclid(self.interval) == 0;
        let minute_ok = if !self.by_minute.is_empty() {
            self.by_minute.contains(&at.minute)
        } else if self.frequency == Frequency::Minutely {
            every((epoch_seconds - start) / 60)
        } else {
            at.minute == from.minute
        };
        let hour_ok = if !self.by_hour.is_empty() {
            self.by_hour.contains(&at.hour)
        } else {
            match self.frequency {
                Frequency::Minutely => true,
                Frequency::Hourly => every((epoch_seconds - start) / 3600),
                _ => at.hour == from.hour,
            }
        };
        let month_ok = self.by_month.is_empty() || self.by_month.contains(&at.month);
        let explicit_day = !self.by_month_day.is_empty() || !self.by_day.is_empty();
        let day_lists_ok = (self.by_month_day.is_empty()
            || self.by_month_day.iter().any(|day| {
                (*day > 0 && *day as u32 == at.day) || (*day == -1 && at.last_day_of_month)
            }))
            && (self.by_day.is_empty() || self.by_day.contains(&at.weekday));
        let period_ok = match self.frequency {
            Frequency::Minutely | Frequency::Hourly => true,
            Frequency::Daily => every(days - start_days),
            Frequency::Weekly => every((days - start_days).div_euclid(7)) && (explicit_day || at.weekday == from.weekday),
            Frequency::Monthly => every(months) && (explicit_day || at.day == from.day),
            Frequency::Yearly => {
                every(months.div_euclid(12)) && (explicit_day || !self.by_month.is_empty() || (at.month == from.month && at.day == from.day))
            }
        };
        minute_ok && hour_ok && month_ok && day_lists_ok && period_ok
    }
}

/// The anonymous block a `DBMS_SCHEDULER` job runs: a `PLSQL_BLOCK` action as written (wrapped
/// in `BEGIN ... END;` when it is not a block), a `STORED_PROCEDURE` as a call of it.
pub(crate) fn scheduler_block(job_type: &str, action: &str) -> String {
    let action = action.trim();
    let starts_block = |word: &str| action.get(..word.len()).is_some_and(|start| start.eq_ignore_ascii_case(word));
    if job_type.eq_ignore_ascii_case("STORED_PROCEDURE") {
        return format!("BEGIN {}; END;", action.trim_end_matches(';'));
    }
    if starts_block("BEGIN") || starts_block("DECLARE") {
        action.to_owned()
    } else {
        format!("BEGIN {action} END;")
    }
}

/// Runs every enabled `DBMS_SCHEDULER` job due in the minute starting `epoch_seconds` (UTC),
/// each in an Oracle session of its own, then drops (with `auto_drop`) or disables a one-time job
/// or one past its end date. A file without scheduler jobs has none due.
pub fn run_due_scheduler_jobs<S: KeyValueStore>(store: &S, epoch_seconds: i64) -> SqlResult<JobsRun> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute(
        &format!(
            "SELECT job_name, job_type, job_action, start_epoch, repeat_interval, end_epoch, auto_drop, last_run \
             FROM {ORACLE_SCHEDULER_TABLE} WHERE enabled ORDER BY job_name"
        ),
        &mut |result| {
            if result.columns.is_some() {
                rows = result.rows;
            }
        },
    );
    if let Err(error) = listed {
        return if error.state() == crate::error::SqlState::UNDEFINED_TABLE
            || error.state() == crate::error::SqlState::INVALID_SCHEMA_NAME
        {
            Ok(JobsRun::default())
        } else {
            Err(error)
        };
    }
    let number = |value: Option<&Value>| match value {
        Some(Value::BigInt(number)) => Some(*number),
        Some(Value::Integer(number)) => Some(i64::from(*number)),
        _ => None,
    };
    let mut outcome = JobsRun::default();
    for row in &rows {
        let (Some(job), Some(job_type), Some(action), Some(start)) = (
            row.first().and_then(text_of),
            row.get(1).and_then(text_of),
            row.get(2).and_then(text_of),
            number(row.get(3)),
        ) else {
            continue;
        };
        let interval = row.get(4).and_then(text_of);
        let end = number(row.get(5));
        let auto_drop = matches!(row.get(6), Some(Value::Boolean(true)));
        let last_run = number(row.get(7));
        let past_end = end.is_some_and(|end| epoch_seconds > end);
        let (due, finished) = match &interval {
            None => (last_run.is_none() && start <= epoch_seconds + 59, true),
            Some(text) => match CalendarRule::parse(text) {
                Ok(rule) => (!past_end && rule.fires(start, epoch_seconds), past_end),
                Err(_) => {
                    outcome.invalid_schedules += 1;
                    (false, false)
                }
            },
        };
        if due {
            let mut runner = Session::with_dialect(store, Dialect::Oracle);
            match runner.execute(&scheduler_block(&job_type, &action), &mut |_| {}) {
                Ok(()) => outcome.succeeded += 1,
                Err(_) => outcome.failed += 1,
            }
            let _ = runner.close();
            session.execute(
                &format!("UPDATE {ORACLE_SCHEDULER_TABLE} SET last_run = {epoch_seconds} WHERE job_name = {}", quoted(&job)),
                &mut |_| {},
            )?;
        }
        if finished && (due || past_end) {
            let change = if auto_drop {
                format!("DELETE FROM {ORACLE_SCHEDULER_TABLE} WHERE job_name = {}", quoted(&job))
            } else {
                format!("UPDATE {ORACLE_SCHEDULER_TABLE} SET enabled = false WHERE job_name = {}", quoted(&job))
            };
            session.execute(&change, &mut |_| {})?;
        }
    }
    session.close()?;
    Ok(outcome)
}

/// Runs every enabled SQL Server Agent job with an enabled schedule that fires in the minute
/// starting `epoch_seconds` (UTC), each job's steps in a SQL Server session of its own. A file
/// without Agent jobs has none due.
pub fn run_due_agent_jobs<S: KeyValueStore>(store: &S, epoch_seconds: i64) -> SqlResult<JobsRun> {
    use crate::session::sqlserver_agent::{ATTACHED_TABLE, JOBS_TABLE, SCHEDULES_TABLE, schedule_fires};
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute(
        &format!(
            "SELECT DISTINCT a.job_name, s.freq_type, s.freq_interval, s.freq_subday_type, s.freq_subday_interval, \
             s.freq_recurrence_factor, s.active_start_date, s.active_end_date, s.active_start_time, s.active_end_time \
             FROM {ATTACHED_TABLE} a JOIN {JOBS_TABLE} j ON j.job_name = a.job_name \
             JOIN {SCHEDULES_TABLE} s ON s.schedule_name = a.schedule_name WHERE j.enabled AND s.enabled ORDER BY 1"
        ),
        &mut |result| {
            if result.columns.is_some() {
                rows = result.rows;
            }
        },
    );
    if let Err(error) = listed {
        return if error.state() == crate::error::SqlState::UNDEFINED_TABLE
            || error.state() == crate::error::SqlState::INVALID_SCHEMA_NAME
        {
            Ok(JobsRun::default())
        } else {
            Err(error)
        };
    }
    let _ = session.close();
    let mut due: Vec<String> = Vec::new();
    for row in &rows {
        let Some(job) = row.first().and_then(text_of) else {
            continue;
        };
        if schedule_fires(&row[1..], epoch_seconds) && !due.contains(&job) {
            due.push(job);
        }
    }
    let mut outcome = JobsRun::default();
    for job in due {
        let mut runner = Session::with_dialect(store, Dialect::SqlServer);
        match runner.run_agent_job(&job) {
            Ok(true) => outcome.succeeded += 1,
            _ => outcome.failed += 1,
        }
        let _ = runner.close();
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests;
