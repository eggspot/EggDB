mod common;

use common::{Client, DATABASE, PASSWORD, TestServer, USER, tags};

fn error_code(messages: &[common::Message]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.tag == b'E')
        .map(common::Message::code)
}

fn first_value(messages: &[common::Message]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.tag == b'D')
        .and_then(|message| message.row().into_iter().next().flatten())
}

#[test]
fn a_stored_role_logs_in_with_its_password_and_holds_only_its_grants() {
    eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
    let mut server = TestServer::start_databases(&[DATABASE]);
    let (mut admin, _) = Client::login(server.address);
    for sql in [
        "CREATE TABLE notes (id integer, body text)",
        "INSERT INTO notes VALUES (1, 'hello')",
        "CREATE USER clerk WITH PASSWORD 'pencil'",
        "GRANT SELECT ON notes TO clerk",
    ] {
        let replies = admin.query(sql);
        assert_eq!(error_code(&replies), None, "{sql}: {}", tags(&replies));
    }
    let (_, refused) = Client::login_as(server.address, "clerk", "wrong", DATABASE);
    assert_eq!(refused.tag, b'E');
    assert_eq!(refused.code(), "28P01");
    let (mut clerk, ok) = Client::login_as(server.address, "clerk", "pencil", DATABASE);
    assert_eq!((ok.tag, ok.body.clone()), (b'R', 0i32.to_be_bytes().to_vec()));
    clerk.read_until_ready();
    assert_eq!(first_value(&clerk.query("SELECT body FROM notes")).as_deref(), Some("hello"));
    assert_eq!(
        error_code(&clerk.query("INSERT INTO notes VALUES (2, 'no')")).as_deref(),
        Some("42501")
    );
    assert_eq!(
        first_value(&clerk.query("SELECT current_user")).as_deref(),
        Some("clerk")
    );
    let (_, unknown) = Client::login_as(server.address, "nobody", "pencil", DATABASE);
    assert_eq!(unknown.code(), "28P01");
    server.stop();
}

#[test]
fn each_database_of_a_server_is_reached_by_its_name() {
    let mut server = TestServer::start_databases(&[DATABASE, "archive"]);
    let (mut shop, _) = Client::login(server.address);
    assert_eq!(error_code(&shop.query("CREATE TABLE orders (id integer)")), None);
    let (mut archive, ok) = Client::login_as(server.address, USER, PASSWORD, "archive");
    assert_eq!(ok.tag, b'R');
    archive.read_until_ready();
    assert_eq!(
        first_value(&archive.query("SELECT current_database()")).as_deref(),
        Some("archive")
    );
    assert_eq!(
        error_code(&archive.query("SELECT * FROM orders")).as_deref(),
        Some("42P01")
    );
    assert_eq!(
        first_value(&archive.query(
            "SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database"
        ))
        .as_deref(),
        Some("archive,shop")
    );
    let (mut missing, ok) = Client::login_as(server.address, USER, PASSWORD, "nowhere");
    assert_eq!(ok.tag, b'R');
    let refused = missing.read().unwrap();
    assert_eq!(refused.code(), "3D000");
    server.stop();
}

#[test]
fn notifications_reach_listeners_at_commit_and_while_they_are_idle() {
    let mut server = TestServer::start(8);
    let (mut listener, _) = Client::login(server.address);
    let (mut sender, _) = Client::login(server.address);
    assert_eq!(tags(&listener.query("LISTEN jobs")), "LISTEN");
    let replies = sender.query("BEGIN; NOTIFY jobs, 'one'; SELECT pg_notify('jobs', 'two')");
    assert_eq!(error_code(&replies), None, "{}", tags(&replies));
    let early: Vec<_> = listener
        .query("SELECT 1")
        .into_iter()
        .filter(|message| message.tag == b'A')
        .collect();
    assert!(early.is_empty(), "nothing arrives before the commit");
    sender.query("COMMIT");
    let mut payloads = Vec::new();
    while payloads.len() < 2 {
        let message = listener.read().expect("a notification while idle");
        assert_eq!(message.tag, b'A');
        let text = String::from_utf8(message.body[4..].to_vec()).unwrap();
        let mut fields = text.split('\0');
        assert_eq!(fields.next(), Some("jobs"));
        payloads.push(fields.next().unwrap_or_default().to_owned());
    }
    assert_eq!(payloads, ["one", "two"]);
    sender.query("BEGIN; NOTIFY jobs, 'lost'; ROLLBACK");
    listener.query("UNLISTEN *");
    sender.query("NOTIFY jobs, 'after'");
    let after: Vec<_> = listener
        .query("SELECT 1")
        .into_iter()
        .filter(|message| message.tag == b'A')
        .collect();
    assert!(after.is_empty());
    server.stop();
}
