//! EggDB for WebAssembly hosts (D221): built for `wasm32-wasip1`, it runs in a browser through
//! the small WASI loader in `web/eggdb.js`, or in any WASI runtime. The host writes its input
//! (a path, or SQL text) into a buffer the module owns, calls one function, and reads the JSON
//! answer from another buffer the module owns, so no pointer the host made is trusted.
//!
//! The functions: [`eggdb_wasm_input`] sizes the input buffer and returns where it starts;
//! [`eggdb_wasm_open`] opens (or creates) the database at the path in the input, `:memory:` for
//! one in memory, in a dialect (0 PostgreSQL, 1 SQLite, 2 MySQL, 3 SQL Server, 4 Oracle) and
//! returns a handle, or 0; [`eggdb_wasm_execute`] runs the SQL in the input on a handle;
//! [`eggdb_wasm_output`] and [`eggdb_wasm_output_len`] locate the last answer;
//! [`eggdb_wasm_close`] ends a handle; [`eggdb_wasm_version`] answers the version.

use eggdb::Database;
use eggdb::sql::Store;
use eggdb_sql::{Dialect, QueryResult, Session};
use std::cell::RefCell;

type OpenSession = Session<'static, Store<'static>>;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SESSIONS: RefCell<Vec<Option<OpenSession>>> = const { RefCell::new(Vec::new()) };
}

const DIALECTS: [Dialect; 5] = [Dialect::PostgreSql, Dialect::Sqlite, Dialect::MySql, Dialect::SqlServer, Dialect::Oracle];

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn answer(json: String) -> u32 {
    let length = u32::try_from(json.len()).unwrap_or(u32::MAX);
    OUTPUT.with(|output| *output.borrow_mut() = json.into_bytes());
    length
}

fn input_text() -> Result<String, String> {
    INPUT.with(|input| String::from_utf8(input.borrow().clone()).map_err(|_| "the input is not UTF-8".to_owned()))
}

fn result_json(result: &QueryResult) -> String {
    let columns = match &result.columns {
        Some(columns) => format!("[{}]", columns.iter().map(|column| json_string(&column.name)).collect::<Vec<_>>().join(",")),
        None => "null".to_owned(),
    };
    let rows: Vec<String> = result
        .rows
        .iter()
        .map(|row| format!("[{}]", row.iter().map(|value| value.to_text().map_or_else(|| "null".to_owned(), |text| json_string(&text))).collect::<Vec<_>>().join(",")))
        .collect();
    format!("{{\"tag\":{},\"columns\":{columns},\"rows\":[{}]}}", json_string(&result.tag), rows.join(","))
}

/// Makes the input buffer `length` bytes long and returns where it starts, for the host to fill.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_input(length: u32) -> *mut u8 {
    INPUT.with(|input| {
        let mut input = input.borrow_mut();
        input.clear();
        input.resize(usize::try_from(length).unwrap_or(0), 0);
        input.as_mut_ptr()
    })
}

/// Where the last answer starts.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_output() -> *const u8 {
    OUTPUT.with(|output| output.borrow().as_ptr())
}

/// How many bytes the last answer has.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_output_len() -> u32 {
    OUTPUT.with(|output| u32::try_from(output.borrow().len()).unwrap_or(u32::MAX))
}

/// Answers `{"version":"..."}` and returns the answer's length.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_version() -> u32 {
    answer(format!("{{\"version\":{}}}", json_string(env!("CARGO_PKG_VERSION"))))
}

fn open(path: &str, dialect: u32) -> Result<OpenSession, String> {
    let dialect = *DIALECTS.get(usize::try_from(dialect).unwrap_or(usize::MAX)).ok_or_else(|| format!("dialect {dialect} is not one of 0 to 4"))?;
    let database = if path == ":memory:" { Database::in_memory() } else { Database::open_with(path, eggdb::Options::default().create_if_missing(true)) };
    let database: &'static Database = Box::leak(Box::new(database.map_err(|error| error.to_string())?));
    let store: &'static Store<'static> = Box::leak(Box::new(database.sql_store()));
    Ok(store.session(dialect))
}

