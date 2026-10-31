//! The MySQL protocol end to end over real sockets with a client written from the protocol
//! documentation: both authentication methods and the switch between them, text queries with
//! several statements, prepared statements with binary parameters and rows, and the errors,
//! warnings and status flags a MySQL client reads.

mod common;

use common::{Client, DATABASE, PASSWORD, Reply, TestServer, USER, handshake_response, texts};
use eggdb_mysqlwire::caching_sha2_response;

#[test]
fn the_handshake_offers_caching_sha2_without_tls_and_names_eggdb() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    let handshake = client.handshake();
    assert_eq!(handshake.plugin, "caching_sha2_password");
    assert_eq!(handshake.version, "8.0.36-EggDB");
    assert!(!handshake.version.contains("MariaDB"));
    assert_eq!(handshake.capabilities & (1 << 11), 0, "no CLIENT_SSL");
    assert_eq!(handshake.capabilities & (1 << 5), 0, "no CLIENT_COMPRESS");
    assert_eq!(
        handshake.capabilities & (1 << 7),
        0,
        "no CLIENT_LOCAL_FILES"
    );
    assert_ne!(handshake.capabilities & (1 << 9), 0, "CLIENT_PROTOCOL_41");
    assert!(handshake.nonce.iter().all(|&byte| byte != 0));
}

#[test]
fn every_authentication_method_logs_in_and_wrong_passwords_are_refused() {
    let server = TestServer::start(8);
    for plugin in [
        "caching_sha2_password",
        "mysql_native_password",
        "sha256_password",
    ] {
        let (_, reply) = Client::login_with(server.address, PASSWORD, plugin);
        assert!(matches!(reply, Reply::Ok { .. }), "{plugin}: {reply:?}");
        let (_, reply) = Client::login_with(server.address, "wrong", plugin);
        assert_eq!(
            reply,
            Reply::Err {
                code: 1045,
                state: "28000".into(),
                message: format!(
                    "Access denied for user '{USER}'@'localhost' (using password: YES)"
                ),
            },
            "{plugin}"
        );
    }
    let (_, reply) = Client::login_with(server.address, "", "caching_sha2_password");
    assert!(
        matches!(reply, Reply::Err { code: 1045, ref message, .. } if message.ends_with("(using password: NO)"))
    );
    assert!(
        server
            .logged
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("password authentication failed"))
    );
}

#[test]
fn an_unknown_user_or_database_is_refused() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    let handshake = client.handshake();
    let response = caching_sha2_response(PASSWORD.as_bytes(), &handshake.nonce);
    client.send(&handshake_response(
        "mallory",
        &response,
        DATABASE,
        "caching_sha2_password",
    ));
    let reply = client.read().unwrap();
    assert_eq!(
        client.parse_reply(reply),
        Reply::Err {
            code: 1045,
            state: "28000".into(),
            message: "Access denied for user 'mallory'@'localhost' (using password: YES)".into(),
        }
    );
    let mut client = Client::connect(server.address);
    let handshake = client.handshake();
    let response = caching_sha2_response(PASSWORD.as_bytes(), &handshake.nonce);
    client.send(&handshake_response(
        USER,
        &response,
        "other",
        "caching_sha2_password",
    ));
    assert_eq!(client.read(), Some(vec![0x01, 0x03]));
    let reply = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(reply),
        Reply::Err { code: 1049, .. }
    ));
}

