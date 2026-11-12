#![allow(non_snake_case, clippy::too_many_arguments)]

use crate::handles::{BoundColumn, BoundParameter, ConnectionHandle, Environment, StatementHandle};
use crate::memory::{SQL_NTS, copy_into, input_bytes, input_text, read, store, terminated_length, write_text};
use crate::native::{Failure, Session};
use crate::values::{Converted, SQL_C_BINARY, convert, fixed_size, hex_literal, parameter_text};
use std::ffi::c_void;

type SqlReturn = i16;

const SQL_SUCCESS: SqlReturn = 0;
const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
const SQL_NO_DATA: SqlReturn = 100;
const SQL_ERROR: SqlReturn = -1;
const SQL_INVALID_HANDLE: SqlReturn = -2;

const SQL_HANDLE_ENV: i16 = 1;
const SQL_HANDLE_DBC: i16 = 2;
const SQL_HANDLE_STMT: i16 = 3;

const SQL_NULL_DATA: isize = -1;
const SQL_ATTR_ODBC_VERSION: i32 = 200;
const SQL_ATTR_AUTOCOMMIT: i32 = 102;
const SQL_COMMIT: i16 = 0;
const SQL_CLOSE: u16 = 0;
const SQL_DROP: u16 = 1;
const SQL_UNBIND: u16 = 2;
const SQL_RESET_PARAMS: u16 = 3;
const SQL_NULLABLE_UNKNOWN: i16 = 2;
const SQL_DESC_COUNT: u16 = 1001;
const SQL_DESC_TYPE: u16 = 1002;
const SQL_DESC_NAME: u16 = 1011;
const SQL_COLUMN_NAME: u16 = 1;
const SQL_DESC_LABEL: u16 = 18;
const SQL_DESC_NULLABLE: u16 = 1008;

const SQL_DATA_SOURCE_NAME: u16 = 2;
const SQL_DRIVER_NAME: u16 = 6;
const SQL_DRIVER_VER: u16 = 7;
const SQL_SERVER_NAME: u16 = 13;
const SQL_DBMS_NAME: u16 = 17;
const SQL_DBMS_VER: u16 = 18;
const SQL_IDENTIFIER_QUOTE_CHAR: u16 = 29;
const SQL_TXN_CAPABLE: u16 = 46;
const SQL_DRIVER_ODBC_VER: u16 = 77;
const SQL_GETDATA_EXTENSIONS: u16 = 81;
const SQL_MAX_CONCURRENT_ACTIVITIES: u16 = 1;
const SQL_TC_ALL: u16 = 2;
const SQL_GD_ANY_COLUMN: u32 = 1;
const SQL_GD_ANY_ORDER: u32 = 2;

const DRIVER_FILE: &str = "eggdb_odbc";
const ODBC_VERSION: &str = "03.80";

fn failed(diagnostics: &mut crate::handles::Diagnostics, failure: Failure) -> SqlReturn {
    diagnostics.push(failure);
    SQL_ERROR
}

fn environment<'h>(handle: *mut c_void) -> Option<&'h mut Environment> {
    // SAFETY: ODBC hands back the environment handle this driver made with SQLAllocHandle, used
    // by one thread at a time.
    unsafe { handle.cast::<Environment>().as_mut() }
}

fn connection<'h>(handle: *mut c_void) -> Option<&'h mut ConnectionHandle> {
    // SAFETY: as for `environment`, for a connection handle.
    unsafe { handle.cast::<ConnectionHandle>().as_mut() }
}

fn statement<'h>(handle: *mut c_void) -> Option<&'h mut StatementHandle> {
    // SAFETY: as for `environment`, for a statement handle.
    unsafe { handle.cast::<StatementHandle>().as_mut() }
}

fn owning_connection<'h>(stmt: &StatementHandle) -> Option<&'h mut ConnectionHandle> {
    // SAFETY: a statement is freed before the connection that made it, as ODBC requires.
    unsafe { stmt.connection.as_mut() }
}

