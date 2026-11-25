//! A pool of server sessions for drivers (D1325): `eggdb_pool_open` keeps up to a number of
//! `eggdb_sql_connect` sessions to one URL, `eggdb_pool_acquire` lends one (opening it when
//! none is idle and the pool is not full, else waiting for one), `eggdb_pool_release` rolls back
//! what it left open and takes it back, and `eggdb_pool_close` closes the idle ones.

use crate::args;
use crate::sql::{EggdbSql, connected, eggdb_sql_close, session_state};
use crate::status::{EGGDB_ERR_IN_USE, Failure, guarded};
use std::collections::HashSet;
use std::ffi::{c_char, c_int};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const MOST_CONNECTIONS: usize = 10_000;

#[derive(Debug, Default)]
struct PoolState {
    idle: Vec<usize>,
    lent: HashSet<usize>,
    opening: usize,
}

impl PoolState {
    fn open(&self) -> usize {
        self.idle.len() + self.lent.len() + self.opening
    }
}

/// A pool of server sessions to one URL (`eggdb_pool` in `eggdb.h`).
#[derive(Debug)]
pub struct EggdbPool {
    url: String,
    most: usize,
    state: Mutex<PoolState>,
    returned: Condvar,
}

fn close_quietly(handle: usize) {
    // SAFETY: the pool owns `handle`, a session from `connected` that nothing else uses now.
    let _ = unsafe { eggdb_sql_close(handle as *mut EggdbSql) };
}

/// Opens a pool of at most `max_connections` sessions (1 to 10,000) to the server at `url`,
/// as `eggdb_sql_connect` takes it; one session is opened at once, so a wrong URL, password
/// or address fails here.
///
/// # Safety
/// `url` is NUL-terminated and `pool` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_pool_open(url: *const c_char, max_connections: usize, pool: *mut *mut EggdbPool) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `pool`.
        unsafe { args::write(pool, std::ptr::null_mut(), "pool") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated URL.
        let url = unsafe { args::text(url, "url") }?;
        if !(1..=MOST_CONNECTIONS).contains(&max_connections) {
            return Err(Failure::invalid_argument(format!("max_connections is 1 to {MOST_CONNECTIONS}")));
        }
        let first = connected(url)?;
        let handle = args::give(EggdbPool {
            url: url.to_owned(),
            most: max_connections,
            state: Mutex::new(PoolState {
                idle: vec![first as usize],
                ..PoolState::default()
            }),
            returned: Condvar::new(),
        });
        // SAFETY: as for the first write to `pool`.
        unsafe { args::write(pool, handle, "pool") }
    })
}

enum Lending {
    Idle(usize),
    Open,
}

/// Lends a session: an idle one, or a new one while the pool holds fewer than its maximum,
/// or else the next one released within `timeout_ms` milliseconds (0 waits not at all), which
/// fails with `EGGDB_ERR_IN_USE` when none is. The session is the caller's until
/// `eggdb_pool_release`; it must not be closed with `eggdb_sql_close`.
///
/// # Safety
/// `pool` is a live pool and `sql` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_pool_acquire(pool: *mut EggdbPool, timeout_ms: u64, sql: *mut *mut EggdbSql) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `sql`.
        unsafe { args::write(sql, std::ptr::null_mut(), "sql") }?;
        // SAFETY: eggdb.h asks for a live pool, which other threads may use at the same time.
        let pool = unsafe { args::handle(pool, "pool") }?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut state = pool.state.lock().unwrap_or_else(PoisonError::into_inner);
        let lending = loop {
            if let Some(handle) = state.idle.pop() {
                state.lent.insert(handle);
                break Lending::Idle(handle);
            }
            if state.open() < pool.most {
                state.opening += 1;
                break Lending::Open;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Failure::new(
                    EGGDB_ERR_IN_USE,
                    format!("all {} sessions of the pool are lent; release one or wait longer", pool.most),
                ));
            }
            state = pool
                .returned
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        drop(state);
        let handle = match lending {
            Lending::Idle(handle) => handle,
            Lending::Open => {
                let opened = connected(&pool.url);
                let mut state = pool.state.lock().unwrap_or_else(PoisonError::into_inner);
                state.opening -= 1;
                match opened {
                    Ok(handle) => {
                        state.lent.insert(handle as usize);
                        handle as usize
                    }
                    Err(failure) => {
                        drop(state);
                        pool.returned.notify_one();
                        return Err(failure);
                    }
                }
            }
        };
        // SAFETY: as for the first write to `sql`.
        unsafe { args::write(sql, handle as *mut EggdbSql, "sql") }
    })
}

