use crate::{
    Destructor, SQLITE_BLOB, SQLITE_ERROR, SQLITE_FLOAT, SQLITE_INTEGER, SQLITE_MISUSE,
    SQLITE_NULL, SQLITE_OK, SQLITE_TEXT, Sqlite3, text_of,
};
use eggdb::sql::{NativeFunction, Value};
use std::cell::RefCell;
use std::ffi::{CString, c_char, c_double, c_int, c_void};
use std::sync::Arc;

const MOST_ARGUMENTS: c_int = 127;
const MOST_NAME_BYTES: usize = 255;

/// A value handed to a user function (`sqlite3_value`).
#[derive(Debug)]
pub struct Sqlite3Value {
    value: Value,
    bytes: RefCell<Option<Vec<u8>>>,
}

/// What a user function answers (`sqlite3_context`).
#[derive(Debug)]
pub struct Sqlite3Context {
    result: Value,
    error: Option<String>,
    user_data: *mut c_void,
    db: *mut Sqlite3,
}

/// A user function's body: `xFunc` in SQLite's interface.
pub type ScalarCallback =
    Option<unsafe extern "C" fn(*mut Sqlite3Context, c_int, *mut *mut Sqlite3Value)>;

/// A user aggregate's final step: `xFinal`.
pub type FinalCallback = Option<unsafe extern "C" fn(*mut Sqlite3Context)>;

struct Callback {
    function: unsafe extern "C" fn(*mut Sqlite3Context, c_int, *mut *mut Sqlite3Value),
    user_data: *mut c_void,
    destroy: Destructor,
    db: *mut Sqlite3,
}

// SAFETY: SQLite's contract lets one thread use a connection at a time; the callback and its
// user data are only used on the thread running the connection's statement, inside that call.
unsafe impl Send for Callback {}
// SAFETY: as above; no two threads call through the same connection at once.
unsafe impl Sync for Callback {}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            // SAFETY: SQLite calls xDestroy once, with the user data, when the function is
            // replaced, removed or its connection closes, which is when its last copy drops.
            unsafe { destroy(self.user_data) };
        }
    }
}

impl Callback {
    fn call(&self, values: &[Value]) -> Result<Value, String> {
        let mut arguments: Vec<Sqlite3Value> = values
            .iter()
            .map(|value| Sqlite3Value {
                value: value.clone(),
                bytes: RefCell::new(None),
            })
            .collect();
        let mut pointers: Vec<*mut Sqlite3Value> = arguments
            .iter_mut()
            .map(|argument| argument as *mut Sqlite3Value)
            .collect();
        let mut context = Sqlite3Context {
            result: Value::Null,
            error: None,
            user_data: self.user_data,
            db: self.db,
        };
        let count = c_int::try_from(pointers.len()).unwrap_or(c_int::MAX);
        // SAFETY: the context and every argument live until the callback returns, as SQLite's
        // contract promises them, and the callback follows that contract.
        unsafe { (self.function)(&mut context, count, pointers.as_mut_ptr()) };
        match context.error {
            Some(message) => Err(message),
            None => Ok(context.result),
        }
    }
}

fn destroy_now(destroy: Destructor, user_data: *mut c_void) {
    if let Some(destroy) = destroy {
        // SAFETY: SQLite calls xDestroy on the user data when registering fails.
        unsafe { destroy(user_data) };
    }
}