#[test]
fn queries_answer_with_rows_ok_packets_and_mysql_errors() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    assert_eq!(
        texts(&client.query("select @@version_comment limit 1")),
        vec![vec!["EggDB".to_owned()]]
    );
    assert!(matches!(
        client.query(
            "CREATE TABLE items (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20), price DOUBLE)"
        ),
        Reply::Ok { .. }
    ));
    let inserted =
        client.query("INSERT INTO items (name, price) VALUES ('egg', 1.5), ('jam', NULL)");
    assert!(
        matches!(
            inserted,
            Reply::Ok {
                affected: 2,
                last_insert_id: 1,
                ..
            }
        ),
        "{inserted:?}"
    );
    let reply = client.query("SELECT id, name, price FROM items ORDER BY id");
    let Reply::Rows { columns, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert_eq!(
        columns.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        vec![3, 253, 5]
    );
    assert_eq!(
        texts(&reply),
        vec![vec!["1", "egg", "1.5"], vec!["2", "jam", "NULL"]]
    );
    assert_eq!(
        client.query("SELECT * FROM missing"),
        Reply::Err {
            code: 1146,
            state: "42S02".into(),
            message: format!("Table '{DATABASE}.missing' doesn't exist"),
        }
    );
    let ignored = client.query("INSERT IGNORE INTO items VALUES (1, 'dup', 0)");
    assert!(
        matches!(
            ignored,
            Reply::Ok {
                affected: 0,
                warnings: 1,
                ..
            }
        ),
        "{ignored:?}"
    );
    assert_eq!(texts(&client.query("SHOW WARNINGS"))[0][1], "1062");
    assert!(matches!(
        client.query("USE other"),
        Reply::Err { code: 1049, .. }
    ));
}

#[test]
fn several_statements_answer_in_order_with_more_results_flags() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let first = client
        .query("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1); SELECT id FROM t");
    assert!(
        matches!(first, Reply::Ok { status, .. } if status & 8 != 0),
        "{first:?}"
    );
    let second = client.next_reply();
    assert!(
        matches!(second, Reply::Ok { affected: 1, status, .. } if status & 8 != 0),
        "{second:?}"
    );
    let third = client.next_reply();
    assert!(
        matches!(third, Reply::Rows { status, .. } if status & 8 == 0),
        "{third:?}"
    );
    let failed = client.query("SELECT 1; SELECT * FROM missing; SELECT 2");
    assert!(matches!(failed, Reply::Rows { status, .. } if status & 8 != 0));
    assert!(matches!(client.next_reply(), Reply::Err { code: 1146, .. }));
    client.command(0x1B, &1u16.to_le_bytes());
    assert_eq!(client.read().unwrap()[0], 0xFE);
    assert!(matches!(
        client.query("SELECT 1; SELECT 2"),
        Reply::Err { code: 1064, .. }
    ));
    assert!(matches!(client.query("SELECT 1"), Reply::Rows { .. }));
}

#[test]
fn transactions_report_their_status_and_roll_back() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.query("CREATE TABLE t (id INT PRIMARY KEY)");
    let begun = client.query("START TRANSACTION");
    assert!(
        matches!(begun, Reply::Ok { status, .. } if status & 1 == 1),
        "{begun:?}"
    );
    client.query("INSERT INTO t VALUES (1)");
    let rolled_back = client.query("ROLLBACK");
    assert!(matches!(rolled_back, Reply::Ok { status, .. } if status & 1 == 0 && status & 2 == 2));
    assert_eq!(
        texts(&client.query("SELECT COUNT(*) FROM t")),
        vec![vec!["0".to_owned()]]
    );
    let off = client.query("SET autocommit = 0");
    assert!(
        matches!(off, Reply::Ok { status, .. } if status & 2 == 0),
        "{off:?}"
    );
}

fn prepare(client: &mut Client, text: &str) -> (u32, u16, u16) {
    client.command(0x16, text.as_bytes());
    let ok = client.read().unwrap();
    assert_eq!(ok[0], 0, "{:?}", client.parse_reply(ok.clone()));
    let id = u32::from_le_bytes([ok[1], ok[2], ok[3], ok[4]]);
    let columns = u16::from_le_bytes([ok[5], ok[6]]);
    let parameters = u16::from_le_bytes([ok[7], ok[8]]);
    for _ in 0..parameters {
        client.read().unwrap();
    }
    if parameters > 0 {
        assert_eq!(client.read().unwrap()[0], 0xFE);
    }
    for _ in 0..columns {
        client.read().unwrap();
    }
    if columns > 0 {
        assert_eq!(client.read().unwrap()[0], 0xFE);
    }
    (id, columns, parameters)
}

