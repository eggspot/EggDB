use crate::args;
use crate::database::EggdbDb;
use crate::status::{Failure, guarded};
use crate::transaction::shared_database;
use eggdb::Database;
use eggdb::sql::{
    CancelSignal, CopyChannel, Dialect, OutputColumn, Parameter, QueryResult, Session, SqlResult,
    Store, Value,
};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `eggdb_value.kind`: SQL's NULL; `data` is NULL.
pub const EGGDB_NULL: i32 = 0;
/// `eggdb_value.kind`: a boolean or an integer, in `integer`.
pub const EGGDB_INTEGER: i32 = 1;
/// `eggdb_value.kind`: a floating-point number, in `real`.
pub const EGGDB_REAL: i32 = 2;
/// `eggdb_value.kind`: any other value, as the dialect's clients print it, in `data`.
pub const EGGDB_TEXT: i32 = 3;
/// `eggdb_value.kind`: binary data (`bytea`, `BLOB`, `varbinary`), its bytes in `data`.
pub const EGGDB_BLOB: i32 = 4;

/// One value of a SQL result (`eggdb_value` in `eggdb.h`). Every value but NULL also has its
/// text, as the dialect's clients print it, in `data` (UTF-8 followed by a NUL byte not counted
/// in `len`); a BLOB's `data` is its bytes instead.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EggdbValue {
    /// `EGGDB_NULL`, `EGGDB_INTEGER`, `EGGDB_REAL`, `EGGDB_TEXT` or `EGGDB_BLOB`.
    pub kind: i32,
    /// The value of an `EGGDB_INTEGER` (1 and 0 for booleans), else 0.
    pub integer: i64,
    /// The value of an `EGGDB_REAL`, else 0.
    pub real: f64,
    /// The text or the bytes; NULL for SQL's NULL.
    pub data: *const u8,
    /// The length of `data` in bytes.
    pub len: usize,
}

/// One parameter of [`eggdb_sql_execute_with_parameters`] (`eggdb_parameter` in `eggdb.h`):
/// the value in text form, as the dialect reads a literal of the placeholder's type (`42`,
/// `2.5`, `2026-10-30`, `\x0001` for PostgreSQL's `bytea`), or SQL's NULL.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EggdbParameter {
    /// The value's UTF-8 text, `len` bytes, not NUL-terminated; NULL for SQL's NULL.
    pub text: *const u8,
    /// The length of `text` in bytes; 0 with NULL.
    pub len: usize,
}

/// The result of one SQL statement (`eggdb_result` in `eggdb.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EggdbResult {
    /// The command tag, such as `"SELECT 2"` or `"INSERT 0 1"`.
    pub tag: *const c_char,
    /// The number of columns; 0 for a statement that returns no rows.
    pub column_count: usize,
    /// The columns' names, `column_count` of them.
    pub column_names: *const *const c_char,
    /// The number of rows.
    pub row_count: usize,
    /// The values, row by row: `row_count × column_count` of them.
    pub values: *const EggdbValue,
}

pub(crate) type SqlSession = Session<'static>;

const OID_BOOL: u32 = 16;
const OID_BYTEA: u32 = 17;
const OID_INT8: u32 = 20;
const OID_INT2: u32 = 21;
const OID_INT4: u32 = 23;
const OID_OID: u32 = 26;
const OID_FLOAT4: u32 = 700;
const OID_FLOAT8: u32 = 701;

#[derive(Debug, Clone, Copy)]
struct PendingValue {
    kind: i32,
    integer: i64,
    real: f64,
    text: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy)]
