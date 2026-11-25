//! A SQL session from `eggdb_sql_connect` on a server in the same process: statements and
//! parameters go over PostgreSQL's protocol, values come back typed, and errors carry their
//! SQLSTATE.

use eggdb::Database;
use eggdb_capi::{
    EGGDB_BLOB, EGGDB_ERR_IN_USE, EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_SQL, EGGDB_INTEGER,
    EGGDB_NULL, EGGDB_OK, EGGDB_REAL, EGGDB_TEXT, EggdbParameter, EggdbPool, EggdbResult, EggdbSql,
    eggdb_last_sqlstate, eggdb_pool_acquire, eggdb_pool_close, eggdb_pool_open, eggdb_pool_release,
    eggdb_pool_stats, eggdb_sql_close, eggdb_sql_connect, eggdb_sql_execute,
    eggdb_sql_execute_with_parameters,
};
use eggdb_pgwire::{ScramVerifier, ServerConfig};
use std::ffi::{CStr, CString, c_int};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const PASSWORD: &str = "remote-test-password";

fn results_of(results: *const EggdbResult, count: usize) -> Vec<EggdbResult> {
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: the session lends `count` results until its next call.
    unsafe { std::slice::from_raw_parts(results, count) }.to_vec()
}

fn run(sql: *mut EggdbSql, text: &str, values: &[Option<&str>]) -> (c_int, Vec<EggdbResult>) {
    let text = CString::new(text).unwrap();
    let parameters: Vec<EggdbParameter> = values
        .iter()
        .map(|value| EggdbParameter {
            text: value.map_or(ptr::null(), str::as_ptr),
            len: value.map_or(0, str::len),
        })
        .collect();
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    let status = if values.is_empty() {
        // SAFETY: a live session, the text and places for the results.
        unsafe { eggdb_sql_execute(sql, text.as_ptr(), &mut results, &mut count) }
    } else {
        // SAFETY: a live session, the text, the parameters and places for the results.
        unsafe {
            eggdb_sql_execute_with_parameters(
                sql,
                text.as_ptr(),
                parameters.as_ptr(),
                parameters.len(),
                &mut results,
                &mut count,
            )
        }
    };
    (status, results_of(results, count))
}

#[test]
fn a_session_on_a_server_runs_statements_with_typed_values() {
    let db = Arc::new(Database::in_memory().unwrap());
    let listener = eggdb_pgwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let config = ServerConfig {
        user: "eggdb".to_owned(),
        verifier: ScramVerifier::derive(PASSWORD.as_bytes(), b"0123456789abcdef", 4096),
        database: "eggdb".to_owned(),
        max_connections: 4,
        authentication_timeout: Duration::from_secs(5),
        idle_timeout: None,
        memory_budget: Arc::default(),
        auth_rules: None,
        tls: None,
    };
    let server_db = Arc::clone(&db);
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let store = eggdb::sql::Store(&server_db);
        let stopped = move || server_stop.load(Ordering::Acquire);
        eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_| {}).unwrap();
    });
    let url = CString::new(format!(
        "postgresql://eggdb:{PASSWORD}@127.0.0.1:{port}/eggdb?sslmode=disable"
    ))
    .unwrap();
    let mut sql: *mut EggdbSql = ptr::null_mut();
    assert_eq!(
        // SAFETY: a URL and a place for the handle.
        unsafe { eggdb_sql_connect(url.as_ptr(), &mut sql) },
        EGGDB_OK
    );
    let (status, _) = run(
        sql,
        "CREATE TABLE items (id integer PRIMARY KEY, name text, price double precision, data bytea)",
        &[],
    );
    assert_eq!(status, EGGDB_OK);
    let (status, _) = run(
        sql,
        "INSERT INTO items VALUES ($1, $2, $3, $4)",
        &[Some("7"), Some("it's"), Some("2.5"), Some("\\x00ff")],
    );
    assert_eq!(status, EGGDB_OK);
    let (status, _) = run(
        sql,
        "INSERT INTO items VALUES ($1, $2, NULL, NULL)",
        &[Some("8"), None],
    );
    assert_eq!(status, EGGDB_OK);
    let (status, results) = run(
        sql,
        "SELECT id, name, price, data FROM items ORDER BY id",
        &[],
    );
    assert_eq!(status, EGGDB_OK);
    let result = results[0];
    assert_eq!((result.column_count, result.row_count), (4, 2));
    // SAFETY: the result holds 2 × 4 values until the session's next call.
    let values = unsafe { std::slice::from_raw_parts(result.values, 8) };
    assert_eq!((values[0].kind, values[0].integer), (EGGDB_INTEGER, 7));
    assert_eq!(values[1].kind, EGGDB_TEXT);
    // SAFETY: a text value's bytes.
    let name = unsafe { std::slice::from_raw_parts(values[1].data, values[1].len) };
    assert_eq!(name, b"it's");
    assert_eq!((values[2].kind, values[2].real), (EGGDB_REAL, 2.5));
    assert_eq!((values[3].kind, values[3].len), (EGGDB_BLOB, 2));
    assert_eq!(values[5].kind, EGGDB_NULL);
    let (status, _) = run(sql, "SELECT * FROM nowhere", &[]);
    assert_eq!(status, EGGDB_ERR_SQL);
    // SAFETY: the thread's last SQLSTATE, a NUL-terminated string.
    let state = unsafe { CStr::from_ptr(eggdb_last_sqlstate()) };
    assert_eq!(state.to_bytes(), b"42P01");
    // SAFETY: the handle, given up.
    assert_eq!(unsafe { eggdb_sql_close(sql) }, EGGDB_OK);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
}