/// Defines (or, with every callback NULL, removes) the SQL function `name` taking `arguments`
/// arguments (-1 for any number) on the connection. Scalar functions only: an aggregate
/// (`xStep`/`xFinal`) is refused with `SQLITE_ERROR` and its `xDestroy` called, as SQLite does
/// when registration fails.
///
/// # Safety
/// `db` is a live connection, `name` a NUL-terminated string, and the callbacks follow SQLite's
/// contract for `sqlite3_create_function_v2`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut Sqlite3,
    name: *const c_char,
    arguments: c_int,
    _text_representation: c_int,
    user_data: *mut c_void,
    function: ScalarCallback,
    step: ScalarCallback,
    last: FinalCallback,
    destroy: Destructor,
) -> c_int {
    // SAFETY: the caller passes a live connection used by this call alone.
    let Some(connection) = (unsafe { db.as_mut() }) else {
        destroy_now(destroy, user_data);
        return SQLITE_MISUSE;
    };
    // SAFETY: the caller passes a NUL-terminated name.
    let Some(written) = (unsafe { text_of(name) }) else {
        destroy_now(destroy, user_data);
        return connection.record(SQLITE_MISUSE, "bad parameters");
    };
    if !(-1..=MOST_ARGUMENTS).contains(&arguments)
        || written.is_empty()
        || written.len() > MOST_NAME_BYTES
        || (function.is_some() && (step.is_some() || last.is_some()))
        || (function.is_none() && (step.is_some() != last.is_some()))
    {
        destroy_now(destroy, user_data);
        return connection.record(SQLITE_MISUSE, "bad parameters");
    }
    let count = usize::try_from(arguments).ok();
    if step.is_some() {
        destroy_now(destroy, user_data);
        return connection.record(
            SQLITE_ERROR,
            "user-defined aggregate functions are not supported yet",
        );
    }
    let functions = Arc::make_mut(&mut connection.functions);
    let Some(function) = function else {
        functions.remove(written, count);
        connection.succeeded();
        return SQLITE_OK;
    };
    let callback = Callback {
        function,
        user_data,
        destroy,
        db,
    };
    let native: NativeFunction = Arc::new(move |values: &[Value]| callback.call(values));
    functions.define(written, count, native);
    connection.succeeded();
    SQLITE_OK
}

/// Defines an SQL function (see [`sqlite3_create_function_v2`]), with no destructor.
///
/// # Safety
/// As for [`sqlite3_create_function_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut Sqlite3,
    name: *const c_char,
    arguments: c_int,
    text_representation: c_int,
    user_data: *mut c_void,
    function: ScalarCallback,
    step: ScalarCallback,
    last: FinalCallback,
) -> c_int {
    // SAFETY: the same contract as sqlite3_create_function_v2's.
    unsafe {
        sqlite3_create_function_v2(
            db,
            name,
            arguments,
            text_representation,
            user_data,
            function,
            step,
            last,
            None,
        )
    }
}

/// # Safety
/// `value` is NULL or a value handed to the running user function.
unsafe fn value_of<'a>(value: *mut Sqlite3Value) -> Option<&'a Sqlite3Value> {
    // SAFETY: the caller passes NULL or a live value.
    unsafe { value.as_ref() }
}

fn type_code(value: &Value) -> c_int {
    match value {
        Value::Null => SQLITE_NULL,
        Value::Boolean(_) | Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            SQLITE_INTEGER
        }
        Value::Real(_) | Value::Double(_) => SQLITE_FLOAT,
        Value::Bytea(_) => SQLITE_BLOB,
        _ => SQLITE_TEXT,
    }
}

fn integer_of(value: &Value) -> i64 {
    match value {
        Value::Boolean(flag) => i64::from(*flag),
        Value::SmallInt(number) => i64::from(*number),
        Value::Integer(number) => i64::from(*number),
        Value::BigInt(number) => *number,
        Value::Double(number) => *number as i64,
        Value::Real(number) => *number as i64,
        value => value
            .to_text()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0),
    }
}

fn double_of(value: &Value) -> f64 {
    match value {
        Value::Double(number) => *number,
        Value::Real(number) => f64::from(*number),
        Value::BigInt(number) => *number as f64,
        Value::Integer(number) => f64::from(*number),
        Value::SmallInt(number) => f64::from(*number),
        value => value
            .to_text()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0.0),
    }
}

impl Sqlite3Value {
    fn bytes(&self) -> Option<*const u8> {
        if self.value.is_null() {
            return None;
        }
        let mut cached = self.bytes.borrow_mut();
        if cached.is_none() {
            let mut bytes = match &self.value {
                Value::Bytea(bytes) => bytes.clone(),
                value => value.to_text()?.into_bytes(),
            };
            bytes.push(0);
            *cached = Some(bytes);
        }
        cached.as_ref().map(|bytes| bytes.as_ptr())
    }

    fn length(&self) -> c_int {
        if self.bytes().is_none() {
            return 0;
        }
        self.bytes
            .borrow()
            .as_ref()
            .map_or(0, |bytes| c_int::try_from(bytes.len() - 1).unwrap_or(c_int::MAX))
    }
}

