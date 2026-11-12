//! EggDB's ADBC driver (D29): the embedded database through the Arrow Database Connectivity C
//! API, version 1.0.0, with query results as Arrow C data interface streams, so ADBC's driver
//! manager, Python's `adbc_driver_manager`, R and Go load EggDB directly with no server.
//! Written from the ADBC and Arrow C data interface specifications. Load it with the entry point
//! `AdbcDriverInit`; set the database option `path` (or `uri`) to the file, or `:memory:`.

mod arrow_c;

pub use arrow_c::{ArrowArray, ArrowArrayStream, ArrowSchema};

use eggdb::Database;
use eggdb::sql::{Dialect, QueryResult, Store};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Arc;

/// An ADBC status code.
pub type AdbcStatusCode = u8;

const ADBC_STATUS_OK: AdbcStatusCode = 0;
const ADBC_STATUS_NOT_IMPLEMENTED: AdbcStatusCode = 2;
const ADBC_STATUS_INVALID_ARGUMENT: AdbcStatusCode = 5;
const ADBC_STATUS_INVALID_STATE: AdbcStatusCode = 6;
const ADBC_STATUS_INTERNAL: AdbcStatusCode = 9;
const ADBC_STATUS_IO: AdbcStatusCode = 10;
const ADBC_VERSION_1_0_0: c_int = 1_000_000;
const MEMORY_PATH: &str = ":memory:";

/// ADBC's `AdbcError`.
#[repr(C)]
#[derive(Debug)]
pub struct AdbcError {
    /// The message, owned by the driver until `release`.
    pub message: *mut c_char,
    /// A vendor code.
    pub vendor_code: i32,
    /// The SQLSTATE.
    pub sqlstate: [c_char; 5],
    /// Frees the message.
    pub release: Option<unsafe extern "C" fn(*mut AdbcError)>,
}

/// ADBC's `AdbcDatabase`, `AdbcConnection` and `AdbcStatement` all have this layout.
#[repr(C)]
#[derive(Debug)]
pub struct AdbcHandle {
    /// The driver's state.
    pub private_data: *mut c_void,
    /// The driver, set by the driver manager.
    pub private_driver: *mut AdbcDriver,
}

type Function0 = unsafe extern "C" fn(*mut AdbcHandle, *mut AdbcError) -> AdbcStatusCode;
type SetOption = unsafe extern "C" fn(*mut AdbcHandle, *const c_char, *const c_char, *mut AdbcError) -> AdbcStatusCode;

