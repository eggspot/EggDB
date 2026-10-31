//! SQLite's C API on EggDB: `sqlite3_open_v2`, `sqlite3_exec`, `sqlite3_prepare_v2`,
//! `sqlite3_bind_*`, `sqlite3_step`, `sqlite3_column_*`, `sqlite3_reset`, `sqlite3_finalize`,
//! `sqlite3_errmsg`, `sqlite3_changes`, `sqlite3_last_insert_rowid` and the rest declared in
//! `include/sqlite3.h`, written from SQLite's published C interface. Built as `libsqlite3`, a
//! program written for SQLite links against it and runs its SQL in EggDB's SQLite dialect. A
//! statement runs at its first step and keeps its rows until it is reset or finalized.

use eggdb::Database;
use eggdb::sql::{Dialect, OutputColumn, Parameter, QueryResult, Session, SqlError, SqlType, Store, Value};
use std::ffi::{CStr, CString, c_char, c_double, c_int, c_void};
use std::sync::Arc;

/// `SQLITE_OK`.
pub const SQLITE_OK: c_int = 0;
/// `SQLITE_ERROR`.
pub const SQLITE_ERROR: c_int = 1;
/// `SQLITE_ABORT`.
pub const SQLITE_ABORT: c_int = 4;
/// `SQLITE_BUSY`.
pub const SQLITE_BUSY: c_int = 5;
/// `SQLITE_NOMEM`.
pub const SQLITE_NOMEM: c_int = 7;
/// `SQLITE_CANTOPEN`.
pub const SQLITE_CANTOPEN: c_int = 14;
/// `SQLITE_CONSTRAINT`.
pub const SQLITE_CONSTRAINT: c_int = 19;
/// `SQLITE_MISUSE`.
pub const SQLITE_MISUSE: c_int = 21;
/// `SQLITE_RANGE`.
pub const SQLITE_RANGE: c_int = 25;
/// `SQLITE_ROW`.
pub const SQLITE_ROW: c_int = 100;
/// `SQLITE_DONE`.
pub const SQLITE_DONE: c_int = 101;
/// `SQLITE_INTEGER`.
pub const SQLITE_INTEGER: c_int = 1;
/// `SQLITE_FLOAT`.
pub const SQLITE_FLOAT: c_int = 2;
/// `SQLITE_TEXT`.
pub const SQLITE_TEXT: c_int = 3;
/// `SQLITE_BLOB`.
pub const SQLITE_BLOB: c_int = 4;
/// `SQLITE_NULL`.
pub const SQLITE_NULL: c_int = 5;
/// `SQLITE_OPEN_READONLY`.
pub const SQLITE_OPEN_READONLY: c_int = 0x01;
/// `SQLITE_OPEN_CREATE`.
pub const SQLITE_OPEN_CREATE: c_int = 0x04;

const VERSION: &CStr = c"3.45.0-eggdb";
const VERSION_NUMBER: c_int = 3_045_000;

type SqlSession = Session<'static, Store<'static>>;

/// An open database connection (`sqlite3`).
pub struct Sqlite3 {
    session: SqlSession,
    error: CString,
    code: c_int,
    changes: i64,
    total_changes: i64,
    store: Arc<Store<'static>>,
    database: Arc<Database>,
}

impl std::fmt::Debug for Sqlite3 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sqlite3")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// A prepared statement (`sqlite3_stmt`).
#[derive(Debug)]
pub struct Sqlite3Stmt {
    db: *mut Sqlite3,
    text: String,
    bindings: Vec<Parameter>,
    columns: Vec<OutputColumn>,
    names: Vec<CString>,
    rows: Vec<Vec<Value>>,
    next_row: usize,
    ran: bool,
    current: Vec<Option<Vec<u8>>>,
}

fn sqlite_code(error: &SqlError) -> c_int {
    match error.vendor_code() {
        Some(code) => c_int::from(code & 0xff),
        None if error.state().code().starts_with("23") => SQLITE_CONSTRAINT,
        None => SQLITE_ERROR,
    }
}

