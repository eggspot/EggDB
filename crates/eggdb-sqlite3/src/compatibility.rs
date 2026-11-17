use crate::{
    Destructor, SQLITE_MISUSE, SQLITE_OK, Sqlite3, Sqlite3Stmt, sqlite3_bind_blob, sqlite3_bind_text,
    sqlite3_prepare_v2, statement_mut,
};
use std::ffi::{CString, c_char, c_int, c_uchar, c_uint, c_void};

const SQLITE_TOOBIG: c_int = 18;
const SQLITE_UTF8: c_uchar = 1;

const LIMITS: [c_int; 12] = [
    1_000_000_000,
    1_000_000_000,
    2_000,
    1_000,
    500,
    250_000_000,
    127,
    10,
    50_000,
    32_766,
    1_000,
    0,
];

/// Prepares the first statement of `sql`, as [`sqlite3_prepare_v2`] does; the legacy form.
///
/// # Safety
/// As for [`sqlite3_prepare_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare(
    db: *mut Sqlite3,
    sql: *const c_char,
    length: c_int,
    statement: *mut *mut Sqlite3Stmt,
    tail: *mut *const c_char,
) -> c_int {
    // SAFETY: the same contract as sqlite3_prepare_v2's.
    unsafe { sqlite3_prepare_v2(db, sql, length, statement, tail) }
}

/// Prepares the first statement of `sql`, as [`sqlite3_prepare_v2`] does; the preparation
/// flags (`SQLITE_PREPARE_PERSISTENT` and the rest) change nothing here.
///
/// # Safety
/// As for [`sqlite3_prepare_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_prepare_v3(
    db: *mut Sqlite3,
    sql: *const c_char,
    length: c_int,
    _flags: c_uint,
    statement: *mut *mut Sqlite3Stmt,
    tail: *mut *const c_char,
) -> c_int {
    // SAFETY: the same contract as sqlite3_prepare_v2's.
    unsafe { sqlite3_prepare_v2(db, sql, length, statement, tail) }
}

/// Binds `length` bytes of UTF-8 text, with a 64-bit length; other encodings are refused.
///
/// # Safety
/// `statement` is a live statement and `text` points to `length` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_text64(
    statement: *mut Sqlite3Stmt,
    index: c_int,
    text: *const c_char,
    length: u64,
    destructor: Destructor,
    encoding: c_uchar,
) -> c_int {
    if encoding != SQLITE_UTF8 {
        return SQLITE_MISUSE;
    }
    let Ok(length) = c_int::try_from(length) else {
        return SQLITE_TOOBIG;
    };
    // SAFETY: the caller passes a live statement and `length` readable bytes.
    unsafe { sqlite3_bind_text(statement, index, text, length, destructor) }
}

/// Binds `length` bytes as a blob, with a 64-bit length.
///
/// # Safety
/// `statement` is a live statement and `blob` points to `length` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_blob64(
    statement: *mut Sqlite3Stmt,
    index: c_int,
    blob: *const c_void,
    length: u64,
    destructor: Destructor,
) -> c_int {
    let Ok(length) = c_int::try_from(length) else {
        return SQLITE_TOOBIG;
    };
    // SAFETY: the caller passes a live statement and `length` readable bytes.
    unsafe { sqlite3_bind_blob(statement, index, blob, length, destructor) }
}

/// Binds a blob of `length` zero bytes (none when negative).
///
/// # Safety
/// `statement` is a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_bind_zeroblob(statement: *mut Sqlite3Stmt, index: c_int, length: c_int) -> c_int {
    let zeros = vec![0u8; usize::try_from(length).unwrap_or(0)];
    let length = c_int::try_from(zeros.len()).unwrap_or(0);
    // SAFETY: the caller passes a live statement; `zeros` holds `length` readable bytes.
    unsafe { sqlite3_bind_blob(statement, index, zeros.as_ptr().cast(), length, None) }
}

/// Starts the library; there is nothing to set up.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_initialize() -> c_int {
    SQLITE_OK
}

/// Stops the library; there is nothing to tear down.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_shutdown() -> c_int {
    SQLITE_OK
}

/// Accepts a busy handler; EggDB waits for the writer itself, so it is never called.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_busy_handler(
    db: *mut Sqlite3,
    _handler: Option<unsafe extern "C" fn(*mut c_void, c_int) -> c_int>,
    _argument: *mut c_void,
) -> c_int {
    if db.is_null() { SQLITE_MISUSE } else { SQLITE_OK }
}

/// Accepts a progress handler, which this library does not call.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_progress_handler(
    _db: *mut Sqlite3,
    _instructions: c_int,
    _handler: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    _argument: *mut c_void,
) {
}

/// The run-time limit `id` (`SQLITE_LIMIT_LENGTH` and the rest, SQLite's defaults); a new
/// value is not kept. An unknown limit is -1.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_limit(db: *mut Sqlite3, id: c_int, _new_value: c_int) -> c_int {
    if db.is_null() {
        return -1;
    }
    usize::try_from(id).ok().and_then(|at| LIMITS.get(at)).copied().unwrap_or(-1)
}

/// A count of the statement's work (`SQLITE_STMTSTATUS_*`), which this library does not keep: 0.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_stmt_status(_statement: *mut Sqlite3Stmt, _operation: c_int, _reset: c_int) -> c_int {
    0
}

fn literal(declared: Option<eggdb::sql::SqlType>, value: Option<&str>) -> String {
    let Some(value) = value else {
        return "NULL".to_owned();
    };
    match declared {
        Some(eggdb::sql::SqlType::Text) => format!("'{}'", value.replace('\'', "''")),
        Some(eggdb::sql::SqlType::Bytea) => format!("X'{}'", value.trim_start_matches("\\x")),
        _ => value.to_owned(),
    }
}

fn expanded(statement: &Sqlite3Stmt) -> String {
    let mut out = String::with_capacity(statement.text.len());
    let mut quote: Option<char> = None;
    let mut next = 0;
    let mut characters = statement.text.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(open) => {
                out.push(character);
                if character == open {
                    quote = None;
                }
            }
            None if matches!(character, '\'' | '"' | '`' | '[') => {
                quote = Some(if character == '[' { ']' } else { character });
                out.push(character);
            }
            None if character == '?' => {
                let mut digits = String::new();
                while let Some(digit) = characters.next_if(char::is_ascii_digit) {
                    digits.push(digit);
                }
                let at = digits.parse::<usize>().ok().and_then(|number| number.checked_sub(1)).unwrap_or(next);
                next = at + 1;
                let bound = statement.bindings.get(at);
                out.push_str(&literal(
                    bound.and_then(|parameter| parameter.declared_type),
                    bound.and_then(|parameter| parameter.value.as_deref()),
                ));
            }
            None => out.push(character),
        }
    }
    out
}

/// The statement's SQL with its bound values written in place of its `?` parameters, in memory
/// the caller frees with `sqlite3_free`; NULL for no statement.
///
/// # Safety
/// `statement` is NULL or a live statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_expanded_sql(statement: *mut Sqlite3Stmt) -> *mut c_char {
    // SAFETY: the caller passes NULL or a live statement.
    let Some(statement) = (unsafe { statement_mut(statement) }) else {
        return std::ptr::null_mut();
    };
    CString::new(expanded(statement).replace('\0', "")).map_or(std::ptr::null_mut(), CString::into_raw)
}
