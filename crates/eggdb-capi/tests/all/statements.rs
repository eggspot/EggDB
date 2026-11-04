//! Prepared statements, cancelling and `COPY` through the C API, called as a C program calls
//! them: prepare once and run many times with typed parameters, column outlines before a run,
//! a running statement cancelled from another thread, bulk loads with `COPY ... FROM STDIN`, and
//! every misuse answered with a status.

use eggdb_capi::{
    EGGDB_BLOB, EGGDB_ERR_IN_USE, EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_SQL, EGGDB_INTEGER,
    EGGDB_NULL, EGGDB_OK, EGGDB_REAL, EGGDB_TEXT, EggdbDb, EggdbResult, EggdbSql, EggdbStmt,
    EggdbValue, eggdb_close, eggdb_last_error, eggdb_last_sqlstate, eggdb_open, eggdb_sql_cancel,
    eggdb_sql_close, eggdb_sql_copy_in, eggdb_sql_execute, eggdb_sql_open, eggdb_sql_prepare,
    eggdb_stmt_bind_blob, eggdb_stmt_bind_bool, eggdb_stmt_bind_double, eggdb_stmt_bind_int64,
    eggdb_stmt_bind_null, eggdb_stmt_bind_text, eggdb_stmt_clear_bindings, eggdb_stmt_column_count,
    eggdb_stmt_column_name, eggdb_stmt_column_type, eggdb_stmt_execute, eggdb_stmt_finalize,
    eggdb_stmt_parameter_count, eggdb_stmt_reset,
};
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct TempPath {
    path: PathBuf,
}

impl TempPath {
    fn new(name: &str) -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let file = format!("eggdb-capi-stmt-{name}-{}-{number}.db", std::process::id());
        Self {
            path: std::env::temp_dir().join(file),
        }
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

type Cell = (i32, i64, f64, Option<Vec<u8>>);

fn c_text(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| panic!("{text:?} holds a NUL byte"))
}

