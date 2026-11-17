//! Per-role statement limits, counted over the hour as MySQL counts an account's
//! `MAX_QUERIES_PER_HOUR` and `MAX_UPDATES_PER_HOUR` (D1204): every session of a role in one
//! process shares the counts, which start again an hour after the first statement counted.
//! Connection limits (D1232): the sessions a role has open at once (`MAX_USER_CONNECTIONS`,
//! PostgreSQL's `CONNECTION LIMIT`) and the logins in the hour (`MAX_CONNECTIONS_PER_HOUR`).

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::roles::RoleLimits;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

const HOUR: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy)]
struct Usage {
    since: Instant,
    queries: u32,
    updates: u32,
}

fn usage() -> &'static Mutex<HashMap<(String, String), Usage>> {
    static USAGE: OnceLock<Mutex<HashMap<(String, String), Usage>>> = OnceLock::new();
    USAGE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn exceeded(role: &str, resource: &str, limit: u32, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("User '{role}' has exceeded the '{resource}' resource (current value: {limit})"),
        )
        .with_vendor_code(1226),
        _ => SqlError::new(
            SqlState::CONFIGURATION_LIMIT_EXCEEDED,
            format!("role \"{role}\" has used its {limit} {} for this hour", match resource {
                "max_updates" => "changing statements",
                _ => "statements",
            }),
        ),
    }
}

/// Counts one statement of `role` in `database` (a change when `changes`) against `limits`,
/// at `now`; refuses it when it would pass a limit, without counting it.
pub(crate) fn admit(
    database: &str,
    role: &str,
    limits: RoleLimits,
    changes: bool,
    dialect: Dialect,
    now: Instant,
) -> SqlResult<()> {
    if limits == RoleLimits::default() {
        return Ok(());
    }
    let mut counts = usage().lock().unwrap_or_else(PoisonError::into_inner);
    let entry = counts
        .entry((database.to_owned(), role.to_owned()))
        .or_insert(Usage {
            since: now,
            queries: 0,
            updates: 0,
        });
    if now.saturating_duration_since(entry.since) >= HOUR {
        *entry = Usage {
            since: now,
            queries: 0,
            updates: 0,
        };
    }
    if limits.queries_per_hour > 0 && entry.queries >= limits.queries_per_hour {
        return Err(exceeded(role, "max_questions", limits.queries_per_hour, dialect));
    }
    if changes && limits.updates_per_hour > 0 && entry.updates >= limits.updates_per_hour {
        return Err(exceeded(role, "max_updates", limits.updates_per_hour, dialect));
    }
    entry.queries += 1;
    if changes {
        entry.updates += 1;
    }
    Ok(())
}

fn open_connections() -> &'static Mutex<HashMap<(String, String), u32>> {
    static OPEN: OnceLock<Mutex<HashMap<(String, String), u32>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn hourly_connections() -> &'static Mutex<HashMap<(String, String), (Instant, u32)>> {
    static HOURLY: OnceLock<Mutex<HashMap<(String, String), (Instant, u32)>>> = OnceLock::new();
    HOURLY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One open session of a role counted against its connection limit; dropping it frees the
/// place.
#[derive(Debug)]
pub(crate) struct OpenConnection {
    key: (String, String),
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        let mut open = open_connections().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = open.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.key);
            }
        }
    }
}

fn too_many_connections(role: &str, resource: &str, limit: u32, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("User '{role}' has exceeded the '{resource}' resource (current value: {limit})"),
        )
        .with_vendor_code(1226),
        _ if resource == "max_user_connections" => SqlError::new(
            SqlState::TOO_MANY_CONNECTIONS,
            format!("too many connections for role \"{role}\""),
        ),
        _ => SqlError::new(
            SqlState::CONFIGURATION_LIMIT_EXCEEDED,
            format!("role \"{role}\" has used its {limit} connections for this hour"),
        ),
    }
}

