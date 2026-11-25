//! Asynchronous SQL calls through the C API (D1347): a batch reported by one callback on the
//! thread that ran it, failures described inside the callback, and the session free again for
//! the next call once the callback runs.

use eggdb_capi::{
    EGGDB_ERR_SQL, EGGDB_OK, EggdbDb, EggdbResult, EggdbSql, eggdb_close, eggdb_last_sqlstate, eggdb_open,
    eggdb_sql_close, eggdb_sql_execute, eggdb_sql_execute_async, eggdb_sql_open,
};
use std::ffi::{CStr, CString, c_int, c_void};
use std::ptr;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

type Outcome = (c_int, usize, String);

unsafe extern "C" fn completed(user_data: *mut c_void, status: c_int, _results: *const EggdbResult, count: usize) {
    // SAFETY: the test passes a pointer to a Sender that outlives every call it starts.
    let sender = unsafe { &*(user_data as *const Sender<Outcome>) };
    // SAFETY: eggdb_last_sqlstate returns a NUL-terminated string valid until the next failing call on this thread.
    let sqlstate = unsafe { CStr::from_ptr(eggdb_last_sqlstate()) }.to_string_lossy().into_owned();
    let _ = sender.send((status, count, sqlstate));
}

fn text(sql: &str) -> CString {
    CString::new(sql).unwrap_or_else(|_| panic!("{sql:?} holds a NUL byte"))
}

#[test]
fn a_batch_runs_on_its_own_thread_and_reports_through_one_callback() {
    let path = std::env::temp_dir().join(format!("eggdb-capi-async-{}.db", std::process::id()));
    let file = text(&path.display().to_string());
    let mut db: *mut EggdbDb = ptr::null_mut();
    // SAFETY: a NUL-terminated path, no options and a place for the handle.
    assert_eq!(unsafe { eggdb_open(file.as_ptr(), ptr::null(), &mut db) }, EGGDB_OK);
    let dialect = text("postgresql");
    let mut sql: *mut EggdbSql = ptr::null_mut();
    // SAFETY: a live database handle, a dialect name and a place for the session.
    assert_eq!(unsafe { eggdb_sql_open(db, dialect.as_ptr(), &mut sql) }, EGGDB_OK);
    let (sender, receiver) = channel::<Outcome>();
    let user_data = (&raw const sender).cast_mut().cast::<c_void>();
    let batch = text("CREATE TABLE t (a integer); INSERT INTO t VALUES (1), (2); SELECT count(*) FROM t");
    // SAFETY: a live session kept open until the callback, a NUL-terminated text and a callback safe on any thread.
    assert_eq!(unsafe { eggdb_sql_execute_async(sql, batch.as_ptr(), Some(completed), user_data) }, EGGDB_OK);
    let (status, count, _) = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!((status, count), (EGGDB_OK, 3));
    let failing = text("SELECT * FROM missing");
    // SAFETY: as above; the session is free again since the callback ran.
    assert_eq!(unsafe { eggdb_sql_execute_async(sql, failing.as_ptr(), Some(completed), user_data) }, EGGDB_OK);
    let (status, count, sqlstate) = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!((status, count, sqlstate.as_str()), (EGGDB_ERR_SQL, 0, "42P01"));
    let mut results: *const EggdbResult = ptr::null();
    let mut found = 0usize;
    let again = text("SELECT 1");
    // SAFETY: a live, idle session, a NUL-terminated text and places for the results.
    assert_eq!(unsafe { eggdb_sql_execute(sql, again.as_ptr(), &mut results, &mut found) }, EGGDB_OK);
    // SAFETY: a live, idle session given up here, then its database.
    assert_eq!(unsafe { eggdb_sql_close(sql) }, EGGDB_OK);
    // SAFETY: a live database handle given up here.
    assert_eq!(unsafe { eggdb_close(db) }, EGGDB_OK);
    let _ = std::fs::remove_file(&path);
}
