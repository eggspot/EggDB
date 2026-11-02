//! Databases created while the server runs (D270) over TDS: `CREATE DATABASE` and
//! `DROP DATABASE`, `sys.databases`, a login naming a new database, and `USE` moving a
//! connection between databases with the database ENVCHANGE, refused (`40508`) while the
//! connection holds a `#temp` table or a session-owned application lock (D294).

mod common;

use common::{
    Client, DATABASE, LoginOptions, MemoryDirectory, TestServer, Token, errors, rows, utf16,
};
use eggdb_sql::ServedDatabases;
use std::time::{Duration, Instant};

const DATABASE_CHANGE: u8 = 1;

fn changed_to(tokens: &[Token], database: &str) -> bool {
    tokens.iter().any(|token| {
        matches!(token, Token::EnvChange { kind: DATABASE_CHANGE, new, .. } if *new == utf16(database))
    })
}

fn listed(client: &mut Client) -> Vec<String> {
    rows(&client.batch("SELECT name FROM sys.databases ORDER BY name"))
        .into_iter()
        .flatten()
        .flatten()
        .collect()
}

#[test]
fn create_database_lists_it_and_use_moves_the_connection_with_an_envchange() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut client = Client::login(server.address);
    assert!(errors(&client.batch("CREATE DATABASE audit")).is_empty());
    assert_eq!(
        listed(&mut client),
        vec!["audit".to_owned(), DATABASE.to_owned()]
    );
    assert_eq!(errors(&client.batch("CREATE DATABASE Audit"))[0].0, 1801);
    assert!(
        errors(&client.batch("CREATE TABLE orders (id INT); INSERT INTO orders VALUES (7)"))
            .is_empty()
    );
    let used = client.batch("USE audit");
    assert!(errors(&used).is_empty(), "{used:?}");
    assert!(changed_to(&used, "audit"), "{used:?}");
    assert!(!errors(&client.batch("SELECT id FROM orders")).is_empty());
    assert_eq!(
        rows(&client.batch("SELECT DB_NAME()")),
        vec![vec![Some("audit".to_owned())]]
    );
    let back = client.batch("USE shop");
    assert!(changed_to(&back, DATABASE), "{back:?}");
    assert_eq!(
        rows(&client.batch("SELECT id FROM orders")),
        vec![vec![Some("7".to_owned())]]
    );
    assert_eq!(errors(&client.batch("USE nowhere"))[0].0, 911);
    let refused = client.batch("USE audit; SELECT 1");
    assert!(changed_to(&refused, "audit"), "{refused:?}");
    assert!(!errors(&refused).is_empty(), "{refused:?}");
    assert_eq!(
        rows(&client.batch("SELECT DB_NAME()")),
        vec![vec![Some("audit".to_owned())]]
    );
}

#[test]
fn a_login_reaches_a_new_database_which_is_not_dropped_while_in_use() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut admin = Client::login(server.address);
    assert!(errors(&admin.batch("CREATE DATABASE audit")).is_empty());
    let (mut reader, tokens) = Client::login_with(
        server.address,
        &LoginOptions {
            database: "audit",
            ..LoginOptions::default()
        },
    );
    assert!(changed_to(&tokens, "audit"), "{tokens:?}");
    assert!(errors(&reader.batch("CREATE TABLE entries (id INT)")).is_empty());
    assert_eq!(errors(&admin.batch("DROP DATABASE audit"))[0].0, 3702);
    assert!(directory.names().contains(&"audit".to_owned()));
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(5);
    let dropped = loop {
        let tokens = admin.batch("DROP DATABASE audit");
        if errors(&tokens).is_empty() || Instant::now() >= deadline {
            break tokens;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(errors(&dropped).is_empty(), "{dropped:?}");
    assert_eq!(listed(&mut admin), vec![DATABASE.to_owned()]);
    let (_, refused) = Client::login_with(
        server.address,
        &LoginOptions {
            database: "audit",
            ..LoginOptions::default()
        },
    );
    assert!(
        !refused
            .iter()
            .any(|token| matches!(token, Token::LoginAck { .. })),
        "{refused:?}"
    );
}

#[test]
fn the_servers_own_login_keeps_its_rights_after_use() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut client = Client::login(server.address);
    assert!(errors(&client.batch("CREATE DATABASE audit")).is_empty());
    assert!(errors(&client.batch("CREATE DATABASE scratch")).is_empty());
    let used = client.batch("USE audit");
    assert!(changed_to(&used, "audit"), "{used:?}");
    let dropped = client.batch("DROP DATABASE scratch");
    assert!(errors(&dropped).is_empty(), "{dropped:?}");
    assert!(!directory.names().contains(&"scratch".to_owned()));
    let created = client.batch("CREATE DATABASE kept");
    assert!(errors(&created).is_empty(), "{created:?}");
}

#[test]
fn use_is_refused_while_a_temp_table_or_a_session_lock_would_be_left_behind() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut client = Client::login(server.address);
    assert!(errors(&client.batch("CREATE DATABASE audit")).is_empty());
    assert!(errors(&client.batch("CREATE TABLE #work (id INT)")).is_empty());
    let refused = client.batch("USE audit");
    let refusal = errors(&refused);
    assert_eq!(refusal.first().map(|error| error.0), Some(40508), "{refused:?}");
    assert!(refusal[0].2.contains("temporary table #work"), "{refusal:?}");
    assert!(!changed_to(&refused, "audit"), "{refused:?}");
    assert!(errors(&client.batch("INSERT INTO #work VALUES (1)")).is_empty());
    assert!(errors(&client.batch("DROP TABLE #work")).is_empty());
    let locked = client.batch(
        "EXEC sp_getapplock @Resource = 'nightly', @LockMode = 'Exclusive', @LockOwner = 'Session'",
    );
    assert!(errors(&locked).is_empty(), "{locked:?}");
    let refused = client.batch("USE audit");
    assert!(
        errors(&refused)
            .first()
            .is_some_and(|error| error.2.contains("application lock 'nightly'")),
        "{refused:?}"
    );
    assert!(
        errors(&client.batch(
            "EXEC sp_releaseapplock @Resource = 'nightly', @LockOwner = 'Session'"
        ))
        .is_empty()
    );
    let used = client.batch("USE audit");
    assert!(changed_to(&used, "audit"), "{used:?}");
}
