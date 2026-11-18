use eggdb_odbc::{
    SQLAllocHandle, SQLBindParameter, SQLDescribeCol, SQLDisconnect, SQLDriverConnect,
    SQLEndTran, SQLExecDirect, SQLExecute, SQLFetch, SQLFreeHandle, SQLGetData, SQLGetDiagRec,
    SQLColumns, SQLForeignKeys, SQLGetInfo, SQLGetTypeInfo, SQLNumResultCols, SQLProcedureColumns, SQLPrepare, SQLPrimaryKeys,
    SQLGetInfoW, SQLNativeSql, SQLNativeSqlW, SQLRowCount, SQLGetFunctions, SQLMoreResults, SQLGetDiagField, SQLBindCol, SQLSetStmtAttr, SQLSetConnectAttr, SQLSpecialColumns, SQLStatistics, SQLTablePrivileges, SQLTables,
};
use std::ffi::c_void;
use std::ptr;

const SQL_HANDLE_ENV: i16 = 1;
const SQL_HANDLE_DBC: i16 = 2;
const SQL_HANDLE_STMT: i16 = 3;
const SQL_NTS: i32 = -3;
const SQL_C_CHAR: i16 = 1;
const SQL_C_SLONG: i16 = -16;
const SQL_PARAM_INPUT: i16 = 1;
const SQL_VARCHAR: i16 = 12;
const SQL_INTEGER: i16 = 4;
const SQL_NO_DATA: i16 = 100;
const SQL_ERROR: i16 = -1;
const SQL_ATTR_AUTOCOMMIT: i32 = 102;
const SQL_ROLLBACK: i16 = 1;

fn handle(kind: i16, parent: *mut c_void) -> *mut c_void {
    let mut made = ptr::null_mut();
    assert_eq!(SQLAllocHandle(kind, parent, &raw mut made), 0);
    made
}

fn run(stmt: *mut c_void, text: &str) -> i16 {
    let text = format!("{text}\0");
    SQLExecDirect(stmt, text.as_ptr(), SQL_NTS)
}

fn text_column(stmt: *mut c_void, number: u16) -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut length = 0isize;
    let status = SQLGetData(stmt, number, SQL_C_CHAR, buffer.as_mut_ptr().cast(), 64, &raw mut length);
    assert_eq!(status, 0);
    usize::try_from(length).ok().map(|length| String::from_utf8_lossy(&buffer[..length]).into_owned())
}

#[test]
fn an_application_connects_runs_binds_fetches_and_rolls_back() {
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = b"Database=:memory:;Dialect=sqlite\0";
    let connected = SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0);
    assert_eq!(connected, 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    assert_eq!(run(stmt, "CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT, weight INTEGER)"), 0);
    let insert = b"INSERT INTO eggs (colour, weight) VALUES (?, ?)\0";
    assert_eq!(SQLPrepare(stmt, insert.as_ptr(), SQL_NTS), 0);
    let mut colour = *b"brown\0";
    let mut weight = 61i32;
    let mut nts = -3isize;
    let mut fixed = 4isize;
    SQLBindParameter(stmt, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 20, 0, colour.as_mut_ptr().cast(), 6, &raw mut nts);
    SQLBindParameter(stmt, 2, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0, (&raw mut weight).cast(), 4, &raw mut fixed);
    assert_eq!(SQLExecute(stmt), 0);
    let mut changed = 0isize;
    SQLRowCount(stmt, &raw mut changed);
    assert_eq!(changed, 1);
    assert_eq!(run(stmt, "SELECT colour, weight, NULL AS nothing FROM eggs"), 0);
    let mut columns = 0i16;
    SQLNumResultCols(stmt, &raw mut columns);
    assert_eq!(columns, 3);
    let mut name = [0u8; 16];
    let (mut name_length, mut data_type, mut size, mut digits, mut nullable) = (0i16, 0i16, 0usize, 0i16, 0i16);
    SQLDescribeCol(stmt, 2, name.as_mut_ptr(), 16, &raw mut name_length, &raw mut data_type, &raw mut size, &raw mut digits, &raw mut nullable);
    assert_eq!(&name[..usize::try_from(name_length).unwrap()], b"weight");
    assert_eq!(data_type, -5);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("brown"));
    assert_eq!(text_column(stmt, 2).as_deref(), Some("61"));
    assert_eq!(text_column(stmt, 3), None);
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    SQLSetConnectAttr(dbc, SQL_ATTR_AUTOCOMMIT, ptr::null_mut(), 0);
    assert_eq!(run(stmt, "DELETE FROM eggs"), 0);
    assert_eq!(SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK), 0);
    assert_eq!(run(stmt, "SELECT count(*) FROM eggs"), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("1"));
    assert_eq!(run(stmt, "SELECT * FROM nowhere"), SQL_ERROR);
    let mut state = [0u8; 6];
    let mut native = 0i32;
    let mut message = [0u8; 256];
    let mut message_length = 0i16;
    assert_eq!(SQLGetDiagRec(SQL_HANDLE_STMT, stmt, 1, state.as_mut_ptr(), &raw mut native, message.as_mut_ptr(), 256, &raw mut message_length), 0);
    assert_ne!(&state[..5], b"00000");
    let eggs = b"eggs\0";
    assert_eq!(SQLTables(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, ptr::null(), 0), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 3).as_deref(), Some("eggs"));
    assert_eq!(text_column(stmt, 4).as_deref(), Some("TABLE"));
    assert_eq!(SQLColumns(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, ptr::null(), 0), 0);
    let mut names = Vec::new();
    while SQLFetch(stmt) == 0 {
        names.push(text_column(stmt, 4).unwrap_or_default());
    }
    assert_eq!(names, ["id", "colour", "weight"]);
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    SQLFreeHandle(SQL_HANDLE_ENV, env);
}