fn last_error() -> String {
    // SAFETY: eggdb_last_error returns a NUL-terminated string that stays valid until the next
    // failing call on this thread, and none happens before it is copied.
    unsafe { CStr::from_ptr(eggdb_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn last_sqlstate() -> String {
    // SAFETY: as for eggdb_last_error.
    unsafe { CStr::from_ptr(eggdb_last_sqlstate()) }
        .to_string_lossy()
        .into_owned()
}

fn ok(status: c_int) {
    assert_eq!(status, EGGDB_OK, "{}", last_error());
}

fn open_db(file: &TempPath) -> *mut EggdbDb {
    let path = c_text(&file.path.display().to_string());
    let mut db = ptr::null_mut();
    // SAFETY: a NUL-terminated path, no options, and a place for the handle.
    ok(unsafe { eggdb_open(path.as_ptr(), ptr::null(), &mut db) });
    db
}

fn close_db(db: *mut EggdbDb) {
    // SAFETY: a live handle from eggdb_open, given up here.
    ok(unsafe { eggdb_close(db) });
}

fn open_sql(db: *mut EggdbDb, dialect: &str) -> *mut EggdbSql {
    let dialect = c_text(dialect);
    let mut sql = ptr::null_mut();
    // SAFETY: a live database handle, a dialect name and a place for the session.
    ok(unsafe { eggdb_sql_open(db, dialect.as_ptr(), &mut sql) });
    sql
}

fn close_sql(sql: *mut EggdbSql) {
    // SAFETY: a live session with no statements left, given up here.
    ok(unsafe { eggdb_sql_close(sql) });
}

fn lent_results(results: *const EggdbResult, count: usize) -> Vec<EggdbResult> {
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: the handle lends `count` results until its next call.
    unsafe { std::slice::from_raw_parts(results, count) }.to_vec()
}

fn execute(sql: *mut EggdbSql, text: &str) -> (c_int, Vec<EggdbResult>) {
    let text = c_text(text);
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: a live session, the statement text and places for the results.
    let status = unsafe { eggdb_sql_execute(sql, text.as_ptr(), &mut results, &mut count) };
    (status, lent_results(results, count))
}

fn copy_in(sql: *mut EggdbSql, text: &str, data: &[u8]) -> (c_int, Vec<EggdbResult>) {
    let text = c_text(text);
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: a live session, the statement text, the data's bytes and places for the results.
    let status = unsafe {
        eggdb_sql_copy_in(
            sql,
            text.as_ptr(),
            data.as_ptr(),
            data.len(),
            &mut results,
            &mut count,
        )
    };
    (status, lent_results(results, count))
}

fn prepare(sql: *mut EggdbSql, text: &str) -> (c_int, *mut EggdbStmt) {
    let mut stmt = ptr::null_mut();
    // SAFETY: a live session, the text's bytes with their length, and a place for the handle.
    let status =
        unsafe { eggdb_sql_prepare(sql, text.as_ptr().cast::<c_char>(), text.len(), &mut stmt) };
    (status, stmt)
}

fn prepared(sql: *mut EggdbSql, text: &str) -> *mut EggdbStmt {
    let (status, stmt) = prepare(sql, text);
    ok(status);
    assert!(!stmt.is_null());
    stmt
}

fn run(stmt: *mut EggdbStmt) -> (c_int, Vec<EggdbResult>) {
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: a live statement and places for the results.
    let status = unsafe { eggdb_stmt_execute(stmt, &mut results, &mut count) };
    (status, lent_results(results, count))
}

fn finalize(stmt: *mut EggdbStmt) {
    // SAFETY: a live statement, given up here.
    ok(unsafe { eggdb_stmt_finalize(stmt) });
}

fn bind_text(stmt: *mut EggdbStmt, index: usize, text: &str) -> c_int {
    // SAFETY: a live statement and the text's bytes with their length.
    unsafe { eggdb_stmt_bind_text(stmt, index, text.as_ptr().cast::<c_char>(), text.len()) }
}

fn bind_blob(stmt: *mut EggdbStmt, index: usize, data: &[u8]) -> c_int {
    // SAFETY: a live statement and the bytes with their length.
    unsafe { eggdb_stmt_bind_blob(stmt, index, data.as_ptr(), data.len()) }
}

fn tag(result: &EggdbResult) -> String {
    // SAFETY: the tag is a NUL-terminated string the handle lends.
    unsafe { CStr::from_ptr(result.tag) }
        .to_string_lossy()
        .into_owned()
}

fn cells(result: &EggdbResult) -> Vec<Vec<Cell>> {
    if result.row_count == 0 || result.column_count == 0 {
        return Vec::new();
    }
    // SAFETY: the handle lends `row_count × column_count` values until its next call.
    let values: &[EggdbValue] = unsafe {
        std::slice::from_raw_parts(result.values, result.row_count * result.column_count)
    };
    values
        .chunks(result.column_count)
        .map(|row| {
            row.iter()
                .map(|value| {
                    let data = (!value.data.is_null()).then(|| {
                        // SAFETY: the handle lends `len` bytes at `data` until its next call.
                        unsafe { std::slice::from_raw_parts(value.data, value.len) }.to_vec()
                    });
                    (value.kind, value.integer, value.real, data)
                })
                .collect()
        })
        .collect()
}

fn single_count(sql: *mut EggdbSql, text: &str) -> i64 {
    let (status, results) = execute(sql, text);
    ok(status);
    cells(&results[0])[0][0].1
}

fn column_outline(stmt: *mut EggdbStmt) -> Vec<(String, i32)> {
    let mut count = usize::MAX;
    // SAFETY: a live statement and a place for the count.
    ok(unsafe { eggdb_stmt_column_count(stmt, &mut count) });
    (0..count)
        .map(|column| {
            let mut name: *const c_char = ptr::null();
            let mut kind = -1;
            // SAFETY: a live statement, a column in range and places for the answers.
            ok(unsafe { eggdb_stmt_column_name(stmt, column, &mut name) });
            // SAFETY: as above.
            ok(unsafe { eggdb_stmt_column_type(stmt, column, &mut kind) });
            // SAFETY: the statement lends a NUL-terminated name until it is finalized.
            let name = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned();
            (name, kind)
        })
        .collect()
}

fn parameter_count(stmt: *mut EggdbStmt) -> usize {
    let mut count = usize::MAX;
    // SAFETY: a live statement and a place for the count.
    ok(unsafe { eggdb_stmt_parameter_count(stmt, &mut count) });
    count
}

#[test]
fn a_statement_prepared_once_runs_many_times_with_typed_parameters() {
    let file = TempPath::new("typed");
    let db = open_db(&file);
    let sql = open_sql(db, "postgresql");
    ok(execute(
        sql,
        "CREATE TABLE items (id bigint PRIMARY KEY, name text, price double precision, active boolean, data bytea)",
    )
    .0);

    let insert = prepared(sql, "INSERT INTO items VALUES ($1, $2, $3, $4, $5)");
    assert_eq!(parameter_count(insert), 5);
    assert!(column_outline(insert).is_empty());
    let rows: [(i64, &str, f64, c_int, &[u8]); 3] = [
        (1, "O'Brien", 2.5, 1, &[0, 1, 0xff]),
        (2, "Zoë", 0.1, 0, b""),
        (i64::MAX, "big", -1e300, 1, b"\\x"),
    ];
    for (id, name, price, active, data) in rows {
        // SAFETY: a live statement and a parameter in range.
        ok(unsafe { eggdb_stmt_bind_int64(insert, 1, id) });
        ok(bind_text(insert, 2, name));
        // SAFETY: as above.
        ok(unsafe { eggdb_stmt_bind_double(insert, 3, price) });
        // SAFETY: as above.
        ok(unsafe { eggdb_stmt_bind_bool(insert, 4, active) });
        ok(bind_blob(insert, 5, data));
        let (status, results) = run(insert);
        ok(status);
        assert_eq!(results.len(), 1);
        assert_eq!(tag(&results[0]), "INSERT 0 1");
    }
    // SAFETY: a live statement.
    ok(unsafe { eggdb_stmt_clear_bindings(insert) });
    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(insert, 1, 4) });
    ok(run(insert).0);
    // SAFETY: a live statement.
    ok(unsafe { eggdb_stmt_reset(insert) });
    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(insert, 1, 5) });
    // SAFETY: as above.
    ok(unsafe { eggdb_stmt_bind_null(insert, 2) });
    ok(run(insert).0);
    finalize(insert);

    let select = prepared(
        sql,
        "SELECT id, name, price, active, data FROM items WHERE id >= $1 ORDER BY id",
    );
    assert_eq!(parameter_count(select), 1);
    assert_eq!(
        column_outline(select),
        [
            ("id".to_owned(), EGGDB_INTEGER),
            ("name".to_owned(), EGGDB_TEXT),
            ("price".to_owned(), EGGDB_REAL),
            ("active".to_owned(), EGGDB_INTEGER),
            ("data".to_owned(), EGGDB_BLOB),
        ]
    );
    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(select, 1, 2) });
    let (status, results) = run(select);
    ok(status);
    assert_eq!(tag(&results[0]), "SELECT 4");
    let rows = cells(&results[0]);
    assert_eq!(rows[0][0].1, 2);
    assert_eq!(rows[0][1].3.as_deref(), Some("Zoë".as_bytes()));
    assert_eq!((rows[0][2].0, rows[0][2].2), (EGGDB_REAL, 0.1));
    assert_eq!((rows[0][3].0, rows[0][3].1), (EGGDB_INTEGER, 0));
    assert_eq!(
        (rows[0][4].0, rows[0][4].3.as_deref()),
        (EGGDB_BLOB, Some(&b""[..]))
    );
    assert_eq!(rows[1][1].0, EGGDB_NULL);
    assert_eq!(rows[1][0].1, 4);
    assert_eq!(rows[2][2].0, EGGDB_NULL);
    assert_eq!(rows[3][0].1, i64::MAX);
    assert_eq!(rows[3][2].2, -1e300);
    assert_eq!(rows[3][4].3.as_deref(), Some(&b"\\x"[..]));

    let (status, other) = execute(sql, "SELECT 1");
    ok(status);
    assert_eq!(other.len(), 1);
    assert_eq!(
        cells(&results[0])[0][1].3.as_deref(),
        Some("Zoë".as_bytes()),
        "a statement's results stay until its own next call"
    );

    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(select, 1, 1) });
    let (status, results) = run(select);
    ok(status);
    let first = &cells(&results[0])[0];
    assert_eq!(first[1].3.as_deref(), Some(&b"O'Brien"[..]));
    assert_eq!(first[4].3.as_deref(), Some(&[0, 1, 0xff][..]));
    assert_eq!((first[3].0, first[3].1), (EGGDB_INTEGER, 1));
    finalize(select);
    close_sql(sql);
    close_db(db);
}