impl Sqlite3 {
    fn record(&mut self, code: c_int, message: &str) -> c_int {
        self.code = code;
        self.error = CString::new(message.replace('\0', " ")).unwrap_or_default();
        code
    }

    fn succeeded(&mut self) {
        self.code = SQLITE_OK;
        self.error = c"not an error".to_owned();
    }

    fn run(&mut self, text: &str, parameters: &[Parameter]) -> Result<Vec<QueryResult>, c_int> {
        let mut results = Vec::new();
        let outcome = self.session.execute_prepared(text, parameters, &mut |result| {
            results.push(result);
            true
        });
        match outcome {
            Ok(()) => {
                let changed: i64 = results.iter().map(|result| affected(&result.tag)).sum();
                self.changes = changed;
                self.total_changes += changed;
                self.succeeded();
                Ok(results)
            }
            Err(error) => Err(self.record(sqlite_code(&error), error.message())),
        }
    }
}

fn affected(tag: &str) -> i64 {
    let verb = tag.split(' ').next().unwrap_or_default();
    if !matches!(verb, "INSERT" | "UPDATE" | "DELETE") {
        return 0;
    }
    tag.rsplit(' ').next().and_then(|count| count.parse().ok()).unwrap_or(0)
}

/// # Safety
/// `text` is NULL or a NUL-terminated string.
unsafe fn text_of<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller passes a NUL-terminated string.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// Opens the database at `filename` (`":memory:"` for one in memory) and puts the connection in
/// `*db`. As SQLite does, `*db` holds a connection even on failure, for `sqlite3_errmsg`.
///
/// # Safety
/// `filename` is a NUL-terminated string and `db` points to writable memory for one pointer;
/// `vfs` is NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    db: *mut *mut Sqlite3,
    flags: c_int,
    _vfs: *const c_char,
) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    // SAFETY: the caller passes a NUL-terminated file name.
    let Some(filename) = (unsafe { text_of(filename) }) else {
        // SAFETY: `db` is writable memory for one pointer.
        unsafe { db.write(std::ptr::null_mut()) };
        return SQLITE_MISUSE;
    };
    let opened = if filename == ":memory:" || filename.is_empty() {
        Database::in_memory()
    } else {
        let options = eggdb::Options::new()
            .read_only(flags & SQLITE_OPEN_READONLY != 0)
            .create_if_missing(flags & SQLITE_OPEN_CREATE != 0);
        Database::open_with(filename, options)
    };
    let (database, failure) = match opened {
        Ok(database) => (Arc::new(database), None),
        Err(error) => match Database::in_memory() {
            Ok(placeholder) => (Arc::new(placeholder), Some(error.to_string())),
            Err(_) => {
                // SAFETY: `db` is writable memory for one pointer.
                unsafe { db.write(std::ptr::null_mut()) };
                return SQLITE_NOMEM;
            }
        },
    };
    // SAFETY: the reference points into the Arc's allocation, which never moves and lives while
    // the connection's `database` clone does; the connection drops `store` after `session`.
    let database_ref: &'static Database = unsafe { &*Arc::as_ptr(&database) };
    let store = Arc::new(database_ref.sql_store());
    // SAFETY: as above, for the store, which the connection keeps until after the session.
    let store_ref: &'static Store<'static> = unsafe { &*Arc::as_ptr(&store) };
    let session = store_ref.session(Dialect::Sqlite);
    let mut connection = Box::new(Sqlite3 {
        session,
        error: c"not an error".to_owned(),
        code: SQLITE_OK,
        changes: 0,
        total_changes: 0,
        store,
        database,
    });
    let code = match failure {
        Some(message) => connection.record(SQLITE_CANTOPEN, &message),
        None => SQLITE_OK,
    };
    // SAFETY: `db` is writable memory for one pointer.
    unsafe { db.write(Box::into_raw(connection)) };
    code
}

