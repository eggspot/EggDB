#![allow(non_snake_case, clippy::too_many_arguments)]

use crate::handles::{BoundColumn, BoundParameter, ConnectionHandle, Environment, StatementHandle};
use crate::memory::{SQL_NTS, copy_into, input_bytes, input_text, read, store, terminated_length, write_text};
use crate::native::{Failure, Session};
use crate::values::{Converted, SQL_C_BINARY, convert, fixed_size, hex_literal, parameter_text};
use std::ffi::c_void;

type SqlReturn = i16;

const SQL_INDEX_UNIQUE: u16 = 0;
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

#[derive(Clone, Copy)]
enum InfoAnswer {
    Text(&'static str),
    Short(u16),
    Long(u32),
}

const IDENTIFIER_BYTES: u16 = 63;
const CVT_TEXT: u32 = 0x1 | 0x100 | 0x200;
const CVT_NUMBERS: u32 = 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x1000 | 0x2000 | 0x4000;
const CVT_DATES: u32 = 0x8000 | 0x1_0000 | 0x2_0000;
const CVT_BINARY: u32 = 0x400 | 0x800 | 0x4_0000;
const CONVERTS_FROM_NUMBERS: u32 = CVT_TEXT | CVT_NUMBERS;
const CONVERTS_FROM_TEXT: u32 = CVT_TEXT | CVT_NUMBERS | CVT_DATES | CVT_BINARY;
const CONVERTS_FROM_DATES: u32 = CVT_TEXT | CVT_DATES;
const CONVERTS_FROM_BINARY: u32 = CVT_TEXT | CVT_BINARY;

const FIXED_INFO: [(u16, InfoAnswer); 71] = [
    (0, InfoAnswer::Short(0)),
    (8, InfoAnswer::Long(1)),
    (9, InfoAnswer::Short(1)),
    (11, InfoAnswer::Text("N")),
    (14, InfoAnswer::Text("\\")),
    (19, InfoAnswer::Text("Y")),
    (20, InfoAnswer::Text("Y")),
    (21, InfoAnswer::Text("Y")),
    (22, InfoAnswer::Short(0)),
    (23, InfoAnswer::Short(2)),
    (24, InfoAnswer::Short(2)),
    (26, InfoAnswer::Long(8)),
    (27, InfoAnswer::Text("Y")),
    (28, InfoAnswer::Short(2)),
    (30, InfoAnswer::Short(IDENTIFIER_BYTES)),
    (31, InfoAnswer::Short(IDENTIFIER_BYTES)),
    (32, InfoAnswer::Short(IDENTIFIER_BYTES)),
    (34, InfoAnswer::Short(0)),
    (35, InfoAnswer::Short(IDENTIFIER_BYTES)),
    (36, InfoAnswer::Text("Y")),
    (37, InfoAnswer::Text("Y")),
    (38, InfoAnswer::Text("Y")),
    (39, InfoAnswer::Text("schema")),
    (40, InfoAnswer::Text("procedure")),
    (41, InfoAnswer::Text("")),
    (42, InfoAnswer::Text("")),
    (44, InfoAnswer::Long(1)),
    (45, InfoAnswer::Text("table")),
    (48, InfoAnswer::Long(crate::escapes::CONVERT_FUNCTIONS)),
    (49, InfoAnswer::Long(crate::escapes::NUMERIC_FUNCTIONS)),
    (50, InfoAnswer::Long(crate::escapes::STRING_FUNCTIONS)),
    (51, InfoAnswer::Long(crate::escapes::SYSTEM_FUNCTIONS)),
    (52, InfoAnswer::Long(crate::escapes::TIMEDATE_FUNCTIONS)),
    (109, InfoAnswer::Long(crate::escapes::TIMESTAMP_INTERVALS)),
    (110, InfoAnswer::Long(crate::escapes::TIMESTAMP_INTERVALS)),
    (53, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (54, InfoAnswer::Long(CONVERTS_FROM_BINARY)),
    (55, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (56, InfoAnswer::Long(CONVERTS_FROM_TEXT)),
    (57, InfoAnswer::Long(CONVERTS_FROM_DATES)),
    (58, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (59, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (60, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (61, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (62, InfoAnswer::Long(CONVERTS_FROM_TEXT)),
    (63, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (64, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (65, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (66, InfoAnswer::Long(CONVERTS_FROM_DATES)),
    (67, InfoAnswer::Long(CONVERTS_FROM_DATES)),
    (68, InfoAnswer::Long(CONVERTS_FROM_NUMBERS)),
    (69, InfoAnswer::Long(CONVERTS_FROM_BINARY)),
    (70, InfoAnswer::Long(CONVERTS_FROM_TEXT)),
    (71, InfoAnswer::Long(CONVERTS_FROM_BINARY)),
    (72, InfoAnswer::Long(2 | 4 | 8)),
    (74, InfoAnswer::Short(2)),
    (75, InfoAnswer::Short(1)),
    (85, InfoAnswer::Short(0)),
    (90, InfoAnswer::Text("N")),
    (94, InfoAnswer::Text("")),
    (101, InfoAnswer::Short(1600)),
    (104, InfoAnswer::Long(0)),
    (105, InfoAnswer::Long(0)),
    (111, InfoAnswer::Text("N")),
    (113, InfoAnswer::Text("Y")),
    (118, InfoAnswer::Long(1)),
    (152, InfoAnswer::Long(1)),
    (10001, InfoAnswer::Long(1)),
    (10002, InfoAnswer::Text("N")),
    (10003, InfoAnswer::Text("N")),
    (10005, InfoAnswer::Short(IDENTIFIER_BYTES)),
];

const SQL_DATABASE_NAME: u16 = 16;
const SQL_DATA_SOURCE_READ_ONLY: u16 = 25;
const SQL_USER_NAME: u16 = 47;

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

/// The `key=value` pairs of a connection string, keys in lowercase; a value in braces may hold
/// `;` and `=`, and `}}` stands for one `}` inside it (D1261).
fn attributes(settings: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut characters = settings.chars().peekable();
    loop {
        let mut key = String::new();
        let mut assigned = false;
        for character in characters.by_ref() {
            if character == '=' || character == ';' {
                assigned = character == '=';
                break;
            }
            key.push(character);
        }
        if !assigned {
            if characters.peek().is_none() {
                break;
            }
            continue;
        }
        let mut value = String::new();
        while characters.peek().is_some_and(|character| *character == ' ') {
            characters.next();
        }
        if characters.peek() == Some(&'{') {
            characters.next();
            while let Some(character) = characters.next() {
                if character == '}' {
                    if characters.peek() == Some(&'}') {
                        characters.next();
                        value.push('}');
                        continue;
                    }
                    break;
                }
                value.push(character);
            }
            for character in characters.by_ref() {
                if character == ';' {
                    break;
                }
            }
        } else {
            for character in characters.by_ref() {
                if character == ';' {
                    break;
                }
                value.push(character);
            }
            value = value.trim().to_owned();
        }
        if !key.trim().is_empty() {
            found.push((key.trim().to_ascii_lowercase(), value));
        }
    }
    found
}

fn connect(dbc: &mut ConnectionHandle, settings: &str) -> SqlReturn {
    dbc.diagnostics.clear();
    let mut database = String::new();
    let mut dialect = "sqlite".to_owned();
    let mut read_only = false;
    let mut server = ServerAddress::default();
    for (key, value) in attributes(settings) {
        let value = value.as_str();
        match key.as_str() {
            "database" | "dbq" | "data source" | "datasource" => database = value.to_owned(),
            "dialect" => dialect = value.to_ascii_lowercase(),
            "readonly" | "read only" => read_only = matches!(value, "1" | "true" | "yes"),
            "server" | "host" | "servername" => server.host = value.to_owned(),
            "port" => server.port = value.to_owned(),
            "uid" | "user" | "user id" | "username" => server.user = value.to_owned(),
            "pwd" | "password" => server.password = value.to_owned(),
            "sslmode" => server.ssl_mode = value.to_ascii_lowercase(),
            "url" => server.url = value.to_owned(),
            _ => {}
        }
    }
    if !server.url.is_empty() || !server.host.is_empty() {
        return connect_to_server(dbc, &server, &database, &dialect, read_only);
    }
    if database.is_empty() {
        return failed(&mut dbc.diagnostics, Failure::new("08001", "the connection string names no Database"));
    }
    match Session::open(&database, &dialect, read_only) {
        Ok(session) => {
            dbc.session = Some(session);
            dbc.database = database;
            dbc.read_only = read_only;
            dbc.dialect = dialect;
            SQL_SUCCESS
        }
        Err(failure) => failed(&mut dbc.diagnostics, Failure { sqlstate: "08001".to_owned(), ..failure }),
    }
}

#[derive(Debug, Default)]
struct ServerAddress {
    url: String,
    host: String,
    port: String,
    user: String,
    password: String,
    ssl_mode: String,
}

fn url_part(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(char::from(byte)),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

impl ServerAddress {
    fn url(&self, database: &str, dialect: &str) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        let mysql = matches!(dialect, "mysql" | "mariadb");
        let scheme = if mysql { "mysql" } else { "postgresql" };
        let port = match (self.port.is_empty(), mysql) {
            (false, _) => self.port.clone(),
            (true, true) => "3306".to_owned(),
            (true, false) => "5432".to_owned(),
        };
        let credentials = match (self.user.is_empty(), self.password.is_empty() || mysql) {
            (true, _) => String::new(),
            (false, true) => format!("{}@", url_part(&self.user)),
            (false, false) => format!("{}:{}@", url_part(&self.user), url_part(&self.password)),
        };
        let mut url = format!("{scheme}://{credentials}{}:{port}/{}", self.host, url_part(database));
        if !self.ssl_mode.is_empty() && !mysql {
            url.push_str("?sslmode=");
            url.push_str(&url_part(&self.ssl_mode));
        }
        url
    }
}

fn server_dialect(url: &str) -> &'static str {
    if url.starts_with("mysql://") || url.starts_with("mariadb://") { "mysql" } else { "postgresql" }
}

fn connect_to_server(dbc: &mut ConnectionHandle, server: &ServerAddress, database: &str, dialect: &str, read_only: bool) -> SqlReturn {
    let url = server.url(database, dialect);
    let mut session = match Session::connect(&url) {
        Ok(session) => session,
        Err(failure) => return failed(&mut dbc.diagnostics, Failure { sqlstate: "08001".to_owned(), ..failure }),
    };
    let dialect = server_dialect(&url);
    if read_only {
        let statement = match dialect {
            "mysql" => "SET SESSION TRANSACTION READ ONLY",
            _ => "SET default_transaction_read_only = on",
        };
        if let Err(failure) = session.run(statement, &[]) {
            session.close();
            return failed(&mut dbc.diagnostics, Failure { sqlstate: "08001".to_owned(), ..failure });
        }
    }
    dbc.session = Some(session);
    dbc.database = if database.is_empty() { server.host.clone() } else { database.to_owned() };
    dbc.read_only = read_only;
    dbc.dialect = dialect.to_owned();
    SQL_SUCCESS
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

fn parameter_element(parameter: &BoundParameter, bind_type: usize, set: usize) -> BoundParameter {
    if set == 0 {
        return *parameter;
    }
    let (value_step, indicator_step) = match bind_type {
        0 => (
            fixed_size(parameter.value_type).unwrap_or_else(|| usize::try_from(parameter.buffer_length).unwrap_or(0)),
            std::mem::size_of::<isize>(),
        ),
        row_size => (row_size, row_size),
    };
    let moved = |base: *const u8, step: usize| if base.is_null() { base } else { base.wrapping_add(step * set) };
    BoundParameter {
        value: moved(parameter.value, value_step),
        indicator: moved(parameter.indicator.cast(), indicator_step).cast(),
        ..*parameter
    }
}

fn bound_parameters(stmt: &StatementHandle, text: &str, set: usize) -> Result<(String, Vec<Option<String>>), Failure> {
    let mut values = Vec::new();
    for (index, parameter) in stmt.parameters.iter().enumerate() {
        let Some(parameter) = parameter else {
            return Err(Failure::new("07002", format!("parameter {} is not bound", index + 1)));
        };
        let parameter = &parameter_element(parameter, stmt.parameter_array.bind_type, set);
        let sent = stmt.awaited.as_ref().and_then(|awaited| awaited.sent.get(index).cloned().flatten());
        let bytes = match sent {
            Some(None) => {
                values.push(None);
                continue;
            }
            Some(Some(bytes)) => bytes,
            None => {
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
                input_bytes(parameter.value, length)
            }
        };
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
    let family = owning_connection(stmt).map_or(crate::escapes::Family::Sqlite, |dbc| crate::escapes::Family::of(&dbc.dialect));
    let text = match crate::escapes::translated(text, family) {
        Ok(text) => text,
        Err(message) => return failed(&mut stmt.diagnostics, Failure::new("37000", message)),
    };
    if stmt.parameter_array.size > 1 && !stmt.parameters.is_empty() {
        return execute_sets(stmt, &text);
    }
    let at_execution: Vec<usize> = stmt
        .parameters
        .iter()
        .enumerate()
        .filter(|(_, parameter)| {
            parameter.is_some_and(|parameter| {
                read(parameter.indicator).is_some_and(|indicator| indicator == SQL_DATA_AT_EXEC || indicator <= SQL_LEN_DATA_AT_EXEC_OFFSET)
            })
        })
        .map(|(index, _)| index)
        .collect();
    if !at_execution.is_empty() && stmt.awaited.is_none() {
        stmt.awaited = Some(crate::handles::AwaitedData {
            text,
            parameters: at_execution,
            next: 0,
            current: None,
            sent: vec![None; stmt.parameters.len()],
        });
        return SQL_NEED_DATA;
    }
    let (text, values) = match bound_parameters(stmt, &text, 0) {
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

const SQL_NEED_DATA: SqlReturn = 99;
const SQL_DATA_AT_EXEC: isize = -2;
const SQL_LEN_DATA_AT_EXEC_OFFSET: isize = -100;

/// Gives the application the next parameter whose value it sends with [`SQLPutData`] (its
/// bound value pointer, as ODBC defines the token), or runs the statement once all are sent
/// (D1281).
#[unsafe(no_mangle)]
pub extern "system" fn SQLParamData(stmt: *mut c_void, token: *mut *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let Some(awaited) = stmt.awaited.as_mut() else {
        return failed(&mut stmt.diagnostics, Failure::new("HY010", "no statement is waiting for data"));
    };
    if let Some(&index) = awaited.parameters.get(awaited.next) {
        awaited.next += 1;
        awaited.current = Some(index);
        if let Some(slot) = awaited.sent.get_mut(index)
            && slot.is_none()
        {
            *slot = Some(Some(Vec::new()));
        }
        let pointer = stmt.parameters.get(index).copied().flatten().map_or(std::ptr::null_mut(), |parameter| parameter.value.cast_mut().cast());
        store(token, pointer);
        return SQL_NEED_DATA;
    }
    let text = awaited.text.clone();
    let outcome = execute(stmt, &text);
    stmt.awaited = None;
    outcome
}

/// Sends part of the value of the parameter [`SQLParamData`] named last: `length` bytes at
/// `data` (`SQL_NTS` for a terminated string), appended to what was sent before, or
/// `SQL_NULL_DATA` for NULL.
#[unsafe(no_mangle)]
pub extern "system" fn SQLPutData(stmt: *mut c_void, data: *mut c_void, length: isize) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let Some(index) = stmt.awaited.as_ref().and_then(|awaited| awaited.current) else {
        return failed(&mut stmt.diagnostics, Failure::new("HY010", "SQLParamData has not named a parameter"));
    };
    let value_type = stmt.parameters.get(index).copied().flatten().map_or(1, |parameter| parameter.value_type);
    let Some(slot) = stmt.awaited.as_mut().and_then(|awaited| awaited.sent.get_mut(index)) else {
        return SQL_ERROR;
    };
    if length == SQL_NULL_DATA {
        *slot = Some(None);
        return SQL_SUCCESS;
    }
    let bytes = match fixed_size(value_type) {
        Some(size) => input_bytes(data.cast_const().cast(), size),
        None if length == SQL_NTS => input_bytes(data.cast_const().cast(), terminated_length(data.cast_const().cast())),
        None => input_bytes(data.cast_const().cast(), usize::try_from(length).unwrap_or(0)),
    };
    match slot {
        Some(Some(sent)) => sent.extend_from_slice(&bytes),
        other => *other = Some(Some(bytes)),
    }
    SQL_SUCCESS
}

const SQL_PARAM_SUCCESS: u16 = 0;
const SQL_PARAM_ERROR: u16 = 5;
const SQL_PARAM_UNUSED: u16 = 7;
const SQL_ATTR_PARAM_BIND_TYPE: i32 = 18;
const SQL_ATTR_PARAM_STATUS_PTR: i32 = 20;
const SQL_ATTR_PARAMS_PROCESSED_PTR: i32 = 21;
const SQL_ATTR_PARAMSET_SIZE: i32 = 22;

fn execute_sets(stmt: &mut StatementHandle, text: &str) -> SqlReturn {
    let array = stmt.parameter_array;
    let mark = |set: usize, status: u16| {
        if !array.status.is_null() {
            store(array.status.wrapping_add(set), status);
        }
    };
    let mut changed: isize = 0;
    let mut processed = 0usize;
    let mut outcome = SQL_SUCCESS;
    for set in 0..array.size {
        let ran = bound_parameters(stmt, text, set).and_then(|(text, values)| {
            owning_connection(stmt)
                .ok_or_else(|| Failure::new("08003", "the connection is not open"))?
                .run(&text, &values)
        });
        processed += 1;
        match ran {
            Ok(outcomes) => {
                stmt.take_outcomes(outcomes);
                changed += stmt.row_count.max(0);
                mark(set, SQL_PARAM_SUCCESS);
            }
            Err(failure) => {
                stmt.diagnostics.push(failure);
                mark(set, SQL_PARAM_ERROR);
                for unused in set + 1..array.size {
                    mark(unused, SQL_PARAM_UNUSED);
                }
                outcome = if processed > 1 { SQL_SUCCESS_WITH_INFO } else { SQL_ERROR };
                break;
            }
        }
    }
    store(array.processed, processed);
    stmt.row_count = changed;
    outcome
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

fn element<T>(base: *mut T, element_bytes: usize, at: usize, offset: isize) -> *mut T {
    if base.is_null() {
        return base;
    }
    base.cast::<u8>()
        .wrapping_add(element_bytes * at)
        .wrapping_offset(offset)
        .cast::<T>()
}

fn fill_row(stmt: &mut StatementHandle, row: &[crate::native::Cell], at: usize, offset: isize) -> SqlReturn {
    let row_wise = stmt.block.row_bind_type;
    let mut outcome = SQL_SUCCESS;
    for (index, bound) in stmt.bound_columns.iter().enumerate() {
        let (Some(bound), Some(cell)) = (bound, row.get(index)) else {
            continue;
        };
        let (target_step, indicator_step) = match row_wise {
            0 => (usize::try_from(bound.buffer_length).unwrap_or(0), std::mem::size_of::<isize>()),
            row_size => (row_size, row_size),
        };
        let target = element(bound.target, target_step, at, offset);
        let indicator = element(bound.indicator, indicator_step, at, offset);
        match convert(cell, bound.target_type) {
            Ok(converted) => {
                let (delivered, _) = deliver(converted, 0, target, bound.buffer_length, indicator);
                if delivered == SQL_SUCCESS_WITH_INFO && outcome == SQL_SUCCESS {
                    outcome = SQL_SUCCESS_WITH_INFO;
                }
            }
            Err(message) => {
                stmt.diagnostics.push(Failure::new("22018", message));
                outcome = SQL_ERROR;
            }
        }
    }
    outcome
}

/// Moves to the next row, or the next `SQL_ATTR_ROW_ARRAY_SIZE` rows (D1279), filling the
/// columns bound with [`SQLBindCol`].
#[unsafe(no_mangle)]
pub extern "system" fn SQLFetch(stmt: *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.diagnostics.clear();
    let block = stmt.block;
    let size = block.row_array_size.max(1);
    let offset = read(block.bind_offset).unwrap_or(0);
    let start = stmt.next_row;
    let mut fetched = 0usize;
    let mut outcome = SQL_SUCCESS;
    for at in 0..size {
        let Some(row) = stmt.rows.get(stmt.next_row).cloned() else {
            break;
        };
        stmt.next_row += 1;
        let filled = fill_row(stmt, &row, at, offset);
        let status = match filled {
            SQL_SUCCESS => SQL_ROW_SUCCESS,
            SQL_SUCCESS_WITH_INFO => SQL_ROW_SUCCESS_WITH_INFO,
            _ => SQL_ROW_ERROR,
        };
        if !block.row_status.is_null() {
            store(block.row_status.wrapping_add(at), status);
        }
        outcome = match (outcome, filled) {
            (SQL_ERROR, _) | (_, SQL_ERROR) if size == 1 => SQL_ERROR,
            (SQL_SUCCESS, SQL_SUCCESS) => SQL_SUCCESS,
            _ => SQL_SUCCESS_WITH_INFO,
        };
        stmt.current = Some(row);
        fetched += 1;
    }
    if !block.row_status.is_null() {
        for at in fetched..size {
            store(block.row_status.wrapping_add(at), SQL_ROW_NOROW);
        }
    }
    store(block.rows_fetched, fetched);
    if fetched == 0 {
        stmt.current = None;
        stmt.rowset_start = None;
        return SQL_NO_DATA;
    }
    stmt.rowset_start = Some(start);
    stmt.read_offsets = vec![0; stmt.columns.len()];
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

const SQL_ATTR_ROW_BIND_TYPE: i32 = 5;
const SQL_ATTR_ROW_BIND_OFFSET_PTR: i32 = 23;
const SQL_ATTR_ROW_STATUS_PTR: i32 = 25;
const SQL_ATTR_ROWS_FETCHED_PTR: i32 = 26;
const SQL_ATTR_ROW_ARRAY_SIZE: i32 = 27;
const SQL_ROW_SUCCESS: u16 = 0;
const SQL_ROW_NOROW: u16 = 3;
const SQL_ROW_ERROR: u16 = 5;
const SQL_ROW_SUCCESS_WITH_INFO: u16 = 6;

/// Sets a statement attribute: the block fetch attributes are kept (D1279), the others are
/// accepted and ignored.
#[unsafe(no_mangle)]
pub extern "system" fn SQLSetStmtAttr(stmt: *mut c_void, attribute: i32, value: *mut c_void, _length: i32) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => stmt.block.row_array_size = value.addr().max(1),
        SQL_ATTR_ROW_BIND_TYPE => stmt.block.row_bind_type = value.addr(),
        SQL_ATTR_ROWS_FETCHED_PTR => stmt.block.rows_fetched = value.cast(),
        SQL_ATTR_ROW_STATUS_PTR => stmt.block.row_status = value.cast(),
        SQL_ATTR_ROW_BIND_OFFSET_PTR => stmt.block.bind_offset = value.cast_const().cast(),
        SQL_ATTR_PARAMSET_SIZE => stmt.parameter_array.size = value.addr().max(1),
        SQL_ATTR_CURSOR_TYPE => stmt.scrollable = value.addr() != SQL_CURSOR_FORWARD_ONLY,
        SQL_ATTR_CURSOR_SCROLLABLE => stmt.scrollable = value.addr() != 0,
        SQL_ATTR_PARAM_BIND_TYPE => stmt.parameter_array.bind_type = value.addr(),
        SQL_ATTR_PARAMS_PROCESSED_PTR => stmt.parameter_array.processed = value.cast(),
        SQL_ATTR_PARAM_STATUS_PTR => stmt.parameter_array.status = value.cast(),
        _ => {}
    }
    SQL_SUCCESS
}

/// The statement text as the driver runs it: ODBC's escape sequences translated for the
/// connection's dialect (D1256).
#[unsafe(no_mangle)]
pub extern "system" fn SQLNativeSql(
    dbc: *mut c_void,
    input: *const u8,
    input_length: i32,
    output: *mut u8,
    capacity: i32,
    written: *mut i32,
) -> SqlReturn {
    let Some(dbc) = connection(dbc) else {
        return SQL_INVALID_HANDLE;
    };
    dbc.diagnostics.clear();
    let Some(text) = input_text(input, isize::try_from(input_length).unwrap_or(-1)) else {
        return failed(&mut dbc.diagnostics, Failure::new("HY009", "no statement text was given"));
    };
    let translated = match crate::escapes::translated(&text, crate::escapes::Family::of(&dbc.dialect)) {
        Ok(translated) => translated,
        Err(message) => return failed(&mut dbc.diagnostics, Failure::new("37000", message)),
    };
    let truncated = write_text(output, isize::try_from(capacity).unwrap_or(0), translated.as_bytes(), 1);
    store(written, i32::try_from(translated.len()).unwrap_or(i32::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { SQL_SUCCESS }
}

/// Whether [`SQLGetInfo`] answers `info` with text, which its UTF-16 form widens.
pub(crate) fn answers_text(info: u16) -> bool {
    matches!(
        info,
        SQL_DRIVER_NAME
            | SQL_DRIVER_VER
            | SQL_DBMS_NAME
            | SQL_DBMS_VER
            | SQL_DRIVER_ODBC_VER
            | SQL_DATA_SOURCE_NAME
            | SQL_SERVER_NAME
            | SQL_DATABASE_NAME
            | SQL_IDENTIFIER_QUOTE_CHAR
            | SQL_DATA_SOURCE_READ_ONLY
            | SQL_USER_NAME
    ) || FIXED_INFO
        .iter()
        .any(|(known, answer)| *known == info && matches!(answer, InfoAnswer::Text(_)))
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
        SQL_DATA_SOURCE_NAME | SQL_SERVER_NAME | SQL_DATABASE_NAME => Some(dbc.database.clone()),
        SQL_IDENTIFIER_QUOTE_CHAR => Some("\"".to_owned()),
        SQL_DATA_SOURCE_READ_ONLY => Some(if dbc.read_only { "Y" } else { "N" }.to_owned()),
        SQL_USER_NAME => Some(String::new()),
        _ => match FIXED_INFO.iter().find(|(known, _)| *known == info).map(|(_, answer)| *answer) {
            Some(InfoAnswer::Text(text)) => Some(text.to_owned()),
            Some(InfoAnswer::Short(number)) => {
                store(value.cast::<u16>(), number);
                store(written, 2);
                return SQL_SUCCESS;
            }
            Some(InfoAnswer::Long(number)) => {
                store(value.cast::<u32>(), number);
                store(written, 4);
                return SQL_SUCCESS;
            }
            None => None,
        },
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

/// Lists the primary key columns of a table, with ODBC's six columns, in key order.
#[unsafe(no_mangle)]
pub extern "system" fn SQLPrimaryKeys(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = "SELECT k.table_catalog AS \"TABLE_CAT\", k.table_schema AS \"TABLE_SCHEM\", k.table_name AS \"TABLE_NAME\", \
         k.column_name AS \"COLUMN_NAME\", k.ordinal_position AS \"KEY_SEQ\", k.constraint_name AS \"PK_NAME\" \
         FROM information_schema.table_constraints t JOIN information_schema.key_column_usage k \
         ON k.constraint_schema = t.constraint_schema AND k.constraint_name = t.constraint_name AND k.table_name = t.table_name \
         WHERE t.constraint_type = 'PRIMARY KEY' AND ($1::text IS NULL OR k.table_schema = $1) \
         AND ($2::text IS NULL OR k.table_name = $2) ORDER BY 2, 3, 5";
    catalog_query(stmt, text, &[pattern(schema, schema_length), pattern(table, table_length)])
}

const ODBC_RULE_OF: &str = "WHEN 'CASCADE' THEN 0 WHEN 'RESTRICT' THEN 1 WHEN 'SET NULL' THEN 2 WHEN 'SET DEFAULT' THEN 4 ELSE 3 END";

/// Lists the foreign keys that refer to a primary key table, or that a foreign key table has,
/// with ODBC's fourteen columns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLForeignKeys(
    stmt: *mut c_void,
    _pk_catalog: *const u8,
    _pk_catalog_length: i16,
    pk_schema: *const u8,
    pk_schema_length: i16,
    pk_table: *const u8,
    pk_table_length: i16,
    _fk_catalog: *const u8,
    _fk_catalog_length: i16,
    fk_schema: *const u8,
    fk_schema_length: i16,
    fk_table: *const u8,
    fk_table_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT pk.table_catalog AS \"PKTABLE_CAT\", pk.table_schema AS \"PKTABLE_SCHEM\", pk.table_name AS \"PKTABLE_NAME\", \
         pk.column_name AS \"PKCOLUMN_NAME\", fk.table_catalog AS \"FKTABLE_CAT\", fk.table_schema AS \"FKTABLE_SCHEM\", \
         fk.table_name AS \"FKTABLE_NAME\", fk.column_name AS \"FKCOLUMN_NAME\", fk.ordinal_position AS \"KEY_SEQ\", \
         CASE rc.update_rule {ODBC_RULE_OF} AS \"UPDATE_RULE\", CASE rc.delete_rule {ODBC_RULE_OF} AS \"DELETE_RULE\", \
         rc.constraint_name AS \"FK_NAME\", rc.unique_constraint_name AS \"PK_NAME\", 7 AS \"DEFERRABILITY\" \
         FROM information_schema.referential_constraints rc \
         JOIN information_schema.key_column_usage fk ON fk.constraint_schema = rc.constraint_schema AND fk.constraint_name = rc.constraint_name \
         JOIN information_schema.key_column_usage pk ON pk.constraint_schema = rc.unique_constraint_schema \
         AND pk.constraint_name = rc.unique_constraint_name AND pk.ordinal_position = fk.position_in_unique_constraint \
         WHERE ($1::text IS NULL OR pk.table_schema = $1) AND ($2::text IS NULL OR pk.table_name = $2) \
         AND ($3::text IS NULL OR fk.table_schema = $3) AND ($4::text IS NULL OR fk.table_name = $4) \
         ORDER BY 6, 7, 2, 3, 9"
    );
    catalog_query(
        stmt,
        &text,
        &[
            pattern(pk_schema, pk_schema_length),
            pattern(pk_table, pk_table_length),
            pattern(fk_schema, fk_schema_length),
            pattern(fk_table, fk_table_length),
        ],
    )
}

const TYPE_INFO_ROWS: &str = "('boolean', -7, 1, NULL, NULL, NULL, 1, 0, 2, NULL, 0, 0, 'boolean', NULL, NULL, -7, NULL, NULL, NULL), \
    ('bigint', -5, 19, NULL, NULL, NULL, 1, 0, 2, 0, 0, 0, 'bigint', 0, 0, -5, NULL, 10, NULL), \
    ('bytea', -3, 1073741823, NULL, NULL, NULL, 1, 0, 0, NULL, 0, 0, 'bytea', NULL, NULL, -3, NULL, NULL, NULL), \
    ('text', -1, 1073741823, , , NULL, 1, 1, 3, NULL, 0, 0, 'text', NULL, NULL, -1, NULL, NULL, NULL), \
    ('numeric', 2, 1000, NULL, NULL, 'precision,scale', 1, 0, 2, 0, 0, 0, 'numeric', 0, 1000, 2, NULL, 10, NULL), \
    ('integer', 4, 10, NULL, NULL, NULL, 1, 0, 2, 0, 0, 0, 'integer', 0, 0, 4, NULL, 10, NULL), \
    ('smallint', 5, 5, NULL, NULL, NULL, 1, 0, 2, 0, 0, 0, 'smallint', 0, 0, 5, NULL, 10, NULL), \
    ('real', 7, 24, NULL, NULL, NULL, 1, 0, 2, 0, 0, 0, 'real', NULL, NULL, 7, NULL, 2, NULL), \
    ('double precision', 8, 53, NULL, NULL, NULL, 1, 0, 2, 0, 0, 0, 'double precision', NULL, NULL, 8, NULL, 2, NULL), \
    ('varchar', 12, 1073741823, , , 'max length', 1, 1, 3, NULL, 0, 0, 'varchar', NULL, NULL, 12, NULL, NULL, NULL), \
    ('date', 91, 10, , , NULL, 1, 0, 2, NULL, 0, 0, 'date', NULL, NULL, 9, 1, NULL, NULL), \
    ('time', 92, 15, , , NULL, 1, 0, 2, NULL, 0, 0, 'time', 0, 6, 9, 2, NULL, NULL), \
    ('timestamp', 93, 26, , , NULL, 1, 0, 2, NULL, 0, 0, 'timestamp', 0, 6, 9, 3, NULL, NULL), \
    ('uuid', -11, 36, , , NULL, 1, 0, 2, NULL, 0, 0, 'uuid', NULL, NULL, -11, NULL, NULL, NULL)";

const TYPE_INFO_COLUMNS: &str = "\"TYPE_NAME\", \"DATA_TYPE\", \"COLUMN_SIZE\", \"LITERAL_PREFIX\", \"LITERAL_SUFFIX\", \
    \"CREATE_PARAMS\", \"NULLABLE\", \"CASE_SENSITIVE\", \"SEARCHABLE\", \"UNSIGNED_ATTRIBUTE\", \"FIXED_PREC_SCALE\", \
    \"AUTO_UNIQUE_VALUE\", \"LOCAL_TYPE_NAME\", \"MINIMUM_SCALE\", \"MAXIMUM_SCALE\", \"SQL_DATA_TYPE\", \
    \"SQL_DATETIME_SUB\", \"NUM_PREC_RADIX\", \"INTERVAL_PRECISION\"";

/// Lists the data types EggDB offers, with ODBC's nineteen columns, ordered by ODBC type;
/// `SQL_ALL_TYPES` (0) lists all of them, another type only its own.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetTypeInfo(stmt: *mut c_void, data_type: i16) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT * FROM (VALUES {TYPE_INFO_ROWS}) AS type_info ({TYPE_INFO_COLUMNS}) \
         WHERE $1::text IS NULL OR \"DATA_TYPE\" = $1::integer ORDER BY 2, 1"
    );
    let wanted = (data_type != 0).then(|| data_type.to_string());
    catalog_query(stmt, &text, &[wanted])
}

/// Lists the functions and procedures whose schema and name match the patterns, with ODBC's
/// eight columns; parameter and result set counts are unknown (-1).
#[unsafe(no_mangle)]
pub extern "system" fn SQLProcedures(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    procedure: *const u8,
    procedure_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT routine_catalog AS \"PROCEDURE_CAT\", routine_schema AS \"PROCEDURE_SCHEM\", routine_name AS \"PROCEDURE_NAME\", \
         -1 AS \"NUM_INPUT_PARAMS\", -1 AS \"NUM_OUTPUT_PARAMS\", -1 AS \"NUM_RESULT_SETS\", NULL::text AS \"REMARKS\", \
         CASE routine_type WHEN 'FUNCTION' THEN 2 ELSE 1 END AS \"PROCEDURE_TYPE\" \
         FROM information_schema.routines WHERE routine_schema NOT IN {HIDDEN_SCHEMAS} \
         AND ($1::text IS NULL OR routine_schema LIKE $1) AND ($2::text IS NULL OR routine_name LIKE $2) ORDER BY 2, 3"
    );
    catalog_query(stmt, &text, &[pattern(schema, schema_length), pattern(procedure, procedure_length)])
}

const SQL_BEST_ROWID: u16 = 1;

/// The columns that identify a row (`SQL_BEST_ROWID`: the primary key's, with ODBC's eight
/// columns); EggDB has no row-version column, so `SQL_ROWVER` lists none.
#[unsafe(no_mangle)]
pub extern "system" fn SQLSpecialColumns(
    stmt: *mut c_void,
    identifier_type: u16,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
    _scope: u16,
    _nullable: u16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT 2 AS \"SCOPE\", c.column_name AS \"COLUMN_NAME\", {ODBC_TYPE_OF_DATA_TYPE} AS \"DATA_TYPE\", \
         c.data_type AS \"TYPE_NAME\", c.character_maximum_length AS \"COLUMN_SIZE\", c.character_octet_length AS \"BUFFER_LENGTH\", \
         c.numeric_scale AS \"DECIMAL_DIGITS\", 1 AS \"PSEUDO_COLUMN\" \
         FROM information_schema.table_constraints t JOIN information_schema.key_column_usage k \
         ON k.constraint_schema = t.constraint_schema AND k.constraint_name = t.constraint_name AND k.table_name = t.table_name \
         JOIN information_schema.columns c ON c.table_schema = k.table_schema AND c.table_name = k.table_name AND c.column_name = k.column_name \
         WHERE t.constraint_type = 'PRIMARY KEY' AND $3::text = '1' \
         AND ($1::text IS NULL OR k.table_schema = $1) AND ($2::text IS NULL OR k.table_name = $2) ORDER BY k.ordinal_position"
    );
    catalog_query(
        stmt,
        &text,
        &[
            pattern(schema, schema_length),
            pattern(table, table_length),
            Some(if identifier_type == SQL_BEST_ROWID { "1" } else { "0" }.to_owned()),
        ],
    )
}

/// A table's indexes, one row a key column with ODBC's thirteen columns (`SQL_INDEX_OTHER`
/// entries, no table statistics row): the ones behind its primary key and unique constraints
/// and the plain ones `CREATE INDEX` made, read from `pg_index`; `SQL_INDEX_UNIQUE` keeps the
/// unique ones only. A descending key column says `D`; a partial index gives its condition.
#[unsafe(no_mangle)]
pub extern "system" fn SQLStatistics(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
    unique: u16,
    _reserved: u16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = "SELECT current_database() AS \"TABLE_CAT\", tmp.schema_name AS \"TABLE_SCHEM\", \
         tmp.table_name AS \"TABLE_NAME\", CASE WHEN tmp.is_unique THEN 0 ELSE 1 END AS \"NON_UNIQUE\", \
         NULL::text AS \"INDEX_QUALIFIER\", tmp.index_name AS \"INDEX_NAME\", 3 AS \"TYPE\", \
         tmp.position AS \"ORDINAL_POSITION\", \
         trim(both '\"' from pg_catalog.pg_get_indexdef(tmp.index_oid, tmp.position, false)) AS \"COLUMN_NAME\", \
         CASE tmp.options[tmp.position - 1] & 1::smallint WHEN 1 THEN 'D' ELSE 'A' END AS \"ASC_OR_DESC\", \
         NULL::integer AS \"CARDINALITY\", NULL::integer AS \"PAGES\", \
         pg_catalog.pg_get_expr(tmp.predicate, tmp.table_oid) AS \"FILTER_CONDITION\" \
         FROM (SELECT n.nspname AS schema_name, ct.relname AS table_name, i.indisunique AS is_unique, \
         ci.relname AS index_name, (information_schema._pg_expandarray(i.indkey)).n AS position, \
         ci.oid AS index_oid, i.indoption AS options, i.indpred AS predicate, ct.oid AS table_oid \
         FROM pg_catalog.pg_class ct JOIN pg_catalog.pg_namespace n ON ct.relnamespace = n.oid \
         JOIN pg_catalog.pg_index i ON ct.oid = i.indrelid JOIN pg_catalog.pg_class ci ON ci.oid = i.indexrelid \
         WHERE ($1::text IS NULL OR n.nspname = $1) AND ($2::text IS NULL OR ct.relname = $2) \
         AND ($3::boolean IS FALSE OR i.indisunique)) AS tmp ORDER BY 4, 7, 6, 8";
    let unique_only = Some(String::from(if unique == SQL_INDEX_UNIQUE { "true" } else { "false" }));
    catalog_query(stmt, text, &[pattern(schema, schema_length), pattern(table, table_length), unique_only])
}

/// Lists the privileges granted on the tables matching the patterns, with ODBC's seven columns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLTablePrivileges(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    table: *const u8,
    table_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT table_catalog AS \"TABLE_CAT\", table_schema AS \"TABLE_SCHEM\", table_name AS \"TABLE_NAME\", \
         grantor AS \"GRANTOR\", grantee AS \"GRANTEE\", privilege_type AS \"PRIVILEGE\", is_grantable AS \"IS_GRANTABLE\" \
         FROM information_schema.table_privileges WHERE table_schema NOT IN {HIDDEN_SCHEMAS} \
         AND ($1::text IS NULL OR table_schema LIKE $1) AND ($2::text IS NULL OR table_name LIKE $2) ORDER BY 2, 3, 6"
    );
    catalog_query(stmt, &text, &[pattern(schema, schema_length), pattern(table, table_length)])
}

/// Lists the privileges granted on a table's columns whose names match the pattern, with
/// ODBC's eight columns.
#[unsafe(no_mangle)]
pub extern "system" fn SQLColumnPrivileges(
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
    let text = "SELECT table_catalog AS \"TABLE_CAT\", table_schema AS \"TABLE_SCHEM\", table_name AS \"TABLE_NAME\", \
         column_name AS \"COLUMN_NAME\", grantor AS \"GRANTOR\", grantee AS \"GRANTEE\", privilege_type AS \"PRIVILEGE\", \
         is_grantable AS \"IS_GRANTABLE\" FROM information_schema.column_privileges \
         WHERE ($1::text IS NULL OR table_schema = $1) AND ($2::text IS NULL OR table_name = $2) \
         AND ($3::text IS NULL OR column_name LIKE $3) ORDER BY 2, 3, 4, 7";
    catalog_query(
        stmt,
        text,
        &[pattern(schema, schema_length), pattern(table, table_length), pattern(column, column_length)],
    )
}

/// Lists the parameters of the functions and procedures matching the patterns, with ODBC's
/// nineteen columns: `IN` as `SQL_PARAM_INPUT`, `OUT` as `SQL_PARAM_OUTPUT`, `INOUT` as
/// `SQL_PARAM_INPUT_OUTPUT`.
#[unsafe(no_mangle)]
pub extern "system" fn SQLProcedureColumns(
    stmt: *mut c_void,
    _catalog: *const u8,
    _catalog_length: i16,
    schema: *const u8,
    schema_length: i16,
    procedure: *const u8,
    procedure_length: i16,
    column: *const u8,
    column_length: i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    let text = format!(
        "SELECT r.routine_catalog AS \"PROCEDURE_CAT\", r.routine_schema AS \"PROCEDURE_SCHEM\", r.routine_name AS \"PROCEDURE_NAME\", \
         c.parameter_name AS \"COLUMN_NAME\", CASE c.parameter_mode WHEN 'OUT' THEN 4 WHEN 'INOUT' THEN 2 ELSE 1 END AS \"COLUMN_TYPE\", \
         {ODBC_TYPE_OF_DATA_TYPE} AS \"DATA_TYPE\", c.data_type AS \"TYPE_NAME\", c.character_maximum_length AS \"COLUMN_SIZE\", \
         c.character_octet_length AS \"BUFFER_LENGTH\", c.numeric_scale AS \"DECIMAL_DIGITS\", c.numeric_precision_radix AS \"NUM_PREC_RADIX\", \
         2 AS \"NULLABLE\", NULL::text AS \"REMARKS\", NULL::text AS \"COLUMN_DEF\", {ODBC_TYPE_OF_DATA_TYPE} AS \"SQL_DATA_TYPE\", \
         NULL::integer AS \"SQL_DATETIME_SUB\", c.character_octet_length AS \"CHAR_OCTET_LENGTH\", c.ordinal_position AS \"ORDINAL_POSITION\", \
         '' AS \"IS_NULLABLE\" \
         FROM information_schema.parameters c JOIN information_schema.routines r \
         ON r.specific_schema = c.specific_schema AND r.specific_name = c.specific_name \
         WHERE r.routine_schema NOT IN {HIDDEN_SCHEMAS} \
         AND ($1::text IS NULL OR r.routine_schema LIKE $1) AND ($2::text IS NULL OR r.routine_name LIKE $2) \
         AND ($3::text IS NULL OR c.parameter_name LIKE $3) ORDER BY 2, 3, 18"
    );
    catalog_query(
        stmt,
        &text,
        &[pattern(schema, schema_length), pattern(procedure, procedure_length), pattern(column, column_length)],
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings_keep_braced_values_whole() {
        let found = attributes("Driver={EggDB ODBC};SERVER=db.local; Port = 6543 ;PWD={a;b=}}c};Flag;Database=sales");
        let pairs: Vec<(&str, &str)> = found.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
        assert_eq!(
            pairs,
            [("driver", "EggDB ODBC"), ("server", "db.local"), ("port", "6543"), ("pwd", "a;b=}c"), ("database", "sales")]
        );
    }

    #[test]
    fn a_server_address_becomes_a_url_with_its_parts_encoded() {
        let server = ServerAddress {
            host: "db.local".to_owned(),
            user: "ann".to_owned(),
            password: "p@ss word".to_owned(),
            ssl_mode: "require".to_owned(),
            ..ServerAddress::default()
        };
        assert_eq!(server.url("sales", "postgresql"), "postgresql://ann:p%40ss%20word@db.local:5432/sales?sslmode=require");
        assert_eq!(server.url("sales", "mysql"), "mysql://ann@db.local:3306/sales");
        assert_eq!(server_dialect("mysql://ann@db.local:3306/sales"), "mysql");
    }
}

const SQL_API_ALL_FUNCTIONS: u16 = 0;
const SQL_API_ODBC3_ALL_FUNCTIONS: u16 = 999;
const SQL_API_ODBC3_ALL_FUNCTIONS_SIZE: usize = 250;
const SQL_API_ALL_FUNCTIONS_SIZE: usize = 100;
const SUPPORTED_FUNCTIONS: [u16; 48] = [
    4, 5, 6, 7, 8, 9, 11, 12, 13, 16, 18, 19, 20, 40, 41, 43, 44, 45, 47, 52, 53, 54, 56, 58, 60, 61, 62, 63, 65, 66, 67,
    70, 72, 48, 49, 1001, 1003, 1005, 1006, 1007, 1010, 1011, 1012, 1014, 1016, 1019, 1020, 1021,
];
const SQL_FETCH_NEXT: i16 = 1;
const SQL_DIAG_RETURNCODE: i16 = 1;
const SQL_DIAG_NUMBER: i16 = 2;
const SQL_DIAG_ROW_COUNT: i16 = 3;
const SQL_DIAG_SQLSTATE: i16 = 4;
const SQL_DIAG_NATIVE: i16 = 5;
const SQL_DIAG_MESSAGE_TEXT: i16 = 6;
const SQL_DIAG_CLASS_ORIGIN: i16 = 8;
const SQL_DIAG_SUBCLASS_ORIGIN: i16 = 9;
const SQL_DIAG_CONNECTION_NAME: i16 = 10;
const SQL_DIAG_SERVER_NAME: i16 = 11;

/// Says which ODBC functions the driver has (D1278): one function's `SQL_TRUE` or `SQL_FALSE`,
/// ODBC 2's array of 100 for `SQL_API_ALL_FUNCTIONS`, or ODBC 3's bitmap of 250 words for
/// `SQL_API_ODBC3_ALL_FUNCTIONS`.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetFunctions(dbc: *mut c_void, function: u16, supported: *mut u16) -> SqlReturn {
    if connection(dbc).is_none() {
        return SQL_INVALID_HANDLE;
    }
    if supported.is_null() {
        return SQL_ERROR;
    }
    match function {
        SQL_API_ODBC3_ALL_FUNCTIONS => {
            let mut bitmap = [0u16; SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
            for id in SUPPORTED_FUNCTIONS {
                if let Some(word) = bitmap.get_mut(usize::from(id >> 4)) {
                    *word |= 1 << (id & 0xF);
                }
            }
            for (at, word) in bitmap.iter().enumerate() {
                store(supported.wrapping_add(at), *word);
            }
        }
        SQL_API_ALL_FUNCTIONS => {
            for at in 0..SQL_API_ALL_FUNCTIONS_SIZE {
                let id = u16::try_from(at).unwrap_or(u16::MAX);
                store(supported.wrapping_add(at), u16::from(SUPPORTED_FUNCTIONS.contains(&id)));
            }
        }
        id => store(supported, u16::from(SUPPORTED_FUNCTIONS.contains(&id))),
    }
    SQL_SUCCESS
}

/// Moves to the statement's next result set, when its text ran several queries.
#[unsafe(no_mangle)]
pub extern "system" fn SQLMoreResults(stmt: *mut c_void) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.diagnostics.clear();
    if stmt.pending.is_empty() {
        stmt.close_cursor();
        return SQL_NO_DATA;
    }
    let next = stmt.pending.remove(0);
    stmt.show(next);
    SQL_SUCCESS
}

/// Answers a statement's attributes: a forward-only, read-only cursor of one row at a time,
/// with no timeout and no row limit.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetStmtAttr(stmt: *mut c_void, attribute: i32, value: *mut c_void, _capacity: i32, written: *mut i32) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    stmt.diagnostics.clear();
    let answer: usize = match attribute {
        SQL_ATTR_CURSOR_TYPE => if stmt.scrollable { SQL_CURSOR_STATIC } else { SQL_CURSOR_FORWARD_ONLY },
        SQL_ATTR_CURSOR_SCROLLABLE => usize::from(stmt.scrollable),
        0 | 1 | -2 => 0,
        7 => 1,
        SQL_ATTR_ROW_ARRAY_SIZE => stmt.block.row_array_size,
        SQL_ATTR_ROW_BIND_TYPE => stmt.block.row_bind_type,
        14 => stmt.next_row,
        _ => return failed(&mut stmt.diagnostics, Failure::new("HY092", format!("statement attribute {attribute} is not supported"))),
    };
    store(value.cast::<usize>(), answer);
    store(written, i32::try_from(std::mem::size_of::<usize>()).unwrap_or(8));
    SQL_SUCCESS
}

/// Stops a running statement; the driver runs each call to its end before returning, so there
/// is never one to stop.
#[unsafe(no_mangle)]
pub extern "system" fn SQLCancel(stmt: *mut c_void) -> SqlReturn {
    match statement(stmt) {
        Some(_) => SQL_SUCCESS,
        None => SQL_INVALID_HANDLE,
    }
}

const SQL_ATTR_CURSOR_TYPE: i32 = 6;
const SQL_ATTR_CURSOR_SCROLLABLE: i32 = -1;
const SQL_CURSOR_FORWARD_ONLY: usize = 0;
const SQL_CURSOR_STATIC: usize = 3;
const SQL_FETCH_FIRST: i16 = 2;
const SQL_FETCH_LAST: i16 = 3;
const SQL_FETCH_PRIOR: i16 = 4;
const SQL_FETCH_ABSOLUTE: i16 = 5;
const SQL_FETCH_RELATIVE: i16 = 6;

/// Fetches a rowset by direction (D1282): the next one on any cursor; on a static cursor
/// (`SQL_ATTR_CURSOR_TYPE` `SQL_CURSOR_STATIC`, or `SQL_ATTR_CURSOR_SCROLLABLE`) also the first,
/// the last, the prior one, or one starting at an absolute or relative row, the result being
/// held whole. A position before the first row or past the last gives `SQL_NO_DATA`.
#[unsafe(no_mangle)]
pub extern "system" fn SQLFetchScroll(stmt_handle: *mut c_void, orientation: i16, offset: isize) -> SqlReturn {
    if orientation == SQL_FETCH_NEXT {
        return SQLFetch(stmt_handle);
    }
    let Some(stmt) = statement(stmt_handle) else {
        return SQL_INVALID_HANDLE;
    };
    if !stmt.scrollable {
        return failed(&mut stmt.diagnostics, Failure::new("HY106", "the cursor is forward-only: only SQL_FETCH_NEXT is supported"));
    }
    let size = isize::try_from(stmt.block.row_array_size.max(1)).unwrap_or(1);
    let rows = isize::try_from(stmt.rows.len()).unwrap_or(isize::MAX);
    let current = stmt.rowset_start.and_then(|start| isize::try_from(start).ok());
    let target = match orientation {
        SQL_FETCH_FIRST => 0,
        SQL_FETCH_LAST => (rows - size).max(0),
        SQL_FETCH_PRIOR => match current {
            None | Some(0) => -1,
            Some(start) => (start - size).max(0),
        },
        SQL_FETCH_ABSOLUTE if offset > 0 => offset - 1,
        SQL_FETCH_ABSOLUTE if offset < 0 => rows + offset,
        SQL_FETCH_ABSOLUTE => -1,
        SQL_FETCH_RELATIVE => current.unwrap_or(-1).saturating_add(offset),
        _ => return failed(&mut stmt.diagnostics, Failure::new("HY106", format!("fetch orientation {orientation} is not supported"))),
    };
    if target < 0 || target >= rows {
        stmt.next_row = if target < 0 { 0 } else { stmt.rows.len() };
        stmt.rowset_start = None;
        stmt.current = None;
        store(stmt.block.rows_fetched, 0);
        return SQL_NO_DATA;
    }
    stmt.next_row = usize::try_from(target).unwrap_or(0);
    SQLFetch(stmt_handle)
}

/// Describes a parameter as text of unknown size and nullability, which every type the
/// statement's place asks for accepts.
#[unsafe(no_mangle)]
pub extern "system" fn SQLDescribeParam(
    stmt: *mut c_void,
    number: u16,
    data_type: *mut i16,
    size: *mut usize,
    digits: *mut i16,
    nullable: *mut i16,
) -> SqlReturn {
    let Some(stmt) = statement(stmt) else {
        return SQL_INVALID_HANDLE;
    };
    if number == 0 {
        return failed(&mut stmt.diagnostics, Failure::new("07009", "parameters are numbered from 1"));
    }
    store(data_type, 12);
    store(size, 0);
    store(digits, 0);
    store(nullable, SQL_NULLABLE_UNKNOWN);
    SQL_SUCCESS
}

/// One field of a handle's diagnostics: the header's record count, return code and row count,
/// or a record's SQLSTATE, native code, message and origins.
#[unsafe(no_mangle)]
pub extern "system" fn SQLGetDiagField(
    handle_type: i16,
    handle: *mut c_void,
    record: i16,
    field: i16,
    value: *mut c_void,
    capacity: i16,
    written: *mut i16,
) -> SqlReturn {
    let (records, row_count) = match handle_type {
        SQL_HANDLE_ENV => match environment(handle) {
            Some(env) => (&env.diagnostics.records, 0),
            None => return SQL_INVALID_HANDLE,
        },
        SQL_HANDLE_DBC => match connection(handle) {
            Some(dbc) => (&dbc.diagnostics.records, 0),
            None => return SQL_INVALID_HANDLE,
        },
        SQL_HANDLE_STMT => match statement(handle) {
            Some(stmt) => (&stmt.diagnostics.records, stmt.row_count),
            None => return SQL_INVALID_HANDLE,
        },
        _ => return SQL_INVALID_HANDLE,
    };
    match field {
        SQL_DIAG_NUMBER => {
            store(value.cast::<i32>(), i32::try_from(records.len()).unwrap_or(i32::MAX));
            return SQL_SUCCESS;
        }
        SQL_DIAG_RETURNCODE => {
            store(value.cast::<i16>(), if records.is_empty() { SQL_SUCCESS } else { SQL_ERROR });
            return SQL_SUCCESS;
        }
        SQL_DIAG_ROW_COUNT => {
            store(value.cast::<isize>(), row_count);
            return SQL_SUCCESS;
        }
        _ => {}
    }
    let Some(found) = usize::try_from(record).ok().and_then(|number| number.checked_sub(1)).and_then(|at| records.get(at)) else {
        return SQL_NO_DATA;
    };
    let text = match field {
        SQL_DIAG_NATIVE => {
            store(value.cast::<i32>(), found.native);
            return SQL_SUCCESS;
        }
        SQL_DIAG_SQLSTATE => found.sqlstate.clone(),
        SQL_DIAG_MESSAGE_TEXT => found.message.clone(),
        SQL_DIAG_CLASS_ORIGIN | SQL_DIAG_SUBCLASS_ORIGIN => {
            if found.sqlstate.starts_with("IM") || found.sqlstate.starts_with("HY") { "ODBC 3.0" } else { "ISO 9075" }.to_owned()
        }
        SQL_DIAG_CONNECTION_NAME | SQL_DIAG_SERVER_NAME => String::new(),
        _ => return SQL_ERROR,
    };
    let truncated = write_text(value.cast(), isize::from(capacity), text.as_bytes(), 1);
    store(written, i16::try_from(text.len()).unwrap_or(i16::MAX));
    if truncated { SQL_SUCCESS_WITH_INFO } else { SQL_SUCCESS }
}
