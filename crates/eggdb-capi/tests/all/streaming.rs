//! Reading a statement's rows one at a time, `COPY ... TO STDOUT` read in pieces, a `COPY ...
//! FROM STDIN` fed piece by piece, and cancelling, called as a C program calls them, on a
//! session in the process and on one from `eggdb_sql_connect` to a server in the same process:
//! every scenario runs on both and must answer the same.

use eggdb::Database;
use eggdb_capi::{
    EGGDB_BLOB, EGGDB_ERR_IN_USE, EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_SQL, EGGDB_INTEGER,
    EGGDB_NULL, EGGDB_OK, EGGDB_REAL, EGGDB_TEXT, EggdbCopy, EggdbDb, EggdbResult, EggdbSql,
    EggdbStmt, EggdbValue, eggdb_close, eggdb_copy_end, eggdb_copy_free, eggdb_copy_put,
    eggdb_copy_read, eggdb_last_error, eggdb_last_sqlstate, eggdb_open, eggdb_sql_cancel,
    eggdb_sql_close, eggdb_sql_connect, eggdb_sql_copy_in, eggdb_sql_copy_in_begin,
    eggdb_sql_copy_out_begin, eggdb_sql_execute, eggdb_sql_open, eggdb_sql_prepare,
    eggdb_stmt_bind_int64, eggdb_stmt_column_count, eggdb_stmt_column_name, eggdb_stmt_execute,
    eggdb_stmt_finalize, eggdb_stmt_parameter_count, eggdb_stmt_reset, eggdb_stmt_row,
    eggdb_stmt_step, eggdb_stmt_value_blob, eggdb_stmt_value_double, eggdb_stmt_value_int64,
    eggdb_stmt_value_kind, eggdb_stmt_value_text,
};
use eggdb_pgwire::{ScramVerifier, ServerConfig};
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PASSWORD: &str = "streaming-test-password";
const PIECE_BYTES: usize = 64 * 1024;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn c_text(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| panic!("{text:?} holds a NUL byte"))
}