#[test]
fn primary_keys_foreign_keys_and_type_info_are_listed() {
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = b"Database=:memory:;Dialect=sqlite\0";
    assert_eq!(SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0), 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    assert_eq!(run(stmt, "CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT)"), 0);
    assert_eq!(run(stmt, "CREATE TABLE boxes (id INTEGER PRIMARY KEY, egg_id INTEGER REFERENCES eggs (id))"), 0);
    let mut short = 0u16;
    let mut written = 0i16;
    assert_eq!(SQLGetInfo(dbc, 30, (&raw mut short).cast(), 2, &raw mut written), 0);
    assert_eq!((short, written), (63, 2));
    let mut term = [0u8; 16];
    assert_eq!(SQLGetInfo(dbc, 39, term.as_mut_ptr().cast(), 16, &raw mut written), 0);
    assert_eq!(&term[..usize::try_from(written).unwrap_or(0)], b"schema");
    assert_eq!(SQLGetInfo(dbc, 25, term.as_mut_ptr().cast(), 16, &raw mut written), 0);
    assert_eq!(&term[..1], b"N");
    assert_eq!(SQLGetInfo(dbc, 9999, term.as_mut_ptr().cast(), 16, &raw mut written), SQL_ERROR);
    let mut long = 0u32;
    assert_eq!(SQLGetInfo(dbc, 50, (&raw mut long).cast(), 4, &raw mut written), 0);
    assert_ne!(long & 0x40, 0);
    let mut native = [0u8; 64];
    let mut native_length = 0i32;
    let escaped = b"SELECT {fn LCASE('A')}\0";
    assert_eq!(SQLNativeSql(dbc, escaped.as_ptr(), SQL_NTS, native.as_mut_ptr(), 64, &raw mut native_length), 0);
    assert_eq!(&native[..usize::try_from(native_length).unwrap_or(0)], b"SELECT lower('A')");
    let wide_escaped: Vec<u16> = "SELECT {fn UCASE('a')}\0".encode_utf16().collect();
    let mut wide_native = [0u16; 64];
    assert_eq!(SQLNativeSqlW(dbc, wide_escaped.as_ptr(), SQL_NTS, wide_native.as_mut_ptr(), 64, &raw mut native_length), 0);
    let shown = String::from_utf16_lossy(&wide_native[..usize::try_from(native_length).unwrap_or(0)]);
    assert_eq!(shown, "SELECT upper('a')");
    let mut wide_term = [0u16; 16];
    assert_eq!(SQLGetInfoW(dbc, 39, wide_term.as_mut_ptr().cast(), 32, &raw mut written), 0);
    assert_eq!(written, 12);
    assert_eq!(String::from_utf16_lossy(&wide_term[..6]), "schema");
    let eggs = b"eggs\0";
    assert_eq!(SQLPrimaryKeys(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 4).as_deref(), Some("id"));
    assert_eq!(text_column(stmt, 5).as_deref(), Some("1"));
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(
        SQLForeignKeys(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, ptr::null(), 0, ptr::null(), 0, ptr::null(), 0),
        0
    );
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 3).as_deref(), Some("eggs"));
    assert_eq!(text_column(stmt, 7).as_deref(), Some("boxes"));
    assert_eq!(text_column(stmt, 8).as_deref(), Some("egg_id"));
    assert_eq!(SQLSpecialColumns(stmt, 1, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, 0, 1), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 2).as_deref(), Some("id"));
    assert_eq!(SQLSpecialColumns(stmt, 2, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, 0, 1), 0);
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(run(stmt, "SELECT {fn UCASE({fn CONCAT('ab', 'c')})} AS shout"), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("ABC"));
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(run(stmt, "SELECT {fn UCASE('x')"), SQL_ERROR);
    assert_eq!(run(stmt, "CREATE INDEX eggs_colour ON eggs (colour DESC)"), 0);
    assert_eq!(SQLStatistics(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, 1, 0), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 9).as_deref(), Some("id"));
    assert_eq!(text_column(stmt, 4).as_deref(), Some("0"));
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 6).as_deref(), Some("eggs_colour"));
    assert_eq!(text_column(stmt, 9).as_deref(), Some("colour"));
    assert_eq!(text_column(stmt, 4).as_deref(), Some("1"));
    assert_eq!(text_column(stmt, 10).as_deref(), Some("D"));
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(SQLStatistics(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3, 0, 0), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(SQLTablePrivileges(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3), 0);
    while SQLFetch(stmt) == 0 {
        assert_eq!(text_column(stmt, 3).as_deref(), Some("eggs"));
    }
    assert_eq!(SQLGetTypeInfo(stmt, SQL_INTEGER), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("integer"));
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(SQLGetTypeInfo(stmt, 0), 0);
    let mut listed = 0;
    while SQLFetch(stmt) == 0 {
        listed += 1;
    }
    assert_eq!(listed, 14);
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    SQLFreeHandle(SQL_HANDLE_ENV, env);
}

