//! SQLite's C API on EggDB, called as a C program would.

use sqlite3::*;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

fn text(pointer: *const u8) -> String {
    assert!(!pointer.is_null());
    // SAFETY: column text is NUL-terminated and valid until the next step.
    unsafe { CStr::from_ptr(pointer.cast()) }.to_string_lossy().into_owned()
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
    let prepared = unsafe { sqlite3_prepare_v2(db, insert.as_ptr(), -1, &mut statement, ptr::null_mut()) };
    assert_eq!(prepared, SQLITE_OK);
    // SAFETY: a live statement.
    assert_eq!(unsafe { sqlite3_bind_parameter_count(statement) }, 3);
    for (index, person) in ["Ada", "Grace"].iter().enumerate() {
        let person = CString::new(*person).unwrap();
        let photo = [index as u8, 0xff];
        // SAFETY: a live statement, NUL-terminated text and two readable bytes.
        unsafe {
            assert_eq!(sqlite3_bind_text(statement, 1, person.as_ptr(), -1, None), SQLITE_OK);
            assert_eq!(sqlite3_bind_double(statement, 2, 1.5 + index as f64), SQLITE_OK);
            assert_eq!(sqlite3_bind_blob(statement, 3, photo.as_ptr().cast(), 2, None), SQLITE_OK);
            assert_eq!(sqlite3_bind_int(statement, 4, 1), SQLITE_RANGE);
            assert_eq!(sqlite3_step(statement), SQLITE_DONE);
            assert_eq!(sqlite3_changes(db), 1);
            assert_eq!(sqlite3_reset(statement), SQLITE_OK);
        }
    }
    // SAFETY: a live connection.
    assert_eq!(unsafe { sqlite3_last_insert_rowid(db) }, 2);
    // SAFETY: the statement is finished with.
    unsafe { sqlite3_finalize(statement) };
    let select = CString::new("SELECT id, name, score, photo FROM people ORDER BY id; SELECT 1").unwrap();
    let mut tail: *const c_char = ptr::null();
    // SAFETY: as above, with memory for the tail pointer.
    let prepared = unsafe { sqlite3_prepare_v2(db, select.as_ptr(), -1, &mut statement, &mut tail) };
    assert_eq!(prepared, SQLITE_OK);
    // SAFETY: the tail points into the given NUL-terminated text.
    assert_eq!(unsafe { CStr::from_ptr(tail) }.to_str().unwrap().trim(), "SELECT 1");
    let mut seen = Vec::new();
    // SAFETY: a live statement, read while its row is current.
    unsafe {
        assert_eq!(sqlite3_column_count(statement), 4);
        while sqlite3_step(statement) == SQLITE_ROW {
            assert_eq!(sqlite3_column_type(statement, 0), SQLITE_INTEGER);
            assert_eq!(sqlite3_column_type(statement, 3), SQLITE_BLOB);
            assert_eq!(sqlite3_column_bytes(statement, 3), 2);
            seen.push((
                sqlite3_column_int64(statement, 0),
                text(sqlite3_column_text(statement, 1)),
                sqlite3_column_double(statement, 2),
            ));
        }
        assert_eq!(text(sqlite3_column_name(statement, 1).cast()), "name");
        sqlite3_finalize(statement);
    }
    assert_eq!(seen, vec![(1, "Ada".to_owned(), 1.5), (2, "Grace".to_owned(), 2.5)]);
    let wrong = CString::new("SELECT * FROM nowhere").unwrap();
    let mut message: *mut c_char = ptr::null_mut();
    // SAFETY: a live connection and memory for the message pointer.
    let failed = unsafe { sqlite3_exec(db, wrong.as_ptr(), None, ptr::null_mut(), &mut message) };
    assert_eq!(failed, SQLITE_ERROR);
    // SAFETY: the message is NUL-terminated and freed once with sqlite3_free.
    unsafe {
        assert!(CStr::from_ptr(message).to_string_lossy().contains("nowhere"));
        sqlite3_free(message.cast::<c_void>());
        assert!(CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().contains("nowhere"));
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
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
    let script = CString::new("CREATE TABLE t(v); INSERT INTO t VALUES (1), (2), (3); SELECT v FROM t").unwrap();
    let mut rows = 0i32;
    // SAFETY: as in the test above; the callback gets the counter.
    unsafe {
        sqlite3_open(name.as_ptr(), &mut db);
        let code = sqlite3_exec(
            db,
            script.as_ptr(),
            Some(count_rows),
            (&raw mut rows).cast(),
            ptr::null_mut(),
        );
        assert_eq!(code, SQLITE_OK);
        sqlite3_close(db);
    }
    assert_eq!(rows, 3);
}