/// Opens or creates the database at `filename` (see [`sqlite3_open_v2`]).
///
/// # Safety
/// As for [`sqlite3_open_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, db: *mut *mut Sqlite3) -> c_int {
    // SAFETY: the same contract as sqlite3_open_v2's.
    unsafe { sqlite3_open_v2(filename, db, 0x02 | SQLITE_OPEN_CREATE, std::ptr::null()) }
}

/// Closes the connection; an open transaction is rolled back. NULL does nothing.
///
/// # Safety
/// `db` is NULL or a connection from `sqlite3_open_v2` whose statements are finalized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_close(db: *mut Sqlite3) -> c_int {
    if !db.is_null() {
        // SAFETY: the caller hands back a connection from sqlite3_open_v2, used no more.
        drop(unsafe { Box::from_raw(db) });
    }
    SQLITE_OK
}

/// Closes the connection (see [`sqlite3_close`]).
///
/// # Safety
/// As for [`sqlite3_close`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut Sqlite3) -> c_int {
    // SAFETY: the same contract as sqlite3_close's.
    unsafe { sqlite3_close(db) }
}

/// The callback `sqlite3_exec` calls for each row.
pub type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

/// Runs every statement in `sql`, calling `callback` for each row of their results. On failure
/// `*errmsg` (when not NULL) gets a message to free with `sqlite3_free`.
///
/// # Safety
/// `db` is a live connection, `sql` a NUL-terminated string, `errmsg` NULL or writable memory
/// for one pointer, and `callback` NULL or a function that follows SQLite's contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut Sqlite3,
    sql: *const c_char,
    callback: ExecCallback,
    argument: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    // SAFETY: the caller passes a live connection used by this call alone.
    let Some(connection) = (unsafe { db.as_mut() }) else {
        return SQLITE_MISUSE;
    };
    if !errmsg.is_null() {
        // SAFETY: `errmsg` is writable memory for one pointer.
        unsafe { errmsg.write(std::ptr::null_mut()) };
    }
    // SAFETY: the caller passes a NUL-terminated string.
    let Some(text) = (unsafe { text_of(sql) }) else {
        return connection.record(SQLITE_MISUSE, "the SQL text is not UTF-8");
    };
    let results = match connection.run(text, &[]) {
        Ok(results) => results,
        Err(code) => {
            if !errmsg.is_null() {
                // SAFETY: `errmsg` is writable memory for one pointer.
                unsafe { errmsg.write(connection.error.clone().into_raw()) };
            }
            return code;
        }
    };
    let Some(callback) = callback else {
        return SQLITE_OK;
    };
    for result in results {
        let Some(columns) = &result.columns else {
            continue;
        };
        let mut names: Vec<CString> = columns
            .iter()
            .map(|column| CString::new(column.name.clone()).unwrap_or_default())
            .collect();
        let mut name_pointers: Vec<*mut c_char> =
            names.iter_mut().map(|name| name.as_ptr().cast_mut()).collect();
        for row in &result.rows {
            let texts: Vec<Option<CString>> = row
                .iter()
                .map(|value| value.to_text().map(|text| CString::new(text.replace('\0', " ")).unwrap_or_default()))
                .collect();
            let mut pointers: Vec<*mut c_char> = texts
                .iter()
                .map(|text| text.as_ref().map_or(std::ptr::null_mut(), |text| text.as_ptr().cast_mut()))
                .collect();
            // SAFETY: the callback gets pointers to `pointers.len()` values and names that live
            // until it returns, as SQLite's contract says.
            let stop = unsafe {
                callback(
                    argument,
                    pointers.len() as c_int,
                    pointers.as_mut_ptr(),
                    name_pointers.as_mut_ptr(),
                )
            };
            if stop != 0 {
                return connection.record(SQLITE_ABORT, "query aborted");
            }
        }
    }
    SQLITE_OK
}

/// Frees a message `sqlite3_exec` gave. NULL does nothing.
///
/// # Safety
/// `pointer` is NULL or a message from `sqlite3_exec`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_free(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: the only memory this library hands out for sqlite3_free is a CString.
        drop(unsafe { CString::from_raw(pointer.cast()) });
    }
}