#[test]
fn procedure_columns_list_a_function_s_parameters_in_order() {
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = b"Database=:memory:;Dialect=postgresql\0";
    assert_eq!(SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0), 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    assert_eq!(run(stmt, "CREATE FUNCTION add_up(a integer, b integer) RETURNS integer LANGUAGE sql AS 'SELECT a + b'"), 0);
    let name = b"add_up\0";
    assert_eq!(SQLProcedureColumns(stmt, ptr::null(), 0, ptr::null(), 0, name.as_ptr(), -3, ptr::null(), 0), 0);
    let mut parameters = Vec::new();
    while SQLFetch(stmt) == 0 {
        parameters.push((text_column(stmt, 4).unwrap_or_default(), text_column(stmt, 5).unwrap_or_default()));
    }
    assert_eq!(parameters, [("a".to_owned(), "1".to_owned()), ("b".to_owned(), "1".to_owned())]);
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    SQLFreeHandle(SQL_HANDLE_ENV, env);
}

#[test]
fn a_connection_string_with_a_server_connects_over_the_postgresql_protocol() {
    let db = std::sync::Arc::new(eggdb::Database::in_memory().unwrap());
    let listener = eggdb_pgwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let config = eggdb_pgwire::ServerConfig {
        user: "eggdb".to_owned(),
        verifier: eggdb_pgwire::ScramVerifier::derive(b"odbc test;pw", b"0123456789abcdef", 4096),
        database: "eggdb".to_owned(),
        max_connections: 4,
        authentication_timeout: std::time::Duration::from_secs(5),
        idle_timeout: None,
        memory_budget: std::sync::Arc::default(),
        auth_rules: None,
        tls: None,
    };
    let server_db = std::sync::Arc::clone(&db);
    let server_stop = std::sync::Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let store = eggdb::sql::Store(&server_db);
        let stopped = move || server_stop.load(std::sync::atomic::Ordering::Acquire);
        eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_| {}).unwrap();
    });
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = format!("Server=127.0.0.1;Port={port};Database=eggdb;UID=eggdb;PWD={{odbc test;pw}};SSLMode=disable\0");
    let connected = SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0);
    assert_eq!(connected, 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    assert_eq!(run(stmt, "CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT)"), 0);
    assert_eq!(run(stmt, "INSERT INTO eggs VALUES (1, 'brown')"), 0);
    assert_eq!(run(stmt, "SELECT {fn UCASE(colour)} FROM eggs"), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("BROWN"));
    assert_eq!(SQLFreeHandle(SQL_HANDLE_STMT, stmt), 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    let eggs = b"eggs\0";
    assert_eq!(SQLPrimaryKeys(stmt, ptr::null(), 0, ptr::null(), 0, eggs.as_ptr(), -3), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 4).as_deref(), Some("id"));
    assert_eq!(SQLFreeHandle(SQL_HANDLE_STMT, stmt), 0);
    assert_eq!(SQLDisconnect(dbc), 0);
    let wrong = format!("Server=127.0.0.1;Port={port};Database=eggdb;UID=eggdb;PWD=wrong;SSLMode=disable\0");
    assert_eq!(SQLDriverConnect(dbc, ptr::null_mut(), wrong.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0), SQL_ERROR);
    stop.store(true, std::sync::atomic::Ordering::Release);
    server.join().unwrap();
}

