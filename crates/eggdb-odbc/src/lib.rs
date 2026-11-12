//! EggDB's ODBC driver: `libeggdb_odbc` (`eggdb_odbc.dll` on Windows) answers the ODBC 3 calls
//! applications and driver managers (unixODBC, iODBC, Windows' ODBC manager) make, on an
//! embedded EggDB file through EggDB's C API, a thin layer with no SQL or storage logic of its
//! own (D16).
//!
//! Connect with `Database=<path>;Dialect=<dialect>;ReadOnly=1` (the dialect is SQLite's unless
//! given), or with `SQLConnect` naming the file as the data source. It runs statements directly
//! or prepared, with `?` parameters bound by `SQLBindParameter`; reads rows with `SQLFetch`,
//! `SQLGetData` (in pieces for long values) and `SQLBindCol`; describes columns; commits and
//! rolls back with autocommit off; answers diagnostics and the first `SQLGetInfo` types; and
//! lists tables and columns with `SQLTables` and `SQLColumns` from `information_schema`. Each
//! text-taking call also has its `W` form, for Unicode applications.

mod api;
mod handles;
mod memory;
mod native;
mod values;
mod wide;

pub use api::{
    SQLAllocHandle, SQLBindCol, SQLBindParameter, SQLCloseCursor, SQLColAttribute, SQLColumns,
    SQLConnect,
    SQLDescribeCol, SQLDisconnect, SQLDriverConnect, SQLEndTran, SQLExecDirect, SQLExecute,
    SQLFetch, SQLFreeHandle, SQLFreeStmt, SQLGetConnectAttr, SQLGetData, SQLGetDiagRec,
    SQLGetEnvAttr, SQLGetInfo, SQLNumParams, SQLNumResultCols, SQLPrepare, SQLRowCount,
    SQLSetConnectAttr, SQLSetEnvAttr, SQLSetStmtAttr, SQLTables,
};
pub use wide::{
    SQLColAttributeW, SQLColumnsW, SQLConnectW, SQLDescribeColW, SQLDriverConnectW,
    SQLExecDirectW, SQLGetDiagRecW, SQLGetInfoW, SQLPrepareW, SQLTablesW,
};
