//! The PostgreSQL wire protocol spoken over a real loopback socket by a minimal client written
//! here: startup, SCRAM-SHA-256, the simple and extended query protocols, errors and recovery,
//! transaction blocks, concurrent connections, the connection limit and a graceful stop.

mod common;

use common::{Client, DATABASE, Message, TestServer, USER, cstring, ready_status, tags};
use std::time::Duration;

fn bind(
    portal: &str,
    statement: &str,
    formats: &[i16],
    values: &[Option<&[u8]>],
    results: &[i16],
) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend(cstring(statement));
    body.extend_from_slice(&(formats.len() as i16).to_be_bytes());
    for format in formats {
        body.extend_from_slice(&format.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for value in values {
        match value {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    body.extend_from_slice(&(results.len() as i16).to_be_bytes());
    for format in results {
        body.extend_from_slice(&format.to_be_bytes());
    }
    body
}

fn parse(name: &str, query: &str, types: &[u32]) -> Vec<u8> {
    let mut body = cstring(name);
    body.extend(cstring(query));
    body.extend_from_slice(&(types.len() as i16).to_be_bytes());
    for oid in types {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .collect()
}

#[test]
fn startup_refuses_encryption_authenticates_and_reports_parameters() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_request(80_877_103);
    assert_eq!(client.read_byte(), Some(b'N'));
    client.send_request(80_877_104);
    assert_eq!(client.read_byte(), Some(b'N'));
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("extra_float_digits", "3"),
            ("no_such_option", "ignored"),
        ],
    );
    let ok = client.authenticate(common::PASSWORD);
    assert_eq!(ok.tag, b'R');
    let greeting = client.read_until_ready();
    let statuses: Vec<String> = greeting
        .iter()
        .filter(|message| message.tag == b'S')
        .map(Message::text)
        .collect();
    for expected in [
        "server_version\u{0}16.0 (EggDB",
        "client_encoding\0UTF8",
        "server_encoding\0UTF8",
        "DateStyle\0ISO, MDY",
        "TimeZone\0UTC",
        "integer_datetimes\0on",
        "standard_conforming_strings\0on",
        "session_authorization\0ada",
        "is_superuser\0on",
    ] {
        assert!(
            statuses.iter().any(|status| status.starts_with(expected)),
            "{expected:?} in {statuses:?}"
        );
    }
    assert!(greeting.iter().any(|message| message.tag == b'K'));
    assert_eq!(ready_status(&greeting), b'I');
    let answer = client.query("SELECT current_user, current_database(), version()");
    let row = &rows(&answer)[0];
    assert_eq!(row[0].as_deref(), Some(USER));
    assert_eq!(row[1].as_deref(), Some(DATABASE));
    assert!(
        row[2]
            .as_deref()
            .unwrap()
            .starts_with("PostgreSQL 16.0 compatible")
    );
}

#[test]
fn a_wrong_password_an_unknown_user_and_another_database_are_refused() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER), ("database", DATABASE)]);
    let refused = client.authenticate("wrong");
    assert_eq!((refused.tag, refused.code()), (b'E', "28P01".to_owned()));
    assert_eq!(refused.field(b'S').as_deref(), Some("FATAL"));
    assert_eq!(client.read(), None);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", "mallory")]);
    let refused = client.authenticate(common::PASSWORD);
    assert_eq!(refused.code(), "28P01");
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER), ("database", "other")]);
    let ok = client.authenticate(common::PASSWORD);
    assert_eq!(ok.tag, b'R');
    let refused = client.read().unwrap();
    assert_eq!(refused.code(), "3D000");
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("database", DATABASE)]);
    assert_eq!(client.read().unwrap().code(), "28000");
    let mut client = Client::connect(server.address);
    client.send_startup(0x0002_0000, &[]);
    assert_eq!(client.read().unwrap().code(), "0A000");
    let logged = server.logged.lock().unwrap().join("\n");
    assert!(
        logged.contains("password authentication failed for user \"ada\""),
        "{logged}"
    );
    assert!(
        logged.contains("database \"other\" does not exist"),
        "{logged}"
    );
}

#[test]
fn a_cancel_request_is_answered_by_closing() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    let mut body = 80_877_102u32.to_be_bytes().to_vec();
    body.extend_from_slice(&1i32.to_be_bytes());
    body.extend_from_slice(&2i32.to_be_bytes());
    let mut packet = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    packet.extend(body);
    client.send_raw(&packet);
    assert_eq!(client.read_byte(), None);
}

