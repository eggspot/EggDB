use crate::args;
use crate::sql::{
    Backend, EGGDB_BLOB, EGGDB_INTEGER, EGGDB_NULL, EGGDB_REAL, EGGDB_TEXT, EggdbResult, EggdbSql,
    EggdbValue, Holder, ResultBatch, SessionState, gather_local, remote_kind, session_state,
    text_values,
};
use crate::status::{EGGDB_ERR_INTERNAL, Failure, guarded};
use eggdb::sql::{Dialect, OutputColumn, Parameter, SqlType, Value};
use eggdb_pgwire::client::{Answer, ClientError};
use std::ffi::{CString, c_char, c_int};

#[derive(Debug)]
struct StatementColumn {
    name: CString,
    kind: i32,
}

#[derive(Debug)]
enum Stepping {
    Idle,
    Local {
        columns: Vec<OutputColumn>,
        dialect: Dialect,
        rows: std::vec::IntoIter<Vec<Value>>,
    },
    Remote {
        column_types: Vec<u32>,
    },
    Ended,
}

/// A prepared statement (`eggdb_stmt` in `eggdb.h`): one SQL statement checked once by
/// [`eggdb_sql_prepare`], with its parameters bound by the `eggdb_stmt_bind_*` calls and run
/// as many times as needed by [`eggdb_stmt_execute`], or stepped through a row at a time by
/// [`eggdb_stmt_step`]; freed by [`eggdb_stmt_finalize`]. It belongs to its session: a call
/// on it counts as a call on the session, and the session cannot close while it lives.
#[derive(Debug)]
pub struct EggdbStmt {
    session: *mut EggdbSql,
    id: u64,
    text: String,
    remote_name: Option<String>,
    parameters: Vec<Parameter>,
    columns: Vec<StatementColumn>,
    results: ResultBatch,
    stepping: Stepping,
    row: ResultBatch,
    has_row: bool,
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

fn statement_columns(columns: &[OutputColumn]) -> Vec<StatementColumn> {
    columns
        .iter()
        .map(|column| StatementColumn {
            name: column_name(&column.name),
            kind: kind_of(column.sql_type),
        })
        .collect()
}

fn remote_columns(names: &[String], types: &[u32]) -> Vec<StatementColumn> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| StatementColumn {
            name: column_name(name),
            kind: remote_kind(types.get(index).copied().unwrap_or(0)),
        })
        .collect()
}

fn not_convertible(column: usize, kind: i32, wanted: &str) -> Failure {
    let found = match kind {
        EGGDB_NULL => "NULL",
        EGGDB_INTEGER => "an integer",
        EGGDB_REAL => "a real number",
        EGGDB_BLOB => "binary data",
        _ => "text",
    };
    Failure::invalid_argument(format!(
        "column {column} of the current row holds {found}, which does not read as {wanted}; check its kind with eggdb_stmt_value_kind"
    ))
}

const I64_BOUND: f64 = 9_223_372_036_854_775_808.0;