#[test]
fn prepared_statements_bind_binary_parameters_and_return_binary_rows() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.query("CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT, price DOUBLE, ok BOOLEAN)");
    let (insert, columns, parameters) =
        prepare(&mut client, "INSERT INTO items VALUES (?, ?, ?, ?)");
    assert_eq!((columns, parameters), (0, 4));
    let mut execute = insert.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0]);
    execute.push(0b1000);
    execute.push(1);
    execute.extend_from_slice(&[8, 0, 253, 0, 5, 0, 1, 0]);
    execute.extend_from_slice(&7i64.to_le_bytes());
    execute.push(3);
    execute.extend_from_slice(b"egg");
    execute.extend_from_slice(&2.5f64.to_le_bytes());
    client.command(0x17, &execute);
    let reply = client.next_reply();
    assert!(matches!(reply, Reply::Ok { affected: 1, .. }), "{reply:?}");
    let (select, columns, parameters) = prepare(
        &mut client,
        "SELECT id, name, price, ok FROM items WHERE id = ?",
    );
    assert_eq!((columns, parameters), (4, 1));
    let mut execute = select.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0]);
    execute.extend_from_slice(&7i32.to_le_bytes());
    client.command(0x17, &execute);
    let count = client.read().unwrap();
    assert_eq!(count, vec![4]);
    for _ in 0..5 {
        client.read().unwrap();
    }
    let row = client.read().unwrap();
    assert_eq!(row[0], 0);
    assert_eq!(row[1], 0b10_0000, "the fourth column, NULL, at bit 3 + 2");
    assert_eq!(&row[2..10], &7i64.to_le_bytes());
    assert_eq!(&row[10..14], b"\x03egg");
    assert_eq!(&row[14..22], &2.5f64.to_le_bytes());
    assert_eq!(client.read().unwrap()[0], 0xFE);
    client.command(0x19, &select.to_le_bytes());
    let mut execute = select.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0]);
    client.command(0x17, &execute);
    assert!(matches!(client.next_reply(), Reply::Err { code: 1243, .. }));
    client.command(0x0E, &[]);
    assert!(matches!(client.next_reply(), Reply::Ok { .. }));
}

#[test]
fn decimal_date_time_and_json_columns_use_their_mysql_types_in_text_and_binary() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    assert!(matches!(
        client.query(
            "CREATE TABLE ledger (id INT PRIMARY KEY, amount DECIMAL(8,2), booked DATE, at DATETIME(3), stamp TIMESTAMP, doc JSON)"
        ),
        Reply::Ok { .. }
    ));
    let inserted = client.query(
        "INSERT INTO ledger VALUES (1, 19.99, '2024-02-29', '2024-02-29 13:45:01.5', '2024-02-29 00:00:00', '{\"b\": 1, \"a\": 2}')",
    );
    assert!(
        matches!(inserted, Reply::Ok { affected: 1, .. }),
        "{inserted:?}"
    );
    let reply = client.query("SELECT amount, booked, at, stamp, doc FROM ledger");
    let Reply::Rows { columns, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert_eq!(
        columns.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        vec![246, 10, 12, 7, 245]
    );
    assert_eq!(
        texts(&reply),
        vec![vec![
            "19.99",
            "2024-02-29",
            "2024-02-29 13:45:01.5",
            "2024-02-29 00:00:00",
            "{\"a\": 2, \"b\": 1}"
        ]]
    );
    let (select, columns, parameters) = prepare(
        &mut client,
        "SELECT amount, booked, at, stamp FROM ledger WHERE id = ?",
    );
    assert_eq!((columns, parameters), (4, 1));
    let mut execute = select.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0]);
    execute.extend_from_slice(&1i32.to_le_bytes());
    client.command(0x17, &execute);
    assert_eq!(client.read().unwrap(), vec![4]);
    for _ in 0..5 {
        client.read().unwrap();
    }
    let row = client.read().unwrap();
    let mut expected = vec![0u8, 0];
    expected.extend_from_slice(b"\x0519.99");
    expected.extend_from_slice(&[4, 0xE8, 0x07, 2, 29]);
    expected.extend_from_slice(&[11, 0xE8, 0x07, 2, 29, 13, 45, 1]);
    expected.extend_from_slice(&500_000u32.to_le_bytes());
    expected.extend_from_slice(&[4, 0xE8, 0x07, 2, 29]);
    assert_eq!(row, expected);
    assert_eq!(client.read().unwrap()[0], 0xFE);
    assert!(matches!(
        client.query("INSERT INTO ledger (id, amount) VALUES (2, 1234567.5)"),
        Reply::Err { code: 1264, .. }
    ));
    assert!(matches!(
        client.query("INSERT INTO ledger (id, booked) VALUES (3, '12345-01-01')"),
        Reply::Ok { .. }
    ));
    let mut execute = select.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0]);
    execute.extend_from_slice(&3i32.to_le_bytes());
    client.command(0x17, &execute);
    assert_eq!(client.read().unwrap(), vec![4]);
    for _ in 0..5 {
        client.read().unwrap();
    }
    let refused = client.next_reply();
    assert!(
        matches!(refused, Reply::Err { code: 1292, .. }),
        "{refused:?}"
    );
    assert_eq!(
        texts(&client.query("SELECT booked FROM ledger WHERE id = 3")),
        vec![vec!["12345-01-01"]]
    );
}

