use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

const WAIT_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Single(i64),
    Pair(i32, i32),
    Application(String),
}

impl Key {
    fn shown(&self) -> String {
        match self {
            Self::Single(key) => format!("advisory lock {key}"),
            Self::Pair(first, second) => format!("advisory lock ({first}, {second})"),
            Self::Application(resource) => {
                format!("application lock '{}'", resource.replace('\'', "''"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LockName {
    database: usize,
    key: Key,
}

/// A lock mode: PostgreSQL's advisory locks are `Exclusive` or `Shared`, and SQL Server's
/// application locks (`sp_getapplock`'s `@LockMode`) may also be `Update`, `IntentShared` or
/// `IntentExclusive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Exclusive,
    Shared,
    Update,
    IntentShared,
    IntentExclusive,
}

impl Mode {
    /// The mode `sp_getapplock` and `APPLOCK_TEST` name, without regard to case.
    pub(crate) fn named(text: &str) -> Option<Self> {
        [
            ("Exclusive", Self::Exclusive),
            ("Shared", Self::Shared),
            ("Update", Self::Update),
            ("IntentShared", Self::IntentShared),
            ("IntentExclusive", Self::IntentExclusive),
        ]
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(text.trim()))
        .map(|(_, mode)| mode)
    }

    fn admits(self, other: Self) -> bool {
        use Mode::{Exclusive, IntentExclusive, IntentShared, Shared, Update};
        match (self, other) {
            (Exclusive, _) | (_, Exclusive) => false,
            (IntentShared, _) | (_, IntentShared) => true,
            (Shared, Shared) | (Shared, Update) | (Update, Shared) => true,
            (IntentExclusive, IntentExclusive) => true,
            _ => false,
        }
    }
}

/// How long a lock is held: until the session releases it or ends, or until its transaction
/// ends (`sp_getapplock`'s `@LockOwner` of `Session` or `Transaction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lasts {
    Session,
    Transaction,
}

impl Lasts {
    /// The owner `sp_getapplock`, `APPLOCK_MODE` and `APPLOCK_TEST` name, without regard to
    /// case: `Session` or `Transaction`.
    pub(crate) fn named(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("Transaction") {
            Some(Self::Transaction)
        } else if text.eq_ignore_ascii_case("Session") {
            Some(Self::Session)
        } else {
            None
        }
    }
}

const MAX_RESOURCE_CHARACTERS: usize = 255;

/// The name an application lock is known by: the first 255 characters of the resource a
/// caller gives, compared byte for byte.
pub(crate) fn application_resource(text: &str) -> String {
    text.chars().take(MAX_RESOURCE_CHARACTERS).collect()
}

/// An error of an application lock procedure or function, numbered as SQL Server numbers it
/// (1223 to 1228): severity 16, ending the statement and not the batch.
pub(crate) fn application_lock_error(code: u16, message: String) -> SqlError {
    SqlError::raised_in_sqlserver(u32::from(code), 16, 1, message, false)
}

fn application_text(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(value) => crate::sqlserver_conversion::text_of(value, None, None).ok(),
    }
}

/// The type SQL Server's `APPLOCK_MODE` (`nvarchar(32)`) or `APPLOCK_TEST` (`int`) returns.
pub(crate) fn application_function_type(name: &str) -> Option<SqlType> {
    match name {
        "applock_mode" => Some(SqlType::Varchar(Some(32))),
        "applock_test" => Some(SqlType::Integer),
        _ => None,
    }
}

/// Runs SQL Server's `APPLOCK_MODE(principal, resource, owner)` or `APPLOCK_TEST(principal,
/// resource, mode, owner)`, or `None` for another function.
pub(crate) fn application_function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let (resource, mode, owner) = match (name, values) {
        ("applock_mode", [_, resource, owner]) => (resource, None, owner),
        ("applock_test", [_, resource, mode, owner]) => (resource, Some(mode), owner),
        ("applock_mode" | "applock_test", _) => {
            return Some(Err(application_lock_error(
                1228,
                format!(
                    "An invalid parameter \"{name}\" was passed to the application lock function or procedure."
                ),
            )));
        }
        _ => return None,
    };
    let shown = name.to_ascii_uppercase();
    let Some(resource) = application_text(Some(resource)).filter(|text| !text.is_empty()) else {
        return Some(Err(application_lock_error(
            1224,
            format!("An invalid application lock resource was passed to {shown}."),
        )));
    };
    let Some(lasts) = application_text(Some(owner)).and_then(|text| Lasts::named(&text)) else {
        return Some(Err(application_lock_error(
            1226,
            format!("An invalid application lock owner was passed to {shown}."),
        )));
    };
    let resource = application_resource(&resource);
    Some(match mode {
        None => application_lock_mode(&resource, lasts).map(|mode| Value::Text(mode.to_owned())),
        Some(mode) => match application_text(Some(mode)).and_then(|text| Mode::named(&text)) {
            None => Err(application_lock_error(
                1225,
                format!("An invalid application lock mode was passed to {shown}."),
            )),
            Some(mode) => {
                application_lock_free(&resource, mode).map(|free| Value::Integer(i32::from(free)))
            }
        },
    })
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
    application_waits: Mutex<HashMap<u64, (LockName, Mode)>>,
    released: Condvar,
}

fn application_waits() -> MutexGuard<'static, HashMap<u64, (LockName, Mode)>> {
    registry()
        .application_waits
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn blockers(
    held: &HashMap<LockName, Vec<Hold>>,
    name: &LockName,
    owner: u64,
    mode: Mode,
) -> Vec<u64> {
    held.get(name).map_or_else(Vec::new, |holds| {
        holds
            .iter()
            .filter(|hold| hold.owner != owner && !hold.mode.admits(mode))
            .map(|hold| hold.owner)
            .collect()
    })
}

fn closes_a_cycle(
    held: &HashMap<LockName, Vec<Hold>>,
    waits: &HashMap<u64, (LockName, Mode)>,
    owner: u64,
) -> bool {
    let Some((name, mode)) = waits.get(&owner) else {
        return false;
    };
    let mut seen = vec![owner];
    let mut next = blockers(held, name, owner, *mode);
    while let Some(blocker) = next.pop() {
        if blocker == owner {
            return true;
        }
        if seen.contains(&blocker) {
            continue;
        }
        seen.push(blocker);
        if let Some((name, mode)) = waits.get(&blocker) {
            next.extend(blockers(held, name, blocker, *mode));
        }
    }
    false
}

struct ApplicationWait {
    owner: u64,
}

impl ApplicationWait {
    fn start(owner: u64, name: LockName, mode: Mode) -> Self {
        application_waits().insert(owner, (name, mode));
        Self { owner }
    }

