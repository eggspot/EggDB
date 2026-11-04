//! PL/SQL procedures called by name over TDS RPC, and mssql-jdbc's `DatabaseMetaData`
//! procedures, on a connection that chose Oracle's dialect with `SET eggdb.dialect oracle`
//! (D295).

use crate::common;

use common::{
    Client, TestServer, Token, call_by_id, call_by_name, errors, int_parameter, rows,
    text_parameter,
};

const SP_EXECUTESQL: u16 = 10;

const LOG_IT: &str = "CREATE OR REPLACE PROCEDURE log_it (\n  p_n   IN NUMBER,\n  p_msg IN VARCHAR2 DEFAULT 'none'\n) AS\nBEGIN\n  INSERT INTO log VALUES (p_n, p_msg);\nEND log_it;";

const SPLIT_IT: &str = "CREATE OR REPLACE PROCEDURE split_it (\n  total IN INTEGER,\n  half  OUT INTEGER,\n  note  IN OUT VARCHAR2\n) AS\nBEGIN\n  half := total / 2;\n  note := note || '!';\nEND;";

const GET_COLUMNS: &str = "DECLARE @mssqljdbc_temp_sp_columns_result TABLE(TABLE_QUALIFIER SYSNAME, TABLE_OWNER SYSNAME,TABLE_NAME SYSNAME, COLUMN_NAME SYSNAME, DATA_TYPE SMALLINT, TYPE_NAME SYSNAME, PRECISION INT,LENGTH INT, SCALE SMALLINT, RADIX SMALLINT, NULLABLE SMALLINT, REMARKS VARCHAR(254), COLUMN_DEF NVARCHAR(4000),SQL_DATA_TYPE SMALLINT, SQL_DATETIME_SUB SMALLINT, CHAR_OCTET_LENGTH INT, ORDINAL_POSITION INT,IS_NULLABLE VARCHAR(254), SS_IS_SPARSE SMALLINT, SS_IS_COLUMN_SET SMALLINT, SS_IS_COMPUTED SMALLINT,SS_IS_IDENTITY SMALLINT, SS_UDT_CATALOG_NAME NVARCHAR(128), SS_UDT_SCHEMA_NAME NVARCHAR(128),SS_UDT_ASSEMBLY_TYPE_NAME NVARCHAR(max), SS_XML_SCHEMACOLLECTION_CATALOG_NAME NVARCHAR(128),SS_XML_SCHEMACOLLECTION_SCHEMA_NAME NVARCHAR(128), SS_XML_SCHEMACOLLECTION_NAME NVARCHAR(128),SS_DATA_TYPE TINYINT);INSERT INTO @mssqljdbc_temp_sp_columns_result EXEC sp_columns_100 @P0,NULL,NULL,N'%',2,3;SELECT TABLE_QUALIFIER AS TABLE_CAT, TABLE_OWNER AS TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, CAST(DATA_TYPE AS INT) AS DATA_TYPE,TYPE_NAME, PRECISION AS COLUMN_SIZE, LENGTH AS BUFFER_LENGTH, CAST(SCALE AS INT) AS DECIMAL_DIGITS, CAST(RADIX AS INT) AS NUM_PREC_RADIX,CAST(NULLABLE AS INT) AS NULLABLE, CAST(REMARKS AS VARCHAR) AS REMARKS, COLUMN_DEF, CAST(SQL_DATA_TYPE AS INT) AS SQL_DATA_TYPE, CAST(SQL_DATETIME_SUB AS INT) AS SQL_DATETIME_SUB, CHAR_OCTET_LENGTH, ORDINAL_POSITION, IS_NULLABLE,CAST(NULL AS VARCHAR) AS SCOPE_CATALOG, CAST(NULL AS VARCHAR) AS SCOPE_SCHEMA, CAST(NULL AS VARCHAR) AS SCOPE_TABLE, CAST(SS_DATA_TYPE AS SMALLINT) AS SOURCE_DATA_TYPE, CASE SS_IS_IDENTITY WHEN 0 THEN 'NO' WHEN 1 THEN 'YES' WHEN '' THEN '' END AS IS_AUTOINCREMENT,CASE SS_IS_COMPUTED WHEN 0 THEN 'NO' WHEN 1 THEN 'YES' WHEN '' THEN '' END AS IS_GENERATEDCOLUMN, SS_IS_SPARSE, SS_IS_COLUMN_SET, SS_UDT_CATALOG_NAME, SS_UDT_SCHEMA_NAME, SS_UDT_ASSEMBLY_TYPE_NAME,SS_XML_SCHEMACOLLECTION_CATALOG_NAME, SS_XML_SCHEMACOLLECTION_SCHEMA_NAME, SS_XML_SCHEMACOLLECTION_NAME FROM @mssqljdbc_temp_sp_columns_result ORDER BY TABLE_CAT, TABLE_SCHEM, TABLE_NAME, ORDINAL_POSITION;";