fn last_error() -> String {
    // SAFETY: eggdb_last_error returns a NUL-terminated string valid until the thread's next
    // failing call, and none happens before it is copied.
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

enum Place {
    InProcess {
        db: *mut EggdbDb,
        path: PathBuf,
    },
    OnAServer {
        stop: Arc<AtomicBool>,
        server: Option<JoinHandle<()>>,
    },
}

struct Fixture {
    sql: *mut EggdbSql,
    place: Place,
}

impl Fixture {
    fn in_process(name: &str) -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-capi-streaming-{name}-{}-{number}.db",
            std::process::id()
        ));
        let path_text = c_text(&path.display().to_string());
        let mut db = ptr::null_mut();
        // SAFETY: a NUL-terminated path, no options and a place for the handle.
        ok(unsafe { eggdb_open(path_text.as_ptr(), ptr::null(), &mut db) });
        let mut sql = ptr::null_mut();
        // SAFETY: a live database, a dialect name and a place for the session.
        ok(unsafe { eggdb_sql_open(db, c"postgresql".as_ptr(), &mut sql) });
        Self {
            sql,
            place: Place::InProcess { db, path },
        }
    }

    fn on_a_server() -> Self {
        let db = Arc::new(Database::in_memory().unwrap_or_else(|error| panic!("{error}")));
        let listener = eggdb_pgwire::listen_on_loopback(
            "127.0.0.1:0"
                .parse()
                .unwrap_or_else(|error| panic!("{error}")),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let port = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"))
            .port();
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
            let served = eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_| {});
            assert!(served.is_ok(), "the server stopped with {served:?}");
        });
        let url = c_text(&format!(
            "postgresql://eggdb:{PASSWORD}@127.0.0.1:{port}/eggdb?sslmode=disable"
        ));
        let mut sql = ptr::null_mut();
        // SAFETY: a NUL-terminated URL and a place for the session.
        ok(unsafe { eggdb_sql_connect(url.as_ptr(), &mut sql) });
        Self {
            sql,
            place: Place::OnAServer {
                stop,
                server: Some(server),
            },
        }
    }

    fn both(name: &str, scenario: impl Fn(*mut EggdbSql)) {
        for fixture in [Self::in_process(name), Self::on_a_server()] {
            scenario(fixture.sql);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: the fixture's session, with every statement and copy freed, given up here.
        let closed = unsafe { eggdb_sql_close(self.sql) };
        match &mut self.place {
            Place::InProcess { db, path } => {
                // SAFETY: the fixture's database, with its one session closed, given up here.
                let _ = unsafe { eggdb_close(*db) };
                let _ = std::fs::remove_file(path);
            }
            Place::OnAServer { stop, server } => {
                stop.store(true, Ordering::Release);
                if let Some(server) = server.take() {
                    let _ = server.join();
                }
            }
        }
        if !std::thread::panicking() {
            assert_eq!(closed, EGGDB_OK, "{}", last_error());
        }
    }
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

fn tag(result: &EggdbResult) -> String {
    // SAFETY: the tag is a NUL-terminated string the handle lends.
    unsafe { CStr::from_ptr(result.tag) }
        .to_string_lossy()
        .into_owned()
}

fn first_integer(sql: *mut EggdbSql, text: &str) -> i64 {
    let (status, results) = execute(sql, text);
    ok(status);
    let result = results[0];
    assert!(result.row_count > 0 && result.column_count > 0);
    // SAFETY: the result lends at least one value until the session's next call.
    let value = unsafe { &*result.values };
    value.integer
}

fn prepared(sql: *mut EggdbSql, text: &str) -> *mut EggdbStmt {
    let mut stmt = ptr::null_mut();
    // SAFETY: a live session, the text's bytes with their length and a place for the handle.
    ok(unsafe { eggdb_sql_prepare(sql, text.as_ptr().cast::<c_char>(), text.len(), &mut stmt) });
    assert!(!stmt.is_null());
    stmt
}

fn step(stmt: *mut EggdbStmt) -> (c_int, bool) {
    let mut has_row = -1;
    // SAFETY: a live statement and a place for the answer.
    let status = unsafe { eggdb_stmt_step(stmt, &mut has_row) };
    (status, has_row == 1)
}

fn finalize(stmt: *mut EggdbStmt) {
    // SAFETY: a live statement, given up here.
    ok(unsafe { eggdb_stmt_finalize(stmt) });
}

fn reset(stmt: *mut EggdbStmt) {
    // SAFETY: a live statement.
    ok(unsafe { eggdb_stmt_reset(stmt) });
}

fn bind_int64(stmt: *mut EggdbStmt, index: usize, value: i64) {
    // SAFETY: a live statement and a parameter in range.
    ok(unsafe { eggdb_stmt_bind_int64(stmt, index, value) });
}

fn value_int64(stmt: *mut EggdbStmt, column: usize) -> (c_int, i64) {
    let mut value = -1;
    // SAFETY: a live statement and a place for the value.
    let status = unsafe { eggdb_stmt_value_int64(stmt, column, &mut value) };
    (status, value)
}

fn value_double(stmt: *mut EggdbStmt, column: usize) -> (c_int, f64) {
    let mut value = -1.0;
    // SAFETY: a live statement and a place for the value.
    let status = unsafe { eggdb_stmt_value_double(stmt, column, &mut value) };
    (status, value)
}

fn value_kind(stmt: *mut EggdbStmt, column: usize) -> i32 {
    let mut kind = -1;
    // SAFETY: a live statement and a place for the kind.
    ok(unsafe { eggdb_stmt_value_kind(stmt, column, &mut kind) });
    kind
}

fn value_text(stmt: *mut EggdbStmt, column: usize) -> (c_int, Option<String>) {
    let mut text: *const c_char = ptr::null();
    let mut len = usize::MAX;
    // SAFETY: a live statement and places for the text.
    let status = unsafe { eggdb_stmt_value_text(stmt, column, &mut text, &mut len) };
    if text.is_null() {
        return (status, None);
    }
    // SAFETY: the statement lends `len` bytes of text, NUL-terminated, until its next step.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len + 1) };
    assert_eq!(bytes[len], 0, "the text is NUL-terminated");
    (
        status,
        Some(String::from_utf8_lossy(&bytes[..len]).into_owned()),
    )
}

fn value_blob(stmt: *mut EggdbStmt, column: usize) -> Option<Vec<u8>> {
    let mut data: *const u8 = ptr::null();
    let mut len = usize::MAX;
    // SAFETY: a live statement and places for the bytes.
    ok(unsafe { eggdb_stmt_value_blob(stmt, column, &mut data, &mut len) });
    if data.is_null() {
        assert_eq!(len, 0);
        return None;
    }
    // SAFETY: the statement lends `len` bytes until its next step.
    Some(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
}

fn row_kinds(stmt: *mut EggdbStmt) -> (c_int, Vec<i32>) {
    let mut values: *const EggdbValue = ptr::null();
    let mut count = usize::MAX;
    // SAFETY: a live statement and places for the row.
    let status = unsafe { eggdb_stmt_row(stmt, &mut values, &mut count) };
    if values.is_null() {
        return (status, Vec::new());
    }
    // SAFETY: the statement lends `count` values until its next step.
    let row = unsafe { std::slice::from_raw_parts(values, count) };
    (status, row.iter().map(|value| value.kind).collect())
}

fn column_names(stmt: *mut EggdbStmt) -> Vec<String> {
    let mut count = usize::MAX;
    // SAFETY: a live statement and a place for the count.
    ok(unsafe { eggdb_stmt_column_count(stmt, &mut count) });
    (0..count)
        .map(|column| {
            let mut name: *const c_char = ptr::null();
            // SAFETY: a live statement, a column in range and a place for the name.
            ok(unsafe { eggdb_stmt_column_name(stmt, column, &mut name) });
            // SAFETY: the statement lends a NUL-terminated name.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn name_pointer(stmt: *mut EggdbStmt, column: usize) -> *const c_char {
    let mut name: *const c_char = ptr::null();
    // SAFETY: a live statement, a column in range and a place for the name.
    ok(unsafe { eggdb_stmt_column_name(stmt, column, &mut name) });
    name
}

fn read_name(name: *const c_char) -> String {
    // SAFETY: the statement lends a NUL-terminated name until it is finalized, and the
    // statement this pointer came from has not been finalized yet.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn a_column_name_read_before_the_run_stays_valid_until_finalize() {
    Fixture::both("names", |sql| {
        load_numbers(sql, 3);
        let select = prepared(sql, "SELECT id, label FROM numbers ORDER BY id");
        let label = name_pointer(select, 1);
        assert_eq!(read_name(label), "label");
        let (status, found) = step(select);
        ok(status);
        assert!(found);
        assert_eq!(read_name(label), "label");
        assert_eq!(name_pointer(select, 1), label);
        reset(select);
        let (status, found) = step(select);
        ok(status);
        assert!(found);
        assert_eq!(read_name(label), "label");
        finalize(select);
    });
}

fn load_numbers(sql: *mut EggdbSql, count: usize) {
    ok(execute(
        sql,
        "CREATE TABLE numbers (id bigint PRIMARY KEY, label text, ratio double precision, raw bytea, amount numeric)",
    )
    .0);
    let rows: Vec<String> = (1..=count)
        .map(|id| {
            let label = if id % 100 == 0 {
                "NULL".to_owned()
            } else {
                format!("'number {id:05} of the streaming test'")
            };
            format!(
                "({id}, {label}, {}, '\\x{:02x}ff', {id}.5)",
                id as f64 / 4.0,
                id % 256
            )
        })
        .collect();
    ok(execute(
        sql,
        &format!("INSERT INTO numbers VALUES {}", rows.join(", ")),
    )
    .0);
}

#[test]
fn a_statement_steps_through_its_rows_one_at_a_time() {
    Fixture::both("step", |sql| {
        load_numbers(sql, 500);
        let select = prepared(
            sql,
            "SELECT id, label, ratio, raw, amount FROM numbers WHERE id > $1 ORDER BY id",
        );
        let mut parameters = 0;
        // SAFETY: a live statement and a place for the count.
        ok(unsafe { eggdb_stmt_parameter_count(select, &mut parameters) });
        assert_eq!(parameters, 1);
        bind_int64(select, 1, 10);
        assert_eq!(row_kinds(select).0, EGGDB_ERR_INVALID_ARGUMENT);

        let (status, found) = step(select);
        ok(status);
        assert!(found);
        assert_eq!(
            column_names(select),
            ["id", "label", "ratio", "raw", "amount"]
        );
        let (status, kinds) = row_kinds(select);
        ok(status);
        assert_eq!(
            kinds,
            [
                EGGDB_INTEGER,
                EGGDB_TEXT,
                EGGDB_REAL,
                EGGDB_BLOB,
                EGGDB_TEXT
            ]
        );
        assert_eq!(value_int64(select, 0), (EGGDB_OK, 11));
        assert_eq!(
            value_text(select, 1),
            (
                EGGDB_OK,
                Some("number 00011 of the streaming test".to_owned())
            )
        );
        assert_eq!(value_double(select, 2), (EGGDB_OK, 2.75));
        assert_eq!(value_blob(select, 3), Some(vec![11, 0xff]));
        assert_eq!(value_double(select, 4), (EGGDB_OK, 11.5));
        assert_eq!(value_int64(select, 4).0, EGGDB_ERR_INVALID_ARGUMENT);
        assert_eq!(value_double(select, 0), (EGGDB_OK, 11.0));
        assert_eq!(value_text(select, 0), (EGGDB_OK, Some("11".to_owned())));
        assert_eq!(value_text(select, 3).0, EGGDB_ERR_INVALID_ARGUMENT);
        assert_eq!(value_int64(select, 5).0, EGGDB_ERR_INVALID_ARGUMENT);

        assert_eq!(execute(sql, "SELECT 1").0, EGGDB_ERR_IN_USE);
        let mut other = ptr::null_mut();
        // SAFETY: a live session, the text's bytes and a place for the handle.
        let status = unsafe { eggdb_sql_prepare(sql, c"SELECT 1".as_ptr(), 8, &mut other) };
        assert_eq!(status, EGGDB_ERR_IN_USE);
        assert!(other.is_null());

        let mut ids = vec![11];
        loop {
            let (status, found) = step(select);
            ok(status);
            if !found {
                break;
            }
            let id = value_int64(select, 0).1;
            if id % 100 == 0 {
                assert_eq!(value_kind(select, 1), EGGDB_NULL);
                assert_eq!(value_text(select, 1), (EGGDB_OK, None));
                assert_eq!(value_int64(select, 1).0, EGGDB_ERR_INVALID_ARGUMENT);
                assert_eq!(value_blob(select, 1), None);
            }
            ids.push(id);
        }
        assert_eq!(ids, (11..=500).collect::<Vec<i64>>());
        assert_eq!(step(select), (EGGDB_OK, false), "the end stays the end");
        assert_eq!(row_kinds(select).0, EGGDB_ERR_INVALID_ARGUMENT);
        assert_eq!(
            first_integer(sql, "SELECT 7"),
            7,
            "the end frees the session"
        );

        reset(select);
        bind_int64(select, 1, 498);
        assert_eq!(step(select), (EGGDB_OK, true));
        assert_eq!(value_int64(select, 0), (EGGDB_OK, 499));
        reset(select);
        assert_eq!(
            first_integer(sql, "SELECT 8"),
            8,
            "a reset frees the session"
        );

        assert_eq!(step(select), (EGGDB_OK, true));
        let mut results: *const EggdbResult = ptr::null();
        let mut count = 0;
        // SAFETY: a live statement and places for the results.
        ok(unsafe { eggdb_stmt_execute(select, &mut results, &mut count) });
        let results = lent_results(results, count);
        assert_eq!(tag(&results[0]), "SELECT 2");
        assert_eq!(first_integer(sql, "SELECT 9"), 9);

        assert_eq!(step(select), (EGGDB_OK, true));
        finalize(select);
        assert_eq!(
            first_integer(sql, "SELECT 10"),
            10,
            "finalizing frees the session"
        );

        let update = prepared(sql, "UPDATE numbers SET label = 'changed' WHERE id = $1");
        bind_int64(update, 1, 3);
        assert_eq!(step(update), (EGGDB_OK, false));
        finalize(update);
        assert_eq!(
            first_integer(sql, "SELECT count(*) FROM numbers WHERE label = 'changed'"),
            1
        );

        let failing = prepared(sql, "SELECT 1 / (id - $1) FROM numbers ORDER BY id");
        bind_int64(failing, 1, 250);
        let mut status = EGGDB_OK;
        for _ in 0..1000 {
            let (stepped, found) = step(failing);
            status = stepped;
            if stepped != EGGDB_OK || !found {
                break;
            }
        }
        assert_eq!(status, EGGDB_ERR_SQL);
        assert_eq!(last_sqlstate(), "22012");
        assert_eq!(
            first_integer(sql, "SELECT 11"),
            11,
            "a failure ends the run"
        );
        finalize(failing);
    });
}

fn copy_out_begin(sql: *mut EggdbSql, text: &str) -> (c_int, *mut EggdbCopy) {
    let text = c_text(text);
    let mut copy = ptr::null_mut();
    // SAFETY: a live session, the statement text and a place for the handle.
    let status = unsafe { eggdb_sql_copy_out_begin(sql, text.as_ptr(), &mut copy) };
    (status, copy)
}

fn copy_in_begin(sql: *mut EggdbSql, text: &str) -> (c_int, *mut EggdbCopy) {
    let text = c_text(text);
    let mut copy = ptr::null_mut();
    // SAFETY: a live session, the statement text and a place for the handle.
    let status = unsafe { eggdb_sql_copy_in_begin(sql, text.as_ptr(), &mut copy) };
    (status, copy)
}

fn read_piece(copy: *mut EggdbCopy) -> (c_int, Option<Vec<u8>>) {
    let mut data: *const u8 = ptr::null();
    let mut len = usize::MAX;
    // SAFETY: a live copy and places for the piece.
    let status = unsafe { eggdb_copy_read(copy, &mut data, &mut len) };
    if data.is_null() {
        assert_eq!(len, 0);
        return (status, None);
    }
    // SAFETY: the copy lends `len` bytes until its next call.
    (
        status,
        Some(unsafe { std::slice::from_raw_parts(data, len) }.to_vec()),
    )
}

fn put(copy: *mut EggdbCopy, data: &[u8]) -> c_int {
    // SAFETY: a live copy and the bytes with their length.
    unsafe { eggdb_copy_put(copy, data.as_ptr(), data.len()) }
}

fn end(copy: *mut EggdbCopy) -> (c_int, Vec<EggdbResult>) {
    let mut results: *const EggdbResult = ptr::null();
    let mut count = usize::MAX;
    // SAFETY: a live copy and places for the results.
    let status = unsafe { eggdb_copy_end(copy, &mut results, &mut count) };
    if status != EGGDB_OK {
        assert!(results.is_null());
        assert_eq!(count, 0);
    }
    (status, lent_results(results, count))
}

fn free(copy: *mut EggdbCopy) {
    // SAFETY: a live copy (or NULL), given up here.
    unsafe { eggdb_copy_free(copy) };
}

#[test]
fn copy_to_stdout_is_read_in_pieces() {
    Fixture::both("copy-out", |sql| {
        load_numbers(sql, 5000);
        let (status, copy) = copy_out_begin(sql, "COPY numbers (id, label) TO STDOUT");
        ok(status);
        assert_eq!(execute(sql, "SELECT 1").0, EGGDB_ERR_IN_USE);
        // SAFETY: a live session with a copy open: closing it is refused.
        assert_eq!(unsafe { eggdb_sql_close(sql) }, EGGDB_ERR_IN_USE);
        assert_eq!(put(copy, b"1\tx\n"), EGGDB_ERR_INVALID_ARGUMENT);
        let mut data = Vec::new();
        let mut pieces = 0;
        loop {
            let (status, piece) = read_piece(copy);
            ok(status);
            let Some(piece) = piece else {
                break;
            };
            assert!(!piece.is_empty());
            assert!(
                piece.len() < 2 * PIECE_BYTES,
                "a piece is about 64 KiB at most"
            );
            data.extend_from_slice(&piece);
            pieces += 1;
        }
        assert!(pieces > 1, "more than 64 KiB comes in more than one piece");
        let expected: String = (1..=5000)
            .map(|id| {
                if id % 100 == 0 {
                    format!("{id}\t\\N\n")
                } else {
                    format!("{id}\tnumber {id:05} of the streaming test\n")
                }
            })
            .collect();
        assert_eq!(String::from_utf8_lossy(&data), expected);
        assert_eq!(read_piece(copy), (EGGDB_OK, None), "the end stays the end");
        let (status, results) = end(copy);
        ok(status);
        assert_eq!(results.len(), 1);
        assert_eq!(tag(&results[0]), "COPY 5000");
        assert_eq!(end(copy).0, EGGDB_ERR_INVALID_ARGUMENT);
        assert_eq!(read_piece(copy).0, EGGDB_ERR_INVALID_ARGUMENT);
        free(copy);
        assert_eq!(first_integer(sql, "SELECT 2"), 2);

        let (status, copy) = copy_out_begin(sql, "COPY numbers TO STDOUT (FORMAT csv)");
        ok(status);
        let (status, piece) = read_piece(copy);
        ok(status);
        assert!(piece.is_some_and(|piece| piece.starts_with(b"1,number 00001")));
        free(copy);
        assert_eq!(
            first_integer(sql, "SELECT 3"),
            3,
            "a copy freed early frees the session"
        );

        let (status, copy) = copy_out_begin(sql, "COPY numbers (id) TO STDOUT");
        ok(status);
        let (status, results) = end(copy);
        ok(status);
        assert_eq!(
            tag(&results[0]),
            "COPY 5000",
            "ending early reads to the end"
        );
        free(copy);

        let (status, copy) = copy_out_begin(sql, "SELECT 1");
        assert_eq!(status, EGGDB_ERR_SQL);
        assert_eq!(last_sqlstate(), "0A000");
        assert!(copy.is_null());
        let (status, copy) = copy_out_begin(sql, "COPY nowhere TO STDOUT");
        assert_eq!(status, EGGDB_ERR_SQL);
        assert_eq!(last_sqlstate(), "42P01");
        assert!(copy.is_null());
        assert_eq!(first_integer(sql, "SELECT 4"), 4);
        free(ptr::null_mut());
    });
}

#[test]
fn copy_from_stdin_is_fed_piece_by_piece() {
    Fixture::both("copy-in", |sql| {
        ok(execute(sql, "CREATE TABLE people (id integer, name text)").0);
        let rows: String = (0..20_000)
            .map(|id| format!("{id},\"Zoë, number {id}\"\n"))
            .collect();
        assert!(rows.len() > 4 * PIECE_BYTES);
        let (status, copy) = copy_in_begin(sql, "COPY people FROM STDIN (FORMAT csv)");
        ok(status);
        assert_eq!(execute(sql, "SELECT 1").0, EGGDB_ERR_IN_USE);
        assert_eq!(read_piece(copy).0, EGGDB_ERR_INVALID_ARGUMENT);
        for piece in rows.as_bytes().chunks(7) {
            ok(put(copy, piece));
        }
        ok(put(copy, b""));
        let (status, results) = end(copy);
        ok(status);
        assert_eq!(tag(&results[0]), "COPY 20000");
        assert_eq!(put(copy, b"1,again\n"), EGGDB_ERR_INVALID_ARGUMENT);
        assert_eq!(end(copy).0, EGGDB_ERR_INVALID_ARGUMENT);
        free(copy);
        assert_eq!(first_integer(sql, "SELECT count(*) FROM people"), 20_000);
        assert_eq!(
            first_integer(
                sql,
                "SELECT count(*) FROM people WHERE name = 'Zoë, number 19999'"
            ),
            1
        );

        let (status, copy) = copy_in_begin(sql, "COPY people FROM STDIN");
        ok(status);
        ok(put(copy, b"1\tok\n"));
        ok(put(copy, b"x\tbad\n"));
        let (status, results) = end(copy);
        assert_eq!(status, EGGDB_ERR_SQL);
        assert_eq!(last_sqlstate(), "22P02");
        assert!(results.is_empty());
        free(copy);
        assert_eq!(
            first_integer(sql, "SELECT count(*) FROM people"),
            20_000,
            "a failed COPY loads nothing"
        );

        let (status, copy) = copy_in_begin(sql, "COPY people FROM STDIN");
        ok(status);
        ok(put(copy, b"7\tabandoned\n"));
        free(copy);
        assert_eq!(
            first_integer(sql, "SELECT count(*) FROM people"),
            20_000,
            "a copy freed before its end loads nothing"
        );

        let (status, copy) = copy_in_begin(sql, "SELECT 1");
        if status == EGGDB_OK {
            assert_eq!(end(copy).0, EGGDB_ERR_SQL);
            free(copy);
        } else {
            assert_eq!(status, EGGDB_ERR_SQL);
            assert!(copy.is_null());
        }
        assert_eq!(last_sqlstate(), "0A000");

        let (status, copy) = copy_in_begin(sql, "COPY people FROM STDIN");
        ok(status);
        // SAFETY: NULL data with a length is refused before it is read.
        let status = unsafe { eggdb_copy_put(copy, ptr::null(), 3) };
        assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
        let (status, results) = end(copy);
        ok(status);
        assert_eq!(tag(&results[0]), "COPY 0");
        free(copy);

        let many: String = (0..10_000).map(|id| format!("{id}\tone call\n")).collect();
        let text = c_text("COPY people FROM STDIN");
        let mut results: *const EggdbResult = ptr::null();
        let mut count = 0;
        // SAFETY: a live session, the text, the data's bytes and places for the results.
        let status = unsafe {
            eggdb_sql_copy_in(
                sql,
                text.as_ptr(),
                many.as_ptr(),
                many.len(),
                &mut results,
                &mut count,
            )
        };
        ok(status);
        assert_eq!(tag(&lent_results(results, count)[0]), "COPY 10000");
        assert_eq!(first_integer(sql, "SELECT count(*) FROM people"), 30_000);

        // SAFETY: a NULL copy handle is refused.
        let status = unsafe { eggdb_copy_put(ptr::null_mut(), ptr::null(), 0) };
        assert_eq!(status, EGGDB_ERR_INVALID_ARGUMENT);
    });
}

#[test]
fn a_running_statement_is_cancelled_from_another_thread_on_either_session() {
    Fixture::both("cancel", |sql| {
        // SAFETY: a live session with nothing running: the cancel does nothing.
        ok(unsafe { eggdb_sql_cancel(sql) });
        assert_eq!(first_integer(sql, "SELECT 1"), 1);
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
        assert_eq!(
            (status, count, sqlstate.as_str()),
            (EGGDB_ERR_SQL, 0, "57014")
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(first_integer(sql, "SELECT 2"), 2, "the next statement runs");
    });
}

#[test]
fn statements_on_a_server_are_prepared_there_and_closed_when_finalized() {
    let fixture = Fixture::on_a_server();
    let sql = fixture.sql;
    ok(execute(sql, "CREATE TABLE items (id bigint PRIMARY KEY, name text)").0);
    for round in 0..4200 {
        let insert = prepared(sql, "INSERT INTO items VALUES ($1, 'item')");
        bind_int64(insert, 1, round);
        let mut results: *const EggdbResult = ptr::null();
        let mut count = 0;
        // SAFETY: a live statement and places for the results.
        ok(unsafe { eggdb_stmt_execute(insert, &mut results, &mut count) });
        finalize(insert);
    }
    assert_eq!(first_integer(sql, "SELECT count(*) FROM items"), 4200);

    let mut stmt = ptr::null_mut();
    let text = "SELECT * FROM nowhere";
    // SAFETY: a live session, the text's bytes and a place for the handle.
    let status =
        unsafe { eggdb_sql_prepare(sql, text.as_ptr().cast::<c_char>(), text.len(), &mut stmt) };
    assert_eq!(status, EGGDB_ERR_SQL);
    assert_eq!(last_sqlstate(), "42P01");
    assert!(stmt.is_null());
    let select = prepared(sql, "SELECT id, name FROM items WHERE id = $1");
    assert_eq!(column_names(select), ["id", "name"]);
    bind_int64(select, 1, 41);
    assert_eq!(step(select), (EGGDB_OK, true));
    assert_eq!(value_int64(select, 0), (EGGDB_OK, 41));
    assert_eq!(step(select), (EGGDB_OK, false));
    finalize(select);
}
