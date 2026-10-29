//! Implicit transactions, portals that end with them, `DISCARD ALL`, `CancelRequest` and
//! Npgsql's type-loading queries, spoken over a real loopback socket by the minimal client in
//! `common`.

mod common;

use common::{Client, Message, TestServer, cstring, ready_status, tags};
use std::time::{Duration, Instant};

fn parse(name: &str, query: &str) -> Vec<u8> {
    let mut body = cstring(name);
    body.extend(cstring(query));
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn bind(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend(cstring(statement));
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn run_in_pipeline(client: &mut Client, statement: &str) {
    client.send(b'P', &parse("", statement));
    client.send(b'B', &bind("", ""));
    client.send(b'E', &execute("", 0));
}

fn count(client: &mut Client) -> String {
    let answer = client.query("SELECT count(*) FROM t");
    answer
        .iter()
        .find(|message| message.tag == b'D')
        .and_then(|message| message.row().into_iter().next().flatten())
        .unwrap_or_default()
}

fn with_table(server: &TestServer) -> Client {
    let (mut client, _) = Client::login(server.address);
    assert_eq!(
        tags(&client.query("CREATE TABLE t (id integer PRIMARY KEY)")),
        "CZ"
    );
    client
}

fn backend_key(greeting: &[Message]) -> (i32, i32) {
    let key = greeting
        .iter()
        .find(|message| message.tag == b'K')
        .expect("BackendKeyData");
    (
        i32::from_be_bytes(key.body[..4].try_into().unwrap()),
        i32::from_be_bytes(key.body[4..8].try_into().unwrap()),
    )
}

fn send_cancel(server: &TestServer, process_id: i32, secret_key: i32) {
    let mut canceller = Client::connect(server.address);
    let mut body = 80_877_102u32.to_be_bytes().to_vec();
    body.extend_from_slice(&process_id.to_be_bytes());
    body.extend_from_slice(&secret_key.to_be_bytes());
    let mut packet = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    packet.extend(body);
    canceller.send_raw(&packet);
    assert_eq!(canceller.read_byte(), None);
}

#[test]
fn a_pipeline_commits_at_sync_and_leaves_nothing_when_it_fails_half_way() {
    let server = TestServer::start(8);
    let mut client = with_table(&server);
    let (mut other, _) = Client::login(server.address);
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (1)");
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (2)");
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12C12CZ");
    assert_eq!(ready_status(&answer), b'I');
    assert_eq!(count(&mut other), "2");

    run_in_pipeline(&mut client, "INSERT INTO t VALUES (3)");
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (1)");
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (4)");
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12C12EZ");
    assert_eq!(answer[5].code(), "23505");
    assert_eq!(ready_status(&answer), b'I');
    assert_eq!(count(&mut other), "2");
    assert_eq!(count(&mut client), "2");
}

#[test]
fn the_statements_of_one_query_message_commit_or_roll_back_together() {
    let server = TestServer::start(8);
    let mut client = with_table(&server);
    let answer = client.query("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); SELECT 1 / 0");
    assert_eq!(tags(&answer), "CCEZ");
    assert_eq!(answer[2].code(), "22012");
    assert_eq!(count(&mut client), "0");

    let answer = client.query(
        "INSERT INTO t VALUES (3); COMMIT; INSERT INTO t VALUES (4); SELECT 1 / 0",
    );
    assert_eq!(tags(&answer), "CNCCEZ");
    assert_eq!(answer[1].code(), "25P01");
    assert_eq!(count(&mut client), "1");
}

#[test]
fn begin_in_a_pipeline_opens_a_block_that_takes_in_the_earlier_statements() {
    let server = TestServer::start(8);
    let mut client = with_table(&server);
    let (mut other, _) = Client::login(server.address);
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (1)");
    run_in_pipeline(&mut client, "BEGIN");
    run_in_pipeline(&mut client, "INSERT INTO t VALUES (2)");
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12C12C12CZ");
    assert_eq!(ready_status(&answer), b'T');
    assert_eq!(count(&mut client), "2");
    assert_eq!(tags(&client.query("ROLLBACK")), "CZ");
    assert_eq!(count(&mut other), "0");
}

#[test]
fn portals_end_with_their_transaction_but_live_on_in_a_block() {
    let server = TestServer::start(8);
    let mut client = with_table(&server);
    client.query("INSERT INTO t VALUES (1), (2), (3)");
    client.send(b'P', &parse("", "SELECT id FROM t ORDER BY id"));
    client.send(b'B', &bind("p", ""));
    client.send(b'E', &execute("p", 1));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "12DsZ");
    client.send(b'E', &execute("p", 1));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "EZ");
    assert_eq!(answer[0].code(), "34000");

    client.query("BEGIN");
    client.send(b'P', &parse("", "SELECT id FROM t ORDER BY id"));
    client.send(b'B', &bind("p", ""));
    client.send(b'E', &execute("p", 1));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "12DsZ");
    client.send(b'E', &execute("p", 5));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "DDCZ");
    client.query("COMMIT");
}

