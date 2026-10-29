//! Remote procedure calls, transaction manager requests and connection resets over a real
//! loopback socket, in the layouts of [MS-TDS] 2.2.6.6, 2.2.6.9 and 2.2.7, as
//! Microsoft.Data.SqlClient and mssql-jdbc send them (D132).

mod common;

use common::{
    Client, TestServer, Token, call_by_id, call_by_name, decimal_parameter, errors, int_parameter,
    rows, text_parameter,
};

const SP_EXECUTESQL: u16 = 10;
const SP_PREPARE: u16 = 11;
const SP_EXECUTE: u16 = 12;
const SP_PREPEXEC: u16 = 13;
const SP_UNPREPARE: u16 = 15;
const DONE_MORE: u16 = 0x01;
const DONE_ERROR: u16 = 0x02;
const DONE_IN_TRANSACTION: u16 = 0x04;
const DONE_COUNT: u16 = 0x10;

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

fn procedure_dones(tokens: &[Token]) -> Vec<u16> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::DoneProc { status } => Some(*status),
            _ => None,
        })
        .collect()
}

fn counts_in_procedures(tokens: &[Token]) -> Vec<(u16, u64)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::DoneInProc { status, rows, .. } => Some((*status, *rows)),
            _ => None,
        })
        .collect()
}

fn environment_changes(tokens: &[Token]) -> Vec<u8> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::EnvChange { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn logged_in_with_table(server: &TestServer) -> Client {
    let mut client = Client::login(server.address);
    let created = client
        .batch("CREATE TABLE items (id INT PRIMARY KEY, name NVARCHAR(40), price DECIMAL(10,2))");
    assert!(errors(&created).is_empty(), "{created:?}");
    client
}

#[test]
fn sp_executesql_runs_parameterized_crud_with_typed_values() {
    let server = TestServer::start(8);
    let mut client = logged_in_with_table(&server);
    let inserted = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some("INSERT INTO items VALUES (@p1, @p2, @p3)"), false),
            text_parameter(
                "",
                Some("@p1 int,@p2 nvarchar(40),@p3 decimal(10,2)"),
                false,
            ),
            int_parameter("@p1", Some(1), false),
            text_parameter("@p2", Some("egg"), false),
            decimal_parameter("@p3", 1250, 10, 2),
        ],
    )]);
    assert!(errors(&inserted).is_empty(), "{inserted:?}");
    assert_eq!(
        counts_in_procedures(&inserted),
        vec![(DONE_COUNT | DONE_MORE, 1)]
    );
    assert!(inserted.contains(&Token::ReturnStatus(0)));
    assert_eq!(procedure_dones(&inserted), vec![0]);
    let positional = client.rpc(&[call_by_name(
        "sys.sp_executesql",
        &[
            text_parameter(
                "",
                Some("INSERT INTO items (id, name) VALUES (@P0, @P1)"),
                false,
            ),
            text_parameter("", Some("@P0 int,@P1 nvarchar(4000)"), false),
            int_parameter("", Some(2), false),
            text_parameter("", None, false),
        ],
    )]);
    assert!(errors(&positional).is_empty(), "{positional:?}");
    let selected = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter(
                "",
                Some("SELECT name, CAST(price AS varchar(20)) AS price FROM items WHERE id >= @low ORDER BY id"),
                false,
            ),
            text_parameter("", Some("@low int"), false),
            int_parameter("@low", Some(1), false),
        ],
    )]);
    assert_eq!(
        rows(&selected),
        vec![
            vec![Some("egg".to_owned()), Some("12.50".to_owned())],
            vec![None, None],
        ]
    );
    let updated = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter(
                "",
                Some(
                    "UPDATE items SET name = @n WHERE id = @id; DELETE FROM items WHERE id = @gone",
                ),
                false,
            ),
            text_parameter("", Some("@n nvarchar(40), @id int, @gone int"), false),
            text_parameter("@n", Some("bread"), false),
            int_parameter("@id", Some(1), false),
            int_parameter("@gone", Some(2), false),
        ],
    )]);
    assert_eq!(
        counts_in_procedures(&updated),
        vec![(DONE_COUNT | DONE_MORE, 1), (DONE_COUNT | DONE_MORE, 1)]
    );
    assert_eq!(
        rows(&client.batch("SELECT id, name FROM items")),
        vec![vec![Some("1".to_owned()), Some("bread".to_owned())]]
    );
}