#[test]
fn several_result_sets_functions_and_diagnostic_fields() {
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = b"Database=:memory:;Dialect=sqlite\0";
    assert_eq!(SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0), 0);
    let mut bitmap = [0u16; 250];
    assert_eq!(SQLGetFunctions(dbc, 999, bitmap.as_mut_ptr()), 0);
    let exists = |id: u16| bitmap[usize::from(id >> 4)] & (1 << (id & 0xF)) != 0;
    assert!(exists(61) && exists(1021) && exists(44));
    assert!(!exists(1004));
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    assert_eq!(run(stmt, "SELECT 1; SELECT 'two'"), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("1"));
    assert_eq!(SQLMoreResults(stmt), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!(text_column(stmt, 1).as_deref(), Some("two"));
    assert_eq!(SQLMoreResults(stmt), SQL_NO_DATA);
    assert_eq!(run(stmt, "SELECT * FROM nowhere"), SQL_ERROR);
    let mut count = 0i32;
    assert_eq!(SQLGetDiagField(SQL_HANDLE_STMT, stmt, 0, 2, (&raw mut count).cast(), 0, ptr::null_mut()), 0);
    assert_eq!(count, 1);
    let mut state = [0u8; 6];
    let mut written = 0i16;
    assert_eq!(SQLGetDiagField(SQL_HANDLE_STMT, stmt, 1, 4, state.as_mut_ptr().cast(), 6, &raw mut written), 0);
    assert_eq!(written, 5);
}

#[test]
fn block_fetches_fill_arrays_of_rows_column_wise() {
    let env = handle(SQL_HANDLE_ENV, ptr::null_mut());
    let dbc = handle(SQL_HANDLE_DBC, env);
    let settings = b"Database=:memory:;Dialect=sqlite\0";
    assert_eq!(SQLDriverConnect(dbc, ptr::null_mut(), settings.as_ptr(), -3, ptr::null_mut(), 0, ptr::null_mut(), 0), 0);
    let stmt = handle(SQL_HANDLE_STMT, dbc);
    let mut numbers = [0i32; 2];
    let mut indicators = [0isize; 2];
    let mut fetched = 0usize;
    let mut statuses = [9u16; 2];
    assert_eq!(SQLSetStmtAttr(stmt, 27, 2usize as *mut c_void, 0), 0);
    assert_eq!(SQLSetStmtAttr(stmt, 26, (&raw mut fetched).cast(), 0), 0);
    assert_eq!(SQLSetStmtAttr(stmt, 25, statuses.as_mut_ptr().cast(), 0), 0);
    assert_eq!(run(stmt, "SELECT 10 UNION ALL SELECT 20 UNION ALL SELECT 30"), 0);
    assert_eq!(SQLBindCol(stmt, 1, SQL_C_SLONG, numbers.as_mut_ptr().cast(), 4, indicators.as_mut_ptr()), 0);
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!((numbers, fetched, statuses), ([10, 20], 2, [0, 0]));
    assert_eq!(SQLFetch(stmt), 0);
    assert_eq!((numbers[0], fetched, statuses), (30, 1, [0, 3]));
    assert_eq!(SQLFetch(stmt), SQL_NO_DATA);
    assert_eq!(fetched, 0);
}
