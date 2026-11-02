use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::types::Value;
use crate::{BatchStep, Session};

fn session(store: &MemoryStore) -> Session<'_> {
    Session::with_dialect(store, Dialect::SqlServer)
}

fn batch(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(crate::SqlError::sqlserver_number)
        .collect()
}

fn rows(steps: &[BatchStep]) -> Vec<Vec<Value>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .flat_map(|result| result.rows.clone())
        .collect()
}

fn status(session: &mut Session<'_>, exec: &str) -> (Value, Vec<Option<u32>>) {
    let steps = batch(
        session,
        &format!("DECLARE @result int; EXEC @result = {exec}; SELECT @result AS result"),
    );
    let value = rows(&steps)
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or(Value::Null);
    (value, errors(&steps))
}

fn single(session: &mut Session<'_>, query: &str) -> Value {
    rows(&batch(session, query))
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or(Value::Null)
}

const EF_CORE_9_LOCK: &str = "sp_getapplock @Resource = '__EFMigrationsLock', @LockOwner = 'Session', @LockMode = 'Exclusive'";
const EF_CORE_9_RELEASE: &str =
    "sp_releaseapplock @Resource = '__EFMigrationsLock', @LockOwner = 'Session'";

#[test]
fn ef_core_9s_migration_lock_is_taken_and_released_with_its_return_codes() {
    let store = MemoryStore::default();
    let mut first = session(&store);
    let mut second = session(&store);
    assert_eq!(
        status(&mut first, EF_CORE_9_LOCK),
        (Value::Integer(0), vec![])
    );
    assert_eq!(
        status(
            &mut second,
            "sp_getapplock @Resource = '__EFMigrationsLock', @LockMode = 'Exclusive', @LockOwner = 'Session', @LockTimeout = 0"
        ),
        (Value::Integer(-1), vec![])
    );
    assert_eq!(
        single(
            &mut first,
            "SELECT APPLOCK_MODE('public', '__EFMigrationsLock', 'Session')"
        ),
        Value::Text("Exclusive".to_owned())
    );
    assert_eq!(
        single(
            &mut second,
            "SELECT APPLOCK_TEST('public', '__EFMigrationsLock', 'Shared', 'Session')"
        ),
        Value::Integer(0)
    );
    assert_eq!(
        status(&mut first, EF_CORE_9_RELEASE),
        (Value::Integer(0), vec![])
    );
    assert_eq!(
        single(
            &mut second,
            "SELECT APPLOCK_TEST('public', '__EFMigrationsLock', 'Exclusive', 'Session')"
        ),
        Value::Integer(1)
    );
    assert_eq!(
        status(&mut second, EF_CORE_9_LOCK),
        (Value::Integer(0), vec![])
    );
    assert_eq!(
        status(&mut first, EF_CORE_9_RELEASE),
        (Value::Integer(-999), vec![Some(1223)])
    );
    drop(second);
    assert_eq!(
        status(&mut first, EF_CORE_9_LOCK),
        (Value::Integer(0), vec![])
    );
}

#[test]
fn a_lock_held_elsewhere_times_out_after_lock_timeout_milliseconds() {
    let store = MemoryStore::default();
    let mut holder = session(&store);
    let mut waiter = session(&store);
    assert_eq!(
        status(&mut holder, "sp_getapplock 'job', 'Exclusive', 'Session'"),
        (Value::Integer(0), vec![])
    );
    let started = std::time::Instant::now();
    assert_eq!(
        status(&mut waiter, "sp_getapplock 'job', 'Shared', 'Session', 40"),
        (Value::Integer(-1), vec![])
    );
    assert!(started.elapsed() >= std::time::Duration::from_millis(40));
    assert_eq!(
        status(
            &mut holder,
            "sp_getapplock 'job', 'Exclusive', 'Session', 0"
        ),
        (Value::Integer(0), vec![])
    );
    for _ in 0..2 {
        assert_eq!(
            status(&mut holder, "sp_releaseapplock 'job', 'Session'"),
            (Value::Integer(0), vec![])
        );
    }
    assert_eq!(
        status(&mut waiter, "sp_getapplock 'job', 'Shared', 'Session', 40"),
        (Value::Integer(0), vec![])
    );
}