/// Opens the database at the path in the input in `dialect` and returns its handle, or 0 with
/// `{"error":"..."}` as the answer.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_open(dialect: u32) -> u32 {
    match input_text().and_then(|path| open(&path, dialect)) {
        Ok(session) => {
            let handle = SESSIONS.with(|sessions| {
                let mut sessions = sessions.borrow_mut();
                sessions.push(Some(session));
                sessions.len()
            });
            answer("{}".to_owned());
            u32::try_from(handle).unwrap_or(0)
        }
        Err(message) => {
            answer(format!("{{\"error\":{}}}", json_string(&message)));
            0
        }
    }
}

/// Runs the SQL in the input on `handle` and returns the length of the answer: `{"results":
/// [{"tag","columns","rows"}]}`, with `"error"` beside the results of the statements that ran
/// when one fails.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_execute(handle: u32) -> u32 {
    let sql = match input_text() {
        Ok(sql) => sql,
        Err(message) => return answer(format!("{{\"results\":[],\"error\":{}}}", json_string(&message))),
    };
    let index = usize::try_from(handle).unwrap_or(0).wrapping_sub(1);
    let json = SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        let Some(Some(session)) = sessions.get_mut(index) else {
            return format!("{{\"results\":[],\"error\":{}}}", json_string("the database handle is not open"));
        };
        let mut results = Vec::new();
        let outcome = session.execute(&sql, &mut |result| results.push(result_json(&result)));
        match outcome {
            Ok(()) => format!("{{\"results\":[{}]}}", results.join(",")),
            Err(error) => format!("{{\"results\":[{}],\"error\":{}}}", results.join(","), json_string(&format!("ERROR:  {}", error.message()))),
        }
    });
    answer(json)
}

/// Ends `handle`'s session; the database stays open for the module's lifetime.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_close(handle: u32) {
    let index = usize::try_from(handle).unwrap_or(0).wrapping_sub(1);
    SESSIONS.with(|sessions| {
        if let Some(slot) = sessions.borrow_mut().get_mut(index) {
            *slot = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_input(text: &str) {
        let start = eggdb_wasm_input(u32::try_from(text.len()).unwrap());
        INPUT.with(|input| {
            assert_eq!(input.borrow().as_ptr(), start.cast_const());
            input.borrow_mut().copy_from_slice(text.as_bytes());
        });
    }

    fn output() -> String {
        let length = eggdb_wasm_output_len() as usize;
        OUTPUT.with(|output| {
            assert_eq!(output.borrow().as_ptr(), eggdb_wasm_output());
            String::from_utf8(output.borrow()[..length].to_vec()).unwrap()
        })
    }

    #[test]
    fn a_host_opens_a_database_and_runs_sql_through_the_buffers() {
        write_input(":memory:");
        let handle = eggdb_wasm_open(0);
        assert_ne!(handle, 0, "{}", output());
        write_input("CREATE TABLE t (id integer PRIMARY KEY, name text); INSERT INTO t VALUES (1, 'a\"b'), (2, NULL); SELECT * FROM t ORDER BY id");
        eggdb_wasm_execute(handle);
        let answer = output();
        assert!(answer.contains("\"tag\":\"CREATE TABLE\""), "{answer}");
        assert!(answer.contains("\"columns\":[\"id\",\"name\"],\"rows\":[[\"1\",\"a\\\"b\"],[\"2\",null]]"), "{answer}");
        write_input("SELECT * FROM missing");
        eggdb_wasm_execute(handle);
        assert!(output().contains("\"error\":\"ERROR:  "));
        eggdb_wasm_close(handle);
        write_input("SELECT 1");
        eggdb_wasm_execute(handle);
        assert!(output().contains("not open"));
        write_input(":memory:");
        assert_eq!(eggdb_wasm_open(9), 0);
        eggdb_wasm_version();
        assert!(output().starts_with("{\"version\":\""));
    }
}