fn returned(tokens: &[Token]) -> Vec<(u16, String, Option<String>)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::ReturnValue {
                ordinal,
                name,
                value,
            } => Some((*ordinal, name.clone(), value.clone())),
            _ => None,
        })
        .collect()
}

fn ran(client: &mut Client, text: &str) {
    let tokens = client.batch(text);
    assert!(errors(&tokens).is_empty(), "{text}: {tokens:?}");
}

fn oracle_client(server: &TestServer) -> Client {
    let mut client = Client::login(server.address);
    ran(&mut client, "SET eggdb.dialect oracle");
    ran(
        &mut client,
        "CREATE TABLE log (n NUMBER(5), msg VARCHAR2(200))",
    );
    ran(&mut client, LOG_IT);
    ran(&mut client, SPLIT_IT);
    client
}

fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

fn column(rows: &[Vec<Option<String>>], at: usize) -> Vec<Option<String>> {
    rows.iter()
        .map(|row| row.get(at).cloned().flatten())
        .collect()
}

#[test]
fn plsql_procedures_called_by_name_bind_arguments_by_position_and_by_name() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    let positional = client.rpc(&[call_by_name(
        "log_it",
        &[
            int_parameter("", Some(1), false),
            text_parameter("", Some("positional"), false),
        ],
    )]);
    assert!(errors(&positional).is_empty(), "{positional:?}");
    assert!(rows(&positional).is_empty(), "{positional:?}");
    let named = client.rpc(&[call_by_name(
        "LOG_IT",
        &[
            text_parameter("@P_MSG", Some("named"), false),
            int_parameter("@p_n", Some(2), false),
        ],
    )]);
    assert!(errors(&named).is_empty(), "{named:?}");
    let defaulted = client.rpc(&[call_by_name(
        "log_it",
        &[int_parameter("@p_n", Some(3), false)],
    )]);
    assert!(errors(&defaulted).is_empty(), "{defaulted:?}");
    assert_eq!(
        rows(&client.batch("SELECT n, msg FROM log ORDER BY n")),
        vec![
            vec![text("1"), text("positional")],
            vec![text("2"), text("named")],
            vec![text("3"), text("none")],
        ]
    );
}

#[test]
fn plsql_out_and_in_out_parameters_come_back_as_return_values() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    let by_name = client.rpc(&[call_by_name(
        "split_it",
        &[
            int_parameter("@total", Some(8), false),
            int_parameter("@half", None, true),
            text_parameter("@note", Some("hi"), true),
        ],
    )]);
    assert!(errors(&by_name).is_empty(), "{by_name:?}");
    assert!(rows(&by_name).is_empty(), "{by_name:?}");
    assert_eq!(
        returned(&by_name),
        vec![
            (1, "@half".to_owned(), text("4")),
            (2, "@note".to_owned(), text("hi!")),
        ]
    );
    let by_position = client.rpc(&[call_by_name(
        "split_it",
        &[
            int_parameter("", Some(6), false),
            int_parameter("", None, true),
            text_parameter("", Some("yo"), true),
        ],
    )]);
    assert!(errors(&by_position).is_empty(), "{by_position:?}");
    assert_eq!(
        returned(&by_position),
        vec![
            (1, String::new(), text("3")),
            (2, String::new(), text("yo!")),
        ]
    );
    let reordered = client.rpc(&[call_by_name(
        "split_it",
        &[
            text_parameter("@note", Some("x"), true),
            int_parameter("@half", None, true),
            int_parameter("@total", Some(2), false),
        ],
    )]);
    assert!(errors(&reordered).is_empty(), "{reordered:?}");
    assert_eq!(
        returned(&reordered),
        vec![
            (0, "@note".to_owned(), text("x!")),
            (1, "@half".to_owned(), text("1")),
        ]
    );
}

