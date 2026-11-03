//! PL/SQL procedures called with `CALL` over the PostgreSQL protocol on a connection that
//! reads Oracle SQL: arguments by position and by name, and `OUT` and `IN OUT` parameters
//! returned as one row, as PostgreSQL returns a procedure's output arguments (D295).

mod common;

use common::{Client, DATABASE, Message, TestServer, USER, cstring, ready_status};

const SPLIT_IT: &str = "CREATE OR REPLACE PROCEDURE split_it (\n  total IN INTEGER,\n  half  OUT INTEGER,\n  note  IN OUT VARCHAR2\n) AS\nBEGIN\n  half := total / 2;\n  note := note || '!';\nEND;";

const LOG_IT: &str = "CREATE OR REPLACE PROCEDURE log_it (\n  p_n   IN NUMBER,\n  p_msg IN VARCHAR2 DEFAULT 'none'\n) AS\nBEGIN\n  INSERT INTO log VALUES (p_n, p_msg);\nEND log_it;";

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .collect()
}

fn text(value: &str) -> Option<String> {
    Some(value.to_owned())
}

fn ran(client: &mut Client, query: &str) {
    let answer = client.query(query);
    assert!(
        answer.iter().all(|message| message.tag != b'E'),
        "{query}: {answer:?}"
    );
}

fn oracle_client(server: &TestServer) -> Client {
    let mut client = Client::connect(server.address);
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("options", "-c eggdb.dialect=oracle"),
        ],
    );
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    client.read_until_ready();
    ran(
        &mut client,
        "CREATE TABLE log (n NUMBER(5), msg VARCHAR2(200))",
    );
    ran(&mut client, SPLIT_IT);
    ran(&mut client, LOG_IT);
    client
}

fn parse(query: &str) -> Vec<u8> {
    let mut body = cstring("");
    body.extend(cstring(query));
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn bind(values: &[&[u8]]) -> Vec<u8> {
    let mut body = cstring("");
    body.extend(cstring(""));
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for value in values {
        body.extend_from_slice(&(value.len() as i32).to_be_bytes());
        body.extend_from_slice(value);
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn execute() -> Vec<u8> {
    let mut body = cstring("");
    body.extend_from_slice(&0i32.to_be_bytes());
    body
}

#[test]
fn a_plsql_call_returns_its_out_and_in_out_parameters_as_one_row() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    let positional = client.query("CALL split_it(8, NULL, 'hi')");
    assert_eq!(rows(&positional), [vec![text("4"), text("hi!")]]);
    let described = positional
        .iter()
        .find(|message| message.tag == b'T')
        .map(Message::column_types)
        .unwrap_or_default();
    let names: Vec<&str> = described.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["HALF", "NOTE"]);
    assert_eq!(ready_status(&positional), b'I');
    let named = client.query("CALL split_it(note => 'x', total => 2)");
    assert_eq!(rows(&named), [vec![text("1"), text("x!")]]);
}

#[test]
fn a_plsql_call_binds_extended_protocol_parameters_by_position_and_by_name() {
    let server = TestServer::start(8);
    let mut client = oracle_client(&server);
    client.send(b'P', &parse("CALL split_it($1, NULL, $2)"));
    client.send(b'B', &bind(&[b"6", b"yo"]));
    client.send(b'E', &execute());
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert!(
        answer.iter().all(|message| message.tag != b'E'),
        "{answer:?}"
    );
    assert_eq!(rows(&answer), [vec![text("3"), text("yo!")]]);
    client.send(b'P', &parse("CALL log_it(p_n => $1)"));
    client.send(b'B', &bind(&[b"5"]));
    client.send(b'E', &execute());
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert!(
        answer.iter().all(|message| message.tag != b'E'),
        "{answer:?}"
    );
    assert!(rows(&answer).is_empty(), "{answer:?}");
    assert_eq!(
        rows(&client.query("SELECT n, msg FROM log")),
        [vec![text("5"), text("none")]]
    );
}
