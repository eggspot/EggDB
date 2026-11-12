use eggdb_odbc::{
    SQLAllocHandle, SQLBindParameter, SQLDescribeCol, SQLDisconnect, SQLDriverConnect,
    SQLEndTran, SQLExecDirect, SQLExecute, SQLFetch, SQLFreeHandle, SQLGetData, SQLGetDiagRec,
    SQLNumResultCols, SQLPrepare, SQLRowCount, SQLSetConnectAttr,
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
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    SQLFreeHandle(SQL_HANDLE_ENV, env);
}