struct PendingResult {
    tag: usize,
    first_name: usize,
    column_count: usize,
    first_value: usize,
    row_count: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ResultBatch {
    bytes: Vec<u8>,
    name_offsets: Vec<usize>,
    pending_values: Vec<PendingValue>,
    pending_results: Vec<PendingResult>,
    names: Vec<*const c_char>,
    values: Vec<EggdbValue>,
    results: Vec<EggdbResult>,
}

fn kind_and_number(value: &Value) -> (i32, i64, f64) {
    match value {
        Value::Null => (EGGDB_NULL, 0, 0.0),
        Value::Boolean(flag) => (EGGDB_INTEGER, i64::from(*flag), 0.0),
        Value::SmallInt(number) => (EGGDB_INTEGER, i64::from(*number), 0.0),
        Value::Integer(number) => (EGGDB_INTEGER, i64::from(*number), 0.0),
        Value::BigInt(number) => (EGGDB_INTEGER, *number, 0.0),
        Value::Oid(number) => (EGGDB_INTEGER, i64::from(*number), 0.0),
        Value::Real(number) => (EGGDB_REAL, 0, f64::from(*number)),
        Value::Double(number) => (EGGDB_REAL, 0, *number),
        Value::Bytea(_) => (EGGDB_BLOB, 0, 0.0),
        _ => (EGGDB_TEXT, 0, 0.0),
    }
}

impl ResultBatch {
    fn clear(&mut self) {
        self.bytes.clear();
        self.name_offsets.clear();
        self.pending_values.clear();
        self.pending_results.clear();
        self.names.clear();
        self.values.clear();
        self.results.clear();
    }

    fn keep(&mut self, bytes: &[u8]) -> (usize, usize) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        self.bytes.push(0);
        (start, bytes.len())
    }

    fn add_value(&mut self, value: &Value, column: &OutputColumn, dialect: Dialect) {
        let (kind, integer, real) = kind_and_number(value);
        let text = match value {
            Value::Null => None,
            Value::Bytea(bytes) => Some(self.keep(bytes)),
            _ => {
                let printed = dialect
                    .value_text_typed(value, column.sql_type)
                    .unwrap_or_default();
                Some(self.keep(printed.as_bytes()))
            }
        };
        self.pending_values.push(PendingValue {
            kind,
            integer,
            real,
            text,
        });
    }

    fn add_remote_value(&mut self, text: Option<&str>, type_oid: u32) {
        let Some(text) = text else {
            self.pending_values.push(PendingValue {
                kind: EGGDB_NULL,
                integer: 0,
                real: 0.0,
                text: None,
            });
            return;
        };
        let (kind, integer, real, bytes) = match type_oid {
            OID_BOOL => (
                EGGDB_INTEGER,
                i64::from(text == "t"),
                0.0,
                text.as_bytes().to_vec(),
            ),
            OID_INT2 | OID_INT4 | OID_INT8 | OID_OID => match text.parse::<i64>() {
                Ok(number) => (EGGDB_INTEGER, number, 0.0, text.as_bytes().to_vec()),
                Err(_) => (EGGDB_TEXT, 0, 0.0, text.as_bytes().to_vec()),
            },
            OID_FLOAT4 | OID_FLOAT8 => match text.parse::<f64>() {
                Ok(number) => (EGGDB_REAL, 0, number, text.as_bytes().to_vec()),
                Err(_) => (EGGDB_TEXT, 0, 0.0, text.as_bytes().to_vec()),
            },
            OID_BYTEA => match hex_bytes(text) {
                Some(bytes) => (EGGDB_BLOB, 0, 0.0, bytes),
                None => (EGGDB_TEXT, 0, 0.0, text.as_bytes().to_vec()),
            },
            _ => (EGGDB_TEXT, 0, 0.0, text.as_bytes().to_vec()),
        };
        let kept = self.keep(&bytes);
        self.pending_values.push(PendingValue {
            kind,
            integer,
            real,
            text: Some(kept),
        });
    }