/// Allocates an environment, connection or statement handle.
#[unsafe(no_mangle)]
pub extern "system" fn SQLAllocHandle(handle_type: i16, input: *mut c_void, output: *mut *mut c_void) -> SqlReturn {
    let made: *mut c_void = match handle_type {
        SQL_HANDLE_ENV => Box::into_raw(Box::new(Environment::default())).cast(),
        SQL_HANDLE_DBC => {
            if environment(input).is_none() {
                return SQL_INVALID_HANDLE;
            }
            Box::into_raw(Box::new(ConnectionHandle::new())).cast()
        }
        SQL_HANDLE_STMT => {
            if connection(input).is_none() {
                return SQL_INVALID_HANDLE;
            }
            Box::into_raw(Box::new(StatementHandle::new(input.cast()))).cast()
        }
        _ => return SQL_ERROR,
    };
    store(output, made);
    SQL_SUCCESS
}

/// Frees a handle; a connection is disconnected first.
#[unsafe(no_mangle)]
pub extern "system" fn SQLFreeHandle(handle_type: i16, handle: *mut c_void) -> SqlReturn {
    if handle.is_null() {
        return SQL_INVALID_HANDLE;
    }
    match handle_type {
        SQL_HANDLE_ENV => {
            // SAFETY: the handle is an environment this driver boxed and ODBC gives it up here.
            drop(unsafe { Box::from_raw(handle.cast::<Environment>()) });
        }
        SQL_HANDLE_DBC => {
            // SAFETY: as above, for a connection.
            let mut connection = unsafe { Box::from_raw(handle.cast::<ConnectionHandle>()) };
            if let Some(session) = connection.session.take() {
                session.close();
            }
        }
        SQL_HANDLE_STMT => {
            // SAFETY: as above, for a statement.
            drop(unsafe { Box::from_raw(handle.cast::<StatementHandle>()) });
        }
        _ => return SQL_ERROR,
    }
    SQL_SUCCESS
}

/// Sets an environment attribute; the ODBC version is kept, the rest accepted.
#[unsafe(no_mangle)]
pub extern "system" fn SQLSetEnvAttr(env: *mut c_void, attribute: i32, value: *mut c_void, _length: i32) -> SqlReturn {
    let Some(env) = environment(env) else {
        return SQL_INVALID_HANDLE;
    };
    if attribute == SQL_ATTR_ODBC_VERSION {
        env.odbc_version = i32::try_from(value.addr()).unwrap_or(0);
    }
    SQL_SUCCESS
}

/// Reads an environment attribute.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetEnvAttr(env: *mut c_void, attribute: i32, value: *mut c_void, _length: i32, written: *mut i32) -> SqlReturn {
    let Some(env) = environment(env) else {
        return SQL_INVALID_HANDLE;
    };
    if attribute == SQL_ATTR_ODBC_VERSION {
        store(value.cast::<i32>(), env.odbc_version);
        store(written, 4);
    }
    SQL_SUCCESS
}

fn connect(dbc: &mut ConnectionHandle, settings: &str) -> SqlReturn {
    dbc.diagnostics.clear();
    let mut database = String::new();
    let mut dialect = "sqlite".to_owned();
    let mut read_only = false;
    for pair in settings.split(';') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|character| character == '{' || character == '}');
        match key.trim().to_ascii_lowercase().as_str() {
            "database" | "dbq" | "data source" | "datasource" => database = value.to_owned(),
            "dialect" => dialect = value.to_ascii_lowercase(),
            "readonly" | "read only" => read_only = matches!(value, "1" | "true" | "yes"),
            _ => {}
        }
    }
    if database.is_empty() {
        return failed(&mut dbc.diagnostics, Failure::new("08001", "the connection string names no Database"));
    }
    match Session::open(&database, &dialect, read_only) {
        Ok(session) => {
            dbc.session = Some(session);
            dbc.database = database;
            SQL_SUCCESS
        }
        Err(failure) => failed(&mut dbc.diagnostics, Failure { sqlstate: "08001".to_owned(), ..failure }),
    }
}

/// Connects to the database file named by the data source name, which this driver reads as a
/// path when the driver manager passes it through.
#[unsafe(no_mangle)]
pub extern "system" fn SQLConnect(
    dbc: *mut c_void,
    server: *const u8,
    server_length: i16,
    _user: *const u8,
    _user_length: i16,
    _password: *const u8,
    _password_length: i16,
) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    let path = input_text(server, isize::from(server_length)).unwrap_or_default();
    connect(dbc, &format!("Database={path}"))
}