    fn is_deadlocked(&self, held: &HashMap<LockName, Vec<Hold>>) -> bool {
        closes_a_cycle(held, &application_waits(), self.owner)
    }
}

impl Drop for ApplicationWait {
    fn drop(&mut self) {
        application_waits().remove(&self.owner);
    }
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

    pub(crate) fn session_lock_names(&self) -> Vec<String> {
        let mut names: Vec<String> = held()
            .iter()
            .filter(|(name, holds)| {
                name.database == self.database
                    && holds
                        .iter()
                        .any(|hold| hold.owner == self.owner && hold.session_count > 0)
            })
            .map(|(name, _)| name.key.shown())
            .collect();
        names.sort();
        names
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

pub(crate) fn release_active_transaction_locks() {
    if let Some(active) = ACTIVE.with(|active| *active.borrow()) {
        release(active.database, active.owner, |hold| {
            hold.transaction_count = 0;
        });
    }
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
/// `unlock` forms, and `void` (an empty value) for the ones that wait and for
/// `pg_advisory_unlock_all`.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if name == "pg_advisory_unlock_all" {
        return Some(SqlType::VOID);
    }
    let (request, _, _) = request_of(name)?;
    Some(match request {
        Request::Wait => SqlType::VOID,
        Request::Try | Request::Unlock => SqlType::Boolean,
    })
}

/// The argument types of a function of the `pg_advisory_lock` family with `count` arguments:
/// one `bigint`, or two `integer`s.
pub(crate) fn argument_types(name: &str, count: usize) -> Option<&'static [Option<SqlType>]> {
    const SINGLE: [Option<SqlType>; 1] = [Some(SqlType::BigInt)];
    const PAIR: [Option<SqlType>; 2] = [Some(SqlType::Integer), Some(SqlType::Integer)];
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
            Request::Wait => {
                wait(name, owner.owner, mode, lasts).map(|()| Value::Text(String::new()))
            }
            Request::Try => Ok(Value::Boolean(take(
                &mut held(),
                name,
                owner.owner,
                mode,
                lasts,
            ))),
            Request::Unlock => Ok(Value::Boolean(unlock(name, owner.owner, mode))),
        }
    }))
}