#[test]
fn discard_all_drops_prepared_statements_and_returns_to_the_startup_settings() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.send(b'P', &parse("s1", "SELECT 1"));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "1Z");
    client.query("SET application_name = 'changed'");
    let answer = client.query("DISCARD ALL");
    assert_eq!(answer[0].text(), "DISCARD ALL");
    assert!(
        answer
            .iter()
            .any(|message| message.tag == b'S' && message.text().starts_with("application_name\0tests")),
        "{answer:?}"
    );
    client.send(b'B', &bind("", "s1"));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(answer[0].code(), "26000");
    client.query("BEGIN");
    assert_eq!(client.query("DISCARD ALL")[0].code(), "25001");
}

#[test]
fn a_cancel_request_with_the_backend_key_stops_the_running_statement() {
    let server = TestServer::start(8);
    let (mut client, greeting) = Client::login(server.address);
    let (process_id, secret_key) = backend_key(&greeting);

    client.send(b'Q', &cstring("SELECT pg_sleep(0.5)"));
    std::thread::sleep(Duration::from_millis(100));
    send_cancel(&server, process_id, secret_key.wrapping_add(1));
    send_cancel(&server, process_id.wrapping_add(1000), secret_key);
    assert_eq!(tags(&client.read_until_ready()), "TDCZ");

    let started = Instant::now();
    client.send(b'Q', &cstring("SELECT pg_sleep(60)"));
    std::thread::sleep(Duration::from_millis(200));
    send_cancel(&server, process_id, secret_key);
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "EZ");
    assert_eq!(answer[0].code(), "57014");
    assert!(started.elapsed() < Duration::from_secs(20));

    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
    let logged = server.logged.lock().unwrap().join("\n");
    assert!(
        logged.contains(&format!("cancel request for process {process_id} ignored")),
        "{logged}"
    );
}

#[test]
fn a_cancel_rolls_back_the_pipeline_it_stops() {
    let server = TestServer::start(8);
    let mut client = with_table(&server);
    let (mut other, greeting) = Client::login(server.address);
    let (process_id, secret_key) = backend_key(&greeting);
    run_in_pipeline(&mut other, "INSERT INTO t VALUES (1)");
    run_in_pipeline(&mut other, "SELECT pg_sleep(60)");
    other.send(b'S', b"");
    std::thread::sleep(Duration::from_millis(200));
    send_cancel(&server, process_id, secret_key);
    let answer = other.read_until_ready();
    assert_eq!(tags(&answer), "12C12EZ");
    assert_eq!(answer[5].code(), "57014");
    assert_eq!(count(&mut client), "0");
}

#[test]
fn npgsql_type_loading_queries_answer_in_one_round_trip() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query(include_str!(
        "../../eggdb/tests/data/npgsql/load-types.sql"
    ));
    let shape: String = tags(&answer)
        .chars()
        .filter(|tag| *tag != 'D')
        .collect();
    assert_eq!(shape, "TCTCTCTCZ", "{answer:?}");
    let int_array = answer
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .find(|row| row.get(2).cloned().flatten().as_deref() == Some("_int4"))
        .expect("_int4");
    assert_eq!(int_array[3].as_deref(), Some("a"));
    assert_eq!(int_array[5].as_deref(), Some("23"));
}