/// Takes back a session `eggdb_pool_acquire` lent: an open transaction is rolled back, and a
/// session whose connection broke is closed instead of kept. Its prepared statements and
/// `COPY` handles must be finalized and freed first (`EGGDB_ERR_IN_USE` otherwise, and the
/// session stays lent).
///
/// # Safety
/// `pool` is a live pool and `sql` a session it lent that the caller no longer uses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_pool_release(pool: *mut EggdbPool, sql: *mut EggdbSql) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live pool.
        let pool = unsafe { args::handle(pool, "pool") }?;
        let key = sql as usize;
        if !pool.state.lock().unwrap_or_else(PoisonError::into_inner).lent.contains(&key) {
            return Err(Failure::invalid_argument("the session was not lent by this pool"));
        }
        // SAFETY: the pool lent `sql`, and the caller gives it back with this call.
        let open = unsafe { args::handle(sql, "sql") }?.open_statements();
        if open > 0 {
            return Err(Failure::new(
                EGGDB_ERR_IN_USE,
                format!("{open} prepared statement(s) or COPY handle(s) still use this session; finalize and free them before releasing it"),
            ));
        }
        // SAFETY: as above; nothing else uses the session during this call.
        let state = unsafe { session_state(sql, "sql") }?;
        let reusable = state.run_quietly("ROLLBACK").is_ok() && !state.connection_broken();
        let mut pool_state = pool.state.lock().unwrap_or_else(PoisonError::into_inner);
        pool_state.lent.remove(&key);
        if reusable {
            pool_state.idle.push(key);
        }
        drop(pool_state);
        if !reusable {
            close_quietly(key);
        }
        pool.returned.notify_one();
        Ok(())
    })
}

/// How many sessions the pool holds, lent and idle, written to `open` and `idle` (either may be
/// NULL).
///
/// # Safety
/// `pool` is a live pool; `open` and `idle` are NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_pool_stats(pool: *mut EggdbPool, open: *mut usize, idle: *mut usize) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live pool.
        let pool = unsafe { args::handle(pool, "pool") }?;
        let state = pool.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (all, resting) = (state.open(), state.idle.len());
        drop(state);
        // SAFETY: eggdb.h asks for NULL or writable memory for one size_t.
        unsafe { args::write_if_wanted(open, all) };
        // SAFETY: as above.
        unsafe { args::write_if_wanted(idle, resting) };
        Ok(())
    })
}

/// Closes the pool and its idle sessions and frees it; NULL does nothing. While any session is
/// still lent it fails with `EGGDB_ERR_IN_USE` and leaves the pool open.
///
/// # Safety
/// `pool` is NULL or a live pool that no other thread uses during the call; unless the status
/// is `EGGDB_ERR_IN_USE`, it must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_pool_close(pool: *mut EggdbPool) -> c_int {
    guarded(|| {
        if pool.is_null() {
            return Ok(());
        }
        // SAFETY: eggdb.h asks for a live pool.
        let lent = unsafe { args::handle(pool, "pool") }?
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lent
            .len();
        if lent > 0 {
            return Err(Failure::new(
                EGGDB_ERR_IN_USE,
                format!("{lent} session(s) of the pool are still lent; release them first"),
            ));
        }
        // SAFETY: the caller gives the pool up with this call (eggdb.h).
        let Some(owned) = (unsafe { args::take(pool) }) else {
            return Ok(());
        };
        let state = owned.state.into_inner().unwrap_or_else(PoisonError::into_inner);
        for handle in state.idle {
            close_quietly(handle);
        }
        Ok(())
    })
}
