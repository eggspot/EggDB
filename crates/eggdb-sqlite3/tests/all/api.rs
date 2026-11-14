//! SQLite's C API on EggDB, called as a C program would.

use sqlite3::*;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

fn text(pointer: *const u8) -> String {
    assert!(!pointer.is_null());
    // SAFETY: column text is NUL-terminated and valid until the next step.
    unsafe { CStr::from_ptr(pointer.cast()) }
        .to_string_lossy()
        .into_owned()
}

#[test]
fn a_program_written_for_sqlite_runs_its_statements_on_eggdb() {
    let mut db = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated name and memory for one pointer.
    assert_eq!(unsafe { sqlite3_open(name.as_ptr(), &mut db) }, SQLITE_OK);
    let script = CString::new(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB);",
    )
    .unwrap();
    // SAFETY: a live connection and a NUL-terminated script.
    let code = unsafe { sqlite3_exec(db, script.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) };
    assert_eq!(code, SQLITE_OK);
    let insert = CString::new("INSERT INTO people (name, score, photo) VALUES (?, ?, ?)").unwrap();
    let mut statement = ptr::null_mut();
    // SAFETY: a live connection, NUL-terminated text and memory for one pointer.
    let prepared =
        unsafe { sqlite3_prepare_v2(db, insert.as_ptr(), -1, &mut statement, ptr::null_mut()) };
    assert_eq!(prepared, SQLITE_OK);
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_bind_parameter_count(statement) }, 3);
    for (index, person) in ["Ada", "Grace"].iter().enumerate() {
        let person = CString::new(*person).unwrap();
        let photo = [index as u8, 0xff];
        assert_eq!(
            // SAFETY: a live statement and NUL-terminated text.
            unsafe { sqlite3_bind_text(statement, 1, person.as_ptr(), -1, None) },
            SQLITE_OK
        );
        assert_eq!(
            // SAFETY: a live statement.
            unsafe { sqlite3_bind_double(statement, 2, 1.5 + index as f64) },
            SQLITE_OK
        );
        assert_eq!(
            // SAFETY: a live statement and two readable bytes.
            unsafe { sqlite3_bind_blob(statement, 3, photo.as_ptr().cast(), 2, None) },
            SQLITE_OK
        );
        // SAFETY: a live statement.
        assert_eq!(unsafe { sqlite3_bind_int(statement, 4, 1) }, SQLITE_RANGE);
        // SAFETY: a live statement with every parameter bound.
        assert_eq!(unsafe { sqlite3_step(statement) }, SQLITE_DONE);
        // SAFETY: a live connection.
        assert_eq!(unsafe { sqlite3_changes(db) }, 1);
        // SAFETY: a live statement.
        assert_eq!(unsafe { sqlite3_reset(statement) }, SQLITE_OK);
    }
    // SAFETY: a live connection.
    assert_eq!(unsafe { sqlite3_last_insert_rowid(db) }, 2);
    // SAFETY: the statement is finished with.
    unsafe { sqlite3_finalize(statement) };
    let select =
        CString::new("SELECT id, name, score, photo FROM people ORDER BY id; SELECT 1").unwrap();
    let mut tail: *const c_char = ptr::null();
    // SAFETY: as above, with memory for the tail pointer.
    let prepared =
        unsafe { sqlite3_prepare_v2(db, select.as_ptr(), -1, &mut statement, &mut tail) };
    assert_eq!(prepared, SQLITE_OK);
    assert_eq!(
        // SAFETY: the tail points into the given NUL-terminated text.
        unsafe { CStr::from_ptr(tail) }.to_str().unwrap().trim(),
        "SELECT 1"
    );
    let mut seen = Vec::new();
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_column_count(statement) }, 4);
    // SAFETY: a live statement.
    while unsafe { sqlite3_step(statement) } == SQLITE_ROW {
        // SAFETY: a live statement on a current row.
        assert_eq!(unsafe { sqlite3_column_type(statement, 0) }, SQLITE_INTEGER);
        // SAFETY: a live statement on a current row.
        assert_eq!(unsafe { sqlite3_column_type(statement, 3) }, SQLITE_BLOB);
        // SAFETY: a live statement on a current row.
        assert_eq!(unsafe { sqlite3_column_bytes(statement, 3) }, 2);
        // SAFETY: a live statement on a current row.
        let id = unsafe { sqlite3_column_int64(statement, 0) };
        // SAFETY: a live statement on a current row.
        let name = unsafe { sqlite3_column_text(statement, 1) };
        // SAFETY: a live statement on a current row.
        let score = unsafe { sqlite3_column_double(statement, 2) };
        seen.push((id, text(name), score));
    }
    // SAFETY: a live statement.
    let column = unsafe { sqlite3_column_name(statement, 1) };
    assert_eq!(text(column.cast()), "name");
    // SAFETY: the statement is finished with.
    unsafe { sqlite3_finalize(statement) };
    assert_eq!(
        seen,
        vec![(1, "Ada".to_owned(), 1.5), (2, "Grace".to_owned(), 2.5)]
    );
    let wrong = CString::new("SELECT * FROM nowhere").unwrap();
    let mut message: *mut c_char = ptr::null_mut();
    // SAFETY: a live connection and memory for the message pointer.
    let failed = unsafe { sqlite3_exec(db, wrong.as_ptr(), None, ptr::null_mut(), &mut message) };
    assert_eq!(failed, SQLITE_ERROR);
    assert!(
        // SAFETY: the message is NUL-terminated and lives until it is freed below.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .contains("nowhere")
    );
    // SAFETY: the message came from sqlite3_exec and is freed once.
    unsafe { sqlite3_free(message.cast::<c_void>()) };
    // SAFETY: a live connection.
    let error = unsafe { sqlite3_errmsg(db) };
    assert!(
        // SAFETY: the connection's message is NUL-terminated and lives while the connection does.
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .contains("nowhere")
    );
    // SAFETY: a live connection with no statement left open.
    assert_eq!(unsafe { sqlite3_close(db) }, SQLITE_OK);
}