    fn add_remote(&mut self, result: &eggdb_pgwire::client::QueryRows) {
        let (tag, _) = self.keep(result.tag.as_bytes());
        let first_name = self.name_offsets.len();
        for column in &result.columns {
            let (name, _) = self.keep(column.as_bytes());
            self.name_offsets.push(name);
        }
        let first_value = self.pending_values.len();
        let column_count = result.columns.len();
        let row_count = if column_count == 0 {
            0
        } else {
            result.rows.len()
        };
        for row in result.rows.iter().take(row_count) {
            for column in 0..column_count {
                let text = row.get(column).and_then(Option::as_deref);
                let type_oid = result.column_types.get(column).copied().unwrap_or(0);
                self.add_remote_value(text, type_oid);
            }
        }
        self.pending_results.push(PendingResult {
            tag,
            first_name,
            column_count,
            first_value,
            row_count,
        });
    }

    fn add(&mut self, result: &QueryResult, dialect: Dialect) {
        let (tag, _) = self.keep(result.tag.as_bytes());
        let columns = result.columns.as_deref().unwrap_or_default();
        let first_name = self.name_offsets.len();
        for column in columns {
            let (name, _) = self.keep(column.name.as_bytes());
            self.name_offsets.push(name);
        }
        let first_value = self.pending_values.len();
        let row_count = if columns.is_empty() {
            0
        } else {
            result.rows.len()
        };
        for row in result.rows.iter().take(row_count) {
            for (index, column) in columns.iter().enumerate() {
                match row.get(index) {
                    Some(value) => self.add_value(value, column, dialect),
                    None => self.add_value(&Value::Null, column, dialect),
                }
            }
        }
        self.pending_results.push(PendingResult {
            tag,
            first_name,
            column_count: columns.len(),
            first_value,
            row_count,
        });
    }

    fn publish(&mut self) -> (*const EggdbResult, usize) {
        let base = self.bytes.as_ptr();
        self.names.extend(
            self.name_offsets
                .iter()
                .map(|&offset| base.wrapping_add(offset).cast::<c_char>()),
        );
        self.values
            .extend(self.pending_values.iter().map(|pending| {
                EggdbValue {
                    kind: pending.kind,
                    integer: pending.integer,
                    real: pending.real,
                    data: pending
                        .text
                        .map_or(std::ptr::null(), |(start, _)| base.wrapping_add(start)),
                    len: pending.text.map_or(0, |(_, len)| len),
                }
            }));
        let names = self.names.as_ptr();
        let values = self.values.as_ptr();
        self.results
            .extend(self.pending_results.iter().map(|pending| EggdbResult {
                tag: base.wrapping_add(pending.tag).cast::<c_char>(),
                column_count: pending.column_count,
                column_names: if pending.column_count == 0 {
                    std::ptr::null()
                } else {
                    names.wrapping_add(pending.first_name)
                },
                row_count: pending.row_count,
                values: if pending.column_count == 0 || pending.row_count == 0 {
                    std::ptr::null()
                } else {
                    values.wrapping_add(pending.first_value)
                },
            }));
        if self.results.is_empty() {
            return (std::ptr::null(), 0);
        }
        (self.results.as_ptr(), self.results.len())
    }
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix("\\x")?;
    if digits.len() % 2 != 0 {
        return None;
    }
    (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok())
        .collect()
}

enum Backend {
    Local {
        session: Box<SqlSession>,
        store: Arc<Store<'static>>,
        database: Arc<Database>,
    },
    Remote(Box<eggdb_pgwire::client::PgClient>),
}

/// A SQL session (`eggdb_sql` in `eggdb.h`): on a database opened in the process, from
/// [`eggdb_sql_open`], or on an EggDB server, from [`eggdb_sql_connect`]; closed by
/// [`eggdb_sql_close`]. Outside a `BEGIN` block each statement is its own transaction; a block
/// holds the database's one write transaction until `COMMIT` or `ROLLBACK`. One call at a time
/// may use it and the statements prepared on it, from any thread; [`eggdb_sql_cancel`] is the
/// one call that may come from another thread meanwhile.
pub struct EggdbSql {
    cancel: Option<CancelSignal>,
    open_statements: AtomicUsize,
    state: UnsafeCell<SessionState>,
}

pub(crate) struct SessionState {
    backend: Backend,
    results: ResultBatch,
}