fn whole_number(real: f64) -> Option<i64> {
    let fits = real.fract() == 0.0 && (-I64_BOUND..I64_BOUND).contains(&real);
    fits.then_some(real as i64)
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

    fn current_value(&self, column: usize) -> Result<EggdbValue, Failure> {
        if !self.has_row {
            return Err(Failure::invalid_argument(
                "there is no current row; call eggdb_stmt_step until it reports a row first",
            ));
        }
        let values = self.row.row_values();
        values.get(column).copied().ok_or_else(|| {
            Failure::invalid_argument(format!(
                "column {column} is out of range; the current row has columns 0 to {} ({} of them)",
                values.len().saturating_sub(1),
                values.len()
            ))
        })
    }

    fn current_text(&self, column: usize) -> Option<&str> {
        std::str::from_utf8(self.row.row_bytes(column)?).ok()
    }

    fn start_stepping(&mut self, state: &mut SessionState) -> Result<(), Failure> {
        match &mut state.backend {
            Backend::Local { session, .. } => {
                let gathered =
                    gather_local(session, &self.text, Some(self.parameters.as_slice()), None)?;
                let dialect = session.dialect();
                let with_rows = gathered.into_iter().find(|result| result.columns.is_some());
                let Some(result) = with_rows else {
                    self.stepping = Stepping::Ended;
                    return Ok(());
                };
                let columns = result.columns.unwrap_or_default();
                self.columns = statement_columns(&columns);
                self.stepping = Stepping::Local {
                    columns,
                    dialect,
                    rows: result.rows.into_iter(),
                };
            }
            Backend::Remote(remote) => {
                let name = self.remote_name.as_deref().ok_or_else(|| {
                    Failure::new(
                        EGGDB_ERR_INTERNAL,
                        "a statement on a server session has no name on the server",
                    )
                })?;
                let bound = text_values(&self.parameters);
                let started = remote.ready_client()?.start_prepared(name, &bound);
                started.map_err(|error| remote.failed(error))?;
                self.stepping = Stepping::Remote {
                    column_types: Vec::new(),
                };
            }
        }
        state.hold(Holder::Stepping(self.id));
        Ok(())
    }

    fn step(&mut self, state: &mut SessionState) -> Result<bool, Failure> {
        self.has_row = false;
        self.row.clear();
        if matches!(self.stepping, Stepping::Idle) {
            state.ensure_free()?;
            if let Err(failure) = self.start_stepping(state) {
                self.stepping = Stepping::Ended;
                return Err(failure);
            }
        }
        let mut failure = None;
        let found = match &mut self.stepping {
            Stepping::Idle | Stepping::Ended => false,
            Stepping::Local {
                columns,
                dialect,
                rows,
            } => match rows.next() {
                Some(row) => {
                    self.row.set_local_row(&row, columns, *dialect);
                    true
                }
                None => false,
            },
            Stepping::Remote { column_types } => {
                let remote = state.remote()?;
                loop {
                    let answer = remote.client().map(|client| client.next_answer());
                    match answer {
                        Ok(Ok(Answer::Columns { names, types })) => {
                            self.columns = remote_columns(&names, &types);
                            *column_types = types;
                        }
                        Ok(Ok(Answer::Row(values))) => {
                            self.row.set_remote_row(&values, column_types);
                            break true;
                        }
                        Ok(Ok(Answer::Complete(_))) => {}
                        Ok(Ok(Answer::Ready)) => break false,
                        Ok(Err(error)) => {
                            failure = Some(remote.failed(error));
                            break false;
                        }
                        Err(broken) => {
                            failure = Some(broken);
                            break false;
                        }
                    }
                }
            }
        };
        if !found {
            self.stepping = Stepping::Ended;
            self.row.clear();
            state.release(Holder::Stepping(self.id));
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        self.has_row = found;
        Ok(found)
    }

    fn end_stepping(&mut self, state: &mut SessionState) -> Result<(), Failure> {
        let stepping = std::mem::replace(&mut self.stepping, Stepping::Idle);
        self.has_row = false;
        self.row = ResultBatch::default();
        let drained = match stepping {
            Stepping::Remote { .. } => drain_answers(state),
            _ => Ok(()),
        };
        state.release(Holder::Stepping(self.id));
        drained
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

fn drain_answers(state: &mut SessionState) -> Result<(), Failure> {
    let remote = state.remote()?;
    loop {
        let answer = remote.client()?.next_answer();
        match answer {
            Ok(Answer::Ready) | Err(ClientError::Server { .. }) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(remote.failed(error)),
        }
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
/// `EGGDB_ERR_SQL`. On a session from `eggdb_sql_connect` the server prepares it as a named
/// statement (Parse and Describe), which it keeps until `eggdb_stmt_finalize`.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` or `eggdb_sql_connect` used by this call alone,
/// `text` points to `len` readable bytes, and `stmt` points to writable memory for one handle.
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
        state.ensure_free()?;
        let id = state.next_handle_id();
        let (parameter_count, columns, remote_name) = match &mut state.backend {
            Backend::Local { session, .. } => {
                let outline = session
                    .prepare(text, &[])
                    .map_err(|error| Failure::sql(&error))?;
                let columns = if outline.is_empty {
                    Vec::new()
                } else {
                    let described = session
                        .describe(text, &[])
                        .map_err(|error| Failure::sql(&error))?;
                    statement_columns(described.columns.as_deref().unwrap_or_default())
                };
                (outline.parameter_count, columns, None)
            }
            Backend::Remote(remote) => {
                let name = format!("eggdb_capi_{id}");
                let described = remote.ready_client()?.prepare_named(&name, text);
                let outline = described.map_err(|error| remote.failed(error))?;
                let columns = remote_columns(&outline.columns, &outline.column_types);
                (outline.parameter_types.len(), columns, Some(name))
            }
        };
        // SAFETY: as for `session_state` above; this only counts the statement.
        unsafe { args::handle(sql, "sql") }?.statement_prepared();
        let handle = args::give(EggdbStmt {
            session: sql,
            id,
            text: text.to_owned(),
            remote_name,
            parameters: vec![NULL_PARAMETER; parameter_count],
            columns,
            results: ResultBatch::default(),
            stepping: Stepping::Idle,
            row: ResultBatch::default(),
            has_row: false,
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
/// A run being stepped through with `eggdb_stmt_step` ends first, as `eggdb_stmt_reset` ends
/// it.
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
        stmt.end_stepping(state)?;
        let (published, len) = state.run_statement_into(
            &mut stmt.results,
            &stmt.text,
            stmt.remote_name.as_deref(),
            &stmt.parameters,
        )?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

/// Frees the results of the statement's last run and ends a run being stepped through
/// (reading and dropping the rows a server still sends), keeping the statement and its
/// bindings for the next one: its next `eggdb_stmt_step` starts a new run. Whatever the
/// status, the session takes other calls again.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, with no other call on
/// its session meanwhile; the results and the current row of its last run must not be used
/// afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_reset(stmt: *mut EggdbStmt) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        stmt.results = ResultBatch::default();
        // SAFETY: a live statement's session is open (it refuses to close while statements
        // live), and eggdb.h asks that no other call uses the session during this one.
        let state = unsafe { session_state(stmt.session, "the statement's session") }?;
        stmt.end_stepping(state)
    })
}

/// Steps the statement to its next row and puts in `*has_row` 1 when there is one, or 0 at
/// the end. The first step runs the statement with the values bound now. In the process the
/// engine gives the statement's rows at once and each step makes one row's values; on a
/// session from `eggdb_sql_connect` each step reads one row from the connection, so a large
/// result never has to fit in memory. Read the current row with `eggdb_stmt_row` or the
/// `eggdb_stmt_value_*` calls, valid until the next step, reset or finalize; once the run has
/// started, `eggdb_stmt_column_count`, `eggdb_stmt_column_name` and `eggdb_stmt_column_type`
/// give the run's columns. After the end it keeps reporting 0 until `eggdb_stmt_reset` or
/// `eggdb_stmt_execute`. From the first step until the end or a reset, other calls that run
/// statements on the session fail with `EGGDB_ERR_IN_USE`. A failure is reported as
/// `eggdb_sql_execute` reports one and ends the run.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, with no other call on
/// its session meanwhile (but `eggdb_sql_cancel`), and `has_row` points to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_step(stmt: *mut EggdbStmt, has_row: *mut c_int) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one int at `has_row`.
        unsafe { args::write(has_row, 0, "has_row") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        // SAFETY: a live statement's session is open, and eggdb.h asks that no other call
        // uses the session during this one.
        let state = unsafe { session_state(stmt.session, "the statement's session") }?;
        let found = stmt.step(state)?;
        // SAFETY: as for the first write to `has_row`.
        unsafe { args::write(has_row, args::flag(found), "has_row") }
    })
}

/// Puts the current row's values in `*values`, one `eggdb_value` per column read as a
/// result's values are, and their number in `*count`. They belong to the statement until its
/// next step, reset or finalize. Without a current row (before the first step or after the
/// end) it fails with `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `values` and
/// `count` point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_row(
    stmt: *mut EggdbStmt,
    values: *mut *const EggdbValue,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `values`.
        unsafe { args::write(values, std::ptr::null(), "values") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        if !stmt.has_row {
            return Err(Failure::invalid_argument(
                "there is no current row; call eggdb_stmt_step until it reports a row first",
            ));
        }
        let row = stmt.row.row_values();
        let pointer = if row.is_empty() {
            std::ptr::null()
        } else {
            row.as_ptr()
        };
        // SAFETY: as for the first write to `values`.
        unsafe { args::write(values, pointer, "values") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, row.len(), "count") }
    })
}

