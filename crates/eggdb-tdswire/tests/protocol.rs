//! The TDS server over a real loopback socket, driven by a client written in the tests: the
//! pre-login, logins, batches, transactions, errors and limits as [MS-TDS] and SQL Server do
//! them (D122).

mod common;

use common::{Client, DATABASE, LoginOptions, TestServer, Token, batch_body, dones, errors, rows};

const DONE_MORE: u16 = 0x01;
const DONE_ERROR: u16 = 0x02;
const DONE_IN_TRANSACTION: u16 = 0x04;
const DONE_COUNT: u16 = 0x10;
const DONE_ATTENTION: u16 = 0x20;

fn login_errors(server: &TestServer, options: &LoginOptions<'_>) -> Vec<(u32, u8, String)> {
    let (_, tokens) = Client::login_with(server.address, options);
    errors(&tokens)
}

#[test]
fn prelogin_answers_that_encryption_is_not_available_and_closes_on_clients_that_need_it() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    let answer = client.prelogin(0x00).unwrap();
    let parsed = eggdb_tdswire::messages::parse_prelogin(&answer).unwrap();
    assert_eq!(parsed.encryption, Some(0x02));
    assert_eq!(parsed.version[..4], [16, 0, 0x03, 0xE8]);
    for encryption in [0x01, 0x03, 0x80] {
        let mut client = Client::connect(server.address);
        let answer = client.prelogin(encryption).unwrap();
        assert_eq!(
            eggdb_tdswire::messages::parse_prelogin(&answer)
                .unwrap()
                .encryption,
            Some(0x02)
        );
        assert!(client.is_closed(), "encryption {encryption:#x}");
    }
    let mut tls = Client::connect(server.address);
    tls.send_raw(&[0x16, 0x03, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5]);
    assert!(tls.is_closed());
    assert!(
        server
            .log()
            .iter()
            .any(|line| line.contains("requires TLS"))
    );
}

#[test]
fn a_login_acknowledges_tds_7_4_and_reports_the_database_language_collation_and_packet_size() {
    let server = TestServer::start(8);
    let (_, tokens) = Client::login_with(
        server.address,
        &LoginOptions {
            packet_size: 8192,
            ..LoginOptions::default()
        },
    );
    assert!(tokens.contains(&Token::LoginAck {
        version: [0x74, 0, 0, 4],
        program: "EggDB".into()
    }));
    let changes: Vec<(u8, Vec<u8>)> = tokens
        .iter()
        .filter_map(|token| match token {
            Token::EnvChange { kind, new, .. } => Some((*kind, new.clone())),
            _ => None,
        })
        .collect();
    assert!(changes.contains(&(1, common::utf16(DATABASE))));
    assert!(changes.contains(&(2, common::utf16("us_english"))));
    assert!(changes.contains(&(7, eggdb_tdswire::COLLATION.to_vec())));
    assert!(changes.contains(&(4, common::utf16("8192"))));
    assert!(tokens.iter().any(|token| matches!(token,
        Token::Info { number: 5701, message } if message.contains(DATABASE))));
    assert_eq!(dones(&tokens).last(), Some(&(0, 0)));
}

#[test]
fn wrong_passwords_unknown_users_and_other_databases_are_refused_with_sql_server_errors() {
    let server = TestServer::start(8);
    for options in [
        LoginOptions {
            password: "wrong",
            ..LoginOptions::default()
        },
        LoginOptions {
            user: "mallory",
            ..LoginOptions::default()
        },
        LoginOptions {
            password: "",
            ..LoginOptions::default()
        },
    ] {
        let failed = login_errors(&server, &options);
        assert_eq!(
            failed,
            vec![(
                18456,
                14,
                format!("Login failed for user '{}'.", options.user)
            )]
        );
    }
    let other = login_errors(
        &server,
        &LoginOptions {
            database: "master",
            ..LoginOptions::default()
        },
    );
    assert_eq!(other[0].0, 4060);
    assert_eq!(other[1].0, 18456);
    let any_case = login_errors(
        &server,
        &LoginOptions {
            database: "SHOP",
            ..LoginOptions::default()
        },
    );
    assert!(any_case.is_empty());
    let log = server.log();
    assert!(
        log.iter()
            .any(|line| line.contains("password authentication failed for user \"ada\""))
    );
    assert!(log.iter().any(|line| line.contains("\"mallory\"")));
    assert!(!log.iter().any(|line| line.contains("wrong")));
}