/// Prepares the first statement of `sql` (`length` bytes, or up to its NUL when negative) and
/// puts it in `*statement`; `*tail`, when not NULL, points past it.
///
/// # Safety
/// `db` is a live connection, `sql` points to `length` bytes or a NUL-terminated string, and
/// `statement` and `tail` are writable memory for one pointer (`tail` may be NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut Sqlite3,
    sql: *const c_char,
    length: c_int,
    statement: *mut *mut Sqlite3Stmt,
    tail: *mut *const c_char,
) -> c_int {
    if statement.is_null() || db.is_null() || sql.is_null() {
        return SQLITE_MISUSE;
    }
    // SAFETY: `statement` is writable memory for one pointer.
    unsafe { statement.write(std::ptr::null_mut()) };
    let bytes = if length < 0 {
        // SAFETY: a negative length means a NUL-terminated string.
        unsafe { CStr::from_ptr(sql) }.to_bytes()
    } else {
        // SAFETY: the caller passes `length` readable bytes.
        unsafe { std::slice::from_raw_parts(sql.cast::<u8>(), length as usize) }
    };
    let bytes = bytes.split(|&byte| byte == 0).next().unwrap_or_default();
    // SAFETY: the caller passes a live connection used by this call alone.
    let connection = unsafe { &mut *db };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return connection.record(SQLITE_ERROR, "the SQL text is not UTF-8");
    };
    let end = first_statement_end(text);
    if !tail.is_null() {
        // SAFETY: `end` lies within the given text, and `tail` is writable memory.
        unsafe { tail.write(sql.add(end)) };
    }
    let first = text[..end].trim();
    if first.is_empty() || first == ";" {
        connection.succeeded();
        return SQLITE_OK;
    }
    let parameters = placeholder_count(first);
    let prepared = Box::new(Sqlite3Stmt {
        db,
        text: first.to_owned(),
        bindings: vec![
            Parameter {
                declared_type: None,
                value: None,
            };
            parameters
        ],
        columns: Vec::new(),
        names: Vec::new(),
        rows: Vec::new(),
        next_row: 0,
        ran: false,
        current: Vec::new(),
    });
    connection.succeeded();
    // SAFETY: `statement` is writable memory for one pointer.
    unsafe { statement.write(Box::into_raw(prepared)) };
    SQLITE_OK
}

fn first_statement_end(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"' | b'`') => quote = Some(byte),
            None if byte == b'-' && bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            None if byte == b';' => return at + 1,
            None => {}
        }
        at += 1;
    }
    bytes.len()
}

fn placeholder_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut count = 0usize;
    let mut highest = 0usize;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"' | b'`') => quote = Some(byte),
            None if byte == b'?' => {
                let digits: String = bytes[at + 1..]
                    .iter()
                    .take_while(|digit| digit.is_ascii_digit())
                    .map(|&digit| char::from(digit))
                    .collect();
                match digits.parse::<usize>() {
                    Ok(number) => highest = highest.max(number),
                    Err(_) => count += 1,
                }
                at += digits.len();
            }
            None => {}
        }
        at += 1;
    }
    count.max(highest)
}

/// # Safety
/// `statement` is NULL or a live statement.
unsafe fn statement_mut<'a>(statement: *mut Sqlite3Stmt) -> Option<&'a mut Sqlite3Stmt> {
    // SAFETY: the caller passes NULL or a live statement used by this call alone.
    unsafe { statement.as_mut() }
}

impl Sqlite3Stmt {
    fn bind(&mut self, index: c_int, declared: Option<SqlType>, value: Option<String>) -> c_int {
        let Some(slot) = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| self.bindings.get_mut(index))
        else {
            return SQLITE_RANGE;
        };
        *slot = Parameter {
            declared_type: declared,
            value,
        };
        SQLITE_OK
    }

    fn connection(&mut self) -> Option<&mut Sqlite3> {
        // SAFETY: a statement's connection outlives it, as SQLite's contract asks.
        unsafe { self.db.as_mut() }
    }

    fn row_value(&self, column: c_int) -> Option<&Value> {
        let row = self.rows.get(self.next_row.checked_sub(1)?)?;
        row.get(usize::try_from(column).ok()?)
    }
}