/// Connects with a connection string: `Database=<path>;Dialect=<dialect>;ReadOnly=1`.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDriverConnect(
    dbc: *mut c_void,
    _window: *mut c_void,
    input: *const u8,
    input_length: i16,
    output: *mut u8,
    output_capacity: i16,
    output_length: *mut i16,
    _completion: u16,
) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    let settings = input_text(input, isize::from(input_length)).unwrap_or_default();
    let outcome = connect(dbc, &settings);
    if outcome == SQL_SUCCESS {
        let truncated = write_text(output, isize::from(output_capacity), settings.as_bytes(), 1);
        store(output_length, i16::try_from(settings.len()).unwrap_or(i16::MAX));
        if truncated {
            return SQL_SUCCESS_WITH_INFO;
        }
    }
    outcome
}

/// Disconnects, rolling back an open transaction.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDisconnect(dbc: *mut c_void) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    let _ = dbc.end_transaction(false);
    if let Some(session) = dbc.session.take() {
        session.close();
    }
    SQL_SUCCESS
}

fn bound_parameters(stmt: &StatementHandle, text: &str) -> Result<(String, Vec<Option<String>>), Failure> {
    let mut values = Vec::new();
    for (index, parameter) in stmt.parameters.iter().enumerate() {
        let Some(parameter) = parameter else {
            return Err(Failure::new("07002", format!("parameter {} is not bound", index + 1)));
        };
        let indicator = read(parameter.indicator).unwrap_or(SQL_NTS);
        if indicator == SQL_NULL_DATA {
            values.push(None);
            continue;
        }
        let length = match fixed_size(parameter.value_type) {
            Some(size) => size,
            None if indicator == SQL_NTS => terminated_length(parameter.value),
            None if indicator >= 0 => usize::try_from(indicator).unwrap_or(0),
            None => usize::try_from(parameter.buffer_length).unwrap_or(0),
        };
        let bytes = input_bytes(parameter.value, length);
        if parameter.value_type == SQL_C_BINARY {
            values.push(Some(hex_literal(&bytes)));
            continue;
        }
        let text = parameter_text(parameter.value_type, &bytes).map_err(|message| Failure::new("22018", message))?;
        values.push(Some(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut kept = Vec::with_capacity(values.len());
    let mut quote = None;
    let mut next = 0;
    for character in text.chars() {
        match quote {
            Some(open) => {
                if character == open {
                    quote = None;
                }
                out.push(character);
            }
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                out.push(character);
            }
            None if character == '?' => {
                let value = values.get(next).cloned().flatten();
                let binary = stmt.parameters.get(next).and_then(|parameter| *parameter).is_some_and(|parameter| parameter.value_type == SQL_C_BINARY);
                next += 1;
                match value {
                    Some(literal) if binary => out.push_str(&literal),
                    other => {
                        out.push('?');
                        kept.push(other);
                    }
                }
            }
            None => out.push(character),
        }
    }
    Ok((out, kept))
}

fn execute(stmt: &mut StatementHandle, text: &str) -> SqlReturn {
    stmt.diagnostics.clear();
    let (text, values) = match bound_parameters(stmt, text) {
        Ok(bound) => bound,
        Err(failure) => return failed(&mut stmt.diagnostics, failure),
    };
    let Some(dbc) = owning_connection(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    match dbc.run(&text, &values) {
        Ok(outcomes) => {
            stmt.take_outcomes(outcomes);
            SQL_SUCCESS
        }
        Err(failure) => failed(&mut stmt.diagnostics, failure),
    }
}

/// Runs a statement at once.
#[unsafe(no_mangle)]
pub extern "system" fn SQLExecDirect(stmt: *mut c_void, text: *const u8, length: i32) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let Some(text) = input_text(text, length as isize) else {
        return failed(&mut stmt.diagnostics, Failure::new("HY009", "the statement text is NULL"));
    };
    execute(stmt, &text)
}

/// Keeps a statement to run with [`SQLExecute`].
#[unsafe(no_mangle)]
pub extern "system" fn SQLPrepare(stmt: *mut c_void, text: *const u8, length: i32) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.diagnostics.clear();
    stmt.prepared = input_text(text, length as isize);
    SQL_SUCCESS
}

/// Runs the prepared statement with the bound parameters.
#[unsafe(no_mangle)]
pub extern "system" fn SQLExecute(stmt: *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let Some(text) = stmt.prepared.clone() else {
        return failed(&mut stmt.diagnostics, Failure::new("HY010", "no statement is prepared"));
    };
    execute(stmt, &text)
}

/// Binds an input parameter's buffer, read when the statement runs.
#[unsafe(no_mangle)]
pub extern "system" fn SQLBindParameter(
    stmt: *mut c_void,
    number: u16,
    _io_type: i16,
    value_type: i16,
    _parameter_type: i16,
    _column_size: usize,
    _digits: i16,
    value: *mut c_void,
    buffer_length: isize,
    indicator: *mut isize,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let index = usize::from(number);
    if index == 0 {
        return failed(&mut stmt.diagnostics, Failure::new("07009", "parameters are numbered from 1"));
    }
    if stmt.parameters.len() < index {
        stmt.parameters.resize(index, None);
    }
    stmt.parameters[index - 1] = Some(BoundParameter {
        value_type,
        value: value.cast_const().cast(),
        buffer_length,
        indicator: indicator.cast_const(),
    });
    SQL_SUCCESS
}

/// The number of `?` placeholders in the prepared statement.
#[unsafe(no_mangle)]
pub extern "system" fn SQLNumParams(stmt: *mut c_void, count: *mut i16) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let placeholders = stmt.prepared.as_deref().map_or(0, |text| text.matches('?').count());
    store(count, i16::try_from(placeholders).unwrap_or(i16::MAX));
    SQL_SUCCESS
}

