use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

const WAIT_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Single(i64),
    Pair(i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LockName {
    database: usize,
    key: Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Exclusive,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lasts {
    Session,
    Transaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Wait,
    Try,
    Unlock,
}

#[derive(Debug)]
struct Hold {
    owner: u64,
    mode: Mode,
    session_count: u32,
    transaction_count: u32,
}

impl Hold {
    fn is_empty(&self) -> bool {
        self.session_count == 0 && self.transaction_count == 0
    }
}

#[derive(Default)]
struct Registry {
    held: Mutex<HashMap<LockName, Vec<Hold>>>,
    released: Condvar,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

fn held() -> MutexGuard<'static, HashMap<LockName, Vec<Hold>>> {
    registry()
        .held
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The advisory locks of one session, as PostgreSQL's `pg_advisory_lock` family takes them:
/// held in a table shared by every session over the same store, released when the session
/// ends (session-level locks), when its transaction ends (transaction-level locks), or on
/// request.
#[derive(Debug)]
pub(crate) struct AdvisoryLocks {
    database: usize,
    owner: u64,
}

impl AdvisoryLocks {
    pub(crate) fn new(database: usize) -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            database,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub(crate) fn release_transaction_locks(&self) {
        release(self.database, self.owner, |hold| hold.transaction_count = 0);
    }

    fn release_everything(&self) {
        release(self.database, self.owner, |hold| {
            hold.session_count = 0;
            hold.transaction_count = 0;
        });
    }
}

impl Drop for AdvisoryLocks {
    fn drop(&mut self) {
        self.release_everything();
    }
}

fn release(database: usize, owner: u64, forget: impl Fn(&mut Hold)) {
    let mut held = held();
    let mut changed = false;
    held.retain(|name, holds| {
        if name.database != database {
            return true;
        }
        for hold in holds.iter_mut().filter(|hold| hold.owner == owner) {
            forget(hold);
            changed = true;
        }
        holds.retain(|hold| !hold.is_empty());
        !holds.is_empty()
    });
    drop(held);
    if changed {
        registry().released.notify_all();
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveOwner {
    database: usize,
    owner: u64,
    statement_scoped: bool,
}

thread_local! {
    static ACTIVE: RefCell<Option<ActiveOwner>> = const { RefCell::new(None) };
}

pub(crate) struct EnteredLocks {
    entered: ActiveOwner,
    previous: Option<ActiveOwner>,
}

impl Drop for EnteredLocks {
    fn drop(&mut self) {
        if self.entered.statement_scoped {
            release(self.entered.database, self.entered.owner, |hold| {
                hold.transaction_count = 0;
            });
        }
        let previous = self.previous.take();
        ACTIVE.with(|active| *active.borrow_mut() = previous);
    }
}

/// Makes `locks` the owner of the advisory locks the statement about to run takes. Outside a
/// transaction, the transaction-level locks it takes are released when the returned guard
/// drops, at the statement's end, as PostgreSQL releases them when its implicit transaction
/// commits.
pub(crate) fn enter(locks: &AdvisoryLocks, in_transaction: bool) -> EnteredLocks {
    let entered = ActiveOwner {
        database: locks.database,
        owner: locks.owner,
        statement_scoped: !in_transaction,
    };
    let previous = ACTIVE.with(|active| active.borrow_mut().replace(entered));
    EnteredLocks { entered, previous }
}

fn active() -> SqlResult<ActiveOwner> {
    ACTIVE.with(|active| *active.borrow()).ok_or_else(|| {
        SqlError::new(
            SqlState::INTERNAL_ERROR,
            "advisory locks are taken only by a session's statements",
        )
    })
}

fn key_of(values: &[Value]) -> Option<Option<Key>> {
    match values {
        [single] if single.is_null() => Some(None),
        [single] => Some(Some(Key::Single(single.as_integer()?))),
        [first, second] if first.is_null() || second.is_null() => Some(None),
        [first, second] => Some(Some(Key::Pair(
            i32::try_from(first.as_integer()?).ok()?,
            i32::try_from(second.as_integer()?).ok()?,
        ))),
        _ => None,
    }
}

fn request_of(name: &str) -> Option<(Request, Mode, Lasts)> {
    Some(match name {
        "pg_advisory_lock" => (Request::Wait, Mode::Exclusive, Lasts::Session),
        "pg_advisory_lock_shared" => (Request::Wait, Mode::Shared, Lasts::Session),
        "pg_try_advisory_lock" => (Request::Try, Mode::Exclusive, Lasts::Session),
        "pg_try_advisory_lock_shared" => (Request::Try, Mode::Shared, Lasts::Session),
        "pg_advisory_xact_lock" => (Request::Wait, Mode::Exclusive, Lasts::Transaction),
        "pg_advisory_xact_lock_shared" => (Request::Wait, Mode::Shared, Lasts::Transaction),
        "pg_try_advisory_xact_lock" => (Request::Try, Mode::Exclusive, Lasts::Transaction),
        "pg_try_advisory_xact_lock_shared" => (Request::Try, Mode::Shared, Lasts::Transaction),
        "pg_advisory_unlock" => (Request::Unlock, Mode::Exclusive, Lasts::Session),
        "pg_advisory_unlock_shared" => (Request::Unlock, Mode::Shared, Lasts::Session),
        _ => return None,
    })
}

/// The type a function of the `pg_advisory_lock` family returns: `boolean` for the `try` and
/// `unlock` forms, and `text` (an empty value, standing in for PostgreSQL's `void`) for the
/// ones that wait and for `pg_advisory_unlock_all`.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if name == "pg_advisory_unlock_all" {
        return Some(SqlType::Text);
    }
    let (request, _, _) = request_of(name)?;
    Some(match request {
        Request::Wait => SqlType::Text,
        Request::Try | Request::Unlock => SqlType::Boolean,
    })
}

/// The argument types of a function of the `pg_advisory_lock` family with `count` arguments:
/// one `bigint`, or two `integer`s.
pub(crate) fn argument_types(name: &str, count: usize) -> Option<&'static [Option<SqlType>]> {
    const SINGLE: [Option<SqlType>; 1] = [Some(SqlType::BigInt)];
    const PAIR: [Option<SqlType>; 2] = [
        Some(SqlType::Integer),
        Some(SqlType::Integer),
    ];
    request_of(name)?;
    match count {
        1 => Some(&SINGLE),
        2 => Some(&PAIR),
        _ => None,
    }
}

/// Runs `name` when it is one of PostgreSQL's advisory lock functions, or `None`.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name == "pg_advisory_unlock_all" {
        if !values.is_empty() {
            return None;
        }
        return Some(active().map(|owner| {
            release(owner.database, owner.owner, |hold| hold.session_count = 0);
            Value::Text(String::new())
        }));
    }
    let (request, mode, lasts) = request_of(name)?;
    let key = key_of(values)?;
    let Some(key) = key else {
        return Some(Ok(Value::Null));
    };
    Some(active().and_then(|owner| {
        let name = LockName {
            database: owner.database,
            key,
        };
        match request {
            Request::Wait => wait(name, owner.owner, mode, lasts).map(|()| Value::Text(String::new())),
            Request::Try => Ok(Value::Boolean(take(&mut held(), name, owner.owner, mode, lasts))),
            Request::Unlock => Ok(Value::Boolean(unlock(name, owner.owner, mode))),
        }
    }))
}

