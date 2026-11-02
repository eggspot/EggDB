use crate::args;
use crate::sql::{
    EGGDB_BLOB, EGGDB_INTEGER, EGGDB_NULL, EGGDB_REAL, EGGDB_TEXT, EggdbResult, EggdbSql,
    ResultBatch, session_state,
};
use crate::status::{Failure, guarded};
use eggdb::sql::{OutputColumn, Parameter, SqlType};
use std::ffi::{CString, c_char, c_int};

#[derive(Debug)]
struct StatementColumn {
    name: CString,
    kind: i32,
}

/// A prepared statement (`eggdb_stmt` in `eggdb.h`): one SQL statement checked once by
/// [`eggdb_sql_prepare`], with its parameters bound by the `eggdb_stmt_bind_*` calls and run
/// as many times as needed by [`eggdb_stmt_execute`]; freed by [`eggdb_stmt_finalize`]. It
/// belongs to its session: a call on it counts as a call on the session, and the session
/// cannot close while it lives.
#[derive(Debug)]
pub struct EggdbStmt {
    session: *mut EggdbSql,
    text: String,
    parameters: Vec<Parameter>,
    columns: Vec<StatementColumn>,
    results: ResultBatch,
}

const NULL_PARAMETER: Parameter = Parameter {
    declared_type: None,
    value: None,
};

fn kind_of(sql_type: Option<SqlType>) -> i32 {
    match sql_type {
        None => EGGDB_NULL,
        Some(SqlType::Boolean | SqlType::SmallInt | SqlType::Integer | SqlType::BigInt) => {
            EGGDB_INTEGER
        }
        Some(SqlType::Real | SqlType::Double) => EGGDB_REAL,
        Some(SqlType::Bytea) => EGGDB_BLOB,
        Some(_) => EGGDB_TEXT,
    }
}

fn column_name(name: &str) -> CString {
    CString::new(name.replace('\0', "\u{fffd}")).unwrap_or_default()
}

fn statement_columns(columns: Option<Vec<OutputColumn>>) -> Vec<StatementColumn> {
    columns
        .unwrap_or_default()
        .iter()
        .map(|column| StatementColumn {
            name: column_name(&column.name),
            kind: kind_of(column.sql_type),
        })
        .collect()
}

fn double_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    format!("{value:?}")
}

fn bytea_text(bytes: &[u8]) -> String {
    let digit = |nibble: u8| char::from_digit(u32::from(nibble), 16).unwrap_or('0');
    let mut text = String::with_capacity(bytes.len().saturating_mul(2).saturating_add(2));
    text.push_str("\\x");
    for byte in bytes {
        text.push(digit(byte >> 4));
        text.push(digit(byte & 0x0f));
    }
    text
}

impl EggdbStmt {
    fn parameter_mut(&mut self, index: usize) -> Result<&mut Parameter, Failure> {
        let count = self.parameters.len();
        index
            .checked_sub(1)
            .and_then(|at| self.parameters.get_mut(at))
            .ok_or_else(|| {
                Failure::invalid_argument(format!(
                    "parameter index {index} is out of range; this statement takes parameters 1 to {count}"
                ))
            })
    }

    fn bind(&mut self, index: usize, bound: Parameter) -> Result<(), Failure> {
        *self.parameter_mut(index)? = bound;
        Ok(())
    }

    fn column(&self, column: usize) -> Result<&StatementColumn, Failure> {
        self.columns.get(column).ok_or_else(|| {
            Failure::invalid_argument(format!(
                "column {column} is out of range; this statement's columns known before it runs are 0 to {} ({} of them)",
                self.columns.len().saturating_sub(1),
                self.columns.len()
            ))
        })
    }
}

unsafe fn statement<'a>(stmt: *mut EggdbStmt) -> Result<&'a mut EggdbStmt, Failure> {
    // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
    unsafe { args::handle_mut(stmt, "stmt") }
}