/// The number of columns of the current result.
#[unsafe(no_mangle)]
pub extern "system" fn SQLNumResultCols(stmt: *mut c_void, count: *mut i16) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    store(count, i16::try_from(stmt.columns.len()).unwrap_or(i16::MAX));
    SQL_SUCCESS
}

/// Describes a result column: its name, type (from its values), size and nullability.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDescribeCol(
    stmt: *mut c_void,
    number: u16,
    name: *mut u8,
    name_capacity: i16,
    name_length: *mut i16,
    data_type: *mut i16,
    column_size: *mut usize,
    digits: *mut i16,
    nullable: *mut i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let index = usize::from(number);
    let Some(column) = index.checked_sub(1).and_then(|at| stmt.columns.get(at)).cloned() else {
        return failed(&mut stmt.diagnostics, Failure::new("07009", format!("there is no column {number}")));
    };
    let truncated = write_text(name, isize::from(name_capacity), column.as_bytes(), 1);
    store(name_length, i16::try_from(column.len()).unwrap_or(i16::MAX));
    store(data_type, stmt.sql_type_of(index - 1));
    store(column_size, 0);
    store(digits, 0);
    store(nullable, SQL_NULLABLE_UNKNOWN);
    if truncated { SQL_SUCCESS_WITH_INFO } else { SQL_SUCCESS }
}

/// A column attribute: the count, name, label, type or nullability.
#[unsafe(no_mangle)]
pub extern "system" fn SQLColAttribute(
    stmt: *mut c_void,
    number: u16,
    field: u16,
    text: *mut c_void,
    text_capacity: i16,
    text_length: *mut i16,
    numeric: *mut isize,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let index = usize::from(number);
    match field {
        SQL_DESC_COUNT => store(numeric, isize::try_from(stmt.columns.len()).unwrap_or(isize::MAX)),
        SQL_DESC_NAME | SQL_COLUMN_NAME | SQL_DESC_LABEL => {
            let Some(column) = index.checked_sub(1).and_then(|at| stmt.columns.get(at)).cloned() else {
                return failed(&mut stmt.diagnostics, Failure::new("07009", format!("there is no column {number}")));
            };
            write_text(text.cast(), isize::from(text_capacity), column.as_bytes(), 1);
            store(text_length, i16::try_from(column.len()).unwrap_or(i16::MAX));
        }
        SQL_DESC_TYPE => store(numeric, isize::from(stmt.sql_type_of(index.saturating_sub(1)))),
        SQL_DESC_NULLABLE => store(numeric, isize::from(SQL_NULLABLE_UNKNOWN)),
        _ => return failed(&mut stmt.diagnostics, Failure::new("HYC00", format!("column attribute {field} is not supported"))),
    }
    SQL_SUCCESS
}