fn conflicts(holds: &[Hold], owner: u64, mode: Mode) -> bool {
    holds.iter().any(|hold| {
        hold.owner != owner && (mode == Mode::Exclusive || hold.mode == Mode::Exclusive)
    })
}

fn take(
    held: &mut HashMap<LockName, Vec<Hold>>,
    name: LockName,
    owner: u64,
    mode: Mode,
    lasts: Lasts,
) -> bool {
    let holds = held.entry(name).or_default();
    if conflicts(holds, owner, mode) {
        if holds.is_empty() {
            held.remove(&name);
        }
        return false;
    }
    let position = match holds
        .iter()
        .position(|hold| hold.owner == owner && hold.mode == mode)
    {
        Some(position) => position,
        None => {
            holds.push(Hold {
                owner,
                mode,
                session_count: 0,
                transaction_count: 0,
            });
            holds.len() - 1
        }
    };
    let hold = &mut holds[position];
    match lasts {
        Lasts::Session => hold.session_count = hold.session_count.saturating_add(1),
        Lasts::Transaction => hold.transaction_count = hold.transaction_count.saturating_add(1),
    }
    true
}

fn wait(name: LockName, owner: u64, mode: Mode, lasts: Lasts) -> SqlResult<()> {
    let mut held = held();
    loop {
        if take(&mut held, name, owner, mode, lasts) {
            return Ok(());
        }
        held = registry()
            .released
            .wait_timeout(held, WAIT_SLICE)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        crate::cancel::check()?;
    }
}