unsafe fn bind_with(stmt: *mut EggdbStmt, index: usize, bound: Parameter) -> c_int {
    guarded(|| {
        // SAFETY: the caller's promise for `stmt` is the one `statement` needs.
        unsafe { statement(stmt) }?.bind(index, bound)
    })
}

/// Checks the one SQL statement in the `len` bytes of UTF-8 at `text` (no NUL needed) on the
/// session and puts a prepared statement for it in `*stmt` (NULL on failure): its parameters
/// (`$1` in PostgreSQL's and Oracle's dialects, `?` or `?1` in SQLite's and MySQL's), all NULL
/// until bound, and the columns of the rows it returns, when they are known before it runs. A
/// syntax error, more than one statement, or a table or column that does not exist fails with
/// `EGGDB_ERR_SQL`. A session from `eggdb_sql_connect` fails with `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` used by this call alone, `text` points to `len`
/// readable bytes, and `stmt` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_prepare(
    sql: *mut EggdbSql,
    text: *const c_char,
    len: usize,
    stmt: *mut *mut EggdbStmt,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `stmt`.
        unsafe { args::write(stmt, std::ptr::null_mut(), "stmt") }?;
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        // SAFETY: eggdb.h asks for `len` readable bytes at `text`.
        let bytes = unsafe { args::bytes(text.cast::<u8>(), len, "text") }?;
        let text = std::str::from_utf8(bytes).map_err(|_| {
            Failure::invalid_argument("text is not UTF-8; EggDB takes text as UTF-8")
        })?;
        let session = state.local_session()?;
        let outline = session
            .prepare(text, &[])
            .map_err(|error| Failure::sql(&error))?;
        let columns = if outline.is_empty {
            Vec::new()
        } else {
            let described = session
                .describe(text, &[])
                .map_err(|error| Failure::sql(&error))?;
            statement_columns(described.columns)
        };
        // SAFETY: as for `session_state` above; this only counts the statement.
        unsafe { args::handle(sql, "sql") }?.statement_prepared();
        let handle = args::give(EggdbStmt {
            session: sql,
            text: text.to_owned(),
            parameters: vec![NULL_PARAMETER; outline.parameter_count],
            columns,
            results: ResultBatch::default(),
        });
        // SAFETY: as for the first write to `stmt`.
        unsafe { args::write(stmt, handle, "stmt") }
    })
}

/// Puts the number of the statement's parameters in `*count`: the highest `$n` it uses, or
/// the number of `?` placeholders.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `count` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_parameter_count(
    stmt: *mut EggdbStmt,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live statement handle.
        let stmt = unsafe { statement(stmt) }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, stmt.parameters.len(), "count") }
    })
}

/// Binds SQL's NULL to parameter `index` (1 for `$1` or the first `?`). Every parameter is
/// NULL until bound.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_null(stmt: *mut EggdbStmt, index: usize) -> c_int {
    // SAFETY: the caller's promise for `stmt` is the one `bind_with` needs.
    unsafe { bind_with(stmt, index, NULL_PARAMETER) }
}

/// Binds a 64-bit integer to parameter `index`, as a `bigint`, which the statement converts
/// to the type its place asks for (a column's type, for example).
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_int64(
    stmt: *mut EggdbStmt,
    index: usize,
    value: i64,
) -> c_int {
    let bound = Parameter {
        declared_type: Some(SqlType::BigInt),
        value: Some(value.to_string()),
    };
    // SAFETY: the caller's promise for `stmt` is the one `bind_with` needs.
    unsafe { bind_with(stmt, index, bound) }
}

/// Binds a double-precision number to parameter `index`, as a `double precision`, exactly
/// (infinities and NaN included).
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_double(
    stmt: *mut EggdbStmt,
    index: usize,
    value: f64,
) -> c_int {
    let bound = Parameter {
        declared_type: Some(SqlType::Double),
        value: Some(double_text(value)),
    };
    // SAFETY: the caller's promise for `stmt` is the one `bind_with` needs.
    unsafe { bind_with(stmt, index, bound) }
}

