//! The sessions the protocol servers of one process serve, for `pg_stat_activity`, MySQL's
//! process list and SQL Server's `sys.dm_exec_sessions`: each connection registers once it
//! has logged in and reports when each statement starts and ends.

use crate::cancel::CancelSignal;
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

const MAX_QUERY_CHARS: usize = 1024;
const MAX_SESSIONS: usize = 65_536;
const POSTGRESQL_EPOCH_UNIX_SECONDS: i64 = 946_684_800;

/// What one session is doing.
#[derive(Debug, Clone)]
pub(crate) struct SessionActivity {
    pub(crate) cancel: Option<CancelSignal>,
    pub(crate) terminate: Arc<AtomicBool>,
    pub(crate) pid: i32,
    pub(crate) protocol: &'static str,
    pub(crate) user: String,
    pub(crate) database: String,
    pub(crate) client: String,
    pub(crate) application: String,
    pub(crate) backend_start: i64,
    pub(crate) query_start: Option<i64>,
    pub(crate) state_change: i64,
    pub(crate) state: &'static str,
    pub(crate) query: String,
}

fn registry() -> &'static Mutex<BTreeMap<u64, SessionActivity>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<u64, SessionActivity>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Microseconds since 2000-01-01 UTC, as `timestamp with time zone` values hold them.
fn now_micros() -> i64 {
    let since = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_micros())
        .unwrap_or(i64::MAX)
        .saturating_sub(POSTGRESQL_EPOCH_UNIX_SECONDS * 1_000_000)
}

fn shortened(text: &str) -> String {
    text.chars().take(MAX_QUERY_CHARS).collect()
}

/// One registered session; dropping it removes the session from every activity view.
#[derive(Debug)]
pub struct ActivityHandle {
    id: Option<u64>,
    terminate: Option<Arc<AtomicBool>>,
}

/// Who a connection is, as its protocol server knows it after login.
#[derive(Debug, Clone, Copy)]
pub struct Connected<'c> {
    /// The connection's process or session id as its protocol shows it.
    pub pid: i32,
    /// `postgresql`, `mysql` or `tds`.
    pub protocol: &'static str,
    /// The user it logged in as.
    pub user: &'c str,
    /// The database it is attached to.
    pub database: &'c str,
    /// The client's address.
    pub client: &'c str,
    /// The client's application name, if it gave one.
    pub application: &'c str,
    /// What cancels the session's running statement.
    pub cancel: Option<&'c CancelSignal>,
}

/// Registers a session that has just logged in, idle.
pub fn register(connected: Connected<'_>) -> ActivityHandle {
    let mut sessions = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if sessions.len() >= MAX_SESSIONS {
        return ActivityHandle {
            id: None,
            terminate: None,
        };
    }
    let id = next_id();
    let now = now_micros();
    let terminate = Arc::new(AtomicBool::new(false));
    sessions.insert(
        id,
        SessionActivity {
            cancel: connected.cancel.cloned(),
            terminate: Arc::clone(&terminate),
            pid: connected.pid,
            protocol: connected.protocol,
            user: connected.user.to_owned(),
            database: connected.database.to_owned(),
            client: connected.client.to_owned(),
            application: shortened(connected.application),
            backend_start: now,
            query_start: None,
            state_change: now,
            state: "idle",
            query: String::new(),
        },
    );
    ActivityHandle {
        id: Some(id),
        terminate: Some(terminate),
    }
}

/// Cancels the running statement of the sessions with `pid`, or ends them when `terminate`,
/// as `pg_cancel_backend` and `pg_terminate_backend` do: `caller` may signal its own sessions,
/// and a superuser any. Answers whether a session had that pid.
pub(crate) fn signal(pid: i32, terminate: bool, caller: &str, superuser: bool) -> SqlResult<bool> {
    let sessions = registry().lock().unwrap_or_else(PoisonError::into_inner);
    let mut found = false;
    for session in sessions.values().filter(|session| session.pid == pid) {
        if !superuser && session.user != caller {
            return Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                if terminate {
                    "permission denied to terminate process"
                } else {
                    "permission denied to cancel query"
                },
            )
            .with_detail(format!(
                "Only roles with the SUPERUSER attribute may signal sessions of role \"{}\".",
                session.user
            )));
        }
        found = true;
        if terminate {
            session.terminate.store(true, Ordering::Release);
        }
        if let Some(cancel) = &session.cancel {
            cancel.cancel();
        }
    }
    Ok(found)
}

impl ActivityHandle {
    /// The flag `pg_terminate_backend` sets; the connection ends when it sees it.
    pub fn termination(&self) -> Option<Arc<AtomicBool>> {
        self.terminate.clone()
    }

    fn update(&self, change: impl FnOnce(&mut SessionActivity)) {
        let Some(id) = self.id else {
            return;
        };
        let mut sessions = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(session) = sessions.get_mut(&id) {
            change(session);
        }
    }

    /// The session starts running `text`.
    pub fn started(&self, text: &str) {
        let now = now_micros();
        self.update(|session| {
            session.state = "active";
            session.query = shortened(text);
            session.query_start = Some(now);
            session.state_change = now;
        });
    }

    /// The session's statement ended; `in_transaction` says whether a transaction is open, and
    /// `failed` whether it has failed and waits for a rollback.
    pub fn finished(&self, in_transaction: bool, failed: bool) {
        let now = now_micros();
        self.update(|session| {
            session.state = match (in_transaction, failed) {
                (_, true) => "idle in transaction (aborted)",
                (true, false) => "idle in transaction",
                (false, false) => "idle",
            };
            session.state_change = now;
        });
    }

    /// The session's application name changed.
    pub fn set_application(&self, name: &str) {
        let name = shortened(name);
        self.update(|session| session.application = name);
    }
}

impl Drop for ActivityHandle {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            registry()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
    }
}

/// Every registered session, in the order they logged in.
pub(crate) fn sessions() -> Vec<SessionActivity> {
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .cloned()
        .collect()
}

/// Seconds since `micros` (as [`SessionActivity`] holds times).
pub(crate) fn seconds_since(micros: i64) -> i64 {
    now_micros().saturating_sub(micros) / 1_000_000
}