unsafe extern "C" fn count_rows(
    argument: *mut c_void,
    columns: c_int,
    _values: *mut *mut c_char,
    _names: *mut *mut c_char,
) -> c_int {
    assert_eq!(columns, 1);
    // SAFETY: the test passes a pointer to its counter.
    unsafe { *argument.cast::<i32>() += 1 };
    0
}

#[test]
fn exec_calls_back_for_each_row() {
    let mut db = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    let script =
        CString::new("CREATE TABLE t(v); INSERT INTO t VALUES (1), (2), (3); SELECT v FROM t")
            .unwrap();
    let mut rows = 0i32;
    // SAFETY: a NUL-terminated name and memory for the connection pointer.
    unsafe { sqlite3_open(name.as_ptr(), &mut db) };
    // SAFETY: a live connection, NUL-terminated SQL, and the counter the callback increments.
    let code = unsafe {
        sqlite3_exec(
            db,
            script.as_ptr(),
            Some(count_rows),
            (&raw mut rows).cast(),
            ptr::null_mut(),
        )
    };
    assert_eq!(code, SQLITE_OK);
    // SAFETY: a live connection.
    unsafe { sqlite3_close(db) };
    assert_eq!(rows, 3);
}

unsafe extern "C" fn doubled(
    context: *mut Sqlite3Context,
    count: c_int,
    arguments: *mut *mut Sqlite3Value,
) {
    assert_eq!(count, 1);
    // SAFETY: SQLite hands the function `count` live values.
    let value = unsafe { *arguments };
    // SAFETY: a live value and the running function's context.
    unsafe {
        if sqlite3_value_type(value) == SQLITE_NULL {
            sqlite3_result_null(context);
        } else {
            let factor = *sqlite3_user_data(context).cast::<i64>();
            sqlite3_result_int64(context, sqlite3_value_int64(value) * factor);
        }
    }
}

