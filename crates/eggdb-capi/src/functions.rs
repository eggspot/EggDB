//! SQL functions the program defines (D1266): `eggdb_sql_create_function` registers a C
//! callback under a name for a SQL session, as `sqlite3_create_function` does, and the
//! `eggdb_function_result_*` calls set what one call of it answers.

use crate::args;
use crate::sql::{EggdbSql, EggdbValue, kind_and_number, session_state};
use crate::status::{Failure, guarded};
use eggdb::sql::{NativeFunction, NativeFunctions, Value};
use std::ffi::{c_char, c_int, c_void};
use std::sync::Arc;

const MOST_ARGUMENTS: c_int = 127;
const MOST_NAME_BYTES: usize = 255;

/// What one call of a defined function answers, set by the `eggdb_function_result_*` calls
/// (`eggdb_function_context` in `eggdb.h`); NULL when the callback sets nothing.
pub struct EggdbFunctionContext {
    answer: Result<Value, String>,
}

/// A defined function's body: it reads `count` arguments and sets its answer on `context`.
pub type EggdbFunction = unsafe extern "C" fn(
    context: *mut EggdbFunctionContext,
    user_data: *mut c_void,
    arguments: *const EggdbValue,
    count: usize,
);

/// Frees a defined function's `user_data` once the definition is replaced or the session ends.
pub type EggdbDestroy = unsafe extern "C" fn(user_data: *mut c_void);

struct Callback {
    function: EggdbFunction,
    user_data: *mut c_void,
    destroy: Option<EggdbDestroy>,
}

// SAFETY: eggdb.h asks the program to make a function and its user data callable from the
// thread that runs the session's statements, the only thread that calls it.
unsafe impl Send for Callback {}
// SAFETY: as above; the callback is never called from two threads at once.
unsafe impl Sync for Callback {}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            // SAFETY: eggdb.h gives `destroy` the user data it was registered with, once.
            unsafe { destroy(self.user_data) };
        }
    }
}

fn argument_bytes(value: &Value) -> Option<Vec<u8>> {
    let mut bytes = match value {
        Value::Null => return None,
        Value::Bytea(bytes) => bytes.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    };
    bytes.push(0);
    Some(bytes)
}

impl Callback {
    fn call(&self, values: &[Value]) -> Result<Value, String> {
        let kept: Vec<Option<Vec<u8>>> = values.iter().map(argument_bytes).collect();
        let arguments: Vec<EggdbValue> = values
            .iter()
            .zip(&kept)
            .map(|(value, bytes)| {
                let (kind, integer, real) = kind_and_number(value);
                EggdbValue {
                    kind,
                    integer,
                    real,
                    data: bytes.as_ref().map_or(std::ptr::null(), |bytes| bytes.as_ptr()),
                    len: bytes.as_ref().map_or(0, |bytes| bytes.len() - 1),
                }
            })
            .collect();
        let mut context = EggdbFunctionContext { answer: Ok(Value::Null) };
        // SAFETY: the context and the arguments live until the callback returns, and eggdb.h
        // asks the callback to read at most `count` arguments.
        unsafe { (self.function)(&raw mut context, self.user_data, arguments.as_ptr(), arguments.len()) };
        context.answer
    }
}

/// The functions defined on a session, shared with the statements it runs.
pub(crate) type DefinedFunctions = Arc<NativeFunctions>;