impl std::fmt::Debug for EggdbSql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EggdbSql")
            .field("cancellable", &self.cancel.is_some())
            .field(
                "open_statements",
                &self.open_statements.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl EggdbSql {
    fn new(backend: Backend) -> Self {
        let cancel = match &backend {
            Backend::Local { session, .. } => Some(session.cancel_signal()),
            Backend::Remote(_) => None,
        };
        Self {
            cancel,
            open_statements: AtomicUsize::new(0),
            state: UnsafeCell::new(SessionState {
                backend,
                results: ResultBatch::default(),
            }),
        }
    }

    pub(crate) fn statement_prepared(&self) {
        self.open_statements.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn statement_finalized(&self) {
        let _ = self
            .open_statements
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                open.checked_sub(1)
            });
    }
}

pub(crate) unsafe fn session_state<'a>(
    sql: *mut EggdbSql,
    what: &str,
) -> Result<&'a mut SessionState, Failure> {
    // SAFETY: eggdb.h asks for a live SQL session handle.
    let sql: &'a EggdbSql = unsafe { args::handle(sql, what) }?;
    // SAFETY: eggdb.h lets one call at a time use a session and its statements, so no other
    // reference to the state is live; eggdb_sql_cancel, the one call allowed alongside, reads
    // only `cancel`, outside the cell.
    Ok(unsafe { &mut *sql.state.get() })
}

fn dialect_named(name: Option<&str>) -> Result<Dialect, Failure> {
    let Some(name) = name else {
        return Ok(Dialect::default());
    };
    Dialect::named(name).ok_or_else(|| {
        let known: Vec<&str> = Dialect::accepted_names().collect();
        Failure::invalid_argument(format!(
            "unknown SQL dialect \"{name}\"; use one of {}",
            known.join(", ")
        ))
    })
}

/// Opens a SQL session on the database in `dialect` (`"postgresql"`, `"mysql"`, `"sqlite"`,
/// `"oracle"` or `"sqlserver"`; NULL: PostgreSQL's) and puts its handle in `*sql` (NULL on
/// failure). Every dialect works on the same tables.
///
/// # Safety
/// `db` is a handle from `eggdb_open`, `dialect` NULL or a NUL-terminated string, and `sql`
/// points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_open(
    db: *mut EggdbDb,
    dialect: *const c_char,
    sql: *mut *mut EggdbSql,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `sql`.
        unsafe { args::write(sql, std::ptr::null_mut(), "sql") }?;
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let db = unsafe { args::handle(db, "db") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated dialect name.
        let dialect = dialect_named(unsafe { args::optional_text(dialect, "dialect") }?)?;
        let (database, database_ref) = shared_database(&db.database);
        let store = Arc::new(database_ref.sql_store());
        // SAFETY: the reference points into the Arc's allocation, which never moves and lives
        // while the handle's `store` clone does; the handle drops `session`, its only user,
        // before `store` (field order).
        let store_ref: &'static Store<'static> = unsafe { &*Arc::as_ptr(&store) };
        let session = store_ref.session(dialect);
        let handle = args::give(EggdbSql::new(Backend::Local {
            session: Box::new(session),
            store,
            database,
        }));
        // SAFETY: as for the first write to `sql`.
        unsafe { args::write(sql, handle, "sql") }
    })
}

pub(crate) const REMOTE_UNSUPPORTED: &str = "a session on a server (eggdb_sql_connect) does not take prepared statements, COPY or cancelling yet; open the database in the process with eggdb_sql_open for them, or use eggdb_sql_execute_with_parameters";

impl SessionState {
    fn run(
        &mut self,
        text: &str,
        parameters: Option<&[Parameter]>,
        copy: Option<&mut dyn CopyChannel>,
    ) -> Result<(*const EggdbResult, usize), Failure> {
        run_on(&mut self.backend, &mut self.results, text, parameters, copy)
    }