#[test]
fn plsql_calls_by_name_refuse_wrong_arguments_and_stay_usable() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    let not_output = client.rpc(&[call_by_name(
        "split_it",
        &[int_parameter("@total", Some(1), true)],
    )]);
    assert_eq!(errors(&not_output)[0].0, 8162, "{not_output:?}");
    let unknown = client.rpc(&[call_by_name(
        "split_it",
        &[int_parameter("@nothing", Some(1), false)],
    )]);
    assert_eq!(errors(&unknown)[0].0, 8145, "{unknown:?}");
    let repeated = client.rpc(&[call_by_name(
        "log_it",
        &[
            int_parameter("@p_n", Some(1), false),
            int_parameter("@P_N", Some(2), false),
        ],
    )]);
    assert_eq!(errors(&repeated)[0].0, 8143, "{repeated:?}");
    let too_many = client.rpc(&[call_by_name(
        "log_it",
        &[
            int_parameter("", Some(1), false),
            text_parameter("", Some("a"), false),
            text_parameter("", Some("b"), false),
        ],
    )]);
    assert_eq!(errors(&too_many)[0].0, 8144, "{too_many:?}");
    let missing = client.rpc(&[call_by_name("no_such_procedure", &[])]);
    assert_eq!(errors(&missing)[0].0, 2812, "{missing:?}");
    let injected = client.rpc(&[call_by_name("log_it; DELETE FROM log", &[])]);
    assert!(!errors(&injected).is_empty(), "{injected:?}");
    let required = client.rpc(&[call_by_name("log_it", &[])]);
    assert!(!errors(&required).is_empty(), "{required:?}");
    let after = client.rpc(&[call_by_name(
        "log_it",
        &[int_parameter("@p_n", Some(9), false)],
    )]);
    assert!(errors(&after).is_empty(), "{after:?}");
    assert_eq!(
        rows(&client.batch("SELECT n, msg FROM log")),
        vec![vec![text("9"), text("none")]]
    );
}

#[test]
fn mssql_jdbc_database_metadata_runs_as_transact_sql_in_oracle_sessions() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    ran(
        &mut client,
        "CREATE TABLE items (id NUMBER(5) PRIMARY KEY, name VARCHAR2(40))",
    );
    let tables = client.rpc(&[call_by_name(
        "sp_tables",
        &[
            text_parameter("", Some("items"), false),
            text_parameter("", None, false),
            text_parameter("", None, false),
            text_parameter("", Some("'TABLE'"), false),
        ],
    )]);
    assert!(errors(&tables).is_empty(), "{tables:?}");
    let found = rows(&tables);
    assert_eq!(column(&found, 2), vec![text("items")], "{tables:?}");
    assert_eq!(column(&found, 3), vec![text("TABLE")], "{tables:?}");
    let keys = client.rpc(&[call_by_name(
        "sp_pkeys",
        &[
            text_parameter("@table_name", Some("items"), false),
            text_parameter("@table_owner", None, false),
            text_parameter("@table_qualifier", None, false),
        ],
    )]);
    assert!(errors(&keys).is_empty(), "{keys:?}");
    assert_eq!(column(&rows(&keys), 3), vec![text("id")], "{keys:?}");
    let columns = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some(GET_COLUMNS), false),
            text_parameter("", Some("@P0 nvarchar(4000)"), false),
            text_parameter("@P0", Some("items"), false),
        ],
    )]);
    assert!(errors(&columns).is_empty(), "{columns:?}");
    assert_eq!(
        column(&rows(&columns), 3),
        vec![text("id"), text("name")],
        "{columns:?}"
    );
    let procedures = client.rpc(&[call_by_name(
        "sp_stored_procedures",
        &[
            text_parameter("", Some("log_it"), false),
            text_parameter("", None, false),
            text_parameter("", None, false),
        ],
    )]);
    assert!(errors(&procedures).is_empty(), "{procedures:?}");
    assert_eq!(
        column(&rows(&procedures), 2),
        vec![text("log_it;1")],
        "{procedures:?}"
    );
    let parameters = client.rpc(&[call_by_name(
        "sp_sproc_columns_100",
        &[
            text_parameter("", Some("split_it"), false),
            text_parameter("", None, false),
            text_parameter("", None, false),
            text_parameter("", None, false),
            int_parameter("", Some(3), false),
        ],
    )]);
    assert!(errors(&parameters).is_empty(), "{parameters:?}");
    let names = column(&rows(&parameters), 3);
    assert_eq!(names.len(), 3, "{parameters:?}");
    for (name, expected) in names.iter().zip(["total", "half", "note"]) {
        assert!(
            name.as_deref()
                .is_some_and(|name| name.trim_start_matches('@') == expected),
            "{parameters:?}"
        );
    }
    let batch_form = client.batch("EXEC sp_pkeys N'items'");
    assert!(errors(&batch_form).is_empty(), "{batch_form:?}");
    assert_eq!(column(&rows(&batch_form), 3), vec![text("id")]);
    let still_oracle = client.batch("SELECT COUNT(*) AS n FROM items WHERE ROWNUM <= 5");
    assert_eq!(
        rows(&still_oracle),
        vec![vec![text("0")]],
        "{still_oracle:?}"
    );
}
