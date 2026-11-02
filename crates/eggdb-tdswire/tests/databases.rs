//! Databases created while the server runs (DNEW-R-M6a-db) over TDS: `CREATE DATABASE` and
//! `DROP DATABASE`, `sys.databases`, a login naming a new database, and `USE` moving a
//! connection between databases with the database ENVCHANGE.

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