#[test]
fn older_tds_versions_integrated_logins_and_password_changes_are_refused() {
    let server = TestServer::start(8);
    for options in [
        LoginOptions {
            version: 0x7209_0002,
            ..LoginOptions::default()
        },
        LoginOptions {
            option_flags2: 0x83,
            ..LoginOptions::default()
        },
        LoginOptions {
            option_flags3: 0x01,
            ..LoginOptions::default()
        },
    ] {
        assert_eq!(login_errors(&server, &options)[0].0, 18456);
    }
    let log = server.log();
    assert!(log.iter().any(|line| line.contains("EggDB needs TDS 7.4")));
    assert!(log.iter().any(|line| line.contains("SSPI")));
    assert!(
        log.iter()
            .any(|line| line.contains("changing the password"))
    );
}

#[test]
fn a_batch_without_semicolons_creates_inserts_and_selects_with_top_identity_and_brackets() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.batch(
        "CREATE TABLE [dbo].[Bakes] ([id] INT IDENTITY(1,1) NOT NULL, [name] NVARCHAR(40) NOT NULL, CONSTRAINT [PK_Bakes] PRIMARY KEY ([id]))\n\
         INSERT INTO bakes (name) VALUES (N'bread'), (N'egg')\n\
         SELECT TOP 1 [Name] FROM dbo.bakes ORDER BY id DESC\n\
         SELECT @@VERSION",
    );
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    assert_eq!(rows(&tokens)[0], vec![Some("egg".to_owned())], "{tokens:?}");
    assert!(
        rows(&tokens)[1][0]
            .as_deref()
            .is_some_and(|version| version.starts_with("EggDB ")
                && version.contains("compatible with Microsoft SQL Server 2022"))
    );
    let done = dones(&tokens);
    assert_eq!(done[1], (DONE_MORE | DONE_COUNT, 2));
    assert_eq!(done.last().unwrap().0 & DONE_MORE, 0);
    let columns: Vec<&Token> = tokens
        .iter()
        .filter(|token| matches!(token, Token::Columns(_)))
        .collect();
    assert_eq!(columns[0], &Token::Columns(vec![("name".into(), 0xE7)]));
    assert_eq!(columns[1], &Token::Columns(vec![(String::new(), 0xE7)]));
    let typed = client.batch("SELECT id, GETDATE() AS at FROM bakes WHERE id = 1");
    let Token::Columns(described) = typed
        .iter()
        .find(|token| matches!(token, Token::Columns(_)))
        .unwrap()
    else {
        unreachable!()
    };
    assert_eq!(described, &vec![("id".into(), 0x26), ("at".into(), 0x2A)]);
    let explicit = client.batch("INSERT INTO bakes (id, name) VALUES (9, N'jam')");
    assert_eq!(errors(&explicit)[0].0, 544);
    let implied = client.batch("INSERT INTO bakes VALUES (N'jam') SELECT count(*) FROM bakes");
    assert!(errors(&implied).is_empty(), "{implied:?}");
    assert_eq!(rows(&implied), vec![vec![Some("3".to_owned())]]);
}

#[test]
fn a_syntax_error_runs_nothing_and_errors_carry_their_number_severity_and_line() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.batch("CREATE TABLE t (id INT PRIMARY KEY)\nSELECT * FORM t");
    assert_eq!(
        errors(&tokens),
        vec![(102, 15, "Incorrect syntax near 'FORM'.".into())]
    );
    assert!(
        tokens
            .iter()
            .any(|token| matches!(token, Token::Error { line: 2, .. }))
    );
    assert_eq!(dones(&tokens).last().unwrap().0 & DONE_ERROR, DONE_ERROR);
    let missing = client.batch("SELECT * FROM t");
    assert_eq!(errors(&missing)[0].0, 208);
    let tokens = client.batch("SELECT 1\nSELECT * FROM missing\nSELECT 2");
    assert_eq!(
        errors(&tokens),
        vec![(208, 16, "Invalid object name 'missing'.".into())]
    );
    assert!(
        tokens
            .iter()
            .any(|token| matches!(token, Token::Error { line: 2, .. }))
    );
    assert_eq!(rows(&tokens), vec![vec![Some("1".to_owned())]]);
}