#[test]
fn placeholders_of_every_dialect_take_bound_values() {
    let file = TempPath::new("dialects");
    let db = open_db(&file);
    let sqlite = open_sql(db, "sqlite");
    let stmt = prepared(sqlite, "SELECT ? + 1, ?");
    assert_eq!(parameter_count(stmt), 2);
    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(stmt, 1, 41) });
    ok(bind_text(stmt, 2, "x"));
    let (status, results) = run(stmt);
    ok(status);
    let row = &cells(&results[0])[0];
    assert_eq!((row[0].0, row[0].1), (EGGDB_INTEGER, 42));
    assert_eq!(row[1].3.as_deref(), Some(&b"x"[..]));
    // SAFETY: a live statement.
    ok(unsafe { eggdb_stmt_clear_bindings(stmt) });
    let (status, results) = run(stmt);
    ok(status);
    assert_eq!(cells(&results[0])[0][1].0, EGGDB_NULL);
    finalize(stmt);
    close_sql(sqlite);

    let postgres = open_sql(db, "postgresql");
    let stmt = prepared(postgres, "SELECT $2, $1");
    assert_eq!(parameter_count(stmt), 2);
    // SAFETY: a live statement and parameters in range.
    ok(unsafe { eggdb_stmt_bind_double(stmt, 1, 1.5) });
    // SAFETY: as above.
    ok(unsafe { eggdb_stmt_bind_bool(stmt, 2, 7) });
    let (status, results) = run(stmt);
    ok(status);
    let row = &cells(&results[0])[0];
    assert_eq!((row[0].0, row[0].1), (EGGDB_INTEGER, 1));
    assert_eq!((row[1].0, row[1].2), (EGGDB_REAL, 1.5));
    finalize(stmt);

    let empty = prepared(postgres, "  ;  ");
    assert_eq!(parameter_count(empty), 0);
    assert!(column_outline(empty).is_empty());
    ok(run(empty).0);
    finalize(empty);
    close_sql(postgres);
    close_db(db);
}