    pub(crate) fn local_session(&mut self) -> Result<&mut SqlSession, Failure> {
        match &mut self.backend {
            Backend::Local { session, .. } => Ok(session.as_mut()),
            Backend::Remote(_) => Err(Failure::invalid_argument(REMOTE_UNSUPPORTED)),
        }
    }

    pub(crate) fn run_into(
        &mut self,
        batch: &mut ResultBatch,
        text: &str,
        parameters: &[Parameter],
    ) -> Result<(*const EggdbResult, usize), Failure> {
        run_on(&mut self.backend, batch, text, Some(parameters), None)
    }
}

fn run_on(
    backend: &mut Backend,
    batch: &mut ResultBatch,
    text: &str,
    parameters: Option<&[Parameter]>,
    copy: Option<&mut dyn CopyChannel>,
) -> Result<(*const EggdbResult, usize), Failure> {
    batch.clear();
    match backend {
        Backend::Local { session, .. } => {
            session.cancel_signal().clear();
            let mut gathered: Vec<QueryResult> = Vec::new();
            let mut on_result = |result: QueryResult| {
                gathered.push(result);
                true
            };
            let ran: SqlResult<()> = match (parameters, copy) {
                (Some(values), Some(copy)) => {
                    session.execute_prepared_with_copy(text, values, &mut on_result, copy)
                }
                (Some(values), None) => session.execute_prepared(text, values, &mut on_result),
                (None, Some(copy)) => session.execute_with_copy(text, &mut on_result, copy),
                (None, None) => session.execute_while(text, &mut on_result),
            };
            ran.map_err(|error| Failure::sql(&error))?;
            let dialect = session.dialect();
            for result in &gathered {
                batch.add(result, dialect);
            }
        }
        Backend::Remote(client) => {
            if copy.is_some() {
                return Err(Failure::invalid_argument(REMOTE_UNSUPPORTED));
            }
            let answered = match parameters {
                Some(values) => {
                    let bound: Vec<Option<&[u8]>> = values
                        .iter()
                        .map(|value| value.value.as_deref().map(str::as_bytes))
                        .collect();
                    client.execute_with_parameters(text, &bound)
                }
                None => client.simple_query(text),
            };
            let gathered = answered.map_err(remote_failure)?;
            for result in &gathered {
                batch.add_remote(result);
            }
        }
    }
    Ok(batch.publish())
}

unsafe fn parameters(
    given: *const EggdbParameter,
    count: usize,
) -> Result<Vec<Parameter>, Failure> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if given.is_null() {
        return Err(Failure::invalid_argument(format!(
            "parameters is NULL but parameter_count is {count}; pass an array of {count} eggdb_parameter, or a count of 0"
        )));
    }
    if count > isize::MAX as usize / std::mem::size_of::<EggdbParameter>() {
        return Err(Failure::invalid_argument(format!(
            "parameter_count is {count}, more than any array can hold"
        )));
    }
    // SAFETY: eggdb.h asks for `count` initialized eggdb_parameter at a non-NULL, aligned
    // `given` that stay unchanged during the call; the array's size fits an isize.
    let given = unsafe { std::slice::from_raw_parts(given, count) };
    let mut converted = Vec::with_capacity(count);
    for (index, parameter) in given.iter().enumerate() {
        let what = format!("parameter {}", index + 1);
        // SAFETY: eggdb.h asks for NULL or `len` readable bytes at each parameter's `text`.
        let bytes = unsafe { args::optional_bytes(parameter.text, parameter.len, &what) }?;
        let value = match bytes {
            None => None,
            Some(bytes) => Some(
                std::str::from_utf8(bytes)
                    .map_err(|_| {
                        Failure::invalid_argument(format!(
                            "{what} is not UTF-8; parameters are text in UTF-8"
                        ))
                    })?
                    .to_owned(),
            ),
        };
        converted.push(Parameter {
            declared_type: None,
            value,
        });
    }
    Ok(converted)
}

