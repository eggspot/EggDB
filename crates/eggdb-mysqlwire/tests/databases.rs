//! Databases created while the server runs (D270) over the MySQL protocol:
//! `CREATE DATABASE` and `DROP DATABASE`, `SHOW DATABASES`, a handshake naming a new database,
//! and `USE` and `COM_INIT_DB` moving a connection between databases.

mod common;

use common::{Client, DATABASE, MemoryDirectory, Reply, TestServer, texts};
use eggdb_sql::ServedDatabases;

const INIT_DB: u8 = 0x02;

fn database_list(client: &mut Client) -> Vec<String> {
    texts(&client.query("SHOW DATABASES"))
        .into_iter()
        .flatten()
        .collect()
}

#[test]
fn create_database_lists_it_and_use_and_init_db_move_the_connection() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut client = Client::login(server.address);
    assert!(matches!(
        client.query("CREATE DATABASE audit"),
        Reply::Ok { .. }
    ));
    assert_eq!(
        database_list(&mut client),
        vec!["audit".to_owned(), DATABASE.to_owned()]
    );
    assert!(matches!(
        client.query("CREATE DATABASE AUDIT"),
        Reply::Err { code: 1007, .. }
    ));
    assert!(matches!(
        client.query("CREATE TABLE orders (id INT)"),
        Reply::Ok { .. }
    ));
    assert!(matches!(
        client.query("INSERT INTO orders VALUES (7)"),
        Reply::Ok { .. }
    ));
    assert!(matches!(client.query("USE audit"), Reply::Ok { .. }));
    assert!(matches!(
        client.query("SELECT id FROM orders"),
        Reply::Err { .. }
    ));
    assert!(matches!(
        client.query("CREATE TABLE entries (id INT)"),
        Reply::Ok { .. }
    ));
    client.command(INIT_DB, b"SHOP");
    assert!(matches!(client.next_reply(), Reply::Ok { .. }));
    assert_eq!(
        texts(&client.query("SELECT id FROM orders")),
        vec![vec!["7".to_owned()]]
    );
    assert!(matches!(
        client.query("SELECT id FROM entries"),
        Reply::Err { .. }
    ));
    client.command(INIT_DB, b"nowhere");
    assert!(matches!(client.next_reply(), Reply::Err { code: 1049, .. }));
    assert!(matches!(client.query("USE nowhere"), Reply::Err { .. }));
    assert!(matches!(client.query("SELECT 1"), Reply::Rows { .. }));
}

#[test]
fn a_new_database_is_reached_from_the_handshake_and_not_dropped_while_in_use() {
    let directory = MemoryDirectory::with(&[DATABASE]);
    let server = TestServer::start_directory(directory.clone());
    let mut admin = Client::login(server.address);
    assert!(matches!(
        admin.query("CREATE DATABASE audit"),
        Reply::Ok { .. }
    ));
    let (mut reader, reply) = Client::login_to(server.address, "audit");
    assert!(matches!(reply, Reply::Ok { .. }), "{reply:?}");
    assert!(matches!(
        reader.query("CREATE TABLE entries (id INT)"),
        Reply::Ok { .. }
    ));
    assert!(matches!(
        admin.query("DROP DATABASE audit"),
        Reply::Err { .. }
    ));
    assert!(directory.names().contains(&"audit".to_owned()));
    reader.command(0x01, &[]);
    assert_eq!(reader.read(), None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let dropped = loop {
        let reply = admin.query("DROP DATABASE audit");
        if matches!(reply, Reply::Ok { .. }) || std::time::Instant::now() >= deadline {
            break reply;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(matches!(dropped, Reply::Ok { .. }), "{dropped:?}");
    assert_eq!(database_list(&mut admin), vec![DATABASE.to_owned()]);
    let (_, refused) = Client::login_to(server.address, "audit");
    assert!(
        matches!(refused, Reply::Err { code: 1049, .. }),
        "{refused:?}"
    );
}
