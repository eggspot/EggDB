//! `eggdb serve` run as a real process: where the password may come from, the loopback-only
//! rule, the JSON line a script reads the port from, a client answered on that port, the file
//! held for writing while it serves, and a graceful stop on `SIGTERM`.

mod common;

use common::TempPath;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn eggdb(args: &[&str], password: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eggdb"));
    command.args(args).env_remove("EGGDB_PASSWORD");
    if let Some(password) = password {
        command.env("EGGDB_PASSWORD", password);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn serve_needs_a_password_from_the_environment_or_a_file() {
    let file = TempPath::new("serve-password");
    let path = file.path().to_str().unwrap();
    let missing = eggdb(&["serve", path, "--create"], None);
    assert_eq!(missing.status.code(), Some(2), "{}", stderr(&missing));
    assert!(stderr(&missing).contains("serve needs a password for user eggdb: set EGGDB_PASSWORD"));
    let empty = eggdb(&["serve", path, "--create"], Some(""));
    assert!(stderr(&empty).contains("EGGDB_PASSWORD is set but empty"));
    let unreadable = eggdb(
        &[
            "serve",
            path,
            "--create",
            "--password-file",
            "/no/such/file",
        ],
        None,
    );
    assert_eq!(unreadable.status.code(), Some(2));
    assert!(stderr(&unreadable).contains("cannot read --password-file /no/such/file"));
    let unknown = eggdb(&["serve", path, "--password", "x"], None);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(stderr(&unknown).contains("unknown option '--password' for eggdb serve"));
}

#[test]
fn serve_refuses_every_address_but_loopback() {
    let file = TempPath::new("serve-loopback");
    let path = file.path().to_str().unwrap();
    for address in ["0.0.0.0:5432", "[::]:5432", "192.168.1.10:5432"] {
        let refused = eggdb(
            &["serve", path, "--create", "--listen", address],
            Some("secret"),
        );
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{address}: {}",
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("it is not a loopback address"),
            "{}",
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("try: eggdb serve"));
    }
    let bad = eggdb(
        &["serve", path, "--listen", "db.local:5432"],
        Some("secret"),
    );
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("--listen needs an address and port"));
    let mysql = eggdb(
        &[
            "serve",
            path,
            "--create",
            "--listen",
            "127.0.0.1:0",
            "--mysql-listen",
            "0.0.0.0:3306",
        ],
        Some("secret"),
    );
    assert_eq!(mysql.status.code(), Some(2), "{}", stderr(&mysql));
    assert!(stderr(&mysql).contains("it is not a loopback address"));
    assert!(stderr(&mysql).contains("--mysql-listen 127.0.0.1:3306"));
    let bad = eggdb(&["serve", path, "--mysql-listen", "x"], Some("secret"));
    assert!(
        stderr(&bad).contains("--mysql-listen needs an address and port such as 127.0.0.1:3306")
    );
    let tds = eggdb(
        &[
            "serve",
            path,
            "--create",
            "--listen",
            "127.0.0.1:0",
            "--tds-listen",
            "0.0.0.0:1433",
        ],
        Some("secret"),
    );
    assert_eq!(tds.status.code(), Some(2), "{}", stderr(&tds));
    assert!(stderr(&tds).contains("it is not a loopback address"));
    assert!(stderr(&tds).contains("--tds-listen 127.0.0.1:1433"));
    let bad = eggdb(&["serve", path, "--tds-listen", "x"], Some("secret"));
    assert!(stderr(&bad).contains("--tds-listen needs an address and port such as 127.0.0.1:1433"));
    assert!(!file.path().exists());
}