/// Binds a boolean to parameter `index`: false for 0, true for any other value.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_bool(
    stmt: *mut EggdbStmt,
    index: usize,
    value: c_int,
) -> c_int {
    let bound = Parameter {
        declared_type: Some(SqlType::Boolean),
        value: Some(if value == 0 { "false" } else { "true" }.to_owned()),
    };
    // SAFETY: the caller's promise for `stmt` is the one `bind_with` needs.
    unsafe { bind_with(stmt, index, bound) }
}

/// Binds the `len` bytes of UTF-8 text at `text` (no NUL needed) to parameter `index`. The
/// text is read as a literal of the type its place asks for (`42`, `2026-10-30`, JSON), so it
/// binds any type that has a text form. The bytes are copied.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `text` points to
/// `len` readable bytes (NULL with 0 binds the empty text).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_text(
    stmt: *mut EggdbStmt,
    index: usize,
    text: *const c_char,
    len: usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for `len` readable bytes at `text`.
        let bytes = unsafe { args::bytes(text.cast::<u8>(), len, "text") }?;
        let text = std::str::from_utf8(bytes).map_err(|_| {
            Failure::invalid_argument(format!("the text for parameter {index} is not UTF-8"))
        })?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        unsafe { statement(stmt) }?.bind(
            index,
            Parameter {
                declared_type: None,
                value: Some(text.to_owned()),
            },
        )
    })
}

/// Binds the `len` bytes at `data` to parameter `index` as binary data (`bytea`, `BLOB`,
/// `varbinary`). Any bytes may be bound; they are copied.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `data` points to
/// `len` readable bytes (NULL with 0 binds empty binary data).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_bind_blob(
    stmt: *mut EggdbStmt,
    index: usize,
    data: *const u8,
    len: usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for `len` readable bytes at `data`.
        let bytes = unsafe { args::bytes(data, len, "data") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        unsafe { statement(stmt) }?.bind(
            index,
            Parameter {
                declared_type: Some(SqlType::Bytea),
                value: Some(bytea_text(bytes)),
            },
        )
    })
}

/// Sets every parameter back to NULL.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_clear_bindings(stmt: *mut EggdbStmt) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        stmt.parameters.fill(NULL_PARAMETER);
        Ok(())
    })
}

/// Runs the statement with the values bound now and puts its results in `*results` (one, or
/// none for an empty statement) and their number in `*count`, read as `eggdb_sql_execute`'s
/// are. The results belong to the statement and stay valid until its next
/// `eggdb_stmt_execute`, `eggdb_stmt_reset` or `eggdb_stmt_finalize`; the bindings stay for
/// the next run. A failure is reported as `eggdb_sql_execute` reports one, with no results.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare`, used by this call alone and with no other call
/// on its session meanwhile (but `eggdb_sql_cancel`), and `results` and `count` point to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_execute(
    stmt: *mut EggdbStmt,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        // SAFETY: a live statement's session is open (it refuses to close while statements
        // live), and eggdb.h asks that no other call uses the session during this one.
        let state = unsafe { session_state(stmt.session, "the statement's session") }?;
        let (published, len) = state.run_into(&mut stmt.results, &stmt.text, &stmt.parameters)?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

/// Frees the results of the statement's last run, keeping the statement and its bindings for
/// the next one.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone; the results of its last
/// run must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_reset(stmt: *mut EggdbStmt) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        stmt.results = ResultBatch::default();
        Ok(())
    })
}

/// Puts in `*count` the number of columns of the rows the statement returns, as far as they
/// are known before it runs: 0 for a statement that returns no rows, or whose columns are
/// only known once it runs (the result's `column_count` is then the one to read).
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `count` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_column_count(stmt: *mut EggdbStmt, count: *mut usize) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live statement handle.
        let stmt = unsafe { statement(stmt) }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, stmt.columns.len(), "count") }
    })
}