/// Runs every statement in `text` (UTF-8, separated by semicolons) in order and puts their
/// results in `*results`, one per statement, and their number in `*count`. The results belong
/// to the session and stay valid until its next `eggdb_sql_execute`,
/// `eggdb_sql_execute_with_parameters`, `eggdb_sql_copy_in` or `eggdb_sql_close`. It stops at the first statement
/// that fails, with `EGGDB_ERR_SQL`, its message from `eggdb_last_error` and its SQLSTATE from
/// `eggdb_last_sqlstate`; the statements before it keep their effects, committed outside a
/// block and pending inside one, and no results are given.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` used by this call alone, `text` a NUL-terminated
/// string, and `results` and `count` point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_execute(
    sql: *mut EggdbSql,
    text: *const c_char,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        let (published, len) = state.run(text, None, None)?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

/// Runs the statements in `text` as `eggdb_sql_execute` does, with `parameter_count` values for
/// their placeholders (`$1` in PostgreSQL's and Oracle's dialects, `?` or `?1` in SQLite's and
/// MySQL's) given as text, so values never need quoting into the SQL. A parameter's type is
/// the one its place in the statement asks for.
///
/// # Safety
/// As for `eggdb_sql_execute`; `parameters` is NULL with a count of 0 or points to
/// `parameter_count` `eggdb_parameter`, each NULL or pointing to its `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_execute_with_parameters(
    sql: *mut EggdbSql,
    text: *const c_char,
    parameters: *const EggdbParameter,
    parameter_count: usize,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        // SAFETY: eggdb.h asks for `parameter_count` parameters at `parameters`.
        let values = unsafe { self::parameters(parameters, parameter_count) }?;
        let (published, len) = state.run(text, Some(&values), None)?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

fn remote_failure(error: eggdb_pgwire::client::ClientError) -> Failure {
    match error {
        eggdb_pgwire::client::ClientError::Server { code, message } => {
            Failure::remote_sql(&code, &message)
        }
        other => Failure::new(crate::status::EGGDB_ERR_IO, other.to_string()),
    }
}

/// Connects to an EggDB server (or any server speaking PostgreSQL's protocol) at `url`, a
/// `postgresql://user:password@host:port/database?sslmode=...` URL, and puts a SQL session on
/// it in `*sql` (NULL on failure). The session works as one from [`eggdb_sql_open`]:
/// `eggdb_sql_execute` sends the simple query protocol, `eggdb_sql_execute_with_parameters`
/// the extended one with the parameters bound as text (`$1`, `$2` placeholders), and values
/// come back typed from the columns' types.
///
/// # Safety
/// `url` is a NUL-terminated string and `sql` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_connect(url: *const c_char, sql: *mut *mut EggdbSql) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `sql`.
        unsafe { args::write(sql, std::ptr::null_mut(), "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated URL.
        let url = unsafe { args::text(url, "url") }?;
        let options = eggdb_pgwire::client::parse_postgres_url_with_password(url)
            .map_err(Failure::invalid_argument)?;
        let client = eggdb_pgwire::client::PgClient::connect(&options).map_err(remote_failure)?;
        let handle = args::give(EggdbSql::new(Backend::Remote(Box::new(client))));
        // SAFETY: as for the first write to `sql`.
        unsafe { args::write(sql, handle, "sql") }
    })
}

/// Ends the SQL session and frees its handle: an open `BEGIN` block is rolled back. NULL does
/// nothing. While statements prepared on it are not finalized it fails with `EGGDB_ERR_IN_USE`
/// and leaves the session open.
///
/// # Safety
/// `sql` is NULL or a handle from `eggdb_sql_open` not closed yet; unless the status is
/// `EGGDB_ERR_IN_USE`, it and its results must not be used afterwards, whatever the status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_close(sql: *mut EggdbSql) -> c_int {
    guarded(|| {
        if sql.is_null() {
            return Ok(());
        }
        // SAFETY: eggdb.h asks for a live SQL session handle.
        let open = unsafe { args::handle(sql, "sql") }?
            .open_statements
            .load(Ordering::Acquire);
        if open > 0 {
            return Err(Failure::new(
                crate::status::EGGDB_ERR_IN_USE,
                format!(
                    "{open} prepared statement(s) still use this session; finalize them with eggdb_stmt_finalize, then close it"
                ),
            ));
        }
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(sql) }) else {
            return Ok(());
        };
        let EggdbSql { state, .. } = *handle;
        let SessionState { backend, results } = state.into_inner();
        drop(results);
        match backend {
            Backend::Local {
                session,
                store,
                database,
            } => {
                let closed = session.close();
                drop(store);
                drop(database);
                closed.map(|_| ()).map_err(|error| Failure::sql(&error))
            }
            Backend::Remote(client) => {
                client.terminate();
                Ok(())
            }
        }
    })
}