#[test]
fn a_constraint_violation_ends_its_statement_but_not_the_batch() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch("CREATE TABLE t (id INT PRIMARY KEY)");
    let tokens = client.batch(
        "INSERT INTO t VALUES (1)\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (2)\nSELECT count(*) FROM t",
    );
    assert_eq!(errors(&tokens)[0].0, 2627);
    assert_eq!(errors(&tokens)[0].1, 14);
    assert_eq!(rows(&tokens), vec![vec![Some("2".to_owned())]]);
}

#[test]
fn transactions_nest_report_their_changes_and_an_error_that_rolls_one_back_ends_the_batch() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch("CREATE TABLE t (id INT PRIMARY KEY)");
    let tokens =
        client.batch("BEGIN TRAN\nBEGIN TRANSACTION\nINSERT INTO t VALUES (1)\nSELECT @@TRANCOUNT");
    let begins = tokens
        .iter()
        .filter(|token| matches!(token, Token::EnvChange { kind: 8, .. }))
        .count();
    assert_eq!(begins, 1, "{tokens:?}");
    assert_eq!(rows(&tokens), vec![vec![Some("2".to_owned())]]);
    assert!(
        dones(&tokens)
            .iter()
            .all(|(status, _)| status & DONE_IN_TRANSACTION != 0)
    );
    let nested = client.batch("COMMIT");
    assert!(
        !nested
            .iter()
            .any(|token| matches!(token, Token::EnvChange { .. }))
    );
    let committed = client.batch("COMMIT TRAN");
    assert!(
        committed
            .iter()
            .any(|token| matches!(token, Token::EnvChange { kind: 9, .. }))
    );
    assert_eq!(errors(&client.batch("COMMIT"))[0].0, 3902);
    assert_eq!(errors(&client.batch("ROLLBACK"))[0].0, 3903);
    let kept_open = client.batch(
        "BEGIN TRAN\nINSERT INTO t VALUES (2)\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (3)\nCOMMIT",
    );
    assert_eq!(errors(&kept_open)[0].0, 2627);
    assert!(
        kept_open
            .iter()
            .any(|token| matches!(token, Token::EnvChange { kind: 9, .. }))
    );
    let rolled_back = client.batch(
        "BEGIN TRAN\nINSERT INTO t VALUES (4)\nINSERT INTO t VALUES (5), (1)\nINSERT INTO t VALUES (6)\nCOMMIT",
    );
    assert_eq!(errors(&rolled_back)[0].0, 2627);
    assert!(
        rolled_back
            .iter()
            .any(|token| matches!(token, Token::EnvChange { kind: 10, .. }))
    );
    assert_eq!(errors(&rolled_back).len(), 1, "{rolled_back:?}");
    let count = client.batch("SELECT count(*) FROM t");
    assert_eq!(rows(&count), vec![vec![Some("3".to_owned())]]);
    assert_eq!(errors(&client.batch("BEGIN SELECT 1 END"))[0].0, 40517);
}

#[test]
fn use_set_nocount_and_unsupported_requests_answer_as_sql_server_does() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let used = client.batch("USE [Shop]");
    assert!(
        used.iter()
            .any(|token| matches!(token, Token::EnvChange { kind: 1, .. }))
    );
    assert_eq!(errors(&client.batch("USE master"))[0].0, 911);
    client.batch("CREATE TABLE t (id INT PRIMARY KEY)");
    let quiet = client.batch("SET NOCOUNT ON\nINSERT INTO t VALUES (1)");
    assert!(
        dones(&quiet)
            .iter()
            .all(|(status, _)| status & DONE_COUNT == 0)
    );
    assert_eq!(
        errors(&client.batch("SET ANSI_NULLS ON\nSET QUOTED_IDENTIFIER ON")).len(),
        0
    );
    assert_eq!(errors(&client.batch("SET XACT_ABORT ON"))[0].0, 40517);
    assert_eq!(errors(&client.batch("SET FOO ON"))[0].0, 195);
    assert_eq!(
        rows(&client.batch("SELECT 'a' + 'b'")),
        vec![vec![Some("ab".to_owned())]]
    );
    assert_eq!(errors(&client.batch("SELECT version()"))[0].0, 195);
    assert_eq!(errors(&client.batch("SELECT @x"))[0].0, 137);
    client.send(0x07, &[1, 2, 3]);
    let (_, body) = client.read_message().unwrap();
    assert_eq!(errors(&common::parse_tokens(&body))[0].0, 40517);
    client.send(0x06, &[]);
    let (_, body) = client.read_message().unwrap();
    assert_eq!(
        dones(&common::parse_tokens(&body)),
        vec![(DONE_ATTENTION, 0)]
    );
    assert_eq!(
        rows(&client.batch("SELECT 1")),
        vec![vec![Some("1".to_owned())]]
    );
}