/// Binds a 32-bit integer to parameter `index` (from 1).
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_int(statement: *mut Sqlite3Stmt, index: c_int, value: c_int) -> c_int {
    // SAFETY: the same contract.
    unsafe { sqlite3_bind_int64(statement, index, i64::from(value)) }
}

/// Binds a 64-bit integer to parameter `index` (from 1).
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_int64(statement: *mut Sqlite3Stmt, index: c_int, value: i64) -> c_int {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) } {
        Some(statement) => statement.bind(index, Some(SqlType::BigInt), Some(value.to_string())),
        None => SQLITE_MISUSE,
    }
}

/// Binds a floating-point number to parameter `index` (from 1).
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_double(statement: *mut Sqlite3Stmt, index: c_int, value: c_double) -> c_int {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) } {
        Some(statement) => statement.bind(index, Some(SqlType::Double), Some(format!("{value:?}"))),
        None => SQLITE_MISUSE,
    }
}

/// Binds NULL to parameter `index` (from 1).
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_null(statement: *mut Sqlite3Stmt, index: c_int) -> c_int {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) } {
        Some(statement) => statement.bind(index, None, None),
        None => SQLITE_MISUSE,
    }
}

/// The destructor argument of the bind functions; this library always copies, so any value works.
pub type Destructor = Option<unsafe extern "C" fn(*mut c_void)>;

/// Binds `length` bytes of UTF-8 text (up to its NUL when negative) to parameter `index`.
///
/// # Safety
/// `statement` is a live statement and `text` points to `length` bytes or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_text(
    statement: *mut Sqlite3Stmt,
    index: c_int,
    text: *const c_char,
    length: c_int,
    _destructor: Destructor,
) -> c_int {
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return SQLITE_MISUSE;
    };
    if text.is_null() {
        return statement.bind(index, None, None);
    }
    let bytes = if length < 0 {
        // SAFETY: a negative length means a NUL-terminated string.
        unsafe { CStr::from_ptr(text) }.to_bytes()
    } else {
        // SAFETY: the caller passes `length` readable bytes.
        unsafe { std::slice::from_raw_parts(text.cast::<u8>(), length as usize) }
    };
    let value = String::from_utf8_lossy(bytes).into_owned();
    statement.bind(index, Some(SqlType::Text), Some(value))
}

/// Binds `length` bytes as a blob to parameter `index`.
///
/// # Safety
/// `statement` is a live statement and `blob` points to `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_blob(
    statement: *mut Sqlite3Stmt,
    index: c_int,
    blob: *const c_void,
    length: c_int,
    _destructor: Destructor,
) -> c_int {
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return SQLITE_MISUSE;
    };
    if blob.is_null() || length < 0 {
        return statement.bind(index, None, None);
    }
    // SAFETY: the caller passes `length` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), length as usize) };
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    statement.bind(index, Some(SqlType::Bytea), Some(text))
}

/// The number of parameters the statement has.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(0, |statement| statement.bindings.len() as c_int)
}

/// Runs the statement at its first step, and steps through its rows: `SQLITE_ROW` while there is
/// one, then `SQLITE_DONE`.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_step(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return SQLITE_MISUSE;
    };
    if !statement.ran {
        statement.ran = true;
        let text = statement.text.clone();
        let bindings = statement.bindings.clone();
        let Some(connection) = statement.connection() else {
            return SQLITE_MISUSE;
        };
        let results = match connection.run(&text, &bindings) {
            Ok(results) => results,
            Err(code) => return code,
        };
        if let Some(last) = results.into_iter().last() {
            statement.columns = last.columns.unwrap_or_default();
            statement.rows = last.rows;
        }
        statement.names = statement
            .columns
            .iter()
            .map(|column| CString::new(column.name.clone()).unwrap_or_default())
            .collect();
        statement.next_row = 0;
    }
    if statement.next_row < statement.rows.len() {
        statement.next_row += 1;
        statement.current = vec![None; statement.columns.len()];
        SQLITE_ROW
    } else {
        SQLITE_DONE
    }
}