/// Puts in `*kind` the kind of column `column` (0 for the first) of the current row:
/// `EGGDB_NULL`, `EGGDB_INTEGER`, `EGGDB_REAL`, `EGGDB_TEXT` or `EGGDB_BLOB`, as
/// `eggdb_value.kind`.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `kind` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_value_kind(
    stmt: *mut EggdbStmt,
    column: usize,
    kind: *mut i32,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one int32_t at `kind`.
        unsafe { args::write(kind, EGGDB_NULL, "kind") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let value = unsafe { statement(stmt) }?.current_value(column)?;
        // SAFETY: as for the first write to `kind`.
        unsafe { args::write(kind, value.kind, "kind") }
    })
}

/// Puts column `column` of the current row in `*value` as a 64-bit integer: an integer or a
/// boolean (1 and 0) as it is, a real number with no fraction that fits, or a text that is a
/// decimal integer (a `numeric` such as `42`). NULL and anything else fail with
/// `EGGDB_ERR_INVALID_ARGUMENT` and leave 0.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `value` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_value_int64(
    stmt: *mut EggdbStmt,
    column: usize,
    value: *mut i64,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one int64_t at `value`.
        unsafe { args::write(value, 0, "value") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        let current = stmt.current_value(column)?;
        let number = match current.kind {
            EGGDB_INTEGER => Some(current.integer),
            EGGDB_REAL => whole_number(current.real),
            EGGDB_TEXT => stmt
                .current_text(column)
                .and_then(|text| text.trim().parse::<i64>().ok()),
            _ => None,
        }
        .ok_or_else(|| not_convertible(column, current.kind, "a 64-bit integer"))?;
        // SAFETY: as for the first write to `value`.
        unsafe { args::write(value, number, "value") }
    })
}