#[test]
fn prepared_handles_run_until_unprepared() {
    let server = TestServer::start(8);
    let mut client = logged_in_with_table(&server);
    let prepared = client.rpc(&[call_by_id(
        SP_PREPEXEC,
        &[
            int_parameter("", None, true),
            text_parameter("", Some("@p1 int, @p2 nvarchar(40)"), false),
            text_parameter(
                "",
                Some("INSERT INTO items (id, name) VALUES (@p1, @p2)"),
                false,
            ),
            int_parameter("", Some(10), false),
            text_parameter("", Some("ten"), false),
        ],
    )]);
    assert!(errors(&prepared).is_empty(), "{prepared:?}");
    assert_eq!(
        returned(&prepared),
        vec![(0, String::new(), Some("1".to_owned()))]
    );
    let status_at = prepared
        .iter()
        .position(|token| token == &Token::ReturnStatus(0))
        .unwrap();
    let handle_at = prepared
        .iter()
        .position(|token| matches!(token, Token::ReturnValue { .. }))
        .unwrap();
    assert!(status_at < handle_at);
    let executed = client.rpc(&[
        call_by_id(
            SP_EXECUTE,
            &[
                int_parameter("", Some(1), false),
                int_parameter("", Some(11), false),
                text_parameter("", Some("eleven"), false),
            ],
        ),
        call_by_id(
            SP_EXECUTE,
            &[
                int_parameter("", Some(1), false),
                int_parameter("@p1", Some(12), false),
                text_parameter("@p2", Some("twelve"), false),
            ],
        ),
    ]);
    assert!(errors(&executed).is_empty(), "{executed:?}");
    assert_eq!(procedure_dones(&executed), vec![DONE_MORE, 0]);
    let handle = client.rpc(&[call_by_id(
        SP_PREPARE,
        &[
            int_parameter("@handle", None, true),
            text_parameter("", Some("@low int"), false),
            text_parameter(
                "",
                Some("SELECT COUNT(*) AS n FROM items WHERE id >= @low"),
                false,
            ),
            int_parameter("", Some(1), false),
        ],
    )]);
    assert_eq!(
        returned(&handle),
        vec![(0, "@handle".to_owned(), Some("2".to_owned()))]
    );
    let counted = client.rpc(&[call_by_id(
        SP_EXECUTE,
        &[
            int_parameter("", Some(2), false),
            int_parameter("", Some(11), false),
        ],
    )]);
    assert_eq!(rows(&counted), vec![vec![Some("2".to_owned())]]);
    let unprepared = client.rpc(&[call_by_id(
        SP_UNPREPARE,
        &[int_parameter("", Some(1), false)],
    )]);
    assert!(errors(&unprepared).is_empty(), "{unprepared:?}");
    let gone = client.rpc(&[call_by_id(
        SP_EXECUTE,
        &[
            int_parameter("", Some(1), false),
            int_parameter("", Some(13), false),
            text_parameter("", Some("thirteen"), false),
        ],
    )]);
    assert_eq!(errors(&gone)[0].0, 8179);
    assert_eq!(procedure_dones(&gone), vec![DONE_ERROR]);
    let bad_syntax = client.rpc(&[call_by_id(
        SP_PREPARE,
        &[
            int_parameter("", None, true),
            text_parameter("", Some(""), false),
            text_parameter("", Some("SELECT * FORM items"), false),
        ],
    )]);
    assert_eq!(errors(&bad_syntax)[0].0, 102);
    assert!(returned(&bad_syntax).is_empty());
}

#[test]
fn output_parameters_come_back_after_the_return_status() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some("SELECT @in + 1 AS n"), false),
            text_parameter("", Some("@in int, @out nvarchar(10) OUTPUT"), false),
            int_parameter("@in", Some(41), false),
            text_parameter("@out", Some("kept"), true),
        ],
    )]);
    assert_eq!(rows(&tokens), vec![vec![Some("42".to_owned())]]);
    assert_eq!(
        returned(&tokens),
        vec![(3, "@out".to_owned(), Some("kept".to_owned()))]
    );
}

#[test]
fn missing_and_unknown_arguments_and_procedures_get_sql_servers_errors() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let cases: [(Vec<Vec<u8>>, u32); 5] = [
        (
            vec![
                text_parameter("", Some("SELECT @a"), false),
                text_parameter("", Some("@a int"), false),
            ],
            8178,
        ),
        (
            vec![
                text_parameter("", Some("SELECT 1"), false),
                text_parameter("", Some(""), false),
                int_parameter("", Some(1), false),
            ],
            8144,
        ),
        (
            vec![
                text_parameter("", Some("SELECT @a"), false),
                text_parameter("", Some("@a int"), false),
                int_parameter("@b", Some(1), false),
            ],
            8145,
        ),
        (
            vec![
                text_parameter("", Some("SELECT @b"), false),
                text_parameter("", Some("@a int"), false),
                int_parameter("@a", Some(1), false),
            ],
            137,
        ),
        (vec![int_parameter("", Some(1), false)], 214),
    ];
    for (parameters, number) in cases {
        let tokens = client.rpc(&[call_by_id(SP_EXECUTESQL, &parameters)]);
        assert_eq!(errors(&tokens)[0].0, number, "{tokens:?}");
    }
    let unknown = client.rpc(&[call_by_name("dbo.orders_by_day", &[])]);
    assert_eq!(errors(&unknown)[0].0, 2812);
    let cursor = client.rpc(&[call_by_id(2, &[])]);
    assert_eq!(errors(&cursor)[0].0, 2812);
    let variant = client.rpc(&[call_by_id(SP_EXECUTESQL, &[vec![0, 0, 0x62, 0, 0, 0, 0]])]);
    assert_eq!(errors(&variant)[0].0, 40517);
    assert_eq!(
        rows(&client.batch("SELECT 1")),
        vec![vec![Some("1".to_owned())]]
    );
}