fn unlock(name: LockName, owner: u64, mode: Mode) -> bool {
    let mut held = held();
    let Some(holds) = held.get_mut(&name) else {
        return false;
    };
    let Some(hold) = holds
        .iter_mut()
        .find(|hold| hold.owner == owner && hold.mode == mode && hold.session_count > 0)
    else {
        return false;
    };
    hold.session_count -= 1;
    holds.retain(|hold| !hold.is_empty());
    if holds.is_empty() {
        held.remove(&name);
    }
    drop(held);
    registry().released.notify_all();
    true
}

#[cfg(test)]
mod tests {
    use super::{AdvisoryLocks, enter, function};
    use crate::types::Value;
    use std::time::Duration;

    fn call(name: &str, values: &[Value]) -> Value {
        function(name, values).unwrap().unwrap()
    }

    #[test]
    fn a_session_lock_excludes_other_sessions_until_it_is_unlocked() {
        let first = AdvisoryLocks::new(1);
        let second = AdvisoryLocks::new(1);
        let key = [Value::BigInt(77_433_833_903_597)];
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&second, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(false));
            assert_eq!(call("pg_try_advisory_lock_shared", &key), Value::Boolean(false));
            assert_eq!(call("pg_advisory_unlock", &key), Value::Boolean(false));
        }
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_advisory_unlock", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&second, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(false));
        }
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_advisory_unlock", &key), Value::Boolean(true));
            assert_eq!(call("pg_advisory_unlock", &key), Value::Boolean(false));
        }
        let _entered = enter(&second, false);
        assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
    }

    #[test]
    fn shared_locks_admit_each_other_and_exclude_an_exclusive_one() {
        let first = AdvisoryLocks::new(2);
        let second = AdvisoryLocks::new(2);
        let key = [Value::Integer(1), Value::Integer(2)];
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_try_advisory_lock_shared", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&second, false);
            assert_eq!(call("pg_try_advisory_lock_shared", &key), Value::Boolean(true));
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(false));
            assert_eq!(call("pg_advisory_unlock_all", &[]), Value::Text(String::new()));
        }
        let _entered = enter(&first, false);
        assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
    }

    #[test]
    fn transaction_locks_end_with_the_statement_outside_a_transaction() {
        let first = AdvisoryLocks::new(3);
        let second = AdvisoryLocks::new(3);
        let key = [Value::BigInt(5)];
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_try_advisory_xact_lock", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&second, true);
            assert_eq!(call("pg_try_advisory_xact_lock", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&first, false);
            assert_eq!(call("pg_try_advisory_xact_lock", &key), Value::Boolean(false));
        }
        second.release_transaction_locks();
        let _entered = enter(&first, false);
        assert_eq!(call("pg_try_advisory_xact_lock", &key), Value::Boolean(true));
    }

    #[test]
    fn locks_of_other_databases_and_ended_sessions_do_not_conflict() {
        let key = [Value::BigInt(9)];
        let elsewhere = AdvisoryLocks::new(4);
        let here = AdvisoryLocks::new(5);
        {
            let _entered = enter(&elsewhere, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
        }
        {
            let _entered = enter(&here, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
        }
        drop(here);
        let later = AdvisoryLocks::new(5);
        let _entered = enter(&later, false);
        assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
        assert_eq!(call("pg_try_advisory_lock", &[Value::Null]), Value::Null);
        assert!(function("pg_try_advisory_lock", &[]).is_none());
    }

    #[test]
    fn a_waiting_lock_is_granted_when_the_holder_releases_it() {
        let holder = AdvisoryLocks::new(6);
        let key = [Value::BigInt(1)];
        {
            let _entered = enter(&holder, false);
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(true));
        }
        let waiter = std::thread::spawn(move || {
            let waiting = AdvisoryLocks::new(6);
            let _entered = enter(&waiting, false);
            call("pg_advisory_lock", &[Value::BigInt(1)])
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished());
        drop(holder);
        assert_eq!(waiter.join().unwrap(), Value::Text(String::new()));
    }
}