/// The value's type: `SQLITE_INTEGER`, `SQLITE_FLOAT`, `SQLITE_TEXT`, `SQLITE_BLOB` or
/// `SQLITE_NULL`.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_type(value: *mut Sqlite3Value) -> c_int {
    // SAFETY: the caller passes a live value.
    unsafe { value_of(value) }.map_or(SQLITE_NULL, |value| type_code(&value.value))
}

/// The value as a 64-bit integer.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_int64(value: *mut Sqlite3Value) -> i64 {
    // SAFETY: the caller passes a live value.
    unsafe { value_of(value) }.map_or(0, |value| integer_of(&value.value))
}

/// The value as a 32-bit integer.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_int(value: *mut Sqlite3Value) -> c_int {
    // SAFETY: the same contract.
    let number = unsafe { sqlite3_value_int64(value) };
    number as c_int
}

/// The value as a floating-point number.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_double(value: *mut Sqlite3Value) -> c_double {
    // SAFETY: the caller passes a live value.
    unsafe { value_of(value) }.map_or(0.0, |value| double_of(&value.value))
}

/// The value as UTF-8 text, valid until the function returns; NULL for SQL's NULL.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_text(value: *mut Sqlite3Value) -> *const u8 {
    // SAFETY: the caller passes a live value.
    unsafe { value_of(value) }
        .and_then(Sqlite3Value::bytes)
        .unwrap_or(std::ptr::null())
}

/// The value as a blob, valid as for `sqlite3_value_text`.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_blob(value: *mut Sqlite3Value) -> *const c_void {
    // SAFETY: the same contract.
    unsafe { sqlite3_value_text(value) }.cast()
}

/// The size in bytes of `sqlite3_value_text` or `sqlite3_value_blob`.
///
/// # Safety
/// `value` is a value handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_value_bytes(value: *mut Sqlite3Value) -> c_int {
    // SAFETY: the caller passes a live value.
    unsafe { value_of(value) }.map_or(0, Sqlite3Value::length)
}

/// # Safety
/// `context` is NULL or the context handed to the running user function.
unsafe fn context_of<'a>(context: *mut Sqlite3Context) -> Option<&'a mut Sqlite3Context> {
    // SAFETY: the caller passes NULL or the live context, used by this call alone.
    unsafe { context.as_mut() }
}

fn answer(context: *mut Sqlite3Context, value: Value) {
    // SAFETY: the result functions are called with the running function's context.
    if let Some(context) = unsafe { context_of(context) } {
        context.result = value;
        context.error = None;
    }
}

/// The function answers NULL.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_null(context: *mut Sqlite3Context) {
    answer(context, Value::Null);
}

/// The function answers a 64-bit integer.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_int64(context: *mut Sqlite3Context, value: i64) {
    answer(context, Value::BigInt(value));
}

/// The function answers a 32-bit integer.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_int(context: *mut Sqlite3Context, value: c_int) {
    answer(context, Value::BigInt(i64::from(value)));
}

/// The function answers a floating-point number.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_double(context: *mut Sqlite3Context, value: c_double) {
    answer(context, Value::Double(value));
}

/// # Safety
/// `bytes` points to `length` bytes, or is a NUL-terminated string when `length` is negative.
unsafe fn copied(bytes: *const c_void, length: c_int) -> Option<Vec<u8>> {
    if bytes.is_null() {
        return None;
    }
    Some(match usize::try_from(length) {
        Ok(length) => {
            // SAFETY: the caller passes `length` readable bytes.
            unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), length) }.to_vec()
        }
        Err(_) => {
            // SAFETY: a negative length means a NUL-terminated string.
            unsafe { std::ffi::CStr::from_ptr(bytes.cast()) }
                .to_bytes()
                .to_vec()
        }
    })
}

/// The function answers `length` bytes of UTF-8 text (up to its NUL when negative); the text
/// is copied, so the destructor is called at once when it is a function.
///
/// # Safety
/// `context` is the running function's context and `text` points to `length` bytes or a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_text(
    context: *mut Sqlite3Context,
    text: *const c_char,
    length: c_int,
    destructor: Destructor,
) {
    // SAFETY: the caller passes `length` bytes or a NUL-terminated string.
    let bytes = unsafe { copied(text.cast(), length) };
    let value = bytes.map_or(Value::Null, |bytes| {
        Value::Text(
            String::from_utf8_lossy(bytes.split(|byte| *byte == 0).next().unwrap_or_default())
                .into_owned(),
        )
    });
    answer(context, value);
    release(destructor, text.cast_mut().cast());
}

