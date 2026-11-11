use crate::advisory_locks::{self, ApplicationLockGrant, Lasts, Mode};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::time::Duration;

pub(crate) const GET_LOCK: &str = "mysql_get_lock";
pub(crate) const RELEASE_LOCK: &str = "mysql_release_lock";
pub(crate) const IS_FREE_LOCK: &str = "mysql_is_free_lock";
pub(crate) const IS_USED_LOCK: &str = "mysql_is_used_lock";
pub(crate) const RELEASE_ALL_LOCKS: &str = "mysql_release_all_locks";
pub(crate) const INTERNAL_NAMES: [&str; 5] = [
    GET_LOCK,
    RELEASE_LOCK,
    IS_FREE_LOCK,
    IS_USED_LOCK,
    RELEASE_ALL_LOCKS,
];

const RESOURCE_PREFIX: &str = "mysql user lock ";
const MAX_NAME_CHARACTERS: usize = 64;

/// The internal function and argument count of MySQL's named-lock function `name`.
pub(crate) fn internal_name(name: &str) -> Option<(&'static str, usize)> {
    Some(match name {
        "get_lock" => (GET_LOCK, 2),
        "release_lock" => (RELEASE_LOCK, 1),
        "is_free_lock" => (IS_FREE_LOCK, 1),
        "is_used_lock" => (IS_USED_LOCK, 1),
        "release_all_locks" => (RELEASE_ALL_LOCKS, 0),
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    INTERNAL_NAMES.contains(&name).then_some(SqlType::BigInt)
}

fn wrong_name(name: &str) -> SqlError {
    SqlError::new(
        SqlState::MYSQL_GENERAL,
        format!("Incorrect user-level lock name '{name}'."),
    )
    .with_vendor_code(3057)
}

fn resource_of(value: &Value) -> SqlResult<String> {
    let name = match value {
        Value::Null => String::new(),
        Value::Text(text) => text.clone(),
        other => other.to_text().unwrap_or_default(),
    };
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARACTERS {
        return Err(wrong_name(&name));
    }
    Ok(format!("{RESOURCE_PREFIX}{}", name.to_lowercase()))
}

fn timeout_of(value: &Value) -> Option<Duration> {
    let seconds = value.as_float().unwrap_or(0.0);
    if seconds < 0.0 || !seconds.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(seconds.min(31_536_000.0)))
}

/// Runs MySQL's `GET_LOCK`, `RELEASE_LOCK`, `IS_FREE_LOCK`, `IS_USED_LOCK` or `RELEASE_ALL_LOCKS`
/// on the application locks SQL Server's `sp_getapplock` uses, under names of their own; `None`
/// for another function.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !INTERNAL_NAMES.contains(&name) {
        return None;
    }
    Some(evaluate(name, values))
}

fn evaluate(name: &str, values: &[Value]) -> SqlResult<Value> {
    match (name, values) {
        (GET_LOCK, [lock, timeout]) => {
            let resource = resource_of(lock)?;
            let grant = advisory_locks::take_application_lock(
                &resource,
                Mode::Exclusive,
                Lasts::Session,
                timeout_of(timeout),
            )?;
            match grant {
                ApplicationLockGrant::AtOnce | ApplicationLockGrant::AfterWaiting => {
                    Ok(Value::BigInt(1))
                }
                ApplicationLockGrant::TimedOut => Ok(Value::BigInt(0)),
                ApplicationLockGrant::DeadlockVictim => Err(SqlError::new(
                    SqlState::from_text("40001").unwrap_or(SqlState::MYSQL_GENERAL),
                    "Deadlock found when trying to get user-level lock; try rolling back transaction/releasing locks and restarting lock acquisition.",
                )
                .with_vendor_code(3058)),
            }
        }
        (RELEASE_LOCK, [lock]) => {
            let resource = resource_of(lock)?;
            if advisory_locks::release_application_lock(&resource, Lasts::Session)? {
                return Ok(Value::BigInt(1));
            }
            Ok(match advisory_locks::application_lock_holder(&resource)? {
                Some(_) => Value::BigInt(0),
                None => Value::Null,
            })
        }
        (IS_FREE_LOCK, [lock]) => {
            let resource = resource_of(lock)?;
            Ok(Value::BigInt(i64::from(
                advisory_locks::application_lock_holder(&resource)?.is_none(),
            )))
        }
        (IS_USED_LOCK, [lock]) => {
            let resource = resource_of(lock)?;
            Ok(advisory_locks::application_lock_holder(&resource)?
                .map_or(Value::Null, |owner| {
                    Value::BigInt(i64::try_from(owner).unwrap_or(i64::MAX))
                }))
        }
        (RELEASE_ALL_LOCKS, []) => advisory_locks::release_application_locks_starting(
            RESOURCE_PREFIX,
        )
        .map(Value::BigInt),
        _ => Ok(Value::Null),
    }
}