#[test]
fn time_and_blob_columns_use_their_mysql_types_in_text_and_binary() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    assert!(matches!(
        client.query(
            "CREATE TABLE clips (id INT PRIMARY KEY, starts TIME(3), body BLOB, tag VARBINARY(16))"
        ),
        Reply::Ok { .. }
    ));
    let inserted = client.query("INSERT INTO clips VALUES (1, '10:30:15.25', 'abc', 'x')");
    assert!(
        matches!(inserted, Reply::Ok { affected: 1, .. }),
        "{inserted:?}"
    );
    let (insert, _, parameters) =
        prepare(&mut client, "INSERT INTO clips (id, body) VALUES (?, ?)");
    assert_eq!(parameters, 2);
    let mut execute = insert.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0, 0xFC, 0]);
    execute.extend_from_slice(&2i32.to_le_bytes());
    execute.extend_from_slice(&[3, 0xFF, 0x00, b'a']);
    client.command(0x17, &execute);
    let reply = client.next_reply();
    assert!(matches!(reply, Reply::Ok { affected: 1, .. }), "{reply:?}");
    for (id, kind, value) in [
        (3i32, 253u8, b"\\x41".to_vec()),
        (4, 0xFC, b"C:\\dir".to_vec()),
        (5, 254, br#"{"a": "\\u0041"}"#.to_vec()),
    ] {
        let mut execute = insert.to_le_bytes().to_vec();
        execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0, kind, 0]);
        execute.extend_from_slice(&id.to_le_bytes());
        execute.push(value.len() as u8);
        execute.extend_from_slice(&value);
        client.command(0x17, &execute);
        let reply = client.next_reply();
        assert!(matches!(reply, Reply::Ok { affected: 1, .. }), "{reply:?}");
    }
    assert_eq!(
        texts(&client.query(
            "SELECT id, LENGTH(body), body, CONCAT('x', body) FROM clips WHERE id IN (1, 3, 4, 5) ORDER BY id"
        )),
        vec![
            vec!["1", "3", "abc", "xabc"],
            vec!["3", "4", "\\x41", "x\\x41"],
            vec!["4", "6", "C:\\dir", "xC:\\dir"],
            vec!["5", "16", r#"{"a": "\\u0041"}"#, r#"x{"a": "\\u0041"}"#],
        ]
    );
    let reply = client.query("SELECT starts, body, LENGTH(body) FROM clips WHERE id = 1");
    let Reply::Rows { columns, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert_eq!(
        columns.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        vec![11, 252, 3]
    );
    assert_eq!(texts(&reply), vec![vec!["10:30:15.25", "abc", "3"]]);
    assert_eq!(
        texts(&client.query("SELECT LENGTH(body) FROM clips WHERE id = 2")),
        vec![vec!["3"]]
    );
    let (select, columns, parameters) =
        prepare(&mut client, "SELECT starts, body FROM clips WHERE id = ?");
    assert_eq!((columns, parameters), (2, 1));
    let mut execute = select.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 3, 0]);
    execute.extend_from_slice(&1i32.to_le_bytes());
    client.command(0x17, &execute);
    assert_eq!(client.read().unwrap(), vec![2]);
    for _ in 0..3 {
        client.read().unwrap();
    }
    let row = client.read().unwrap();
    let mut expected = vec![0u8, 0];
    expected.extend_from_slice(&[12, 0, 0, 0, 0, 0, 10, 30, 15]);
    expected.extend_from_slice(&250_000u32.to_le_bytes());
    expected.extend_from_slice(b"\x03abc");
    assert_eq!(row, expected);
}