unsafe extern "C" fn refusing(
    context: *mut Sqlite3Context,
    _count: c_int,
    _arguments: *mut *mut Sqlite3Value,
) {
    let message = CString::new("refused by the program").unwrap();
    // SAFETY: the running function's context and a NUL-terminated message.
    unsafe { sqlite3_result_error(context, message.as_ptr(), -1) };
}

fn single_text(db: *mut Sqlite3, sql: &str) -> Option<String> {
    let sql = CString::new(sql).unwrap();
    let mut statement = ptr::null_mut();
    // SAFETY: a live connection, NUL-terminated text and memory for one pointer.
    let prepared =
        unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut statement, ptr::null_mut()) };
    assert_eq!(prepared, SQLITE_OK);
    // SAFETY: a live statement.
    let stepped = unsafe { sqlite3_step(statement) };
    let found = if stepped == SQLITE_ROW {
        // SAFETY: a live statement on a row.
        let pointer = unsafe { sqlite3_column_text(statement, 0) };
        (!pointer.is_null()).then(|| text(pointer))
    } else {
        None
    };
    // SAFETY: a live statement, used no more.
    unsafe { sqlite3_finalize(statement) };
    found
}

#[test]
fn functions_a_program_defines_run_in_its_sql() {
    let mut db = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated name and memory for one pointer.
    assert_eq!(unsafe { sqlite3_open(name.as_ptr(), &mut db) }, SQLITE_OK);
    let mut factor: i64 = 3;
    let function = CString::new("triple").unwrap();
    // SAFETY: a live connection, a NUL-terminated name and a function that follows the contract.
    let created = unsafe {
        sqlite3_create_function_v2(
            db,
            function.as_ptr(),
            1,
            1,
            (&raw mut factor).cast::<c_void>(),
            Some(doubled),
            None,
            None,
            None,
        )
    };
    assert_eq!(created, SQLITE_OK);
    assert_eq!(single_text(db, "SELECT TRIPLE(14)").as_deref(), Some("42"));
    assert_eq!(single_text(db, "SELECT triple(NULL)"), None);
    let refuse = CString::new("refuse").unwrap();
    // SAFETY: as above.
    let created = unsafe {
        sqlite3_create_function(db, refuse.as_ptr(), 0, 1, ptr::null_mut(), Some(refusing), None, None)
    };
    assert_eq!(created, SQLITE_OK);
    let sql = CString::new("SELECT refuse()").unwrap();
    let mut statement = ptr::null_mut();
    // SAFETY: a live connection, NUL-terminated text and memory for one pointer.
    unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut statement, ptr::null_mut()) };
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_step(statement) }, SQLITE_ERROR);
    // SAFETY: a live connection.
    let message = unsafe { CStr::from_ptr(sqlite3_errmsg(db)) };
    assert!(message.to_string_lossy().contains("refused by the program"));
    // SAFETY: a live statement, used no more.
    unsafe { sqlite3_finalize(statement) };
    // SAFETY: removing the function: every callback NULL.
    let removed = unsafe {
        sqlite3_create_function_v2(db, function.as_ptr(), 1, 1, ptr::null_mut(), None, None, None, None)
    };
    assert_eq!(removed, SQLITE_OK);
    // SAFETY: a live connection, used no more.
    unsafe { sqlite3_close(db) };
}

