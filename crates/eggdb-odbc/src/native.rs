use eggdb_capi::{
    EggdbDb, EggdbOptions, EggdbParameter, EggdbResult, EggdbSql, EggdbValue, eggdb_close,
    eggdb_last_error, eggdb_last_sqlstate, eggdb_open, eggdb_sql_close,
    eggdb_sql_execute_with_parameters, eggdb_sql_open,
};
use std::ffi::{CStr, CString, c_char};
use std::ptr;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Outcome {
    pub(crate) tag: String,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) sqlstate: String,
    pub(crate) message: String,
    pub(crate) native: i32,
}

impl Failure {
    pub(crate) fn new(sqlstate: &str, message: impl Into<String>) -> Self {
        Self {
            sqlstate: sqlstate.to_owned(),
            message: message.into(),
            native: 0,
        }
    }
}

fn text_at(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: the C API's texts are NUL-terminated and stay valid until the next failing call
    // on this thread, which cannot happen while this copy is made.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

fn last_failure(status: i32) -> Failure {
    let state = text_at(eggdb_last_sqlstate());
    let message = text_at(eggdb_last_error());
    Failure {
        sqlstate: if state.is_empty() { "HY000".to_owned() } else { state },
        message: if message.is_empty() {
            format!("EggDB status {status}")
        } else {
            message
        },
        native: status,
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    database: *mut EggdbDb,
    sql: *mut EggdbSql,
    catalog: *mut EggdbSql,
}

// SAFETY: the C API's handles may move between threads; an ODBC connection handle is used by
// one thread at a time, as ODBC requires of applications that share it.
unsafe impl Send for Session {}

impl Session {
    pub(crate) fn open(path: &str, dialect: &str, read_only: bool) -> Result<Self, Failure> {
        let path = CString::new(path).map_err(|_| Failure::new("HY090", "the database path holds a NUL character"))?;
        let dialect = CString::new(dialect).map_err(|_| Failure::new("HY090", "the dialect holds a NUL character"))?;
        let options = EggdbOptions {
            flags: u32::from(read_only),
            durability: 0,
            cache_bytes: 0,
        };
        let mut database: *mut EggdbDb = ptr::null_mut();
        // SAFETY: `path` is NUL-terminated, `options` and `database` are live for the call.
        let opened = unsafe { eggdb_open(path.as_ptr(), &raw const options, &raw mut database) };
        if opened != 0 {
            return Err(last_failure(opened));
        }
        let mut sql: *mut EggdbSql = ptr::null_mut();
        // SAFETY: `database` is the handle just opened, `dialect` is NUL-terminated and `sql` a
        // live out-pointer.
        let started = unsafe { eggdb_sql_open(database, dialect.as_ptr(), &raw mut sql) };
        if started != 0 {
            let failure = last_failure(started);
            // SAFETY: `database` was opened above and nothing else uses it.
            unsafe { eggdb_close(database) };
            return Err(failure);
        }
        Ok(Self {
            database,
            sql,
            catalog: ptr::null_mut(),
        })
    }

    fn catalog_session(&mut self) -> Result<*mut EggdbSql, Failure> {
        if !self.catalog.is_null() {
            return Ok(self.catalog);
        }
        let mut catalog: *mut EggdbSql = ptr::null_mut();
        // SAFETY: `database` is this session's open database, the dialect a NUL-terminated
        // literal, and `catalog` a live out-pointer.
        let started = unsafe { eggdb_sql_open(self.database, c"postgresql".as_ptr(), &raw mut catalog) };
        if started != 0 {
            return Err(last_failure(started));
        }
        self.catalog = catalog;
        Ok(catalog)
    }

    pub(crate) fn run_catalog(&mut self, text: &str, parameters: &[Option<String>]) -> Result<Vec<Outcome>, Failure> {
        let catalog = self.catalog_session()?;
        execute_on(catalog, text, parameters)
    }

    pub(crate) fn run(&mut self, text: &str, parameters: &[Option<String>]) -> Result<Vec<Outcome>, Failure> {
        execute_on(self.sql, text, parameters)
    }

    pub(crate) fn close(self) {
        if !self.catalog.is_null() {
            // SAFETY: the catalog session is this connection's, given up here.
            unsafe { eggdb_sql_close(self.catalog) };
        }
        // SAFETY: `sql` is this session's handle, given up here; nothing uses it afterwards.
        unsafe { eggdb_sql_close(self.sql) };
        // SAFETY: as above, for the database handle the session ran on.
        unsafe { eggdb_close(self.database) };
    }
}

fn execute_on(sql: *mut EggdbSql, text: &str, parameters: &[Option<String>]) -> Result<Vec<Outcome>, Failure> {
    let text = CString::new(text).map_err(|_| Failure::new("HY090", "the statement holds a NUL character"))?;
    let bound: Vec<EggdbParameter> = parameters
        .iter()
        .map(|value| match value {
            Some(text) => EggdbParameter {
                text: text.as_ptr(),
                len: text.len(),
            },
            None => EggdbParameter {
                text: ptr::null(),
                len: 0,
            },
        })
        .collect();
    let mut results: *const EggdbResult = ptr::null();
    let mut count = 0usize;
    let first = if bound.is_empty() { ptr::null() } else { bound.as_ptr() };
    // SAFETY: `sql` is a live session handle of this connection, `text` is NUL-terminated, `bound`
    // holds `bound.len()` parameters whose texts outlive the call, and the out-pointers are
    // live.
    let status = unsafe {
        eggdb_sql_execute_with_parameters(sql, text.as_ptr(), first, bound.len(), &raw mut results, &raw mut count)
    };
    if status != 0 {
        return Err(last_failure(status));
    }
    if results.is_null() || count == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the call succeeded, so `results` points to `count` results that stay valid until
    // the next call on this session, which cannot happen during this copy.
    let results = unsafe { std::slice::from_raw_parts(results, count) };
    Ok(results.iter().map(outcome_of).collect())
}

fn bytes_at(data: *const u8, len: usize) -> Vec<u8> {
    if data.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: a value's `data` points to `len` bytes the result batch owns while it is read.
    unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
}

fn cell_of(value: &EggdbValue) -> Cell {
    match value.kind {
        1 => Cell::Integer(value.integer),
        2 => Cell::Real(value.real),
        3 => Cell::Text(String::from_utf8_lossy(&bytes_at(value.data, value.len)).into_owned()),
        4 => Cell::Blob(bytes_at(value.data, value.len)),
        _ => Cell::Null,
    }
}

fn outcome_of(result: &EggdbResult) -> Outcome {
    let columns: Vec<String> = (0..result.column_count)
        .map(|column| {
            // SAFETY: `column_names` holds `column_count` name pointers in a live result.
            let name = unsafe { *result.column_names.add(column) };
            text_at(name)
        })
        .collect();
    let values: &[EggdbValue] = if result.values.is_null() || result.column_count == 0 {
        &[]
    } else {
        // SAFETY: a live result's `values` holds `row_count * column_count` values.
        unsafe { std::slice::from_raw_parts(result.values, result.row_count * result.column_count) }
    };
    let rows = values
        .chunks(result.column_count.max(1))
        .map(|row| row.iter().map(cell_of).collect())
        .collect();
    Outcome {
        tag: text_at(result.tag),
        columns,
        rows,
    }
}