#[test]
fn serve_prints_its_port_answers_clients_holds_the_file_and_stops_on_sigterm() {
    let file = TempPath::new("serve-run");
    let path = file.path().to_str().unwrap().to_owned();
    let password_file = TempPath::new("serve-run-password");
    std::fs::write(password_file.path(), "s3cret\n").unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args([
            "serve",
            &path,
            "--create",
            "--listen",
            "0",
            "--password-file",
            password_file.path().to_str().unwrap(),
            "--database",
            "shop",
        ])
        .env_remove("EGGDB_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("{\"listening\":\"127.0.0.1:"), "{first}");
    assert!(first.contains("\"database\":\"shop\""), "{first}");
    let port: u16 = first
        .split("\"port\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|port| port.parse().ok())
        .unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .unwrap();
    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(answer, *b"N");
    let busy = eggdb(&["put", &path, "k", "v"], None);
    assert_eq!(busy.status.code(), Some(5), "{}", stderr(&busy));
    drop(client);
    stop(&mut server);
    let status = server.wait().unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    if cfg!(unix) {
        assert_eq!(status.code(), Some(0));
        assert!(rest.contains("{\"stopped\":true"), "{rest}");
    }
    let check = eggdb(&["check", &path], None);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
}

#[cfg(unix)]
fn stop(server: &mut std::process::Child) {
    let status = Command::new("kill")
        .args(["-TERM", &server.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(not(unix))]
fn stop(server: &mut std::process::Child) {
    let _ = server.kill();
}

#[test]
fn tls_options_must_come_in_pairs_and_not_together() {
    let file = TempPath::new("serve-tls-options");
    let path = file.path().to_str().unwrap().to_owned();
    for (args, expected) in [
        (vec!["--tls-key", "key.pem"], "--tls-key needs --tls-certificate"),
        (vec!["--tls-certificate", "cert.pem"], "--tls-certificate needs --tls-key"),
        (
            vec!["--tls-self-signed", "--tls-certificate", "cert.pem", "--tls-key", "key.pem"],
            "--tls-self-signed and --tls-certificate exclude each other",
        ),
        (vec!["--tls-min-version", "1.3"], "--tls-min-version needs TLS"),
        (vec!["--tls-self-signed", "--tls-min-version", "1.1"], "--tls-min-version needs 1.2 or 1.3"),
        (
            vec!["--tls-certificate", "missing-cert.pem", "--tls-key", "missing-key.pem"],
            "cannot read --tls-certificate missing-cert.pem",
        ),
    ] {
        let mut full = vec!["serve", path.as_str(), "--create", "--listen", "127.0.0.1:0"];
        full.extend(args);
        let refused = eggdb(&full, Some("secret"));
        assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
        assert!(stderr(&refused).contains(expected), "{}", stderr(&refused));
    }
}

#[test]
fn serve_with_a_self_signed_certificate_listens_beyond_loopback_and_answers_ssl_requests() {
    let file = TempPath::new("serve-tls-run");
    let path = file.path().to_str().unwrap().to_owned();
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(["serve", &path, "--create", "--listen", "0.0.0.0:0", "--tls-self-signed", "--format", "json"])
        .env("EGGDB_PASSWORD", "s3cret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.contains("\"tls\":true"), "{first}");
    assert!(first.contains("\"loopback_only\":false"), "{first}");
    assert!(first.contains("\"tls_certificate\":\"self-signed\""), "{first}");
    let fingerprint = first
        .split("\"tls_fingerprint_sha256\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    assert_eq!(fingerprint.len(), 64);
    let port: u16 = first
        .split("\"port\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|port| port.parse().ok())
        .unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    client.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]).unwrap();
    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(answer[0], b'S');
    let config = eggdb_tls::ClientConfig::new(eggdb_tls::ServerVerification::EncryptionOnly, "localhost");
    let tls = eggdb_tls::TlsStream::connect(client, &config).expect("TLS handshake with eggdb serve");
    let served: String = tls
        .info()
        .server_certificate
        .as_ref()
        .map(|certificate| {
            eggdb_tls::signature::SignatureHash::Sha256
                .digest(certificate.der())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        })
        .unwrap();
    assert_eq!(served, fingerprint, "the printed fingerprint is the served certificate's");
    stop(&mut server);
    assert!(server.wait().unwrap().success());
}