fn deliver(converted: Converted, offset: usize, target: *mut u8, buffer_length: isize, indicator: *mut isize) -> (SqlReturn, usize) {
    match converted {
        Converted::Null => {
            store(indicator, SQL_NULL_DATA);
            (SQL_SUCCESS, 0)
        }
        Converted::Fixed(bytes) => {
            copy_into(target, &bytes);
            store(indicator, isize::try_from(bytes.len()).unwrap_or(isize::MAX));
            (SQL_SUCCESS, 0)
        }
        Converted::Text(bytes) | Converted::Wide(bytes) | Converted::Binary(bytes) if offset > 0 && offset >= bytes.len() => (SQL_NO_DATA, offset),
        Converted::Text(bytes) => deliver_piece(&bytes, offset, target, buffer_length, indicator, 1),
        Converted::Wide(bytes) => deliver_piece(&bytes, offset, target, buffer_length, indicator, 2),
        Converted::Binary(bytes) => deliver_piece(&bytes, offset, target, buffer_length, indicator, 0),
    }
}

fn deliver_piece(bytes: &[u8], offset: usize, target: *mut u8, buffer_length: isize, indicator: *mut isize, terminator: usize) -> (SqlReturn, usize) {
    let rest = &bytes[offset.min(bytes.len())..];
    store(indicator, isize::try_from(rest.len()).unwrap_or(isize::MAX));
    let capacity = usize::try_from(buffer_length).unwrap_or(0);
    let room = capacity.saturating_sub(terminator);
    let room = room - room % terminator.max(1);
    let taken = rest.len().min(room);
    if terminator > 0 {
        write_text(target, buffer_length, &rest[..taken], terminator);
    } else {
        copy_into(target, &rest[..taken]);
    }
    let next = offset + taken;
    if taken < rest.len() {
        (SQL_SUCCESS_WITH_INFO, next)
    } else {
        (SQL_SUCCESS, next.max(1))
    }
}

/// Moves to the next row, filling the columns bound with [`SQLBindCol`].
#[unsafe(no_mangle)]
pub extern "system" fn SQLFetch(stmt: *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.diagnostics.clear();
    let Some(row) = stmt.rows.get(stmt.next_row).cloned() else {
        stmt.current = None;
        return SQL_NO_DATA;
    };
    stmt.next_row += 1;
    stmt.read_offsets = vec![0; stmt.columns.len()];
    let mut outcome = SQL_SUCCESS;
    for (index, bound) in stmt.bound_columns.iter().enumerate() {
        let (Some(bound), Some(cell)) = (bound, row.get(index)) else {
            continue;
        };
        match convert(cell, bound.target_type) {
            Ok(converted) => {
                let (delivered, _) = deliver(converted, 0, bound.target, bound.buffer_length, bound.indicator);
                if delivered == SQL_SUCCESS_WITH_INFO {
                    outcome = SQL_SUCCESS_WITH_INFO;
                }
            }
            Err(message) => {
                stmt.diagnostics.push(Failure::new("22018", message));
                outcome = SQL_ERROR;
            }
        }
    }
    stmt.current = Some(row);
    outcome
}