#[test]
fn long_data_and_resets_fill_parameters() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let (statement, _, _) = prepare(&mut client, "SELECT CONCAT(?, '!')");
    let mut long = statement.to_le_bytes().to_vec();
    long.extend_from_slice(&0u16.to_le_bytes());
    long.extend_from_slice(b"hello");
    client.command(0x18, &long);
    let mut execute = statement.to_le_bytes().to_vec();
    execute.extend_from_slice(&[0, 1, 0, 0, 0, 0, 1, 252, 0]);
    client.command(0x17, &execute);
    let count = client.read().unwrap();
    assert_eq!(count, vec![1]);
    client.read().unwrap();
    client.read().unwrap();
    let row = client.read().unwrap();
    assert_eq!(&row[2..], b"\x06hello!");
    client.read().unwrap();
    client.command(0x1A, &statement.to_le_bytes());
    assert!(matches!(client.next_reply(), Reply::Ok { .. }));
}

#[test]
fn unknown_commands_and_a_reset_connection_answer_as_mysql_does() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.command(0x09, &[]);
    assert_eq!(
        client.next_reply(),
        Reply::Err {
            code: 1047,
            state: "08S01".into(),
            message: "Unknown command".into(),
        }
    );
    client.query("SET @a = 5");
    client.command(0x1F, &[]);
    assert!(matches!(client.next_reply(), Reply::Ok { .. }));
    assert_eq!(
        texts(&client.query("SELECT @a")),
        vec![vec!["NULL".to_owned()]]
    );
    client.command(0x02, DATABASE.as_bytes());
    assert!(matches!(client.next_reply(), Reply::Ok { .. }));
    client.command(0x01, &[]);
    assert_eq!(client.read(), None);
}

#[test]
fn a_stop_ends_idle_connections_with_error_1053() {
    let mut server = TestServer::start(8);
    let mut client = Client::login(server.address);
    assert_eq!(server.stop(), 1);
    let reply = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(reply),
        Reply::Err { code: 1053, .. }
    ));
}

#[test]
fn connections_over_the_limit_get_error_1040() {
    let server = TestServer::start(1);
    let _first = Client::login(server.address);
    let mut second = Client::connect(server.address);
    let refused = second.read().unwrap();
    assert!(matches!(
        second.parse_reply(refused),
        Reply::Err { code: 1040, .. }
    ));
}

#[test]
fn empty_queries_and_too_many_placeholders_answer_as_mysql_does() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    for empty in ["", "   ", ";", "/* nothing */"] {
        assert_eq!(
            client.query(empty),
            Reply::Err {
                code: 1065,
                state: "42000".into(),
                message: "Query was empty".into(),
            },
            "{empty:?}"
        );
    }
    client.command(0x16, b"  ");
    assert!(matches!(client.next_reply(), Reply::Err { code: 1065, .. }));
    let mut many = "SELECT ?".to_owned();
    many.push_str(&", ?".repeat(65_535));
    client.command(0x16, many.as_bytes());
    assert!(matches!(client.next_reply(), Reply::Err { code: 1390, .. }));
    assert!(matches!(
        client.query("SELECT ?"),
        Reply::Err { code: 1064, .. }
    ));
    assert!(matches!(client.query("SELECT 1"), Reply::Rows { .. }));
}

#[test]
fn the_server_refuses_to_serve_an_empty_password() {
    let store = common::SharedMemoryStore::default();
    let listener = eggdb_mysqlwire::listen_on_loopback("127.0.0.1:0".parse().unwrap()).unwrap();
    let config = eggdb_mysqlwire::ServerConfig {
        user: USER.to_owned(),
        credentials: eggdb_mysqlwire::MySqlCredentials::derive(b""),
        database: DATABASE.to_owned(),
        max_connections: 4,
        authentication_timeout: std::time::Duration::from_secs(1),
        tls: None,
    };
    let refused = eggdb_mysqlwire::serve(&store, &listener, &config, &|| false, &|_| {});
    assert_eq!(
        refused.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::InvalidInput)
    );
}