#[test]
fn large_results_and_batches_span_packets_of_the_negotiated_size() {
    let server = TestServer::start(8);
    for size in [512u32, 32_767] {
        let (mut client, tokens) = Client::login_with(
            server.address,
            &LoginOptions {
                packet_size: size,
                ..LoginOptions::default()
            },
        );
        assert!(errors(&tokens).is_empty());
        assert_eq!(client.packet_size, size as usize);
        let long = "x".repeat(300_000);
        let tokens = client.batch(&format!("SELECT N'{long}' AS v, 1 AS one"));
        assert_eq!(rows(&tokens)[0][0].as_deref(), Some(long.as_str()));
        let mut statements = String::new();
        for index in 0..2_000 {
            statements.push_str(&format!("SELECT {index} AS n\n"));
        }
        let tokens = client.batch(&statements);
        assert_eq!(rows(&tokens).len(), 2_000);
    }
}

#[test]
fn the_65th_connection_is_refused_after_its_login_without_a_password_check() {
    let server = TestServer::start(1);
    let _first = Client::login(server.address);
    let (_, tokens) = Client::login_with(
        server.address,
        &LoginOptions {
            password: "wrong",
            ..LoginOptions::default()
        },
    );
    assert_eq!(errors(&tokens)[0].0, 17809);
    assert!(
        !server
            .log()
            .iter()
            .any(|line| line.contains("password authentication failed"))
    );
}

#[test]
fn a_stop_ends_logged_in_connections_and_rolls_back_their_transactions() {
    let mut server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch("CREATE TABLE t (id INT PRIMARY KEY) BEGIN TRAN INSERT INTO t VALUES (1)");
    assert_eq!(server.stop(), 1);
    assert!(client.is_closed());
}

#[test]
fn a_malformed_batch_header_ends_the_connection_with_a_protocol_error() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let mut body = batch_body("SELECT 1");
    body[0] = 200;
    client.send(0x01, &body);
    let (_, reply) = client.read_message().unwrap();
    assert_eq!(errors(&common::parse_tokens(&reply))[0].0, 4002);
    assert!(client.is_closed());
}

#[test]
fn a_connection_reads_oracle_sql_after_set_eggdb_dialect_and_keeps_its_transaction_requests() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let created = client.batch(
        "CREATE TABLE emp (id INT PRIMARY KEY, name NVARCHAR(20))\n\
         SET eggdb.dialect oracle\n\
         INSERT INTO emp VALUES (1, 'Ada')",
    );
    assert!(errors(&created).is_empty(), "{created:?}");
    let listed = client.batch(
        "SELECT table_name FROM user_tables; SELECT NVL(name, 'x') AS name FROM emp WHERE ROWNUM <= 1",
    );
    assert!(errors(&listed).is_empty(), "{listed:?}");
    assert_eq!(
        rows(&listed),
        vec![vec![Some("EMP".to_owned())], vec![Some("Ada".to_owned())]]
    );
    let begun = client.transaction(5, &[0, 0]);
    assert!(errors(&begun).is_empty(), "{begun:?}");
    client.batch("INSERT INTO emp VALUES (2, 'Bob')");
    let rolled_back = client.transaction(8, &[0, 0]);
    assert!(errors(&rolled_back).is_empty(), "{rolled_back:?}");
    assert_eq!(
        rows(&client.batch("SELECT COUNT(*) AS n FROM emp")),
        vec![vec![Some("1".to_owned())]]
    );
    let back = client.batch("SET eggdb.dialect = sqlserver\nSELECT TOP 1 name FROM emp");
    assert_eq!(rows(&back), vec![vec![Some("Ada".to_owned())]], "{back:?}");
}