/// Reads a column of the current row, in pieces for long text and bytes.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetData(
    stmt: *mut c_void,
    number: u16,
    target_type: i16,
    target: *mut c_void,
    buffer_length: isize,
    indicator: *mut isize,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let index = usize::from(number);
    let Some(cell) = stmt.current.as_ref().and_then(|row| index.checked_sub(1).and_then(|at| row.get(at))).cloned() else {
        return failed(&mut stmt.diagnostics, Failure::new("07009", format!("there is no column {number} in a fetched row")));
    };
    let converted = match convert(&cell, target_type) {
        Ok(converted) => converted,
        Err(message) => return failed(&mut stmt.diagnostics, Failure::new("22018", message)),
    };
    let offset = stmt.read_offsets.get(index - 1).copied().unwrap_or(0);
    let (outcome, next) = deliver(converted, offset, target.cast(), buffer_length, indicator);
    if let Some(slot) = stmt.read_offsets.get_mut(index - 1) {
        *slot = next;
    }
    if outcome == SQL_SUCCESS_WITH_INFO {
        stmt.diagnostics.push(Failure::new("01004", "the value was truncated; read the rest with another SQLGetData"));
    }
    outcome
}

/// Binds a column's buffer, filled by each [`SQLFetch`].
#[unsafe(no_mangle)]
pub extern "system" fn SQLBindCol(stmt: *mut c_void, number: u16, target_type: i16, target: *mut c_void, buffer_length: isize, indicator: *mut isize) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let index = usize::from(number);
    if index == 0 {
        return failed(&mut stmt.diagnostics, Failure::new("07009", "bookmark columns are not supported"));
    }
    if stmt.bound_columns.len() < index {
        stmt.bound_columns.resize(index, None);
    }
    stmt.bound_columns[index - 1] = (!target.is_null()).then_some(BoundColumn {
        target_type,
        target: target.cast(),
        buffer_length,
        indicator,
    });
    SQL_SUCCESS
}

/// The rows the last `INSERT`, `UPDATE` or `DELETE` changed, or the rows of the last result.
#[unsafe(no_mangle)]
pub extern "system" fn SQLRowCount(stmt: *mut c_void, count: *mut isize) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    store(count, stmt.row_count);
    SQL_SUCCESS
}

/// Closes the statement's cursor.
#[unsafe(no_mangle)]
pub extern "system" fn SQLCloseCursor(stmt: *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.close_cursor();
    SQL_SUCCESS
}

/// Closes the cursor, unbinds columns or parameters, or frees the statement.
#[unsafe(no_mangle)]
pub extern "system" fn SQLFreeStmt(stmt: *mut c_void, option: u16) -> SqlReturn {
    if option == SQL_DROP {
        return SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    }
    let Some(handle) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    match option {
        SQL_CLOSE => handle.close_cursor(),
        SQL_UNBIND => handle.bound_columns.clear(),
        SQL_RESET_PARAMS => handle.parameters.clear(),
        _ => {}
    }
    SQL_SUCCESS
}

/// Commits or rolls back the connection's transaction.
#[unsafe(no_mangle)]
pub extern "system" fn SQLEndTran(handle_type: i16, handle: *mut c_void, completion: i16) -> SqlReturn {
    if handle_type != SQL_HANDLE_DBC {
        return SQL_SUCCESS;
    }
    let Some(dbc) = connection(handle) else {
        return SQL_INVALID_HANDLE;
    };
    match dbc.end_transaction(completion == SQL_COMMIT) {
        Ok(()) => SQL_SUCCESS,
        Err(failure) => failed(&mut dbc.diagnostics, failure),
    }
}

/// Sets a connection attribute; autocommit is kept, the rest accepted.
#[unsafe(no_mangle)]
pub extern "system" fn SQLSetConnectAttr(dbc: *mut c_void, attribute: i32, value: *mut c_void, _length: i32) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    if attribute == SQL_ATTR_AUTOCOMMIT {
        let enabled = value.addr() != 0;
        if enabled && !dbc.autocommit && dbc.end_transaction(true).is_err() {
            return SQL_ERROR;
        }
        dbc.autocommit = enabled;
    }
    SQL_SUCCESS
}

/// Reads a connection attribute: autocommit.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetConnectAttr(dbc: *mut c_void, attribute: i32, value: *mut c_void, _capacity: i32, written: *mut i32) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    if attribute == SQL_ATTR_AUTOCOMMIT {
        store(value.cast::<u32>(), u32::from(dbc.autocommit));
        store(written, 4);
        return SQL_SUCCESS;
    }
    failed(&mut dbc.diagnostics, Failure::new("HYC00", format!("connection attribute {attribute} is not supported")))
}

