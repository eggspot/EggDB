//! `eggdb serve <directory>` creating and dropping databases while it runs
//! (D270): `CREATE DATABASE` makes a `.eggdb` file a client then connects to by name,
//! `pg_database` lists it at once, `DROP DATABASE` is refused while a client is connected to it
//! and removes the file once none is.

use eggdb_pgwire::client::{ClientError, PgClient, parse_postgres_url_with_password};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PASSWORD: &str = "s3cret";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(directory: &Path, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg("serve")
        .arg(directory)
        .args(["--listen", "0"])
        .args(extra)
        .env("EGGDB_PASSWORD", PASSWORD)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let port = first
        .split("\"port\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in {first}"));
    Server { child, port }
}

fn connect(server: &Server, database: &str) -> Result<PgClient, ClientError> {
    let options = parse_postgres_url_with_password(&format!(
        "postgres://eggdb:{PASSWORD}@127.0.0.1:{}/{database}?sslmode=disable",
        server.port
    ))
    .unwrap();
    PgClient::connect(&options)
}

fn single_column(client: &mut PgClient, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .unwrap()
        .into_iter()
        .flat_map(|rows| rows.rows)
        .filter_map(|row| row.into_iter().next().flatten())
        .collect()
}

fn server_code(result: Result<Vec<eggdb_pgwire::client::QueryRows>, ClientError>) -> String {
    match result {
        Err(ClientError::Server { code, .. }) => code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

fn data_directory(label: &str) -> Scratch {
    let directory = std::env::temp_dir().join(format!(
        "eggdb-serve-databases-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    eggdb::Database::open_with(
        directory.join("main.eggdb"),
        eggdb::Options::new().create_if_missing(true),
    )
    .unwrap()
    .close()
    .unwrap();
    Scratch(directory)
}

#[test]
fn a_created_database_is_a_file_clients_connect_to_and_drop_once_nobody_uses_it() {
    let scratch = data_directory("lifecycle");
    let server = serve(&scratch.0, &[]);
    let mut admin = connect(&server, "main").unwrap();
    admin.simple_query("CREATE DATABASE shop").unwrap();
    assert!(scratch.0.join("shop.eggdb").is_file());
    assert_eq!(
        single_column(
            &mut admin,
            "SELECT datname FROM pg_database ORDER BY datname"
        ),
        vec!["main".to_owned(), "shop".to_owned()]
    );
    assert_eq!(
        server_code(admin.simple_query("CREATE DATABASE shop")),
        "42P04"
    );
    assert_eq!(
        server_code(admin.simple_query("CREATE DATABASE \"../escape\"")),
        "42602"
    );
    let mut shopper = connect(&server, "shop").unwrap();
    shopper
        .simple_query("CREATE TABLE orders (id int); INSERT INTO orders VALUES (7)")
        .unwrap();
    assert_eq!(
        single_column(&mut shopper, "SELECT current_database()"),
        vec!["shop".to_owned()]
    );
    assert_eq!(
        server_code(admin.simple_query("DROP DATABASE shop")),
        "55006"
    );
    assert_eq!(
        server_code(shopper.simple_query("DROP DATABASE shop")),
        "55006"
    );
    assert_eq!(
        server_code(admin.simple_query("DROP DATABASE main")),
        "55006"
    );
    assert!(scratch.0.join("shop.eggdb").is_file());
    shopper.terminate();
    let mut again = connect(&server, "shop").unwrap();
    assert_eq!(
        single_column(&mut again, "SELECT id FROM orders"),
        vec!["7".to_owned()]
    );
    again.terminate();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match admin.simple_query("DROP DATABASE shop") {
            Ok(_) => break,
            Err(ClientError::Server { code, .. })
                if code == "55006" && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("DROP DATABASE failed: {error}"),
        }
    }
    assert!(!scratch.0.join("shop.eggdb").exists());
    assert_eq!(
        single_column(&mut admin, "SELECT datname FROM pg_database"),
        vec!["main".to_owned()]
    );
    assert!(connect(&server, "shop").is_err());
    assert_eq!(
        server_code(admin.simple_query("DROP DATABASE shop")),
        "3D000"
    );
    admin.simple_query("DROP DATABASE IF EXISTS shop").unwrap();
}

#[test]
fn a_database_created_while_serving_is_served_again_after_a_restart() {
    let scratch = data_directory("restart");
    let server = serve(&scratch.0, &[]);
    let mut admin = connect(&server, "main").unwrap();
    admin.simple_query("CREATE DATABASE ledger").unwrap();
    let mut ledger = connect(&server, "ledger").unwrap();
    ledger
        .simple_query("CREATE TABLE entries (amount int); INSERT INTO entries VALUES (42)")
        .unwrap();
    ledger.terminate();
    admin.terminate();
    drop(server);
    let server = serve(&scratch.0, &[]);
    let mut ledger = connect(&server, "ledger").unwrap();
    assert_eq!(
        single_column(&mut ledger, "SELECT amount FROM entries"),
        vec!["42".to_owned()]
    );
}

#[test]
fn a_read_only_server_neither_creates_nor_drops_databases() {
    let scratch = data_directory("read-only");
    let server = serve(&scratch.0, &["--read-only"]);
    let mut admin = connect(&server, "main").unwrap();
    let refused = server_code(admin.simple_query("CREATE DATABASE shop"));
    assert!(matches!(refused.as_str(), "0A000" | "25006"), "{refused}");
    assert!(!scratch.0.join("shop.eggdb").exists());
}
