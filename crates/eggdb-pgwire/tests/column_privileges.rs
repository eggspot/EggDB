//! Column privileges and evaluated row-security `WITH CHECK` policies, served over the
//! PostgreSQL protocol to two roles logged in with their own passwords.

mod common;

use common::{Client, DATABASE, Message, TestServer, cstring, tags};

fn error_code(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.tag == b'E')
        .map(Message::code)
}

fn error_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.tag == b'E')
        .and_then(|message| message.field(b'M'))
}

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .collect()
}

fn admin_runs(admin: &mut Client, statements: &[&str]) {
    for sql in statements {
        let replies = admin.query(sql);
        assert_eq!(error_code(&replies), None, "{sql}: {}", tags(&replies));
    }
}

fn logged_in(server: &TestServer, user: &str) -> Client {
    let (mut client, ok) = Client::login_as(server.address, user, "pencil", DATABASE);
    assert_eq!(ok.tag, b'R', "{user}");
    client.read_until_ready();
    client
}

fn text(value: &str) -> Vec<Option<String>> {
    vec![Some(value.to_owned())]
}

#[test]
fn a_role_with_column_grants_reads_and_writes_only_those_columns() {
    eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
    let mut server = TestServer::start_databases(&[DATABASE]);
    let (mut admin, _) = Client::login(server.address);
    admin_runs(
        &mut admin,
        &[
            "CREATE TABLE staff (id integer PRIMARY KEY, name text, salary integer)",
            "INSERT INTO staff VALUES (1, 'ada', 100), (2, 'lin', 200)",
            "CREATE USER clerk WITH PASSWORD 'pencil'",
            "CREATE USER boss WITH PASSWORD 'pencil'",
            "GRANT SELECT (id, name), UPDATE (name) ON staff TO clerk",
            "GRANT SELECT, UPDATE ON staff TO boss",
        ],
    );
    let mut clerk = logged_in(&server, "clerk");
    assert_eq!(
        rows(&clerk.query("SELECT name FROM staff ORDER BY id")),
        [text("ada"), text("lin")]
    );
    assert_eq!(
        rows(&clerk.query("UPDATE staff SET name = 'kim' WHERE id = 2 RETURNING name")),
        [text("kim")]
    );
    for sql in [
        "SELECT * FROM staff",
        "SELECT id FROM staff WHERE salary > 150",
        "SELECT id FROM staff ORDER BY salary",
        "SELECT (SELECT max(salary) FROM staff)",
        "SELECT row_to_json(s) FROM staff s",
        "UPDATE staff SET salary = 0",
        "UPDATE staff SET name = 'x' RETURNING salary",
        "COPY staff TO STDOUT",
    ] {
        let replies = clerk.query(sql);
        assert_eq!(error_code(&replies).as_deref(), Some("42501"), "{sql}");
        assert_eq!(
            error_text(&replies).as_deref(),
            Some("permission denied for table staff"),
            "{sql}"
        );
    }
    let mut boss = logged_in(&server, "boss");
    assert_eq!(
        rows(&boss.query("SELECT salary FROM staff WHERE id = 1")),
        [text("100")]
    );
    assert_eq!(
        rows(&admin.query(
            "SELECT column_name FROM information_schema.column_privileges \
             WHERE grantee = 'clerk' AND privilege_type = 'UPDATE'"
        )),
        [text("name")]
    );
    server.stop();
}

#[test]
fn a_policy_check_refuses_new_rows_outside_it_over_both_query_protocols() {
    eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
    let mut server = TestServer::start_databases(&[DATABASE]);
    let (mut admin, _) = Client::login(server.address);
    admin_runs(
        &mut admin,
        &[
            "CREATE TABLE docs (id integer PRIMARY KEY, owner text)",
            "CREATE USER ada WITH PASSWORD 'pencil'",
            "CREATE USER grace WITH PASSWORD 'pencil'",
            "GRANT SELECT, INSERT, UPDATE ON docs TO ada, grace",
            "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY own ON docs USING (owner = current_user)",
        ],
    );
    let mut ada = logged_in(&server, "ada");
    assert_eq!(error_code(&ada.query("INSERT INTO docs VALUES (1, 'ada')")), None);
    for sql in [
        "INSERT INTO docs VALUES (2, 'grace')",
        "UPDATE docs SET owner = 'grace'",
    ] {
        let replies = ada.query(sql);
        assert_eq!(error_code(&replies).as_deref(), Some("42501"), "{sql}");
        assert_eq!(
            error_text(&replies).as_deref(),
            Some("new row violates row-level security policy for table \"docs\""),
            "{sql}"
        );
    }
    let mut parse = cstring("");
    parse.extend(cstring("INSERT INTO docs VALUES ($1, $2) RETURNING id"));
    parse.extend_from_slice(&0i16.to_be_bytes());
    let bind = |id: &str, owner: &str| {
        let mut body = cstring("");
        body.extend(cstring(""));
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&2i16.to_be_bytes());
        for value in [id, owner] {
            body.extend_from_slice(&(value.len() as i32).to_be_bytes());
            body.extend_from_slice(value.as_bytes());
        }
        body.extend_from_slice(&0i16.to_be_bytes());
        body
    };
    let mut describe = vec![b'P'];
    describe.extend(cstring(""));
    let mut execute = cstring("");
    execute.extend_from_slice(&0i32.to_be_bytes());
    ada.send(b'P', &parse);
    ada.send(b'B', &bind("3", "ada"));
    ada.send(b'D', &describe);
    ada.send(b'E', &execute);
    ada.send(b'S', b"");
    let answer = ada.read_until_ready();
    assert_eq!(tags(&answer), "12TDCZ");
    assert_eq!(answer[2].column_types().len(), 1);
    assert_eq!(answer[3].row(), text("3"));
    assert_eq!(answer[4].text(), "INSERT 0 1");
    ada.send(b'P', &parse);
    ada.send(b'B', &bind("4", "grace"));
    ada.send(b'E', &execute);
    ada.send(b'S', b"");
    let answer = ada.read_until_ready();
    assert_eq!(error_code(&answer).as_deref(), Some("42501"));
    let mut grace = logged_in(&server, "grace");
    assert_eq!(rows(&grace.query("SELECT id FROM docs")), Vec::<Vec<Option<String>>>::new());
    assert_eq!(
        rows(&admin.query("SELECT id, owner FROM docs ORDER BY id")),
        [
            vec![Some("1".to_owned()), Some("ada".to_owned())],
            vec![Some("3".to_owned()), Some("ada".to_owned())],
        ]
    );
    server.stop();
}