fn conflicts(holds: &[Hold], owner: u64, mode: Mode) -> bool {
    holds
        .iter()
        .any(|hold| hold.owner != owner && !hold.mode.admits(mode))
}

fn take(
    held: &mut HashMap<LockName, Vec<Hold>>,
    name: LockName,
    owner: u64,
    mode: Mode,
    lasts: Lasts,
) -> bool {
    let holds = held.entry(name.clone()).or_default();
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
        if take(&mut held, name.clone(), owner, mode, lasts) {
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

/// What asking for an application lock came to, as `sp_getapplock` returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationLockGrant {
    /// Granted at once (status 0).
    AtOnce,
    /// Granted after waiting for other sessions to release it (status 1).
    AfterWaiting,
    /// Not granted before the time-out (status -1).
    TimedOut,
    /// Not granted because waiting for it would never end: a session this one waits for
    /// waits, directly or through others, for an application lock this one holds (status -3).
    DeadlockVictim,
}

impl ApplicationLockGrant {
    /// The status `sp_getapplock` returns for it.
    pub(crate) fn status(self) -> i32 {
        match self {
            Self::AtOnce => 0,
            Self::AfterWaiting => 1,
            Self::TimedOut => -1,
            Self::DeadlockVictim => -3,
        }
    }
}

fn application_name(database: usize, resource: &str) -> LockName {
    LockName {
        database,
        key: Key::Application(resource.to_owned()),
    }
}

/// Takes the application lock `resource` of the active session in `mode`, as SQL Server's
/// `sp_getapplock` does: waiting for other sessions to release it for at most `timeout`, or
/// for as long as it takes when `timeout` is `None` or too long to have a deadline. Resource
/// names compare byte for byte.
pub(crate) fn take_application_lock(
    resource: &str,
    mode: Mode,
    lasts: Lasts,
    timeout: Option<Duration>,
) -> SqlResult<ApplicationLockGrant> {
    let owner = active()?;
    let name = application_name(owner.database, resource);
    let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    let mut held = held();
    if take(&mut held, name.clone(), owner.owner, mode, lasts) {
        return Ok(ApplicationLockGrant::AtOnce);
    }
    let waiting = ApplicationWait::start(owner.owner, name.clone(), mode);
    loop {
        if waiting.is_deadlocked(&held) {
            return Ok(ApplicationLockGrant::DeadlockVictim);
        }
        let slice = match deadline {
            None => WAIT_SLICE,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(ApplicationLockGrant::TimedOut);
                }
                left.min(WAIT_SLICE)
            }
        };
        held = registry()
            .released
            .wait_timeout(held, slice)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        crate::cancel::check()?;
        if take(&mut held, name.clone(), owner.owner, mode, lasts) {
            return Ok(ApplicationLockGrant::AfterWaiting);
        }
    }
}