#[test]
fn the_interface_reports_text_state_and_completeness() {
    // SAFETY: SQLite's static texts.
    let busy = unsafe { CStr::from_ptr(sqlite3_errstr(SQLITE_BUSY)) };
    assert_eq!(busy.to_str().unwrap(), "database is locked");
    let complete = CString::new("SELECT 1;").unwrap();
    let partial = CString::new("SELECT 'a;").unwrap();
    // SAFETY: NUL-terminated texts.
    assert_eq!(unsafe { sqlite3_complete(complete.as_ptr()) }, 1);
    // SAFETY: as above.
    assert_eq!(unsafe { sqlite3_complete(partial.as_ptr()) }, 0);
    let mut db = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated name and memory for one pointer.
    assert_eq!(unsafe { sqlite3_open(name.as_ptr(), &mut db) }, SQLITE_OK);
    // SAFETY: a live connection.
    assert_eq!(unsafe { sqlite3_get_autocommit(db) }, 1);
    let begin = CString::new("BEGIN").unwrap();
    // SAFETY: a live connection and NUL-terminated SQL.
    unsafe { sqlite3_exec(db, begin.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) };
    // SAFETY: a live connection.
    assert_eq!(unsafe { sqlite3_get_autocommit(db) }, 0);
    let sql = CString::new("SELECT ?2, ?1").unwrap();
    let mut statement = ptr::null_mut();
    // SAFETY: a live connection, NUL-terminated text and memory for one pointer.
    unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut statement, ptr::null_mut()) };
    // SAFETY: a live statement.
    let first = unsafe { CStr::from_ptr(sqlite3_bind_parameter_name(statement, 1)) };
    assert_eq!(first.to_str().unwrap(), "?1");
    let second = CString::new("?2").unwrap();
    // SAFETY: a live statement and a NUL-terminated name.
    assert_eq!(unsafe { sqlite3_bind_parameter_index(statement, second.as_ptr()) }, 2);
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_stmt_readonly(statement) }, 1);
    // SAFETY: a live statement.
    let shown = unsafe { CStr::from_ptr(sqlite3_sql(statement)) };
    assert_eq!(shown.to_str().unwrap(), "SELECT ?2, ?1");
    // SAFETY: a live statement, used no more, and its connection.
    unsafe {
        sqlite3_finalize(statement);
        sqlite3_close(db);
    }
}

fn open_memory() -> *mut Sqlite3 {
    let mut db = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated name and memory for one pointer.
    assert_eq!(unsafe { sqlite3_open(name.as_ptr(), &mut db) }, SQLITE_OK);
    db
}

fn exec(db: *mut Sqlite3, sql: &str) {
    let sql = CString::new(sql).unwrap();
    // SAFETY: a live connection and a NUL-terminated script.
    let code = unsafe { sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) };
    assert_eq!(code, SQLITE_OK);
}

#[test]
fn an_online_backup_copies_one_connections_database_into_anothers() {
    let source = open_memory();
    let target = open_memory();
    exec(source, "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES (1, 'kept'), (2, 'also');");
    exec(target, "CREATE TABLE old (x INTEGER); INSERT INTO old VALUES (9);");
    let main = CString::new("main").unwrap();
    // SAFETY: two live, distinct connections and NUL-terminated names.
    let backup = unsafe { sqlite3_backup_init(target, main.as_ptr(), source, main.as_ptr()) };
    assert!(!backup.is_null());
    // SAFETY: a live backup.
    assert_eq!(unsafe { sqlite3_backup_step(backup, -1) }, SQLITE_DONE);
    // SAFETY: a live backup.
    assert_eq!(unsafe { sqlite3_backup_remaining(backup) }, 0);
    // SAFETY: a live backup, used no more.
    assert_eq!(unsafe { sqlite3_backup_finish(backup) }, SQLITE_OK);
    let query = CString::new("SELECT count(*) FROM notes").unwrap();
    let mut statement = ptr::null_mut();
    // SAFETY: a live connection, NUL-terminated text and memory for one pointer.
    assert_eq!(unsafe { sqlite3_prepare_v2(target, query.as_ptr(), -1, &mut statement, ptr::null_mut()) }, SQLITE_OK);
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_step(statement) }, SQLITE_ROW);
    // SAFETY: a live statement on a row.
    assert_eq!(unsafe { sqlite3_column_int(statement, 0) }, 2);
    // SAFETY: a live statement, used no more.
    unsafe { sqlite3_finalize(statement) };
    // SAFETY: the same connection on both sides is refused.
    assert!(unsafe { sqlite3_backup_init(target, main.as_ptr(), target, main.as_ptr()) }.is_null());
    // SAFETY: live connections, used no more.
    unsafe {
        sqlite3_close(source);
    }
    // SAFETY: as above.
    unsafe {
        sqlite3_close(target);
    }
}