/// Defines the SQL function `name` with `arguments` arguments (-1 for any number) on the
/// session `sql`: calls of it in the session's statements run `function`, which sets its
/// answer with the `eggdb_function_result_*` calls. It replaces an earlier definition of the
/// same name and number of arguments, and a function of EggDB's of that name; `function` NULL
/// removes the definition. `destroy`, when not NULL, frees `user_data` once the definition is
/// replaced or removed or the session is closed. A session from `eggdb_sql_connect` refuses
/// it with `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `sql` is a live session used by this call alone, `name` a NUL-terminated string, and
/// `function` and `user_data` stay callable until `destroy` runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_create_function(
    sql: *mut EggdbSql,
    name: *const c_char,
    arguments: c_int,
    function: Option<EggdbFunction>,
    user_data: *mut c_void,
    destroy: Option<EggdbDestroy>,
) -> c_int {
    let callback = function.map(|function| Callback { function, user_data, destroy });
    guarded(move || {
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated name.
        let name = unsafe { args::text(name, "name") }?;
        if name.is_empty() || name.len() > MOST_NAME_BYTES {
            return Err(Failure::invalid_argument(format!("a function name has 1 to {MOST_NAME_BYTES} bytes")));
        }
        if !(-1..=MOST_ARGUMENTS).contains(&arguments) {
            return Err(Failure::invalid_argument(format!("arguments is -1 or 0 to {MOST_ARGUMENTS}")));
        }
        let functions = state.defined_functions()?;
        let count = usize::try_from(arguments).ok();
        let defined = Arc::make_mut(functions);
        match callback {
            Some(callback) => {
                let native: NativeFunction = Arc::new(move |values: &[Value]| callback.call(values));
                defined.define(name, count, native);
            }
            None => {
                defined.remove(name, count);
            }
        }
        Ok(())
    })
}

fn answer(context: *mut EggdbFunctionContext, value: Result<Value, String>) {
    // SAFETY: eggdb.h asks for the context the callback was given, during its call.
    if let Some(context) = unsafe { context.as_mut() } {
        context.answer = value;
    }
}

/// Answers SQL's NULL.
///
/// # Safety
/// `context` is the one the running callback was given.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_null(context: *mut EggdbFunctionContext) {
    answer(context, Ok(Value::Null));
}

/// Answers an integer.
///
/// # Safety
/// As for [`eggdb_function_result_null`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_int64(context: *mut EggdbFunctionContext, value: i64) {
    answer(context, Ok(Value::BigInt(value)));
}

/// Answers a floating-point number.
///
/// # Safety
/// As for [`eggdb_function_result_null`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_double(context: *mut EggdbFunctionContext, value: f64) {
    answer(context, Ok(Value::Double(value)));
}

fn bytes_at<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if data.is_null() {
        return None;
    }
    // SAFETY: eggdb.h asks for `len` readable bytes at `data`.
    Some(unsafe { std::slice::from_raw_parts(data, len) })
}

/// Answers the `len` bytes of UTF-8 text at `data`, copied before the call returns; text that
/// is not UTF-8 fails the statement.
///
/// # Safety
/// As for [`eggdb_function_result_null`]; `data` points to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_text(context: *mut EggdbFunctionContext, data: *const u8, len: usize) {
    let value = match bytes_at(data, len).map(std::str::from_utf8) {
        Some(Ok(text)) => Ok(Value::Text(text.to_owned())),
        _ => Err("the function answered text that is not UTF-8".to_owned()),
    };
    answer(context, value);
}

/// Answers the `len` bytes at `data` as binary data, copied before the call returns.
///
/// # Safety
/// As for [`eggdb_function_result_text`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_blob(context: *mut EggdbFunctionContext, data: *const u8, len: usize) {
    let value = match bytes_at(data, len) {
        Some(bytes) => Ok(Value::Bytea(bytes.to_vec())),
        None => Err("the function answered NULL data with a length".to_owned()),
    };
    answer(context, value);
}

/// Fails the statement that called the function with `message` (a NUL-terminated string;
/// NULL gives a generic one), SQLSTATE 38000.
///
/// # Safety
/// As for [`eggdb_function_result_null`]; `message` is NULL or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_function_result_error(context: *mut EggdbFunctionContext, message: *const c_char) {
    let text = if message.is_null() {
        "the function failed".to_owned()
    } else {
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated message.
        unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned()
    };
    answer(context, Err(text));
}