/// Releases one hold of the application lock `resource` the active session took for
/// `lasts`, as `sp_releaseapplock` does; `false` when it holds none.
pub(crate) fn release_application_lock(resource: &str, lasts: Lasts) -> SqlResult<bool> {
    let owner = active()?;
    let name = application_name(owner.database, resource);
    let mut held = held();
    let Some(holds) = held.get_mut(&name) else {
        return Ok(false);
    };
    let Some(hold) = holds.iter_mut().find(|hold| {
        hold.owner == owner.owner
            && match lasts {
                Lasts::Session => hold.session_count > 0,
                Lasts::Transaction => hold.transaction_count > 0,
            }
    }) else {
        return Ok(false);
    };
    match lasts {
        Lasts::Session => hold.session_count -= 1,
        Lasts::Transaction => hold.transaction_count -= 1,
    }
    holds.retain(|hold| !hold.is_empty());
    if holds.is_empty() {
        held.remove(&name);
    }
    drop(held);
    registry().released.notify_all();
    Ok(true)
}

/// The mode in which the active session holds the application lock `resource` for `lasts`,
/// as SQL Server's `APPLOCK_MODE` names it: `NoLock` when it holds none.
pub(crate) fn application_lock_mode(resource: &str, lasts: Lasts) -> SqlResult<&'static str> {
    let owner = active()?;
    let held = held();
    let modes: Vec<Mode> = held
        .get(&application_name(owner.database, resource))
        .map(|holds| {
            holds
                .iter()
                .filter(|hold| {
                    hold.owner == owner.owner
                        && match lasts {
                            Lasts::Session => hold.session_count > 0,
                            Lasts::Transaction => hold.transaction_count > 0,
                        }
                })
                .map(|hold| hold.mode)
                .collect()
        })
        .unwrap_or_default();
    let has = |mode: Mode| modes.contains(&mode);
    Ok(if has(Mode::Exclusive) {
        "Exclusive"
    } else if has(Mode::Update) && has(Mode::IntentExclusive) {
        "UpdateIntentExclusive"
    } else if has(Mode::Shared) && has(Mode::IntentExclusive) {
        "SharedIntentExclusive"
    } else if has(Mode::Update) {
        "Update"
    } else if has(Mode::IntentExclusive) {
        "IntentExclusive"
    } else if has(Mode::Shared) {
        "Shared"
    } else if has(Mode::IntentShared) {
        "IntentShared"
    } else {
        "NoLock"
    })
}

/// Whether the active session could take the application lock `resource` in `mode` now
/// without waiting, as SQL Server's `APPLOCK_TEST` answers it.
pub(crate) fn application_lock_free(resource: &str, mode: Mode) -> SqlResult<bool> {
    let owner = active()?;
    let held = held();
    Ok(held
        .get(&application_name(owner.database, resource))
        .is_none_or(|holds| !conflicts(holds, owner.owner, mode)))
}

#[cfg(test)]
mod tests {
    use super::{AdvisoryLocks, enter, function, release_active_transaction_locks};
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
            assert_eq!(
                call("pg_try_advisory_lock_shared", &key),
                Value::Boolean(false)
            );
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
    fn a_procedures_commit_releases_the_transaction_locks_and_keeps_the_session_locks() {
        let first = AdvisoryLocks::new(3);
        let second = AdvisoryLocks::new(3);
        let transaction_key = [Value::Integer(31)];
        let session_key = [Value::Integer(32)];
        {
            let _entered = enter(&first, true);
            assert_eq!(
                call("pg_try_advisory_xact_lock", &transaction_key),
                Value::Boolean(true)
            );
            assert_eq!(
                call("pg_try_advisory_lock", &session_key),
                Value::Boolean(true)
            );
            release_active_transaction_locks();
        }
        {
            let _entered = enter(&second, true);
            assert_eq!(
                call("pg_try_advisory_xact_lock", &transaction_key),
                Value::Boolean(true)
            );
            assert_eq!(
                call("pg_try_advisory_lock", &session_key),
                Value::Boolean(false)
            );
        }
        second.release_transaction_locks();
        let _entered = enter(&first, false);
        assert_eq!(
            call("pg_advisory_unlock", &session_key),
            Value::Boolean(true)
        );
    }