/// ADBC 1.0.0's `AdbcDriver`: the driver's functions, filled by [`AdbcDriverInit`].
#[repr(C)]
#[derive(Debug)]
pub struct AdbcDriver {
    /// The driver's state.
    pub private_data: *mut c_void,
    /// The driver manager's state.
    pub private_manager: *mut c_void,
    /// Frees the driver.
    pub release: Option<unsafe extern "C" fn(*mut AdbcDriver, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcDatabaseInit`.
    pub database_init: Option<Function0>,
    /// `AdbcDatabaseNew`.
    pub database_new: Option<Function0>,
    /// `AdbcDatabaseSetOption`.
    pub database_set_option: Option<SetOption>,
    /// `AdbcDatabaseRelease`.
    pub database_release: Option<Function0>,
    /// `AdbcConnectionCommit`.
    pub connection_commit: Option<Function0>,
    /// `AdbcConnectionGetInfo`.
    pub connection_get_info: Option<unsafe extern "C" fn(*mut AdbcHandle, *const u32, usize, *mut ArrowArrayStream, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcConnectionGetObjects`.
    pub connection_get_objects: Option<
        unsafe extern "C" fn(
            *mut AdbcHandle,
            c_int,
            *const c_char,
            *const c_char,
            *const c_char,
            *const *const c_char,
            *const c_char,
            *mut ArrowArrayStream,
            *mut AdbcError,
        ) -> AdbcStatusCode,
    >,
    /// `AdbcConnectionGetTableSchema`.
    pub connection_get_table_schema: Option<
        unsafe extern "C" fn(*mut AdbcHandle, *const c_char, *const c_char, *const c_char, *mut ArrowSchema, *mut AdbcError) -> AdbcStatusCode,
    >,
    /// `AdbcConnectionGetTableTypes`.
    pub connection_get_table_types: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowArrayStream, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcConnectionInit`.
    pub connection_init: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut AdbcHandle, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcConnectionNew`.
    pub connection_new: Option<Function0>,
    /// `AdbcConnectionSetOption`.
    pub connection_set_option: Option<SetOption>,
    /// `AdbcConnectionReadPartition`.
    pub connection_read_partition:
        Option<unsafe extern "C" fn(*mut AdbcHandle, *const u8, usize, *mut ArrowArrayStream, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcConnectionRelease`.
    pub connection_release: Option<Function0>,
    /// `AdbcConnectionRollback`.
    pub connection_rollback: Option<Function0>,
    /// `AdbcStatementBind`.
    pub statement_bind: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowArray, *mut ArrowSchema, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementBindStream`.
    pub statement_bind_stream: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowArrayStream, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementExecuteQuery`.
    pub statement_execute_query:
        Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowArrayStream, *mut i64, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementExecutePartitions`.
    pub statement_execute_partitions:
        Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowSchema, *mut c_void, *mut i64, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementGetParameterSchema`.
    pub statement_get_parameter_schema: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut ArrowSchema, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementNew`.
    pub statement_new: Option<unsafe extern "C" fn(*mut AdbcHandle, *mut AdbcHandle, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementPrepare`.
    pub statement_prepare: Option<Function0>,
    /// `AdbcStatementRelease`.
    pub statement_release: Option<Function0>,
    /// `AdbcStatementSetOption`.
    pub statement_set_option: Option<SetOption>,
    /// `AdbcStatementSetSqlQuery`.
    pub statement_set_sql_query: Option<unsafe extern "C" fn(*mut AdbcHandle, *const c_char, *mut AdbcError) -> AdbcStatusCode>,
    /// `AdbcStatementSetSubstraitPlan`.
    pub statement_set_substrait_plan: Option<unsafe extern "C" fn(*mut AdbcHandle, *const u8, usize, *mut AdbcError) -> AdbcStatusCode>,
}

struct DatabaseState {
    path: Option<String>,
    dialect: Dialect,
    database: Option<Arc<Database>>,
}

struct ConnectionState {
    database: Option<Arc<Database>>,
    dialect: Dialect,
}

struct StatementState {
    database: Arc<Database>,
    dialect: Dialect,
    sql: Option<String>,
}

unsafe extern "C" fn release_error(error: *mut AdbcError) {
    if error.is_null() {
        return;
    }
    // SAFETY: the caller releases an error this driver filled.
    let error = unsafe { &mut *error };
    if !error.message.is_null() {
        // SAFETY: the message came from `CString::into_raw` in `fail` and is freed once here.
        drop(unsafe { CString::from_raw(error.message) });
    }
    error.message = std::ptr::null_mut();
    error.release = None;
}

fn fail(error: *mut AdbcError, status: AdbcStatusCode, message: &str) -> AdbcStatusCode {
    if error.is_null() {
        return status;
    }
    // SAFETY: ADBC passes NULL or writable memory for one error.
    let error = unsafe { &mut *error };
    if let Some(release) = error.release {
        // SAFETY: an error with a release function holds an earlier message to free first.
        unsafe { release(error) };
    }
    error.message = CString::new(message.replace('\0', "")).unwrap_or_default().into_raw();
    error.vendor_code = 0;
    error.sqlstate = [0; 5];
    error.release = Some(release_error);
    status
}

fn text(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: ADBC passes NUL-terminated strings for keys, values and queries.
    let text = unsafe { CStr::from_ptr(pointer) };
    Some(text.to_string_lossy().into_owned())
}

fn handle_mut<'h>(handle: *mut AdbcHandle) -> Option<&'h mut AdbcHandle> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: ADBC passes a live handle the caller owns for the call.
    Some(unsafe { &mut *handle })
}

fn state<'h, T>(handle: *mut AdbcHandle) -> Option<&'h mut T> {
    let handle = handle_mut(handle)?;
    if handle.private_data.is_null() {
        return None;
    }
    // SAFETY: a handle this driver set up holds a box of its own state type in `private_data`.
    Some(unsafe { &mut *handle.private_data.cast::<T>() })
}

fn set_state<T>(handle: *mut AdbcHandle, value: T) -> bool {
    match handle_mut(handle) {
        Some(handle) => {
            handle.private_data = Box::into_raw(Box::new(value)).cast();
            true
        }
        None => false,
    }
}

fn free_state<T>(handle: *mut AdbcHandle) -> bool {
    let Some(handle) = handle_mut(handle) else {
        return false;
    };
    if handle.private_data.is_null() {
        return false;
    }
    // SAFETY: `private_data` is the box `set_state` leaked for this type, freed once here.
    drop(unsafe { Box::from_raw(handle.private_data.cast::<T>()) });
    handle.private_data = std::ptr::null_mut();
    true
}

fn dialect_named(name: &str) -> Option<Dialect> {
    Some(match name.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" => Dialect::PostgreSql,
        "mysql" => Dialect::MySql,
        "sqlite" => Dialect::Sqlite,
        "sqlserver" | "tsql" => Dialect::SqlServer,
        "oracle" => Dialect::Oracle,
        _ => return None,
    })
}

fn run(database: &Database, dialect: Dialect, sql: &str) -> Result<Vec<QueryResult>, String> {
    let store = Store(database);
    let mut session = store.session(dialect);
    let mut results = Vec::new();
    let ran = session.execute(sql, &mut |result| results.push(result));
    let _ = session.close();
    ran.map_err(|error| error.message().to_owned())?;
    Ok(results)
}

unsafe extern "C" fn database_new(database: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    let fresh = DatabaseState { path: None, dialect: Dialect::PostgreSql, database: None };
    if set_state(database, fresh) { ADBC_STATUS_OK } else { fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no database handle") }
}

unsafe extern "C" fn database_set_option(
    database: *mut AdbcHandle,
    key: *const c_char,
    value: *const c_char,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    let Some(state) = state::<DatabaseState>(database) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the database is not set up");
    };
    let (Some(key), Some(value)) = (text(key), text(value)) else {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "an option needs a key and a value");
    };
    match key.as_str() {
        "path" | "uri" => state.path = Some(value.trim_start_matches("file:").to_owned()),
        "adbc.eggdb.dialect" => match dialect_named(&value) {
            Some(dialect) => state.dialect = dialect,
            None => return fail(error, ADBC_STATUS_INVALID_ARGUMENT, &format!("unknown dialect {value}")),
        },
        other => return fail(error, ADBC_STATUS_NOT_IMPLEMENTED, &format!("unknown database option {other}")),
    }
    ADBC_STATUS_OK
}

unsafe extern "C" fn database_init(database: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    let Some(state) = state::<DatabaseState>(database) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the database is not set up");
    };
    let path = state.path.clone().unwrap_or_else(|| MEMORY_PATH.to_owned());
    let opened = if path == MEMORY_PATH { Database::in_memory() } else { Database::open(&path) };
    match opened {
        Ok(opened) => {
            state.database = Some(Arc::new(opened));
            ADBC_STATUS_OK
        }
        Err(cause) => fail(error, ADBC_STATUS_IO, &format!("cannot open {path}: {cause}")),
    }
}

unsafe extern "C" fn database_release(database: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    if free_state::<DatabaseState>(database) { ADBC_STATUS_OK } else { fail(error, ADBC_STATUS_INVALID_STATE, "the database is not set up") }
}

unsafe extern "C" fn connection_new(connection: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    let fresh = ConnectionState { database: None, dialect: Dialect::PostgreSql };
    if set_state(connection, fresh) { ADBC_STATUS_OK } else { fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no connection handle") }
}

unsafe extern "C" fn connection_set_option(
    connection: *mut AdbcHandle,
    key: *const c_char,
    value: *const c_char,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    let (Some(key), Some(value)) = (text(key), text(value)) else {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "an option needs a key and a value");
    };
    match (key.as_str(), value.as_str()) {
        ("adbc.connection.autocommit", "true") => ADBC_STATUS_OK,
        ("adbc.connection.autocommit", _) => {
            fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "EggDB's ADBC driver commits each statement; run BEGIN and COMMIT as SQL")
        }
        ("adbc.eggdb.dialect", name) => match (state::<ConnectionState>(connection), dialect_named(name)) {
            (Some(state), Some(dialect)) => {
                state.dialect = dialect;
                ADBC_STATUS_OK
            }
            _ => fail(error, ADBC_STATUS_INVALID_ARGUMENT, &format!("unknown dialect {name}")),
        },
        (other, _) => fail(error, ADBC_STATUS_NOT_IMPLEMENTED, &format!("unknown connection option {other}")),
    }
}

unsafe extern "C" fn connection_init(connection: *mut AdbcHandle, database: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    let Some(opened) = state::<DatabaseState>(database).and_then(|state| state.database.clone().map(|db| (db, state.dialect))) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "initialize the database first");
    };
    let Some(state) = state::<ConnectionState>(connection) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the connection is not set up");
    };
    state.database = Some(opened.0);
    state.dialect = opened.1;
    ADBC_STATUS_OK
}

unsafe extern "C" fn connection_release(connection: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    if free_state::<ConnectionState>(connection) { ADBC_STATUS_OK } else { fail(error, ADBC_STATUS_INVALID_STATE, "the connection is not set up") }
}

unsafe extern "C" fn connection_commit(_connection: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_INVALID_STATE, "the connection commits each statement")
}

unsafe extern "C" fn connection_get_table_types(
    _connection: *mut AdbcHandle,
    out: *mut ArrowArrayStream,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    if out.is_null() {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no stream to write");
    }
    let columns = vec![eggdb_parquet::Column {
        name: "table_type".to_owned(),
        column_type: eggdb_parquet::ColumnType::Text,
        nullable: false,
        cells: vec![
            eggdb_parquet::Cell::Bytes(b"table".to_vec()),
            eggdb_parquet::Cell::Bytes(b"view".to_vec()),
        ],
    }];
    // SAFETY: ADBC passes writable memory for one stream.
    let out = unsafe { &mut *out };
    arrow_c::export_stream(columns, out);
    ADBC_STATUS_OK
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

unsafe extern "C" fn connection_get_table_schema(
    connection: *mut AdbcHandle,
    _catalog: *const c_char,
    schema: *const c_char,
    table: *const c_char,
    out: *mut ArrowSchema,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    let Some(state) = state::<ConnectionState>(connection) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the connection is not set up");
    };
    let (Some(database), Some(table)) = (state.database.clone(), text(table)) else {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "name a table of an initialized connection");
    };
    if out.is_null() {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no schema to write");
    }
    let name = match text(schema) {
        Some(schema) => format!("{}.{}", quoted(&schema), quoted(&table)),
        None => quoted(&table),
    };
    match run(&database, Dialect::PostgreSql, &format!("SELECT * FROM {name} LIMIT 0")) {
        Ok(results) => {
            let columns = results.into_iter().last().map(eggdb_flight::columns_of).unwrap_or_default();
            // SAFETY: ADBC passes writable memory for one schema.
            let out = unsafe { &mut *out };
            arrow_c::export_schema(&columns, out);
            ADBC_STATUS_OK
        }
        Err(message) => fail(error, ADBC_STATUS_INVALID_ARGUMENT, &message),
    }
}

unsafe extern "C" fn statement_new(connection: *mut AdbcHandle, statement: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    let Some((database, dialect)) = state::<ConnectionState>(connection).and_then(|state| state.database.clone().map(|db| (db, state.dialect))) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "initialize the connection first");
    };
    if set_state(statement, StatementState { database, dialect, sql: None }) {
        ADBC_STATUS_OK
    } else {
        fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no statement handle")
    }
}

unsafe extern "C" fn statement_set_sql_query(statement: *mut AdbcHandle, query: *const c_char, error: *mut AdbcError) -> AdbcStatusCode {
    let Some(state) = state::<StatementState>(statement) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the statement is not set up");
    };
    state.sql = text(query);
    ADBC_STATUS_OK
}

unsafe extern "C" fn statement_prepare(statement: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    match state::<StatementState>(statement) {
        Some(state) if state.sql.is_some() => ADBC_STATUS_OK,
        _ => fail(error, ADBC_STATUS_INVALID_STATE, "set the statement's SQL first"),
    }
}

unsafe extern "C" fn statement_execute_query(
    statement: *mut AdbcHandle,
    out: *mut ArrowArrayStream,
    rows_affected: *mut i64,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    let Some(state) = state::<StatementState>(statement) else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "the statement is not set up");
    };
    let Some(sql) = state.sql.clone() else {
        return fail(error, ADBC_STATUS_INVALID_STATE, "set the statement's SQL first");
    };
    let results = match run(&state.database, state.dialect, &sql) {
        Ok(results) => results,
        Err(message) => return fail(error, ADBC_STATUS_INVALID_ARGUMENT, &message),
    };
    let Some(last) = results.into_iter().last() else {
        return fail(error, ADBC_STATUS_INTERNAL, "the statement returned nothing");
    };
    let affected = if last.columns.is_some() {
        -1
    } else {
        last.tag.rsplit(' ').next().and_then(|count| count.parse().ok()).unwrap_or(-1)
    };
    if !rows_affected.is_null() {
        // SAFETY: ADBC passes NULL or writable memory for one count.
        unsafe { rows_affected.write(affected) };
    }
    if !out.is_null() {
        // SAFETY: ADBC passes NULL or writable memory for one stream.
        let out = unsafe { &mut *out };
        arrow_c::export_stream(eggdb_flight::columns_of(last), out);
    }
    ADBC_STATUS_OK
}

unsafe extern "C" fn statement_release(statement: *mut AdbcHandle, error: *mut AdbcError) -> AdbcStatusCode {
    if free_state::<StatementState>(statement) { ADBC_STATUS_OK } else { fail(error, ADBC_STATUS_INVALID_STATE, "the statement is not set up") }
}

unsafe extern "C" fn statement_set_option(
    _statement: *mut AdbcHandle,
    key: *const c_char,
    _value: *const c_char,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    let key = text(key).unwrap_or_default();
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, &format!("the statement option {key} is not supported yet"))
}

unsafe extern "C" fn not_implemented_bind(_: *mut AdbcHandle, _: *mut ArrowArray, _: *mut ArrowSchema, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "binding parameters is not supported yet")
}

unsafe extern "C" fn not_implemented_bind_stream(_: *mut AdbcHandle, _: *mut ArrowArrayStream, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "binding parameters is not supported yet")
}

unsafe extern "C" fn not_implemented_partitions(_: *mut AdbcHandle, _: *mut ArrowSchema, _: *mut c_void, _: *mut i64, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "partitioned results are not supported")
}

unsafe extern "C" fn not_implemented_parameter_schema(_: *mut AdbcHandle, _: *mut ArrowSchema, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "parameter schemas are not supported yet")
}

unsafe extern "C" fn not_implemented_substrait(_: *mut AdbcHandle, _: *const u8, _: usize, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "Substrait plans are not supported")
}

unsafe extern "C" fn not_implemented_read_partition(_: *mut AdbcHandle, _: *const u8, _: usize, _: *mut ArrowArrayStream, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "partitioned results are not supported")
}

unsafe extern "C" fn not_implemented_info(_: *mut AdbcHandle, _: *const u32, _: usize, _: *mut ArrowArrayStream, error: *mut AdbcError) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "GetInfo is not supported yet")
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn not_implemented_objects(
    _: *mut AdbcHandle,
    _: c_int,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const *const c_char,
    _: *const c_char,
    _: *mut ArrowArrayStream,
    error: *mut AdbcError,
) -> AdbcStatusCode {
    fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "GetObjects is not supported yet; query information_schema")
}

unsafe extern "C" fn release_driver(driver: *mut AdbcDriver, _error: *mut AdbcError) -> AdbcStatusCode {
    if !driver.is_null() {
        // SAFETY: the driver manager releases a driver this function filled.
        let driver = unsafe { &mut *driver };
        driver.release = None;
    }
    ADBC_STATUS_OK
}

/// The driver's entry point: fills `raw_driver` (an `AdbcDriver` of ADBC 1.0.0) with the
/// driver's functions; any other version is refused with `ADBC_STATUS_NOT_IMPLEMENTED`, so the
/// driver manager falls back to 1.0.0.
///
/// # Safety
/// `raw_driver` points to writable memory for an ADBC 1.0.0 `AdbcDriver`, and `error` is NULL or
/// writable memory for an `AdbcError`.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "C" fn AdbcDriverInit(version: c_int, raw_driver: *mut c_void, error: *mut AdbcError) -> AdbcStatusCode {
    if version != ADBC_VERSION_1_0_0 {
        return fail(error, ADBC_STATUS_NOT_IMPLEMENTED, "EggDB's ADBC driver speaks ADBC 1.0.0");
    }
    if raw_driver.is_null() {
        return fail(error, ADBC_STATUS_INVALID_ARGUMENT, "no driver to fill");
    }
    let filled = AdbcDriver {
        private_data: std::ptr::null_mut(),
        private_manager: std::ptr::null_mut(),
        release: Some(release_driver),
        database_init: Some(database_init),
        database_new: Some(database_new),
        database_set_option: Some(database_set_option),
        database_release: Some(database_release),
        connection_commit: Some(connection_commit),
        connection_get_info: Some(not_implemented_info),
        connection_get_objects: Some(not_implemented_objects),
        connection_get_table_schema: Some(connection_get_table_schema),
        connection_get_table_types: Some(connection_get_table_types),
        connection_init: Some(connection_init),
        connection_new: Some(connection_new),
        connection_set_option: Some(connection_set_option),
        connection_read_partition: Some(not_implemented_read_partition),
        connection_release: Some(connection_release),
        connection_rollback: Some(connection_commit),
        statement_bind: Some(not_implemented_bind),
        statement_bind_stream: Some(not_implemented_bind_stream),
        statement_execute_query: Some(statement_execute_query),
        statement_execute_partitions: Some(not_implemented_partitions),
        statement_get_parameter_schema: Some(not_implemented_parameter_schema),
        statement_new: Some(statement_new),
        statement_prepare: Some(statement_prepare),
        statement_release: Some(statement_release),
        statement_set_option: Some(statement_set_option),
        statement_set_sql_query: Some(statement_set_sql_query),
        statement_set_substrait_plan: Some(not_implemented_substrait),
    };
    // SAFETY: the caller passes writable memory for one ADBC 1.0.0 driver, which this layout is.
    unsafe { raw_driver.cast::<AdbcDriver>().write(filled) };
    ADBC_STATUS_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_runs_through_the_driver_functions() {
        let mut error = AdbcError { message: std::ptr::null_mut(), vendor_code: 0, sqlstate: [0; 5], release: None };
        let mut database = AdbcHandle { private_data: std::ptr::null_mut(), private_driver: std::ptr::null_mut() };
        let mut connection = AdbcHandle { private_data: std::ptr::null_mut(), private_driver: std::ptr::null_mut() };
        let mut statement = AdbcHandle { private_data: std::ptr::null_mut(), private_driver: std::ptr::null_mut() };
        let query = CString::new("SELECT 1 AS one, 'a' AS letter").unwrap();
        let mut stream = ArrowArrayStream { get_schema: None, get_next: None, get_last_error: None, release: None, private_data: std::ptr::null_mut() };
        let mut affected = 0i64;
        let mut statuses = Vec::new();
        // SAFETY: the handle and error are live locals, as ADBC's caller gives.
        statuses.push(unsafe { database_new(&mut database, &mut error) });
        // SAFETY: as above, on the database just set up.
        statuses.push(unsafe { database_init(&mut database, &mut error) });
        // SAFETY: as above.
        statuses.push(unsafe { connection_new(&mut connection, &mut error) });
        // SAFETY: as above, with the initialized database.
        statuses.push(unsafe { connection_init(&mut connection, &mut database, &mut error) });
        // SAFETY: as above, with the initialized connection.
        statuses.push(unsafe { statement_new(&mut connection, &mut statement, &mut error) });
        // SAFETY: as above, with a NUL-terminated query that outlives the call.
        statuses.push(unsafe { statement_set_sql_query(&mut statement, query.as_ptr(), &mut error) });
        // SAFETY: as above, with a writable stream and count.
        statuses.push(unsafe { statement_execute_query(&mut statement, &mut stream, &mut affected, &mut error) });
        assert_eq!(statuses, [ADBC_STATUS_OK; 7]);
        assert_eq!(affected, -1);
        let mut schema = std::mem::MaybeUninit::<ArrowSchema>::zeroed();
        let get_schema = stream.get_schema.unwrap();
        // SAFETY: the stream was just filled by the driver and the schema memory is writable.
        assert_eq!(unsafe { get_schema(&mut stream, schema.as_mut_ptr()) }, 0);
        // SAFETY: `get_schema` succeeded, so the schema is written.
        let mut schema = unsafe { schema.assume_init() };
        assert_eq!(schema.n_children, 2);
        let release_schema = schema.release.unwrap();
        // SAFETY: the schema is released once.
        unsafe { release_schema(&mut schema) };
        let release_stream = stream.release.unwrap();
        // SAFETY: the stream is released once.
        unsafe { release_stream(&mut stream) };
        // SAFETY: the statement was set up above and is released once.
        let statement_released = unsafe { statement_release(&mut statement, &mut error) };
        // SAFETY: the connection was set up above and is released once.
        let connection_released = unsafe { connection_release(&mut connection, &mut error) };
        // SAFETY: the database was set up above and is released once.
        let database_released = unsafe { database_release(&mut database, &mut error) };
        let released = [statement_released, connection_released, database_released];
        assert_eq!(released, [ADBC_STATUS_OK; 3]);
    }
}