/// Makes the statement ready to run again, keeping its bindings.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_reset(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return SQLITE_OK;
    };
    statement.ran = false;
    statement.rows.clear();
    statement.next_row = 0;
    statement.current.clear();
    SQLITE_OK
}

/// Sets every parameter back to NULL.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_clear_bindings(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    if let Some(statement) = unsafe { statement_mut(statement) } {
        for binding in &mut statement.bindings {
            *binding = Parameter {
                declared_type: None,
                value: None,
            };
        }
    }
    SQLITE_OK
}

/// Frees the statement. NULL does nothing.
///
/// # Safety
/// `statement` is NULL or a statement from `sqlite3_prepare_v2`, used no more.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_finalize(statement: *mut Sqlite3Stmt) -> c_int {
    if !statement.is_null() {
        // SAFETY: the caller hands back a statement from sqlite3_prepare_v2, used no more.
        drop(unsafe { Box::from_raw(statement) });
    }
    SQLITE_OK
}

/// The number of columns the statement returns.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_count(statement: *mut Sqlite3Stmt) -> c_int {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }.map_or(0, |statement| statement.columns.len() as c_int)
}

/// The name of column `column` (from 0), valid until the statement is finalized.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_name(statement: *mut Sqlite3Stmt, column: c_int) -> *const c_char {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }
        .and_then(|statement| statement.names.get(usize::try_from(column).ok()?))
        .map_or(std::ptr::null(), |name| name.as_ptr())
}

/// The type of column `column` in the current row: `SQLITE_INTEGER`, `SQLITE_FLOAT`,
/// `SQLITE_TEXT`, `SQLITE_BLOB` or `SQLITE_NULL`.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_type(statement: *mut Sqlite3Stmt, column: c_int) -> c_int {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) }.and_then(|statement| statement.row_value(column)) {
        None | Some(Value::Null) => SQLITE_NULL,
        Some(Value::Boolean(_) | Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_)) => SQLITE_INTEGER,
        Some(Value::Real(_) | Value::Double(_)) => SQLITE_FLOAT,
        Some(Value::Bytea(_)) => SQLITE_BLOB,
        Some(_) => SQLITE_TEXT,
    }
}

/// Column `column` of the current row as a 64-bit integer.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_int64(statement: *mut Sqlite3Stmt, column: c_int) -> i64 {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) }.and_then(|statement| statement.row_value(column)) {
        Some(Value::Boolean(flag)) => i64::from(*flag),
        Some(Value::SmallInt(number)) => i64::from(*number),
        Some(Value::Integer(number)) => i64::from(*number),
        Some(Value::BigInt(number)) => *number,
        Some(Value::Double(number)) => *number as i64,
        Some(Value::Real(number)) => *number as i64,
        Some(value) => value.to_text().and_then(|text| text.trim().parse().ok()).unwrap_or(0),
        None => 0,
    }
}

/// Column `column` of the current row as a 32-bit integer.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_int(statement: *mut Sqlite3Stmt, column: c_int) -> c_int {
    // SAFETY: the same contract.
    unsafe { sqlite3_column_int64(statement, column) } as c_int
}

/// Column `column` of the current row as a floating-point number.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_double(statement: *mut Sqlite3Stmt, column: c_int) -> c_double {
    // SAFETY: the caller passes a live statement.
    match unsafe { statement_mut(statement) }.and_then(|statement| statement.row_value(column)) {
        Some(Value::Double(number)) => *number,
        Some(Value::Real(number)) => f64::from(*number),
        Some(Value::BigInt(number)) => *number as f64,
        Some(Value::Integer(number)) => f64::from(*number),
        Some(Value::SmallInt(number)) => f64::from(*number),
        Some(value) => value.to_text().and_then(|text| text.trim().parse().ok()).unwrap_or(0.0),
        None => 0.0,
    }
}

