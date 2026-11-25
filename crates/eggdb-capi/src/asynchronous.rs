//! Asynchronous SQL calls (D1347): `eggdb_sql_execute_async` and
//! `eggdb_sql_execute_with_parameters_async` run a batch of statements on a thread of their own
//! and report it through one callback, so an event loop never waits on the database.

use crate::args;
use crate::sql::{EggdbParameter, EggdbResult, EggdbSql, owned_session_state};
use crate::status::{EGGDB_OK, Failure, guarded};
use eggdb::sql::Parameter;
use std::ffi::{c_char, c_int, c_void};

/// Called once when an asynchronous call ends, on the thread that ran it: `status` is
/// `EGGDB_OK` with the batch's `count` results at `results`, valid until the next call on the
/// session, or a failure, which `eggdb_last_error` and `eggdb_last_sqlstate` describe inside
/// the callback. The session is free again when the callback runs, so it may start the next call.
pub type EggdbCompletion =
    Option<unsafe extern "C" fn(user_data: *mut c_void, status: c_int, results: *const EggdbResult, count: usize)>;

struct Job {
    sql: *const EggdbSql,
    text: String,
    parameters: Option<Vec<Parameter>>,
    callback: unsafe extern "C" fn(*mut c_void, c_int, *const EggdbResult, usize),
    user_data: *mut c_void,
}

// SAFETY: eggdb.h gives the session to the asynchronous call until its callback, which the session's `running` flag enforces, and leaves `user_data` to the caller's callback alone.
unsafe impl Send for Job {}

impl Job {
    fn run(self) {
        // SAFETY: eggdb.h keeps the handle alive until the callback, and the call set its `running` flag.
        let sql: &EggdbSql = unsafe { &*self.sql };
        let mut finished = (std::ptr::null(), 0usize);
        let status = guarded(|| {
            // SAFETY: this call set the session's `running` flag in `start`.
            let state = unsafe { owned_session_state(sql) };
            finished = state.run_for_asynchronous_call(&self.text, self.parameters.as_deref())?;
            Ok(())
        });
        sql.release_after_asynchronous_call();
        let (results, count) = if status == EGGDB_OK { finished } else { (std::ptr::null(), 0) };
        // SAFETY: eggdb.h asks for a callback that takes these four arguments.
        unsafe { (self.callback)(self.user_data, status, results, count) };
    }
}

fn start(sql: &EggdbSql, job: Job) -> Result<(), Failure> {
    sql.claim_for_asynchronous_call()?;
    let spawned = std::thread::Builder::new()
        .name("eggdb-async".to_owned())
        .spawn(move || job.run());
    if spawned.is_err() {
        sql.release_after_asynchronous_call();
        return Err(Failure::new(crate::status::EGGDB_ERR_OUT_OF_MEMORY, "no thread could be started for the call"));
    }
    Ok(())
}

/// Starts running the statements in `text` as `eggdb_sql_execute` does, on a thread of its
/// own, and returns at once: `callback` gets `user_data` and the batch's outcome when it ends.
/// Until then every other call on the session fails with `EGGDB_ERR_IN_USE`, but
/// `eggdb_sql_cancel`, which stops the running statement. Fails at once, without calling
/// `callback`, for a bad argument or a session already busy.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` or `eggdb_sql_connect` that stays open until the
/// callback, `text` a NUL-terminated string, and `callback` a function safe to call from another
/// thread with `user_data`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_execute_async(
    sql: *mut EggdbSql,
    text: *const c_char,
    callback: EggdbCompletion,
    user_data: *mut c_void,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live SQL session handle.
        let handle: &EggdbSql = unsafe { args::handle(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?.to_owned();
        let callback = callback.ok_or_else(|| Failure::invalid_argument("callback is NULL"))?;
        start(handle, Job { sql: handle, text, parameters: None, callback, user_data })
    })
}

/// Starts running the statements in `text` with parameters as
/// `eggdb_sql_execute_with_parameters` does, asynchronously as `eggdb_sql_execute_async` does.
/// The parameters are copied before it returns.
///
/// # Safety
/// As for `eggdb_sql_execute_async`, and `parameters` as for
/// `eggdb_sql_execute_with_parameters`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_execute_with_parameters_async(
    sql: *mut EggdbSql,
    text: *const c_char,
    parameters: *const EggdbParameter,
    parameter_count: usize,
    callback: EggdbCompletion,
    user_data: *mut c_void,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live SQL session handle.
        let handle: &EggdbSql = unsafe { args::handle(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?.to_owned();
        // SAFETY: eggdb.h asks for `parameter_count` parameters at `parameters`.
        let values = unsafe { crate::sql::parameters(parameters, parameter_count) }?;
        let callback = callback.ok_or_else(|| Failure::invalid_argument("callback is NULL"))?;
        start(handle, Job { sql: handle, text, parameters: Some(values), callback, user_data })
    })
}