    #[test]
    fn a_session_names_only_its_own_session_locks() {
        let first = AdvisoryLocks::new(41);
        let second = AdvisoryLocks::new(41);
        {
            let _entered = enter(&first, true);
            call("pg_advisory_lock", &[Value::Integer(9)]);
            call("pg_advisory_lock", &[Value::Integer(1), Value::Integer(2)]);
            call("pg_try_advisory_xact_lock", &[Value::Integer(5)]);
        }
        {
            let _entered = enter(&second, false);
            call("pg_advisory_lock", &[Value::Integer(7)]);
        }
        assert_eq!(
            first.session_lock_names(),
            ["advisory lock (1, 2)", "advisory lock 9"]
        );
        assert_eq!(second.session_lock_names(), ["advisory lock 7"]);
        first.release_transaction_locks();
        drop(first);
        assert_eq!(second.session_lock_names(), ["advisory lock 7"]);
    }

    #[test]
    fn shared_locks_admit_each_other_and_exclude_an_exclusive_one() {
        let first = AdvisoryLocks::new(2);
        let second = AdvisoryLocks::new(2);
        let key = [Value::Integer(1), Value::Integer(2)];
        {
            let _entered = enter(&first, false);
            assert_eq!(
                call("pg_try_advisory_lock_shared", &key),
                Value::Boolean(true)
            );
        }
        {
            let _entered = enter(&second, false);
            assert_eq!(
                call("pg_try_advisory_lock_shared", &key),
                Value::Boolean(true)
            );
            assert_eq!(call("pg_try_advisory_lock", &key), Value::Boolean(false));
            assert_eq!(
                call("pg_advisory_unlock_all", &[]),
                Value::Text(String::new())
            );
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
            assert_eq!(
                call("pg_try_advisory_xact_lock", &key),
                Value::Boolean(true)
            );
        }
        {
            let _entered = enter(&second, true);
            assert_eq!(
                call("pg_try_advisory_xact_lock", &key),
                Value::Boolean(true)
            );
        }
        {
            let _entered = enter(&first, false);
            assert_eq!(
                call("pg_try_advisory_xact_lock", &key),
                Value::Boolean(false)
            );
        }
        second.release_transaction_locks();
        let _entered = enter(&first, false);
        assert_eq!(
            call("pg_try_advisory_xact_lock", &key),
            Value::Boolean(true)
        );
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

    #[test]
    fn an_application_lock_wait_that_closes_a_cycle_is_the_deadlock_victim() {
        use super::{
            ApplicationLockGrant, Lasts, Mode, application_waits, release_application_lock,
            take_application_lock,
        };
        let second = AdvisoryLocks::new(9_291);
        let entered = enter(&second, false);
        assert_eq!(
            take_application_lock("b", Mode::Exclusive, Lasts::Session, None).unwrap(),
            ApplicationLockGrant::AtOnce
        );
        let (holding, held) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || {
            let locks = AdvisoryLocks::new(9_291);
            let _entered = enter(&locks, false);
            let taken = take_application_lock("a", Mode::Exclusive, Lasts::Session, None).unwrap();
            holding.send(locks.owner).unwrap();
            let waited = take_application_lock(
                "b",
                Mode::Exclusive,
                Lasts::Session,
                Some(Duration::from_secs(30)),
            )
            .unwrap();
            (taken, waited)
        });
        let first_owner = held.recv().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !application_waits().contains_key(&first_owner) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            take_application_lock(
                "a",
                Mode::Shared,
                Lasts::Session,
                Some(Duration::from_secs(30))
            )
            .unwrap(),
            ApplicationLockGrant::DeadlockVictim
        );
        assert_eq!(ApplicationLockGrant::DeadlockVictim.status(), -3);
        assert!(release_application_lock("b", Lasts::Session).unwrap());
        assert_eq!(
            first.join().unwrap(),
            (
                ApplicationLockGrant::AtOnce,
                ApplicationLockGrant::AfterWaiting
            )
        );
        assert!(!application_waits().contains_key(&first_owner));
        drop(entered);
    }
}