/// Admits a login of `role` in `database` against its connection limits at `now`, returning
/// the place it holds while open (none when the role has no connection limits).
///
/// # Errors
///
/// The role already has as many sessions open as it may, or has logged in as often as it may
/// this hour.
pub(crate) fn connect(
    database: &str,
    role: &str,
    limits: RoleLimits,
    dialect: Dialect,
    now: Instant,
) -> SqlResult<Option<OpenConnection>> {
    if limits.connections == 0 && limits.connections_per_hour == 0 {
        return Ok(None);
    }
    let key = (database.to_owned(), role.to_owned());
    let mut open = open_connections().lock().unwrap_or_else(PoisonError::into_inner);
    let mut hourly = hourly_connections().lock().unwrap_or_else(PoisonError::into_inner);
    let opened = open.get(&key).copied().unwrap_or(0);
    if limits.connections > 0 && opened >= limits.connections {
        return Err(too_many_connections(role, "max_user_connections", limits.connections, dialect));
    }
    let entry = hourly.entry(key.clone()).or_insert((now, 0));
    if now.saturating_duration_since(entry.0) >= HOUR {
        *entry = (now, 0);
    }
    if limits.connections_per_hour > 0 && entry.1 >= limits.connections_per_hour {
        return Err(too_many_connections(role, "max_connections_per_hour", limits.connections_per_hour, dialect));
    }
    entry.1 += 1;
    if limits.connections == 0 {
        return Ok(None);
    }
    *open.entry(key.clone()).or_insert(0) += 1;
    Ok(Some(OpenConnection { key }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_sessions_and_hourly_logins_are_limited_and_freed_on_drop() {
        let limits = RoleLimits {
            connections: 2,
            ..RoleLimits::default()
        };
        let start = Instant::now();
        let database = "connection-test";
        let first = connect(database, "ada", limits, Dialect::PostgreSql, start).unwrap();
        let second = connect(database, "ada", limits, Dialect::PostgreSql, start).unwrap();
        assert!(first.is_some() && second.is_some());
        let refused = connect(database, "ada", limits, Dialect::PostgreSql, start).unwrap_err();
        assert_eq!(refused.state(), SqlState::TOO_MANY_CONNECTIONS);
        drop(first);
        assert!(connect(database, "ada", limits, Dialect::PostgreSql, start).unwrap().is_some());
        let hourly = RoleLimits {
            connections_per_hour: 1,
            ..RoleLimits::default()
        };
        assert!(connect(database, "grace", hourly, Dialect::MySql, start).unwrap().is_none());
        let refused = connect(database, "grace", hourly, Dialect::MySql, start).unwrap_err();
        assert_eq!(refused.vendor_code(), Some(1226));
        assert!(connect(database, "grace", hourly, Dialect::MySql, start + HOUR).is_ok());
        assert!(connect(database, "nobody", RoleLimits::default(), Dialect::MySql, start).unwrap().is_none());
    }

    #[test]
    fn statements_are_counted_per_hour_and_refused_past_the_limit() {
        let limits = RoleLimits {
            queries_per_hour: 3,
            updates_per_hour: 1,
            ..RoleLimits::default()
        };
        let start = Instant::now();
        let database = "admission-test";
        assert!(admit(database, "ada", limits, true, Dialect::MySql, start).is_ok());
        let refused = admit(database, "ada", limits, true, Dialect::MySql, start).unwrap_err();
        assert_eq!(refused.state(), SqlState::MYSQL_SYNTAX_OR_ACCESS);
        assert!(admit(database, "ada", limits, false, Dialect::MySql, start).is_ok());
        assert!(admit(database, "ada", limits, false, Dialect::MySql, start).is_ok());
        let refused = admit(database, "ada", limits, false, Dialect::PostgreSql, start).unwrap_err();
        assert_eq!(refused.state(), SqlState::CONFIGURATION_LIMIT_EXCEEDED);
        assert!(admit(database, "ada", limits, false, Dialect::MySql, start + HOUR).is_ok());
        assert!(admit(database, "grace", limits, false, Dialect::MySql, start).is_ok());
    }
}