#[test]
fn shared_and_update_modes_follow_sql_servers_compatibility() {
    let store = MemoryStore::default();
    let mut first = session(&store);
    let mut second = session(&store);
    let take = |mode: &str| {
        format!(
            "sp_getapplock @Resource = N'Report', @LockMode = '{mode}', @LockOwner = 'Session', @LockTimeout = 0"
        )
    };
    assert_eq!(status(&mut first, &take("Shared")).0, Value::Integer(0));
    assert_eq!(status(&mut second, &take("Update")).0, Value::Integer(0));
    assert_eq!(
        status(&mut second, &take("IntentExclusive")).0,
        Value::Integer(-1)
    );
    assert_eq!(
        status(&mut second, &take("IntentShared")).0,
        Value::Integer(0)
    );
    assert_eq!(
        single(
            &mut second,
            "SELECT APPLOCK_MODE('public', 'Report', 'Session')"
        ),
        Value::Text("Update".to_owned())
    );
    assert_eq!(
        single(
            &mut first,
            "SELECT APPLOCK_MODE('public', 'report', 'Session')"
        ),
        Value::Text("NoLock".to_owned())
    );
}

#[test]
fn transaction_locks_need_a_transaction_and_end_with_it() {
    let store = MemoryStore::default();
    let mut first = session(&store);
    let mut second = session(&store);
    let outside = batch(
        &mut first,
        "EXEC sp_getapplock @Resource = 'tx', @LockMode = 'Exclusive'",
    );
    assert_eq!(errors(&outside).len(), 1);
    batch(&mut first, "BEGIN TRANSACTION");
    assert_eq!(
        status(
            &mut first,
            "sp_getapplock @Resource = 'tx', @LockMode = 'Exclusive'"
        ),
        (Value::Integer(0), vec![])
    );
    assert_eq!(
        single(
            &mut first,
            "SELECT APPLOCK_MODE('public', 'tx', 'Transaction')"
        ),
        Value::Text("Exclusive".to_owned())
    );
    assert_eq!(
        single(
            &mut second,
            "SELECT APPLOCK_TEST('public', 'tx', 'Exclusive', 'Session')"
        ),
        Value::Integer(0)
    );
    batch(&mut first, "ROLLBACK TRANSACTION");
    assert_eq!(
        single(
            &mut second,
            "SELECT APPLOCK_TEST('public', 'tx', 'Exclusive', 'Session')"
        ),
        Value::Integer(1)
    );
}

#[test]
fn invalid_arguments_return_minus_999_with_sql_servers_numbers() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for (exec, number) in [
        (
            "sp_getapplock @Resource = NULL, @LockMode = 'Exclusive', @LockOwner = 'Session'",
            1224,
        ),
        (
            "sp_getapplock @Resource = 'r', @LockMode = 'Huge', @LockOwner = 'Session'",
            1225,
        ),
        (
            "sp_getapplock @Resource = 'r', @LockMode = 'Shared', @LockOwner = 'Nobody'",
            1226,
        ),
        (
            "sp_getapplock @Resource = 'r', @LockMode = 'Shared', @LockOwner = 'Session', @LockTimeout = -5",
            1227,
        ),
        ("sp_getapplock @Resource = 'r', @Unknown = 1", 1228),
        (
            "sp_releaseapplock @Resource = 'never', @LockOwner = 'Session'",
            1223,
        ),
    ] {
        assert_eq!(
            status(&mut session, exec),
            (Value::Integer(-999), vec![Some(number)]),
            "{exec}"
        );
    }
    let steps = batch(
        &mut session,
        "SELECT APPLOCK_MODE('public', NULL, 'Session')",
    );
    assert_eq!(errors(&steps), vec![Some(1224)]);
}