/// Cancels the statement the session is running, as PostgreSQL's cancel request does: it stops
/// at its next row (within ten milliseconds in `pg_sleep`) and its call returns `EGGDB_ERR_SQL`
/// with SQLSTATE `57014`, rolled back as any failing statement is. It may be called from any
/// thread while another thread runs `eggdb_sql_execute`, `eggdb_sql_execute_with_parameters`,
/// `eggdb_sql_copy_in` or `eggdb_stmt_execute` on the session. A cancel that comes while no
/// statement runs does nothing: the session's next call starts afresh. A session from
/// `eggdb_sql_connect` fails with `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `sql` is a handle from `eggdb_sql_open` that stays open during the call (closing it on
/// another thread at the same time is not allowed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_cancel(sql: *mut EggdbSql) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live SQL session handle; this reads only `cancel`, which
        // no other call changes, so it may run beside the call using the session's state.
        let sql = unsafe { args::handle(sql, "sql") }?;
        let signal = sql
            .cancel
            .as_ref()
            .ok_or_else(|| Failure::invalid_argument(REMOTE_UNSUPPORTED))?;
        signal.cancel();
        Ok(())
    })
}

const COPY_PIECE_BYTES: usize = 64 * 1024;

struct CopyInData<'a> {
    remaining: &'a [u8],
    started: bool,
}

fn copy_out_unsupported() -> eggdb::sql::SqlError {
    eggdb::sql::SqlError::new(
        eggdb::sql::SqlState::FEATURE_NOT_SUPPORTED,
        "COPY ... TO STDOUT is not available through eggdb_sql_copy_in; read the rows with a SELECT",
    )
}