fn served() -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let db = Arc::new(Database::in_memory().unwrap());
    let listener = eggdb_pgwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let config = ServerConfig {
        user: "eggdb".to_owned(),
        verifier: ScramVerifier::derive(PASSWORD.as_bytes(), b"0123456789abcdef", 4096),
        database: "eggdb".to_owned(),
        max_connections: 8,
        authentication_timeout: Duration::from_secs(5),
        idle_timeout: None,
        memory_budget: Arc::default(),
        auth_rules: None,
        tls: None,
    };
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let store = eggdb::sql::Store(&db);
        let stopped = move || server_stop.load(Ordering::Acquire);
        eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_| {}).unwrap();
    });
    (format!("postgresql://eggdb:{PASSWORD}@127.0.0.1:{port}/eggdb?sslmode=disable"), stop, server)
}

fn pool_counts(pool: *mut EggdbPool) -> (usize, usize) {
    let (mut open, mut idle) = (0, 0);
    // SAFETY: a live pool and places for the counts.
    assert_eq!(unsafe { eggdb_pool_stats(pool, &mut open, &mut idle) }, EGGDB_OK);
    (open, idle)
}

fn acquire(pool: *mut EggdbPool, timeout_ms: u64) -> (c_int, *mut EggdbSql) {
    let mut sql: *mut EggdbSql = ptr::null_mut();
    // SAFETY: a live pool and a place for the session.
    let status = unsafe { eggdb_pool_acquire(pool, timeout_ms, &mut sql) };
    (status, sql)
}

#[test]
fn a_pool_lends_sessions_up_to_its_size_and_rolls_back_what_they_leave_open() {
    let (url, stop, server) = served();
    let url = CString::new(url).unwrap();
    let mut pool: *mut EggdbPool = ptr::null_mut();
    // SAFETY: a URL, a size and a place for the pool.
    assert_eq!(unsafe { eggdb_pool_open(url.as_ptr(), 2, &mut pool) }, EGGDB_OK);
    assert_eq!(pool_counts(pool), (1, 1));
    let (status, first) = acquire(pool, 0);
    assert_eq!(status, EGGDB_OK);
    assert_eq!(run(first, "CREATE TABLE counters (n integer)", &[]).0, EGGDB_OK);
    assert_eq!(run(first, "BEGIN; INSERT INTO counters VALUES (1)", &[]).0, EGGDB_OK);
    let (status, second) = acquire(pool, 0);
    assert_eq!(status, EGGDB_OK);
    assert_eq!(pool_counts(pool), (2, 0));
    assert_eq!(acquire(pool, 20).0, EGGDB_ERR_IN_USE);
    // SAFETY: the pool lent `first`, which this test no longer uses.
    assert_eq!(unsafe { eggdb_pool_release(pool, first) }, EGGDB_OK);
    assert_eq!(pool_counts(pool), (2, 1));
    let (status, again) = acquire(pool, 0);
    assert_eq!(status, EGGDB_OK);
    let (status, results) = run(again, "SELECT count(*) FROM counters", &[]);
    assert_eq!(status, EGGDB_OK);
    let rows = results.iter().find(|result| result.column_count > 0).unwrap();
    // SAFETY: the session lends the result's values until its next call.
    assert_eq!(unsafe { (*rows.values).integer }, 0);
    // SAFETY: as above, for a session the pool did not lend.
    assert_eq!(unsafe { eggdb_pool_release(pool, ptr::null_mut()) }, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: a live pool with sessions still lent.
    assert_eq!(unsafe { eggdb_pool_close(pool) }, EGGDB_ERR_IN_USE);
    // SAFETY: the pool lent both, which this test no longer uses.
    assert_eq!(unsafe { eggdb_pool_release(pool, again) }, EGGDB_OK);
    // SAFETY: as above.
    assert_eq!(unsafe { eggdb_pool_release(pool, second) }, EGGDB_OK);
    // SAFETY: a live pool, given up here.
    assert_eq!(unsafe { eggdb_pool_close(pool) }, EGGDB_OK);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
}
