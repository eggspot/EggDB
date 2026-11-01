//! The copy sub-protocol over a real socket: `COPY ... FROM STDIN` with `CopyData`, `CopyDone`
//! and `CopyFail`, `COPY ... TO STDOUT`, both from the simple and the extended query
//! protocols, errors in the middle of the data, messages that do not belong in a copy, and a
//! client that disconnects before it finishes.

mod common;

use common::{Client, Message, TestServer, cstring, ready_status, tags};
use std::time::Duration;

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .collect()
}

fn copy_data(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|message| message.tag == b'd')
        .map(|message| String::from_utf8_lossy(&message.body).into_owned())
        .collect()
}

fn ready_table(client: &mut Client) {
    let created = client.query("CREATE TABLE people (id integer PRIMARY KEY, name text)");
    assert_eq!(tags(&created), "CZ");
}

fn count(client: &mut Client) -> String {
    let messages = client.query("SELECT count(*) FROM people");
    rows(&messages)[0][0].clone().unwrap_or_default()
}

fn start_copy_in(client: &mut Client, sql: &str) -> Message {
    client.send(b'Q', &cstring(sql));
    client.read().expect("a CopyInResponse")
}

#[test]
fn copy_from_stdin_takes_data_in_any_pieces_and_reports_its_count() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    let response = start_copy_in(&mut client, "COPY people FROM STDIN");
    assert_eq!(response.tag, b'G');
    assert_eq!(response.body, [0, 0, 2, 0, 0, 0, 0]);
    client.send(b'd', b"1\tada\n2\tgr");
    client.send(b'H', b"");
    client.send(b'S', b"");
    client.send(b'd', b"ace\n3\t\\N\n");
    client.send(b'c', b"");
    let done = client.read_until_ready();
    assert_eq!(tags(&done), "CZ");
    assert_eq!(done[0].text(), "COPY 3");
    assert_eq!(ready_status(&done), b'I');
    let selected = client.query("SELECT id, name FROM people ORDER BY id");
    assert_eq!(
        rows(&selected),
        vec![
            vec![Some("1".into()), Some("ada".into())],
            vec![Some("2".into()), Some("grace".into())],
            vec![Some("3".into()), None],
        ]
    );
}

#[test]
fn copy_to_stdout_sends_one_copy_data_per_row() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    client.query("INSERT INTO people VALUES (2, 'b,c'), (1, 'a\"q')");
    let messages = client.query("COPY people TO STDOUT (FORMAT csv, HEADER)");
    assert_eq!(tags(&messages), "HdddcCZ");
    assert_eq!(messages[0].body, [0, 0, 2, 0, 0, 0, 0]);
    assert_eq!(copy_data(&messages), "id,name\n1,\"a\"\"q\"\n2,\"b,c\"\n");
    assert_eq!(messages[5].text(), "COPY 2");
    let both = client.query("SELECT 1; COPY (SELECT 'x' AS y) TO STDOUT; SELECT 2");
    assert_eq!(tags(&both), "TDCHdcCTDCZ");
}

fn binary_rows(rows: &[(i32, Option<&str>)]) -> Vec<u8> {
    let mut out = b"PGCOPY\n\xff\r\n\0".to_vec();
    out.extend_from_slice(&[0; 8]);
    for (id, name) in rows {
        out.extend_from_slice(&2i16.to_be_bytes());
        out.extend_from_slice(&4i32.to_be_bytes());
        out.extend_from_slice(&id.to_be_bytes());
        match name {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(name) => {
                out.extend_from_slice(&(name.len() as i32).to_be_bytes());
                out.extend_from_slice(name.as_bytes());
            }
        }
    }
    out.extend_from_slice(&(-1i16).to_be_bytes());
    out
}

#[test]
fn the_binary_format_flows_both_ways_with_binary_format_codes() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    let file = binary_rows(&[(1, Some("ada")), (2, None)]);
    let response = start_copy_in(&mut client, "COPY people FROM STDIN (FORMAT binary)");
    assert_eq!(response.tag, b'G');
    assert_eq!(response.body, [1, 0, 2, 0, 1, 0, 1]);
    let (first, rest) = file.split_at(13);
    client.send(b'd', first);
    client.send(b'd', rest);
    client.send(b'c', b"");
    let done = client.read_until_ready();
    assert_eq!(tags(&done), "CZ");
    assert_eq!(done[0].text(), "COPY 2");
    let messages = client.query("COPY people TO STDOUT (FORMAT binary)");
    assert_eq!(tags(&messages), "HdddcCZ");
    assert_eq!(messages[0].body, [1, 0, 2, 0, 1, 0, 1]);
    let sent: Vec<u8> = messages
        .iter()
        .filter(|message| message.tag == b'd')
        .flat_map(|message| message.body.clone())
        .collect();
    assert_eq!(sent, file);
    start_copy_in(&mut client, "COPY people FROM STDIN (FORMAT binary)");
    client.send(b'd', b"PGCOPY");
    client.send(b'c', b"");
    let failed = client.read_until_ready();
    assert_eq!(failed[0].code(), "22P04");
    assert_eq!(count(&mut client), "2");
}