fn cached_bytes(statement: &mut Sqlite3Stmt, column: c_int) -> Option<&Vec<u8>> {
    let index = usize::try_from(column).ok()?;
    if statement.current.get(index)?.is_none() {
        let bytes = match statement.row_value(column)? {
            Value::Null => return None,
            Value::Bytea(bytes) => {
                let mut copy = bytes.clone();
                copy.push(0);
                copy
            }
            value => {
                let mut text = value.to_text()?.into_bytes();
                text.push(0);
                text
            }
        };
        statement.current[index] = Some(bytes);
    }
    statement.current.get(index)?.as_ref()
}

/// Column `column` of the current row as UTF-8 text, valid until the next step, reset or
/// finalize; NULL for SQL's NULL.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_text(statement: *mut Sqlite3Stmt, column: c_int) -> *const u8 {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }
        .and_then(|statement| cached_bytes(statement, column))
        .map_or(std::ptr::null(), |bytes| bytes.as_ptr())
}

/// Column `column` of the current row as a blob, valid as for `sqlite3_column_text`.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_blob(statement: *mut Sqlite3Stmt, column: c_int) -> *const c_void {
    // SAFETY: the same contract.
    unsafe { sqlite3_column_text(statement, column) }.cast()
}

/// The size in bytes of `sqlite3_column_text` or `sqlite3_column_blob` for column `column`.
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_column_bytes(statement: *mut Sqlite3Stmt, column: c_int) -> c_int {
    // SAFETY: the caller passes a live statement.
    unsafe { statement_mut(statement) }
        .and_then(|statement| cached_bytes(statement, column))
        .map_or(0, |bytes| (bytes.len() - 1) as c_int)
}

/// The last error's message, valid until the next call on the connection.
///
/// # Safety
/// `db` is NULL or a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut Sqlite3) -> *const c_char {
    // SAFETY: the caller passes NULL or a live connection.
    match unsafe { db.as_ref() } {
        Some(connection) => connection.error.as_ptr(),
        None => c"out of memory".as_ptr(),
    }
}

/// The last error's result code.
///
/// # Safety
/// `db` is NULL or a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut Sqlite3) -> c_int {
    // SAFETY: the caller passes NULL or a live connection.
    unsafe { db.as_ref() }.map_or(SQLITE_NOMEM, |connection| connection.code)
}

/// The rows the last `INSERT`, `UPDATE` or `DELETE` changed.
///
/// # Safety
/// `db` is a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_changes(db: *mut Sqlite3) -> c_int {
    // SAFETY: the caller passes a live connection.
    unsafe { db.as_ref() }.map_or(0, |connection| connection.changes as c_int)
}

/// The rows every `INSERT`, `UPDATE` and `DELETE` on the connection changed.
///
/// # Safety
/// `db` is a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_total_changes(db: *mut Sqlite3) -> c_int {
    // SAFETY: the caller passes a live connection.
    unsafe { db.as_ref() }.map_or(0, |connection| connection.total_changes as c_int)
}

/// The rowid of the last row inserted on the connection.
///
/// # Safety
/// `db` is a live connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut Sqlite3) -> i64 {
    // SAFETY: the caller passes a live connection used by this call alone.
    let Some(connection) = (unsafe { db.as_mut() }) else {
        return 0;
    };
    let mut found = 0;
    let _ = connection
        .session
        .execute_prepared("SELECT last_insert_rowid()", &[], &mut |result| {
            found = result
                .rows
                .first()
                .and_then(|row| row.first())
                .and_then(Value::to_text)
                .and_then(|text| text.parse().ok())
                .unwrap_or(0);
            true
        });
    found
}

/// The library's version text.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    VERSION.as_ptr()
}

/// The library's version number.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    VERSION_NUMBER
}