/// Sets a statement attribute; accepted and ignored.
#[unsafe(no_mangle)]
pub extern "system" fn SQLSetStmtAttr(_stmt: *mut c_void, _attribute: i32, _value: *mut c_void, _length: i32) -> SqlReturn {
    SQL_SUCCESS
}

/// Answers the driver and database information applications ask for first.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetInfo(dbc: *mut c_void, info: u16, value: *mut c_void, capacity: i16, written: *mut i16) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    let text = match info {
        SQL_DRIVER_NAME => Some(DRIVER_FILE.to_owned()),
        SQL_DRIVER_VER => Some(format!("{:02}.{:02}.0000", 0, 5)),
        SQL_DBMS_NAME => Some("EggDB".to_owned()),
        SQL_DBMS_VER => Some(env!("CARGO_PKG_VERSION").to_owned()),
        SQL_DRIVER_ODBC_VER => Some(ODBC_VERSION.to_owned()),
        SQL_DATA_SOURCE_NAME | SQL_SERVER_NAME => Some(dbc.database.clone()),
        SQL_IDENTIFIER_QUOTE_CHAR => Some("\"".to_owned()),
        _ => None,
    };
    if let Some(text) = text {
        let truncated = write_text(value.cast(), isize::from(capacity), text.as_bytes(), 1);
        store(written, i16::try_from(text.len()).unwrap_or(i16::MAX));
        return if truncated { SQL_SUCCESS_WITH_INFO } else { SQL_SUCCESS };
    }
    match info {
        SQL_TXN_CAPABLE => {
            store(value.cast::<u16>(), SQL_TC_ALL);
            store(written, 2);
        }
        SQL_MAX_CONCURRENT_ACTIVITIES => {
            store(value.cast::<u16>(), 0);
            store(written, 2);
        }
        SQL_GETDATA_EXTENSIONS => {
            store(value.cast::<u32>(), SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER);
            store(written, 4);
        }
        _ => return failed(&mut dbc.diagnostics, Failure::new("HY096", format!("information type {info} is not supported"))),
    }
    SQL_SUCCESS
}

