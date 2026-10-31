mod common;

use common::TempPath;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};

fn eggdb(args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eggdb"));
    command.args(args).env_remove("PGPASSWORD").env_remove("SQLCMDPASSWORD").env_remove("EGGDB_PASSWORD");
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn follow_refuses_bad_sources_and_options_before_connecting() {
    let file = TempPath::new("follow-usage");
    let path = file.path().to_str().unwrap().to_owned();
    for (args, expected) in [
        (vec!["follow", path.as_str()], "the source"),
        (vec!["follow", path.as_str(), "postgres://ada:secret@db/app"], "the password is in the URL"),
        (vec!["follow", path.as_str(), "mysql://ada@db/app"], "the source must be postgres:// or sqlserver://"),
        (vec!["follow", path.as_str(), "postgres://ada@db/app", "--poll", "2"], "--poll is for SQL Server sources"),
        (vec!["follow", path.as_str(), "sqlserver://sa@db/app?encrypt=no", "--publication", "p"], "--publication is for PostgreSQL sources"),
        (vec!["follow", path.as_str(), "postgres://ada@db/app", "--until", "later"], "--until takes caught-up"),
        (vec!["follow", path.as_str(), "postgres://ada@db/app?sslmode=verify-full"], "needs sslrootcert"),
        (vec!["follow", path.as_str(), "sqlserver://sa@db/app"], "trustServerCertificate"),
        (vec!["follow", "-", "postgres://ada@db/app"], "stdin is not one"),
    ] {
        let refused = eggdb(&args, &[]);
        assert_eq!(refused.status.code(), Some(2), "{args:?}: {}", stderr(&refused));
        assert!(stderr(&refused).contains(expected), "{args:?}: {}", stderr(&refused));
    }
}

#[test]
fn follow_reports_a_source_it_cannot_reach() {
    let file = TempPath::new("follow-unreachable");
    let path = file.path().to_str().unwrap().to_owned();
    let url = format!("postgres://ada@127.0.0.1:{}/app?sslmode=disable", unused_port());
    let failed = eggdb(&["follow", "--create", &path, &url], &[("PGPASSWORD", "secret")]);
    assert_eq!(failed.status.code(), Some(3), "{}", stderr(&failed));
    assert!(stderr(&failed).contains("cannot connect to 127.0.0.1:"), "{}", stderr(&failed));
    assert!(!file.path().exists(), "a failed follow leaves no new file");
}

#[test]
fn follow_from_a_sql_server_without_change_tracking_says_how_to_turn_it_on() {
    let served = TempPath::new("follow-tds-source");
    let served_path = served.path().to_str().unwrap().to_owned();
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(["serve", &served_path, "--create", "--listen", "127.0.0.1:0", "--tds-listen", "127.0.0.1:0", "--format", "json"])
        .env("EGGDB_PASSWORD", "s3cret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(server.stdout.take().unwrap()).read_line(&mut first).unwrap();
    let port: u16 = first
        .split("\"tds_port\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|port| port.parse().ok())
        .unwrap();
    let file = TempPath::new("follow-tds-target");
    let path = file.path().to_str().unwrap().to_owned();
    let url = format!("sqlserver://eggdb@127.0.0.1:{port}/{}?encrypt=no", "follow-tds-source".split('/').next().unwrap());
    let database = first
        .split("\"database\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let url = url.replace("follow-tds-source", &database);
    let refused = eggdb(&["follow", "--create", &path, &url], &[("SQLCMDPASSWORD", "s3cret")]);
    let _ = server.kill();
    let _ = server.wait();
    assert_eq!(refused.status.code(), Some(3), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("change tracking is off"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("SET CHANGE_TRACKING = ON"), "{}", stderr(&refused));
}
