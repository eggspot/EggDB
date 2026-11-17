//! Per-role statement limits, counted over the hour as MySQL counts an account's
//! `MAX_QUERIES_PER_HOUR` and `MAX_UPDATES_PER_HOUR` (D1204): every session of a role in one
//! process shares the counts, which start again an hour after the first statement counted.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_are_counted_per_hour_and_refused_past_the_limit() {
        let limits = RoleLimits {
            queries_per_hour: 3,
            updates_per_hour: 1,
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