/// Puts column `column` of the current row in `*value` as a double: a real number as it is,
/// an integer converted, or a text that is a number (a `numeric`, `Infinity`, `NaN`). NULL and
/// anything else fail with `EGGDB_ERR_INVALID_ARGUMENT` and leave 0.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `value` points to
/// writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_value_double(
    stmt: *mut EggdbStmt,
    column: usize,
    value: *mut f64,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one double at `value`.
        unsafe { args::write(value, 0.0, "value") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let stmt = unsafe { statement(stmt) }?;
        let current = stmt.current_value(column)?;
        let number = match current.kind {
            EGGDB_REAL => Some(current.real),
            EGGDB_INTEGER => Some(current.integer as f64),
            EGGDB_TEXT => stmt
                .current_text(column)
                .and_then(|text| text.trim().parse::<f64>().ok()),
            _ => None,
        }
        .ok_or_else(|| not_convertible(column, current.kind, "a double"))?;
        // SAFETY: as for the first write to `value`.
        unsafe { args::write(value, number, "value") }
    })
}

/// Puts the text of column `column` of the current row in `*text`, UTF-8 followed by a NUL
/// byte not counted in `*len`, as the dialect's clients print it (numbers and booleans too).
/// NULL gives a NULL `*text` and 0; binary data fails with `EGGDB_ERR_INVALID_ARGUMENT` (read
/// it with `eggdb_stmt_value_blob`). The text belongs to the statement until its next step,
/// reset or finalize.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `text` and `len`
/// point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_value_text(
    stmt: *mut EggdbStmt,
    column: usize,
    text: *mut *const c_char,
    len: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `text`.
        unsafe { args::write(text, std::ptr::null(), "text") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `len`.
        unsafe { args::write(len, 0, "len") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let current = unsafe { statement(stmt) }?.current_value(column)?;
        if current.kind == EGGDB_BLOB {
            return Err(not_convertible(column, current.kind, "text"));
        }
        // SAFETY: as for the first write to `text`.
        unsafe { args::write(text, current.data.cast::<c_char>(), "text") }?;
        // SAFETY: as for the first write to `len`.
        unsafe { args::write(len, current.len, "len") }
    })
}