#[test]
fn copy_fail_and_bad_data_roll_the_whole_copy_back_and_later_data_is_ignored() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    start_copy_in(&mut client, "COPY people FROM STDIN");
    client.send(b'd', b"1\ta\n");
    client.send(b'f', &cstring("the user changed their mind"));
    let failed = client.read_until_ready();
    assert_eq!(tags(&failed), "EZ");
    assert_eq!(failed[0].code(), "57014");
    assert_eq!(
        failed[0].field(b'M').as_deref(),
        Some("COPY from stdin failed: the user changed their mind")
    );
    assert_eq!(count(&mut client), "0");
    start_copy_in(&mut client, "COPY people FROM STDIN");
    client.send(b'd', b"1\ta\n2\tb\nx\tc\n");
    let failed = client.read_until_ready();
    assert_eq!(tags(&failed), "EZ");
    assert_eq!(failed[0].code(), "22P02");
    assert_eq!(
        failed[0].field(b'W').as_deref(),
        Some("COPY people, line 3, column id: \"x\"")
    );
    client.send(b'd', b"4\td\n");
    client.send(b'c', b"");
    client.send(b'f', &cstring("late"));
    assert_eq!(count(&mut client), "0");
    client.query("BEGIN");
    start_copy_in(&mut client, "COPY people FROM STDIN");
    client.send(b'd', b"1\ta\n1\tb\n");
    let failed = client.read_until_ready();
    assert_eq!(failed[0].code(), "23505");
    assert_eq!(ready_status(&failed), b'E');
    client.send(b'c', b"");
    assert_eq!(tags(&client.query("ROLLBACK")), "CZ");
    assert_eq!(count(&mut client), "0");
}

#[test]
fn a_message_that_does_not_belong_in_a_copy_fails_it_and_the_connection_goes_on() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    start_copy_in(&mut client, "COPY people FROM STDIN");
    client.send(b'd', b"1\ta\n");
    client.send(b'Q', &cstring("SELECT 1"));
    let failed = client.read_until_ready();
    assert_eq!(tags(&failed), "EZ");
    assert_eq!(failed[0].code(), "08P01");
    assert_eq!(count(&mut client), "0");
    let refused = client.query("COPY people FROM '/etc/passwd'");
    assert_eq!(refused[0].code(), "42501");
    let refused = client.query("COPY people TO PROGRAM 'sh'");
    assert_eq!(refused[0].code(), "42501");
}

fn parse(query: &str) -> Vec<u8> {
    let mut body = cstring("");
    body.extend(cstring(query));
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn bind() -> Vec<u8> {
    let mut body = cstring("");
    body.extend(cstring(""));
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn execute() -> Vec<u8> {
    let mut body = cstring("");
    body.extend_from_slice(&0i32.to_be_bytes());
    body
}

fn send_extended(client: &mut Client, query: &str) {
    client.send(b'P', &parse(query));
    client.send(b'B', &bind());
    client.send(b'D', b"P\0");
    client.send(b'E', &execute());
    client.send(b'S', b"");
}

#[test]
fn the_extended_protocol_copies_in_and_out_as_libpq_drives_it() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    send_extended(&mut client, "COPY people FROM STDIN (FORMAT csv)");
    let started: Vec<u8> = (0..4)
        .filter_map(|_| client.read())
        .map(|message| message.tag)
        .collect();
    assert_eq!(started, b"12nG");
    client.send(b'd', b"1,\"x, y\"\n2,\n");
    client.send(b'c', b"");
    client.send(b'S', b"");
    let done = client.read_until_ready();
    assert_eq!(tags(&done), "CZ");
    assert_eq!(done[0].text(), "COPY 2");
    send_extended(&mut client, "COPY people TO STDOUT");
    let out = client.read_until_ready();
    assert_eq!(tags(&out), "12nHddcCZ");
    assert_eq!(copy_data(&out), "1\tx, y\n2\t\\N\n");
    send_extended(&mut client, "COPY people FROM STDIN");
    let started: Vec<u8> = (0..4)
        .filter_map(|_| client.read())
        .map(|message| message.tag)
        .collect();
    assert_eq!(started, b"12nG");
    client.send(b'd', b"9\tnine\n9\tagain\n");
    client.send(b'c', b"");
    client.send(b'S', b"");
    let failed = client.read_until_ready();
    assert_eq!(tags(&failed), "EZ");
    assert_eq!(failed[0].code(), "23505");
    assert_eq!(count(&mut client), "2");
}

#[test]
fn a_client_that_disconnects_mid_copy_leaves_nothing_committed() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    ready_table(&mut client);
    start_copy_in(&mut client, "COPY people FROM STDIN");
    let data: String = (0..2000).map(|id| format!("{id}\tname {id}\n")).collect();
    client.send(b'd', data.as_bytes());
    drop(client);
    std::thread::sleep(Duration::from_millis(300));
    let (mut client, _) = Client::login(server.address);
    assert_eq!(count(&mut client), "0");
    start_copy_in(&mut client, "COPY people FROM STDIN");
    client.send(b'd', data.as_bytes());
    client.send(b'c', b"");
    assert_eq!(client.read_until_ready()[0].text(), "COPY 2000");
}