/// Reads a diagnostic record of a handle: its SQLSTATE, native code and message.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetDiagRec(
    handle_type: i16,
    handle: *mut c_void,
    record: i16,
    sqlstate: *mut u8,
    native: *mut i32,
    message: *mut u8,
    capacity: i16,
    written: *mut i16,
) -> SqlReturn {
    let records = match handle_type {
        SQL_HANDLE_ENV => environment(handle).map(|env| &env.diagnostics.records),
        SQL_HANDLE_DBC => connection(handle).map(|dbc| &dbc.diagnostics.records),
        SQL_HANDLE_STMT => statement(handle).map(|stmt| &stmt.diagnostics.records),
        _ => None,
    };
    let Some(records) = records else {
        return SQL_INVALID_HANDLE;
    };
    let Some(found) = usize::try_from(record).ok().and_then(|number| number.checked_sub(1)).and_then(|at| records.get(at)) else {
        return SQL_NO_DATA;
    };
    write_text(sqlstate, 6, found.sqlstate.as_bytes(), 1);
    store(native, found.native);
    let truncated = write_text(message, isize::from(capacity), found.message.as_bytes(), 1);
    store(written, i16::try_from(found.message.len()).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { SQL_SUCCESS }
}

const HIDDEN_SCHEMAS: &str = "('pg_catalog', 'information_schema', 'eggdb_internal')";

const ODBC_TYPE_OF_DATA_TYPE: &str = "CASE c.data_type WHEN 'integer' THEN 4 WHEN 'bigint' THEN -5 WHEN 'smallint' THEN 5 \
    WHEN 'boolean' THEN -7 WHEN 'double precision' THEN 8 WHEN 'real' THEN 7 WHEN 'numeric' THEN 2 \
    WHEN 'date' THEN 91 WHEN 'time without time zone' THEN 92 WHEN 'timestamp without time zone' THEN 93 \
    WHEN 'timestamp with time zone' THEN 93 WHEN 'bytea' THEN -3 WHEN 'text' THEN -1 ELSE 12 END";

fn pattern(text: *const u8, length: i16) -> Option<String> {
    input_text(text, isize::from(length)).filter(|pattern| !pattern.is_empty() && pattern != "%")
}

fn catalog_query(stmt: &mut StatementHandle, text: &str, parameters: &[Option<String>]) -> SqlReturn {
    stmt.diagnostics.clear();
    let Some(dbc) = owning_connection(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    match dbc.run_catalog(text, parameters) {
        Ok(outcomes) => {
            stmt.take_outcomes(outcomes);
            SQL_SUCCESS
        }
        Err(failure) => failed(&mut stmt.diagnostics, failure),
    }
}

/// Lists the tables and views whose schema and name match the patterns, and whose type is in
/// the comma-separated list of types when one is given.
#[unsafe(no_mangle)]
pub extern "system" fn SQLTables(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
    types: *const u8,
    types_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT table_catalog AS \"TABLE_CAT\", table_schema AS \"TABLE_SCHEM\", table_name AS \"TABLE_NAME\", \
         CASE table_type WHEN 'BASE TABLE' THEN 'TABLE' ELSE table_type END AS \"TABLE_TYPE\", NULL::text AS \"REMARKS\" \
         FROM information_schema.tables WHERE table_schema NOT IN {HIDDEN_SCHEMAS} \
         AND ($1::text IS NULL OR table_schema LIKE $1) AND ($2::text IS NULL OR table_name LIKE $2) \
         ORDER BY 4, 2, 3"
    );
    let outcome = catalog_query(stmt, &text, &[pattern(schema, schema_length), pattern(table, table_length)]);
    if outcome != SQL_SUCCESS {
        return outcome;
    }
    if let Some(types) = pattern(types, types_length) {
        let wanted: Vec<String> = types
            .split(',')
            .map(|kind| kind.trim().trim_matches('\'').to_ascii_uppercase())
            .collect();
        stmt.rows.retain(|row| match row.get(3) {
            Some(crate::native::Cell::Text(kind)) => wanted.contains(kind),
            _ => false,
        });
        stmt.row_count = isize::try_from(stmt.rows.len()).unwrap_or(isize::MAX);
    }
    SQL_SUCCESS
}

/// Lists the columns of the tables matching the patterns, with ODBC's eighteen columns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLColumns(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
    column: *const u8,
    column_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT c.table_catalog AS \"TABLE_CAT\", c.table_schema AS \"TABLE_SCHEM\", c.table_name AS \"TABLE_NAME\", \
         c.column_name AS \"COLUMN_NAME\", {ODBC_TYPE_OF_DATA_TYPE} AS \"DATA_TYPE\", c.data_type AS \"TYPE_NAME\", \
         c.character_maximum_length AS \"COLUMN_SIZE\", c.character_octet_length AS \"BUFFER_LENGTH\", \
         c.numeric_scale AS \"DECIMAL_DIGITS\", c.numeric_precision_radix AS \"NUM_PREC_RADIX\", \
         CASE c.is_nullable WHEN 'YES' THEN 1 ELSE 0 END AS \"NULLABLE\", NULL::text AS \"REMARKS\", \
         c.column_default AS \"COLUMN_DEF\", {ODBC_TYPE_OF_DATA_TYPE} AS \"SQL_DATA_TYPE\", \
         NULL::integer AS \"SQL_DATETIME_SUB\", c.character_octet_length AS \"CHAR_OCTET_LENGTH\", \
         c.ordinal_position AS \"ORDINAL_POSITION\", c.is_nullable AS \"IS_NULLABLE\" \
         FROM information_schema.columns c WHERE c.table_schema NOT IN {HIDDEN_SCHEMAS} \
         AND ($1::text IS NULL OR c.table_schema LIKE $1) AND ($2::text IS NULL OR c.table_name LIKE $2) \
         AND ($3::text IS NULL OR c.column_name LIKE $3) ORDER BY 2, 3, 17"
    );
    catalog_query(
        stmt,
        &text,
        &[pattern(schema, schema_length), pattern(table, table_length), pattern(column, column_length)],
    )
}
