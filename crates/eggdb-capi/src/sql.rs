use crate::args;
use crate::database::EggdbDb;
use crate::status::{Failure, guarded};
use crate::transaction::shared_database;
use eggdb::Database;
use eggdb::sql::{Dialect, OutputColumn, Parameter, QueryResult, Session, SqlResult, Store, Value};
use std::ffi::{c_char, c_int};
use std::sync::Arc;

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

type SqlSession = Session<'static, Store<'static>>;

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
struct ResultBatch {
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
            .extend(self.pending_values.iter().map(|pending| EggdbValue {
                kind: pending.kind,
                integer: pending.integer,
                real: pending.real,
                data: pending
                    .text
                    .map_or(std::ptr::null(), |(start, _)| base.wrapping_add(start)),
                len: pending.text.map_or(0, |(_, len)| len),
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

/// A SQL session on a database (`eggdb_sql` in `eggdb.h`), from [`eggdb_sql_open`], closed by
/// [`eggdb_sql_close`]. Outside a `BEGIN` block each statement is its own transaction; a block
/// holds the database's one write transaction until `COMMIT` or `ROLLBACK`. One call at a time
/// may use it, from any thread.
pub struct EggdbSql {
    session: SqlSession,
    results: ResultBatch,
    store: Arc<Store<'static>>,
    database: Arc<Database>,
}

impl std::fmt::Debug for EggdbSql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EggdbSql")
            .field("dialect", &self.session.dialect())
            .field("database", &self.database.path())
            .finish_non_exhaustive()
    }
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
        let handle = args::give(EggdbSql {
            session,
            results: ResultBatch::default(),
            store,
            database,
        });
        // SAFETY: as for the first write to `sql`.
        unsafe { args::write(sql, handle, "sql") }
    })
}

impl EggdbSql {
    fn run(
        &mut self,
        run: impl FnOnce(&mut SqlSession, &mut Vec<QueryResult>) -> SqlResult<()>,
    ) -> Result<(*const EggdbResult, usize), Failure> {
        self.results.clear();
        let mut gathered = Vec::new();
        run(&mut self.session, &mut gathered).map_err(|error| Failure::sql(&error))?;
        let dialect = self.session.dialect();
        for result in &gathered {
            self.results.add(result, dialect);
        }
        Ok(self.results.publish())
    }
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
/// `eggdb_sql_execute_with_parameters` or `eggdb_sql_close`. It stops at the first statement
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
        let sql = unsafe { args::handle_mut(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        let (published, len) = sql.run(|session, gathered| {
            session.execute(text, &mut |result| gathered.push(result))
        })?;
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
        let sql = unsafe { args::handle_mut(sql, "sql") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated statement text.
        let text = unsafe { args::text(text, "text") }?;
        // SAFETY: eggdb.h asks for `parameter_count` parameters at `parameters`.
        let values = unsafe { self::parameters(parameters, parameter_count) }?;
        let (published, len) = sql.run(|session, gathered| {
            session.execute_prepared(text, &values, &mut |result| {
                gathered.push(result);
                true
            })
        })?;
        // SAFETY: as for the first write to `results`.
        unsafe { args::write(results, published, "results") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

/// Ends the SQL session and frees its handle: an open `BEGIN` block is rolled back. NULL does
/// nothing.
///
/// # Safety
/// `sql` is NULL or a handle from `eggdb_sql_open` not closed yet; it and its results must not
/// be used afterwards, whatever the status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_sql_close(sql: *mut EggdbSql) -> c_int {
    guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(sql) }) else {
            return Ok(());
        };
        let EggdbSql {
            session,
            results,
            store,
            database,
        } = *handle;
        let closed = session.close();
        drop(results);
        drop(store);
        drop(database);
        closed.map(|_| ()).map_err(|error| Failure::sql(&error))
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
        let given = [parameter(Some(b"42")), parameter(None), parameter(Some(b""))];
        // SAFETY: three parameters, each NULL or pointing to its bytes.
        let converted = unsafe { parameters(given.as_ptr(), given.len()) }
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        let values: Vec<Option<&str>> = converted
            .iter()
            .map(|parameter| parameter.value.as_deref())
            .collect();
        assert_eq!(values, [Some("42"), None, Some("")]);
        assert!(converted.iter().all(|parameter| parameter.declared_type.is_none()));
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
        assert_eq!(kind_and_number(&Value::BigInt(-7)), (EGGDB_INTEGER, -7, 0.0));
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