#[test]
fn transaction_manager_requests_begin_commit_and_roll_back_with_their_descriptors() {
    let server = TestServer::start(8);
    let mut client = logged_in_with_table(&server);
    let begun = client.transaction(5, &[2, 0]);
    assert!(errors(&begun).is_empty(), "{begun:?}");
    assert_eq!(environment_changes(&begun), vec![8]);
    let inserted = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some("INSERT INTO items (id) VALUES (@id)"), false),
            text_parameter("", Some("@id int"), false),
            int_parameter("@id", Some(5), false),
        ],
    )]);
    assert_eq!(procedure_dones(&inserted), vec![DONE_IN_TRANSACTION]);
    let rolled_back = client.transaction(8, &[0, 0]);
    assert_eq!(environment_changes(&rolled_back), vec![10]);
    assert_eq!(
        rows(&client.batch("SELECT COUNT(*) AS n FROM items")),
        vec![vec![Some("0".to_owned())]]
    );
    client.transaction(5, &[0, 0]);
    client.batch("INSERT INTO items (id) VALUES (6)");
    let committed = client.transaction(7, &[0, 0]);
    assert_eq!(environment_changes(&committed), vec![9]);
    let chained = client.transaction(5, &[0, 0]);
    assert_eq!(environment_changes(&chained), vec![8]);
    let again = client.transaction(7, &[0, 1, 2, 0]);
    assert_eq!(environment_changes(&again), vec![9, 8]);
    client.transaction(8, &[0, 0]);
    assert_eq!(
        rows(&client.batch("SELECT COUNT(*) AS n FROM items")),
        vec![vec![Some("1".to_owned())]]
    );
    let save = client.transaction(9, &[1, b's', 0]);
    assert_eq!(errors(&save)[0].0, 40517);
    let distributed = client.transaction(1, &[]);
    assert_eq!(errors(&distributed)[0].0, 40517);
}

#[test]
fn the_reset_flag_rolls_back_restores_options_and_is_acknowledged_first() {
    let server = TestServer::start(8);
    let mut client = logged_in_with_table(&server);
    client.batch("SET NOCOUNT ON BEGIN TRAN INSERT INTO items (id) VALUES (7)");
    client.send_with_status(
        0x01,
        0x08,
        &common::batch_body("INSERT INTO items (id) VALUES (8)"),
    );
    let (_, body) = client.read_message().unwrap();
    let tokens = common::parse_tokens(&body);
    assert_eq!(
        tokens.first(),
        Some(&Token::EnvChange {
            kind: 18,
            new: Vec::new(),
            old: Vec::new()
        })
    );
    assert!(
        common::dones(&tokens)
            .iter()
            .any(|(status, rows)| status & DONE_COUNT != 0 && *rows == 1)
    );
    assert_eq!(
        rows(&client.batch("SELECT id FROM items")),
        vec![vec![Some("8".to_owned())]]
    );
    client.batch("BEGIN TRAN INSERT INTO items (id) VALUES (9)");
    client.send_with_status(0x01, 0x10, &common::batch_body("SELECT @@TRANCOUNT AS n"));
    let (_, body) = client.read_message().unwrap();
    assert_eq!(
        rows(&common::parse_tokens(&body)),
        vec![vec![Some("1".to_owned())]]
    );
    client.batch("COMMIT");
    assert_eq!(
        rows(&client.batch("SELECT COUNT(*) AS n FROM items")),
        vec![vec![Some("2".to_owned())]]
    );
}

#[test]
fn implicit_transactions_as_mssql_jdbc_drives_them() {
    let server = TestServer::start(8);
    let mut client = logged_in_with_table(&server);
    assert!(errors(&client.batch("set implicit_transactions on ")).is_empty());
    let inserted = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some("INSERT INTO items (id) VALUES (@P0)"), false),
            text_parameter("", Some("@P0 int"), false),
            int_parameter("", Some(20), false),
        ],
    )]);
    assert_eq!(environment_changes(&inserted), vec![8]);
    let rolled_back = client.batch("IF @@TRANCOUNT > 0 ROLLBACK TRAN");
    assert_eq!(environment_changes(&rolled_back), vec![10]);
    client.batch("INSERT INTO items (id) VALUES (21)");
    let committed = client.batch("IF @@TRANCOUNT > 0 COMMIT TRAN");
    assert_eq!(environment_changes(&committed), vec![9]);
    let off = client.batch("set implicit_transactions off IF @@TRANCOUNT > 0 COMMIT TRAN ");
    assert!(errors(&off).is_empty(), "{off:?}");
    assert_eq!(
        rows(&client.batch("SELECT id FROM items")),
        vec![vec![Some("21".to_owned())]]
    );
}