#[test]
fn misused_statements_are_answered_with_a_status() {
    let file = TempPath::new("misuse");
    let db = open_db(&file);
    let sql = open_sql(db, "postgresql");
    ok(execute(sql, "CREATE TABLE t (n integer)").0);

    let (status, stmt) = prepare(sql, "SELECT 1; SELECT 2");
    assert_eq!(status, EGGDB_ERR_SQL);
    assert!(stmt.is_null());
    let (status, _) = prepare(sql, "SELEC 1");
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(last_sqlstate(), "42601");
    let (status, _) = prepare(sql, "SELECT * FROM no_such_table");
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(last_sqlstate(), "42P01");

    let mut stmt = ptr::null_mut();
    let not_utf8 = [b'S', 0xff];
    // SAFETY: a live session, two readable bytes and a place for the handle.
    let status =
        unsafe { eggdb_sql_prepare(sql, not_utf8.as_ptr().cast::<c_char>(), 2, &mut stmt) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: NULL text with a length is refused before it is read.
    let status = unsafe { eggdb_sql_prepare(sql, ptr::null(), 3, &mut stmt) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: a NULL session is refused.
    let status = unsafe { eggdb_sql_prepare(ptr::null_mut(), c"SELECT 1".as_ptr(), 8, &mut stmt) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: a NULL place for the handle is refused before anything else.
    let status = unsafe { eggdb_sql_prepare(sql, c"SELECT 1".as_ptr(), 8, ptr::null_mut()) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);

    let stmt = prepared(sql, "INSERT INTO t VALUES ($1)");
    // SAFETY: a live statement; index 0 and 2 are out of range and refused.
    let status = unsafe { eggdb_stmt_bind_int64(stmt, 0, 1) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: as above.
    let status = unsafe { eggdb_stmt_bind_int64(stmt, 2, 1) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("1 to 1"), "{}", last_error());
    assert_eq!(bind_text(stmt, 1, "not a number"), EGGDB_OK);
    let (status, results) = run(stmt);
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(last_sqlstate(), "22P02");
    assert!(results.is_empty());
    let invalid = [0xc3_u8];
    // SAFETY: a live statement and one readable byte.
    let status = unsafe { eggdb_stmt_bind_text(stmt, 1, invalid.as_ptr().cast::<c_char>(), 1) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    let mut name: *const c_char = ptr::null();
    // SAFETY: a live statement; it returns no rows, so column 0 is out of range.
    let status = unsafe { eggdb_stmt_column_name(stmt, 0, &mut name) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    assert!(name.is_null());
    let mut kind = 0;
    // SAFETY: as above.
    let status = unsafe { eggdb_stmt_column_type(stmt, 3, &mut kind) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);

    // SAFETY: a live session with a live statement; the refusal leaves it open.
    let status = unsafe { eggdb_sql_close(sql) };
    assert_eq!(status, EGGDB_ERR_IN_USE);
    assert!(last_error().contains("eggdb_stmt_finalize"));
    ok(execute(sql, "SELECT 1").0);
    finalize(stmt);

    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: NULL handles are refused.
    let status = unsafe { eggdb_stmt_execute(ptr::null_mut(), &mut results, &mut count) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: as above.
    let status = unsafe { eggdb_stmt_bind_null(ptr::null_mut(), 1) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: as above.
    let status = unsafe { eggdb_stmt_reset(ptr::null_mut()) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: as above.
    let status = unsafe { eggdb_stmt_parameter_count(ptr::null_mut(), &mut count) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: as above.
    let status = unsafe { eggdb_sql_cancel(ptr::null_mut()) };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    // SAFETY: NULL is accepted and does nothing.
    let status = unsafe { eggdb_stmt_finalize(ptr::null_mut()) };
    ok(status);
    close_sql(sql);
    close_db(db);
}

#[test]
fn a_running_statement_is_cancelled_from_another_thread() {
    let file = TempPath::new("cancel");
    let db = open_db(&file);
    let sql = open_sql(db, "postgresql");
    // SAFETY: a live session with no statement running: the cancel does nothing.
    ok(unsafe { eggdb_sql_cancel(sql) });
    assert_eq!(single_count(sql, "SELECT 1"), 1);

    let address = sql as usize;
    let finished = Arc::new(AtomicBool::new(false));
    let runner_finished = Arc::clone(&finished);
    let started = Instant::now();
    let runner = std::thread::spawn(move || {
        let sql = address as *mut EggdbSql;
        let (status, results) = execute(sql, "SELECT pg_sleep(30)");
        let sqlstate = last_sqlstate();
        runner_finished.store(true, Ordering::Release);
        (status, results.len(), sqlstate)
    });
    while !finished.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: a live session; cancelling is the one call allowed beside the running one.
        ok(unsafe { eggdb_sql_cancel(sql) });
    }
    let (status, count, sqlstate) = runner
        .join()
        .unwrap_or_else(|_| panic!("the statement's thread panicked"));
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(count, 0);
    assert_eq!(sqlstate, "57014");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        single_count(sql, "SELECT 2"),
        2,
        "the next statement runs after a cancel"
    );
    close_sql(sql);
    close_db(db);
}

#[test]
fn copy_from_stdin_bulk_loads_rows() {
    let file = TempPath::new("copy");
    let db = open_db(&file);
    let sql = open_sql(db, "postgresql");
    ok(execute(sql, "CREATE TABLE people (id integer, name text)").0);

    let (status, results) = copy_in(sql, "COPY people FROM STDIN", b"1\tAda\n2\tGrace\n");
    ok(status);
    assert_eq!(results.len(), 1);
    assert_eq!(tag(&results[0]), "COPY 2");
    let (status, results) = copy_in(
        sql,
        "COPY people (id, name) FROM STDIN (FORMAT csv)",
        b"3,\"Lin, Wu\"\n",
    );
    ok(status);
    assert_eq!(tag(&results[0]), "COPY 1");
    assert_eq!(single_count(sql, "SELECT count(*) FROM people"), 3);

    let (status, results) = copy_in(sql, "COPY people FROM STDIN", b"4\tok\nx\tbad\n");
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(last_sqlstate(), "22P02");
    assert!(results.is_empty());
    assert_eq!(
        single_count(sql, "SELECT count(*) FROM people"),
        3,
        "a failed COPY loads nothing"
    );

    let many: String = (0..10_000)
        .map(|id| format!("{id}\tperson {id}\n"))
        .collect();
    assert!(many.len() > 64 * 1024);
    let (status, results) = copy_in(sql, "COPY people FROM STDIN", many.as_bytes());
    ok(status);
    assert_eq!(tag(&results[0]), "COPY 10000");
    assert_eq!(single_count(sql, "SELECT count(*) FROM people"), 10_003);

    let (status, _) = copy_in(
        sql,
        "COPY people FROM STDIN; COPY people FROM STDIN",
        b"5\tagain\n",
    );
    assert_eq!(status, EGGDB_ERR_SQL);
    let (status, _) = copy_in(sql, "COPY people TO STDOUT", b"");
    assert_eq!(status, EGGDB_ERR_SQL);
    let (status, results) = copy_in(sql, "COPY people FROM STDIN", b"");
    ok(status);
    assert_eq!(tag(&results[0]), "COPY 0");

    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0;
    // SAFETY: NULL data with a length is refused before it is read.
    let status = unsafe {
        eggdb_sql_copy_in(
            sql,
            c"COPY people FROM STDIN".as_ptr(),
            ptr::null(),
            4,
            &mut results,
            &mut count,
        )
    };
    assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    close_sql(sql);
    close_db(db);
}