#[test]
fn simple_queries_run_statements_and_a_join() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query(
        "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL, price double precision);
         CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity smallint);
         INSERT INTO items VALUES (1, 'egg', 0.25), (2, 'bread', 2.5);
         INSERT INTO sales VALUES (1, 1, 3), (2, 2, 1), (3, 1, 4);",
    );
    assert_eq!(tags(&answer), "CCCCZ");
    let completed: Vec<String> = answer[..4].iter().map(Message::text).collect();
    assert_eq!(
        completed,
        ["CREATE TABLE", "CREATE TABLE", "INSERT 0 2", "INSERT 0 3"]
    );
    let answer = client.query(
        "SELECT i.name, sum(s.quantity) AS sold, i.price FROM items i JOIN sales s ON s.item_id = i.id GROUP BY i.name, i.price ORDER BY i.name",
    );
    assert_eq!(tags(&answer), "TDDCZ");
    assert_eq!(
        answer[0].column_types(),
        [
            ("name".to_owned(), 25, 0),
            ("sold".to_owned(), 20, 0),
            ("price".to_owned(), 701, 0)
        ]
    );
    assert_eq!(
        rows(&answer),
        [
            vec![Some("bread".into()), Some("1".into()), Some("2.5".into())],
            vec![Some("egg".into()), Some("7".into()), Some("0.25".into())],
        ]
    );
    assert_eq!(answer[3].text(), "SELECT 2");
    let answer = client.query("UPDATE items SET price = NULL WHERE id = 2; DELETE FROM sales WHERE id = 3; SELECT price FROM items WHERE id = 2");
    assert_eq!(answer[0].text(), "UPDATE 1");
    assert_eq!(answer[1].text(), "DELETE 1");
    assert_eq!(rows(&answer), [vec![None]]);
    assert_eq!(tags(&client.query("  ;  ")), "IZ");
    assert_eq!(tags(&client.query("")), "IZ");
}

#[test]
fn errors_carry_their_sqlstate_and_position_and_blocks_fail_until_rollback() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query("SELECT * FROM missing");
    assert_eq!(tags(&answer), "EZ");
    assert_eq!(answer[0].code(), "42P01");
    assert_eq!(answer[0].field(b'S').as_deref(), Some("ERROR"));
    assert_eq!(answer[0].field(b'V').as_deref(), Some("ERROR"));
    assert_eq!(answer[0].field(b'P').as_deref(), Some("15"));
    assert_eq!(ready_status(&answer), b'I');
    client.query("CREATE TABLE t (id integer PRIMARY KEY)");
    assert_eq!(ready_status(&client.query("BEGIN")), b'T');
    assert_eq!(
        ready_status(&client.query("INSERT INTO t VALUES (1)")),
        b'T'
    );
    let failed = client.query("INSERT INTO t VALUES (1)");
    assert_eq!(
        (failed[0].code(), ready_status(&failed)),
        ("23505".to_owned(), b'E')
    );
    let aborted = client.query("SELECT 1");
    assert_eq!(
        (aborted[0].code(), ready_status(&aborted)),
        ("25P02".to_owned(), b'E')
    );
    let rolled_back = client.query("COMMIT");
    assert_eq!(
        (rolled_back[0].text(), ready_status(&rolled_back)),
        ("ROLLBACK".to_owned(), b'I')
    );
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM t")),
        [vec![Some("0".into())]]
    );
    let warned = client.query("COMMIT");
    assert_eq!(tags(&warned), "NCZ");
    assert_eq!(warned[0].code(), "25P01");
    assert_eq!(warned[0].field(b'S').as_deref(), Some("WARNING"));
}

#[test]
fn set_and_show_report_parameter_changes() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query("SET application_name = 'reports'; SHOW application_name");
    assert_eq!(answer[0].text(), "SET");
    assert!(
        answer
            .iter()
            .any(|message| message.tag == b'S' && message.text() == "application_name\0reports")
    );
    assert_eq!(rows(&answer), [vec![Some("reports".into())]]);
    let refused = client.query("SET client_encoding = 'LATIN1'");
    assert_eq!(refused[0].code(), "0A000");
    let answer = client.query("SET extra_float_digits = 3");
    assert_eq!(tags(&answer), "CZ");
}

#[test]
fn the_extended_protocol_binds_text_and_binary_parameters() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE people (id integer PRIMARY KEY, name text, age smallint)");
    client.send(
        b'P',
        &parse("", "INSERT INTO people VALUES ($1, $2, $3)", &[23, 0, 0]),
    );
    client.send(
        b'B',
        &bind(
            "",
            "",
            &[1, 0, 0],
            &[Some(&7i32.to_be_bytes()), Some(b"Ada"), None],
            &[],
        ),
    );
    client.send(b'D', b"P\0");
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12nCZ");
    assert_eq!(answer[3].text(), "INSERT 0 1");
    client.send(
        b'P',
        &parse(
            "by_age",
            "SELECT id, name, age FROM people WHERE age IS NULL OR age > $1",
            &[],
        ),
    );
    client.send(b'D', b"Sby_age\0");
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "1tTZ");
    assert_eq!(answer[1].body, [0, 1, 0, 0, 0, 25]);
    assert_eq!(
        answer[2].column_types(),
        [
            ("id".to_owned(), 23, 0),
            ("name".to_owned(), 25, 0),
            ("age".to_owned(), 21, 0)
        ]
    );
    client.send(b'B', &bind("p1", "by_age", &[], &[Some(b"30")], &[1]));
    client.send(b'D', b"Pp1\0");
    client.send(b'E', &execute("p1", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "2TDCZ");
    assert_eq!(answer[1].column_types()[0], ("id".to_owned(), 23, 1));
    assert_eq!(
        answer[2].raw_row(),
        [
            Some(7i32.to_be_bytes().to_vec()),
            Some(b"Ada".to_vec()),
            None
        ]
    );
    assert_eq!(answer[3].text(), "SELECT 1");
    client.send(b'C', b"Pp1\0");
    client.send(b'C', b"Sby_age\0");
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "33Z");
}