/// Puts the name of column `column` (0 for the first) in `*name`, a NUL-terminated UTF-8
/// string the statement owns until `eggdb_stmt_finalize`.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `name` points to
/// writable memory for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_column_name(
    stmt: *mut EggdbStmt,
    column: usize,
    name: *mut *const c_char,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `name`.
        unsafe { args::write(name, std::ptr::null(), "name") }?;
        // SAFETY: eggdb.h asks for a live statement handle.
        let stmt = unsafe { statement(stmt) }?;
        let found = stmt.column(column)?.name.as_ptr();
        // SAFETY: as for the first write to `name`.
        unsafe { args::write(name, found, "name") }
    })
}

/// Puts in `*kind` the `eggdb_value.kind` the values of column `column` (0 for the first)
/// come back with, from the column's type: `EGGDB_INTEGER`, `EGGDB_REAL`, `EGGDB_BLOB` or
/// `EGGDB_TEXT`, or `EGGDB_NULL` when the type is only known once the statement runs. Any
/// value may still be NULL.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `kind` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_column_type(
    stmt: *mut EggdbStmt,
    column: usize,
    kind: *mut i32,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one int32_t at `kind`.
        unsafe { args::write(kind, EGGDB_NULL, "kind") }?;
        // SAFETY: eggdb.h asks for a live statement handle.
        let stmt = unsafe { statement(stmt) }?;
        let found = stmt.column(column)?.kind;
        // SAFETY: as for the first write to `kind`.
        unsafe { args::write(kind, found, "kind") }
    })
}

/// Frees the statement, its bindings and its results. NULL does nothing.
///
/// # Safety
/// `stmt` is NULL or a handle from `eggdb_sql_prepare` not finalized yet, with no other call on
/// its session meanwhile; it and its results must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_finalize(stmt: *mut EggdbStmt) -> c_int {
    guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(stmt) }) else {
            return Ok(());
        };
        // SAFETY: a live statement's session is open, since it refuses to close while
        // statements live.
        unsafe { args::handle(handle.session, "the statement's session") }?.statement_finalized();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{bytea_text, double_text, kind_of};
    use crate::sql::{EGGDB_BLOB, EGGDB_INTEGER, EGGDB_NULL, EGGDB_REAL, EGGDB_TEXT};
    use eggdb::sql::SqlType;

    #[test]
    fn doubles_are_bound_in_a_text_that_reads_back_exactly() {
        assert_eq!(double_text(2.5), "2.5");
        assert_eq!(double_text(0.1), "0.1");
        assert_eq!(double_text(-3.0), "-3.0");
        assert_eq!(double_text(f64::INFINITY), "Infinity");
        assert_eq!(double_text(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(double_text(f64::NAN), "NaN");
        let tiny = 5e-324_f64;
        assert_eq!(double_text(tiny).parse::<f64>().ok(), Some(tiny));
    }

    #[test]
    fn binary_data_is_bound_in_bytea_hex_form() {
        assert_eq!(bytea_text(&[]), "\\x");
        assert_eq!(bytea_text(&[0, 0x1f, 0xff]), "\\x001fff");
    }

    #[test]
    fn column_types_give_the_kind_their_values_come_back_with() {
        assert_eq!(kind_of(None), EGGDB_NULL);
        assert_eq!(kind_of(Some(SqlType::Boolean)), EGGDB_INTEGER);
        assert_eq!(kind_of(Some(SqlType::BigInt)), EGGDB_INTEGER);
        assert_eq!(kind_of(Some(SqlType::Double)), EGGDB_REAL);
        assert_eq!(kind_of(Some(SqlType::Bytea)), EGGDB_BLOB);
        assert_eq!(kind_of(Some(SqlType::Text)), EGGDB_TEXT);
    }
}