/// The function answers `length` bytes as a blob, copied as for `sqlite3_result_text`.
///
/// # Safety
/// `context` is the running function's context and `blob` points to `length` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_blob(
    context: *mut Sqlite3Context,
    blob: *const c_void,
    length: c_int,
    destructor: Destructor,
) {
    let length = length.max(0);
    // SAFETY: the caller passes `length` readable bytes.
    let bytes = unsafe { copied(blob, length) }.unwrap_or_default();
    answer(context, Value::Bytea(bytes));
    release(destructor, blob.cast_mut());
}

fn release(destructor: Destructor, pointer: *mut c_void) {
    let Some(destroy) = destructor else {
        return;
    };
    if destroy as usize == usize::MAX || pointer.is_null() {
        return;
    }
    // SAFETY: a destructor other than SQLITE_STATIC and SQLITE_TRANSIENT frees the buffer the
    // caller handed over; this library copied it already.
    unsafe { destroy(pointer) };
}

/// The function fails with `length` bytes of `message` (up to its NUL when negative).
///
/// # Safety
/// `context` is the running function's context and `message` points to `length` bytes or a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_error(
    context: *mut Sqlite3Context,
    message: *const c_char,
    length: c_int,
) {
    // SAFETY: the caller passes `length` bytes or a NUL-terminated string.
    let text = unsafe { copied(message.cast(), length) }
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    // SAFETY: the caller passes the running function's context.
    if let Some(context) = unsafe { context_of(context) } {
        context.error = Some(text);
    }
}

/// The function fails for lack of memory.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_error_nomem(context: *mut Sqlite3Context) {
    // SAFETY: the caller passes the running function's context.
    if let Some(context) = unsafe { context_of(context) } {
        context.error = Some("out of memory".to_owned());
    }
}

/// The function fails because a value is too large.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_error_toobig(context: *mut Sqlite3Context) {
    // SAFETY: the caller passes the running function's context.
    if let Some(context) = unsafe { context_of(context) } {
        context.error = Some("string or blob too big".to_owned());
    }
}

/// The function answers a copy of `value`.
///
/// # Safety
/// `context` is the running function's context and `value` a value handed to it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_result_value(
    context: *mut Sqlite3Context,
    value: *mut Sqlite3Value,
) {
    // SAFETY: the caller passes a live value.
    let copy = unsafe { value_of(value) }.map_or(Value::Null, |value| value.value.clone());
    answer(context, copy);
}

/// The user data given when the running function was defined.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_user_data(context: *mut Sqlite3Context) -> *mut c_void {
    // SAFETY: the caller passes the running function's context.
    unsafe { context_of(context) }.map_or(std::ptr::null_mut(), |context| context.user_data)
}

/// The connection running the function.
///
/// # Safety
/// `context` is the context handed to the running user function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_context_db_handle(context: *mut Sqlite3Context) -> *mut Sqlite3 {
    // SAFETY: the caller passes the running function's context.
    unsafe { context_of(context) }.map_or(std::ptr::null_mut(), |context| context.db)
}

pub(crate) fn error_text(code: c_int) -> CString {
    CString::new(match code & 0xff {
        0 => "not an error",
        1 => "SQL logic error",
        2 => "internal logic error",
        3 => "access permission denied",
        4 => "query aborted",
        5 => "database is locked",
        6 => "database table is locked",
        7 => "out of memory",
        8 => "attempt to write a readonly database",
        9 => "interrupted",
        10 => "disk I/O error",
        11 => "database disk image is malformed",
        12 => "unknown operation",
        13 => "database or disk is full",
        14 => "unable to open database file",
        15 => "locking protocol",
        17 => "database schema has changed",
        18 => "string or blob too big",
        19 => "constraint failed",
        20 => "datatype mismatch",
        21 => "bad parameter or other API misuse",
        23 => "authorization denied",
        25 => "column index out of range",
        26 => "file is not a database",
        27 => "notification message",
        28 => "warning message",
        100 => "another row available",
        101 => "no more rows available",
        _ => "unknown error",
    })
    .unwrap_or_default()
}