impl CopyChannel for CopyInData<'_> {
    fn start_copy_in(&mut self, _start: &eggdb::sql::CopyStart) -> SqlResult<()> {
        if self.started {
            return Err(eggdb::sql::SqlError::new(
                eggdb::sql::SqlState::FEATURE_NOT_SUPPORTED,
                "eggdb_sql_copy_in gives its data to one COPY ... FROM STDIN; run each COPY in a call of its own",
            ));
        }
        self.started = true;
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if self.remaining.is_empty() {
            return Ok(false);
        }
        let (piece, rest) = self
            .remaining
            .split_at(self.remaining.len().min(COPY_PIECE_BYTES));
        data.extend_from_slice(piece);
        self.remaining = rest;
        Ok(true)
    }

    fn start_copy_out(&mut self, _start: &eggdb::sql::CopyStart) -> SqlResult<()> {
        Err(copy_out_unsupported())
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(copy_out_unsupported())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

/// Runs the statements in `text` as `eggdb_sql_execute` does, with `len` bytes at `data` as
/// the data of the `COPY ... FROM STDIN` among them: text, CSV or binary, as the `COPY`'s
/// options say. The `COPY` loads all its rows or none, and its result's tag is `"COPY <rows>"`.
/// Load a large file in pieces of whole rows, one call each, inside a `BEGIN` block when they
/// must commit together. A second `COPY ... FROM STDIN` in `text`, and any `COPY ... TO
/// STDOUT`, fail with `EGGDB_ERR_SQL`; a session from `eggdb_sql_connect` fails with
/// `EGGDB_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// As for `eggdb_sql_execute`; `data` is NULL with a length of 0 or points to `len` readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_copy_in(
    sql: *mut EggdbSql,
    text: *const c_char,
    data: *const u8,
    len: usize,
    results: *mut *const EggdbResult,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `results`.
        unsafe { args::write(results, std::ptr::null(), "results") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live SQL session handle used by this call alone.
        let state = unsafe { session_state(sql, "sql") }?;
        state.local_session()?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        // SAFETY: eggdb.h asks for NULL with 0 or `len` readable bytes at `data`.
        let data = unsafe { args::bytes(data, len, "data") }?;
        let mut channel = CopyInData {
            remaining: data,
            started: false,
        };
        let copy: &mut dyn CopyChannel = &mut channel;
        let (published, len) = state.run(text, None, Some(copy))?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        EGGDB_BLOB, EGGDB_INTEGER, EGGDB_NULL, EGGDB_REAL, EGGDB_TEXT, EggdbParameter,
        dialect_named, kind_and_number, parameters,
    };
    use eggdb::sql::{Dialect, Value};

    fn parameter(text: Option<&[u8]>) -> EggdbParameter {
        EggdbParameter {
            text: text.map_or(std::ptr::null(), <[u8]>::as_ptr),
            len: text.map_or(0, <[u8]>::len),
        }
    }

    #[test]
    fn parameters_become_text_values_or_null() {
        let given = [
            parameter(Some(b"42")),
            parameter(None),
            parameter(Some(b"")),
        ];
        // SAFETY: three parameters, each NULL or pointing to its bytes.
        let converted = unsafe { parameters(given.as_ptr(), given.len()) }
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        let values: Vec<Option<&str>> = converted
            .iter()
            .map(|parameter| parameter.value.as_deref())
            .collect();
        assert_eq!(values, [Some("42"), None, Some("")]);
        assert!(
            converted
                .iter()
                .all(|parameter| parameter.declared_type.is_none())
        );
    }

    #[test]
    fn bad_parameters_are_refused() {
        // SAFETY: a count of 0 reads nothing.
        assert!(unsafe { parameters(std::ptr::null(), 0) }.is_ok_and(|none| none.is_empty()));
        // SAFETY: NULL with a count is refused before anything is read.
        assert!(unsafe { parameters(std::ptr::null(), 2) }.is_err());
        let not_utf8 = [parameter(Some(b"\xff"))];
        // SAFETY: one parameter pointing to its byte.
        assert!(unsafe { parameters(not_utf8.as_ptr(), 1) }.is_err());
        let null_with_length = [EggdbParameter {
            text: std::ptr::null(),
            len: 3,
        }];
        // SAFETY: a NULL text with a length is refused before it is read.
        assert!(unsafe { parameters(null_with_length.as_ptr(), 1) }.is_err());
    }

    #[test]
    fn values_get_the_kind_their_type_reads_as() {
        assert_eq!(kind_and_number(&Value::Null).0, EGGDB_NULL);
        assert_eq!(kind_and_number(&Value::Boolean(true)).1, 1);
        assert_eq!(
            kind_and_number(&Value::BigInt(-7)),
            (EGGDB_INTEGER, -7, 0.0)
        );
        assert_eq!(kind_and_number(&Value::Double(2.5)), (EGGDB_REAL, 0, 2.5));
        assert_eq!(kind_and_number(&Value::Text("x".into())).0, EGGDB_TEXT);
        assert_eq!(kind_and_number(&Value::Bytea(vec![1])).0, EGGDB_BLOB);
    }

    #[test]
    fn dialects_are_found_by_any_of_their_names() {
        assert_eq!(dialect_named(None).ok(), Some(Dialect::PostgreSql));
        assert_eq!(dialect_named(Some("SQLite")).ok(), Some(Dialect::Sqlite));
        assert_eq!(dialect_named(Some("tsql")).ok(), Some(Dialect::SqlServer));
        assert!(dialect_named(Some("cobol")).is_err());
    }
}