#[test]
fn portals_return_rows_in_batches_and_errors_skip_to_sync() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query(
        "CREATE TABLE n (x integer PRIMARY KEY); INSERT INTO n VALUES (1), (2), (3), (4), (5)",
    );
    client.send(b'P', &parse("", "SELECT x FROM n ORDER BY x", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 2));
    client.send(b'E', &execute("", 2));
    client.send(b'E', &execute("", 2));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12DDsDDsDCZ");
    assert_eq!(answer[9].text(), "SELECT 1");
    client.send(b'P', &parse("", "SELECT * FROM missing", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12EZ");
    assert_eq!(answer[2].code(), "42P01");
    client.send(b'B', &bind("", "nope", &[], &[], &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[0].code(), "26000");
    client.send(b'P', &parse("", "SELECT $1", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[1].code(), "08P01");
    client.send(b'P', &parse("", "SELECT 1; SELECT 2", &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[0].code(), "42601");
    client.send(b'P', &parse("", "", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "12IZ");
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM n")),
        [vec![Some("5".into())]]
    );
}

#[test]
fn two_connections_share_the_database_and_a_second_writer_waits_then_fails() {
    let server = TestServer::start(8);
    let (mut first, _) = Client::login(server.address);
    let (mut second, _) = Client::login(server.address);
    first.query("CREATE TABLE t (id integer PRIMARY KEY)");
    assert_eq!(tags(&second.query("INSERT INTO t VALUES (1)")), "CZ");
    assert_eq!(
        rows(&first.query("SELECT id FROM t")),
        [vec![Some("1".into())]]
    );
    first.query("BEGIN; INSERT INTO t VALUES (2)");
    let busy = second.query("INSERT INTO t VALUES (3)");
    assert_eq!(busy[0].code(), "55P03");
    assert_eq!(
        rows(&second.query("SELECT count(*) FROM t")),
        [vec![Some("1".into())]]
    );
    first.query("COMMIT");
    assert_eq!(tags(&second.query("INSERT INTO t VALUES (3)")), "CZ");
    assert_eq!(
        rows(&first.query("SELECT count(*) FROM t")),
        [vec![Some("3".into())]]
    );
}

#[test]
fn connections_over_the_limit_get_too_many_connections() {
    let server = TestServer::start(1);
    let (mut first, _) = Client::login(server.address);
    let mut second = Client::connect(server.address);
    second.send_request(80_877_103);
    assert_eq!(second.read_byte(), Some(b'N'));
    second.send_startup(196_608, &[("user", USER)]);
    let refused = second.read().unwrap();
    assert_eq!(refused.code(), "53300");
    assert_eq!(tags(&first.query("SELECT 1")), "TDCZ");
}

#[test]
fn a_stop_ends_idle_connections_with_admin_shutdown() {
    let mut server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE t (id integer PRIMARY KEY); BEGIN; INSERT INTO t VALUES (1)");
    let open = server.stop();
    assert_eq!(open, 1);
    let goodbye = client.read().unwrap();
    assert_eq!((goodbye.tag, goodbye.code()), (b'E', "57P01".to_owned()));
    assert_eq!(goodbye.field(b'S').as_deref(), Some("FATAL"));
}

#[test]
fn a_slow_client_is_cut_off_at_the_authentication_timeout() {
    let server = TestServer::start_with(8, Duration::from_millis(600));
    let mut client = Client::connect(server.address);
    client.send_raw(&[0, 0]);
    let started = std::time::Instant::now();
    assert_eq!(client.read_byte(), None);
    assert!(started.elapsed() < Duration::from_secs(10));
    let (mut working, _) = Client::login(server.address);
    assert_eq!(tags(&working.query("SELECT 1")), "TDCZ");
}

#[test]
fn unsupported_startup_settings_and_function_calls_are_answered_without_hanging() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("client_encoding", "LATIN1"),
            ("_pq_.unknown_option", "1"),
        ],
    );
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    let greeting = client.read_until_ready();
    assert_eq!(greeting[0].tag, b'v');
    assert!(
        greeting
            .iter()
            .any(|message| message.text() == "client_encoding\0UTF8")
    );
    client.send(b'F', &[0, 0, 0, 1]);
    let answer = client.read_until_ready();
    assert_eq!(
        (tags(&answer), answer[0].code()),
        ("EZ".to_owned(), "0A000".to_owned())
    );
    client.send(b'd', b"stray copy data");
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
}