/// Puts the bytes of column `column` of the current row in `*data` and their number in
/// `*len`: binary data's bytes, or any other value's text. NULL gives NULL and 0. The bytes
/// belong to the statement until its next step, reset or finalize.
///
/// # Safety
/// `stmt` is a handle from `eggdb_sql_prepare` used by this call alone, and `data` and `len`
/// point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_value_blob(
    stmt: *mut EggdbStmt,
    column: usize,
    data: *mut *const u8,
    len: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `data`.
        unsafe { args::write(data, std::ptr::null(), "data") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `len`.
        unsafe { args::write(len, 0, "len") }?;
        // SAFETY: eggdb.h asks for a live statement handle used by this call alone.
        let current = unsafe { statement(stmt) }?.current_value(column)?;
        // SAFETY: as for the first write to `data`.
        unsafe { args::write(data, current.data, "data") }?;
        // SAFETY: as for the first write to `len`.
        unsafe { args::write(len, current.len, "len") }
    })
}

/// Puts in `*count` the number of columns of the rows the statement returns, as far as they
/// are known before it runs: 0 for a statement that returns no rows, or whose columns are
/// only known once it runs (the result's `column_count` is then the one to read). Once
/// `eggdb_stmt_step` has started a run, they are the run's columns.
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

/// Frees the statement, its bindings and its results, ending a run being stepped through as
/// `eggdb_stmt_reset` does. On a session from `eggdb_sql_connect` the server's statement is
/// closed with the session's next call. NULL does nothing. The statement is freed whatever
/// the status.
///
/// # Safety
/// `stmt` is NULL or a handle from `eggdb_sql_prepare` not finalized yet, with no other call on
/// its session meanwhile; it and its results must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stmt_finalize(stmt: *mut EggdbStmt) -> c_int {
    guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(mut handle) = (unsafe { args::take(stmt) }) else {
            return Ok(());
        };
        // SAFETY: a live statement's session is open, since it refuses to close while
        // statements live, and eggdb.h asks that no other call uses the session meanwhile.
        let state = unsafe { session_state(handle.session, "the statement's session") }?;
        let ended = handle.end_stepping(state);
        if let Some(name) = handle.remote_name.take()
            && let Ok(remote) = state.remote()
        {
            remote.close_later(name);
        }
        // SAFETY: as above; this only counts the statement.
        unsafe { args::handle(handle.session, "the statement's session") }?.statement_finalized();
        ended
    })
}

#[cfg(test)]
mod tests {
    use super::{bytea_text, double_text, kind_of, remote_columns, whole_number};
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
    fn only_whole_reals_that_fit_read_as_integers() {
        assert_eq!(whole_number(42.0), Some(42));
        assert_eq!(whole_number(-0.0), Some(0));
        assert_eq!(whole_number(-9_223_372_036_854_775_808.0), Some(i64::MIN));
        assert_eq!(whole_number(9_223_372_036_854_775_808.0), None);
        assert_eq!(whole_number(2.5), None);
        assert_eq!(whole_number(f64::NAN), None);
        assert_eq!(whole_number(f64::INFINITY), None);
    }

    #[test]
    fn server_column_types_give_the_kind_their_values_come_back_with() {
        let names = [
            "id".to_owned(),
            "price".to_owned(),
            "raw".to_owned(),
            "at".to_owned(),
        ];
        let kinds: Vec<i32> = remote_columns(&names, &[20, 701, 17, 1184])
            .iter()
            .map(|column| column.kind)
            .collect();
        assert_eq!(kinds, [EGGDB_INTEGER, EGGDB_REAL, EGGDB_BLOB, EGGDB_TEXT]);
        let unknown = remote_columns(&names[..1], &[]);
        assert_eq!(unknown[0].kind, EGGDB_NULL);
        assert_eq!(unknown[0].name.to_bytes(), b"id");
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
