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

fn exit_within(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("eggdb did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn eggdb_within(args: &[&str], password: &str, limit: Duration) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .env("EGGDB_PASSWORD", password)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    exit_within(&mut child, limit);
    child.wait_with_output().unwrap()
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
fn serve_takes_syslog_only_on_a_loopback_address() {
    let file = TempPath::new("serve-syslog-loopback");
    let path = file.path().to_str().unwrap();
    let refused = eggdb(
        &[
            "serve",
            path,
            "--create",
            "--listen",
            "127.0.0.1:0",
            "--syslog-listen",
            "0.0.0.0:5514",
        ],
        Some("secret"),
    );
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(
        stderr(&refused)
            .contains("--syslog-listen 0.0.0.0:5514: syslog has no authentication or TLS"),
        "{}",
        stderr(&refused)
    );
    let any_port = eggdb(
        &[
            "serve",
            path,
            "--create",
            "--listen",
            "127.0.0.1:0",
            "--syslog-listen",
            "127.0.0.1:0",
        ],
        Some("secret"),
    );
    assert_eq!(any_port.status.code(), Some(2), "{}", stderr(&any_port));
    assert!(
        stderr(&any_port).contains("--syslog-listen 127.0.0.1:0: give a port"),
        "{}",
        stderr(&any_port)
    );
    let bad = eggdb(&["serve", path, "--syslog-listen", "x"], Some("secret"));
    assert!(
        stderr(&bad).contains("--syslog-listen needs an address and port such as 127.0.0.1:514"),
        "{}",
        stderr(&bad)
    );
}

fn free_syslog_port() -> u16 {
    for _ in 0..20 {
        let Ok(datagrams) = std::net::UdpSocket::bind("127.0.0.1:0") else {
            continue;
        };
        let Ok(address) = datagrams.local_addr() else {
            continue;
        };
        if std::net::TcpListener::bind(address).is_ok() {
            return address.port();
        }
    }
    panic!("no port free for both UDP and TCP on loopback");
}

#[test]
fn serve_stores_syslog_sent_over_udp_and_tcp_and_stops_on_sigterm() {
    let file = TempPath::new("serve-syslog-run");
    let path = file.path().to_str().unwrap().to_owned();
    let password_file = TempPath::new("serve-syslog-run-password");
    std::fs::write(password_file.path(), "s3cret\n").unwrap();
    let port = free_syslog_port();
    let syslog_address = format!("127.0.0.1:{port}");
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args([
            "serve",
            &path,
            "--create",
            "--listen",
            "0",
            "--password-file",
            password_file.path().to_str().unwrap(),
            "--syslog-listen",
            &syslog_address,
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
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sender
        .send_to(
            b"<13>1 2026-11-04T10:00:00Z host app - - - over udp",
            &syslog_address,
        )
        .unwrap();
    let message = "<13>1 2026-11-04T10:00:01Z host app - - - over tcp";
    let mut stream = TcpStream::connect(&syslog_address).unwrap();
    stream
        .write_all(format!("{} {message}", message.len()).as_bytes())
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    drop(stream);
    stop(&mut server);
    let status = exit_within(&mut server, Duration::from_secs(30));
    if cfg!(unix) {
        assert_eq!(status.code(), Some(0));
        let stored = eggdb(
            &[
                "sql",
                &path,
                "-c",
                "SELECT string_agg(message, '+' ORDER BY message) FROM syslog",
            ],
            None,
        );
        assert_eq!(stored.status.code(), Some(0), "{}", stderr(&stored));
        let shown = String::from_utf8_lossy(&stored.stdout);
        assert!(shown.contains("over tcp+over udp"), "{shown}");
    }
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
    let busy = eggdb(&["put", &path, "k", "v", "--busy-timeout", "0"], None);
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
        (
            vec!["--tls-key", "key.pem"],
            "--tls-key needs --tls-certificate",
        ),
        (
            vec!["--tls-certificate", "cert.pem"],
            "--tls-certificate needs --tls-key",
        ),
        (
            vec![
                "--tls-self-signed",
                "--tls-certificate",
                "cert.pem",
                "--tls-key",
                "key.pem",
            ],
            "--tls-self-signed and --tls-certificate exclude each other",
        ),
        (
            vec!["--tls-min-version", "1.3"],
            "--tls-min-version needs TLS",
        ),
        (
            vec!["--tls-self-signed", "--tls-min-version", "1.1"],
            "--tls-min-version needs 1.2 or 1.3",
        ),
        (
            vec![
                "--tls-certificate",
                "missing-cert.pem",
                "--tls-key",
                "missing-key.pem",
            ],
            "cannot read --tls-certificate missing-cert.pem",
        ),
    ] {
        let mut full = vec![
            "serve",
            path.as_str(),
            "--create",
            "--listen",
            "127.0.0.1:0",
        ];
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
        .args([
            "serve",
            &path,
            "--create",
            "--listen",
            "0.0.0.0:0",
            "--tls-self-signed",
            "--format",
            "json",
        ])
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
    assert!(
        first.contains("\"tls_certificate\":\"self-signed\""),
        "{first}"
    );
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
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .unwrap();
    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(answer[0], b'S');
    let config =
        eggdb_tls::ClientConfig::new(eggdb_tls::ServerVerification::EncryptionOnly, "localhost");
    let tls =
        eggdb_tls::TlsStream::connect(client, &config).expect("TLS handshake with eggdb serve");
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
    assert_eq!(
        served, fingerprint,
        "the printed fingerprint is the served certificate's"
    );
    stop(&mut server);
    let _ = server.wait();
}

#[cfg(any(unix, windows))]
#[test]
fn serve_answers_postgresql_clients_on_a_unix_socket_and_removes_it_at_stop() {
    use eggdb_platform::local_socket::LocalStream;
    let file = TempPath::new("serve-socket");
    let path = file.path().to_str().unwrap().to_owned();
    let directory = std::env::temp_dir().join(format!("eggdb-serve-socket-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args([
            "serve",
            &path,
            "--create",
            "--listen",
            "127.0.0.1:0",
            "--socket-dir",
            directory.to_str().unwrap(),
            "--format",
            "json",
        ])
        .env("EGGDB_PASSWORD", "s3cret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let socket = first
        .split("\"socket\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .replace("\\\\", "\\");
    assert!(socket.contains(".s.PGSQL."), "{first}");
    let mut client = LocalStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .unwrap();
    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).unwrap();
    assert_eq!(answer[0], b'N', "no TLS on a local socket");
    drop(client);
    stop(&mut server);
    let _ = server.wait();
    if cfg!(unix) {
        assert!(
            !std::path::Path::new(&socket).exists(),
            "the socket file is removed at stop"
        );
    }
    let refused = eggdb(
        &[
            "serve",
            &path,
            "--socket-dir",
            "/nonexistent-eggdb-directory",
        ],
        Some("secret"),
    );
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("does not exist"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn max_connection_memory_needs_a_size_of_at_least_one_mebibyte() {
    let file = TempPath::new("serve-memory-size");
    let path = file.path().to_str().unwrap();
    for refused in ["10k", "lots", "0"] {
        let output = eggdb(
            &[
                "serve",
                path,
                "--create",
                "--max-connection-memory",
                refused,
            ],
            Some("secret"),
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{refused}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("--max-connection-memory"),
            "{refused}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn max_connection_memory_bounds_what_a_resp_client_may_send_and_others_go_on() {
    let file = TempPath::new("serve-memory-resp");
    let path = file.path().to_str().unwrap().to_owned();
    let resp_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let resp_address = format!("127.0.0.1:{resp_port}");
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args([
            "serve",
            &path,
            "--create",
            "--listen",
            "0",
            "--resp-listen",
            &resp_address,
            "--max-connection-memory",
            "1m",
        ])
        .env("EGGDB_PASSWORD", "secret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("{\"listening\":"), "{first}");

    let mut greedy = TcpStream::connect(&resp_address).unwrap();
    greedy
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    greedy
        .write_all(
            b"*2\r\n$4\r\nAUTH\r\n$6\r\nsecret\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2097152\r\n",
        )
        .unwrap();
    let mut answer = String::new();
    greedy.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("+OK\r\n-OOM "), "{answer}");

    let mut other = TcpStream::connect(&resp_address).unwrap();
    other
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    other
        .write_all(b"*2\r\n$4\r\nAUTH\r\n$6\r\nsecret\r\nPING\r\nQUIT\r\n")
        .unwrap();
    let mut answer = String::new();
    other.read_to_string(&mut answer).unwrap();
    assert_eq!(answer, "+OK\r\n+PONG\r\n+OK\r\n");
    stop(&mut server);
    let _ = server.wait();
}

fn post_mcp(address: &str, token: &str, session: Option<&str>, body: &str) -> (String, String) {
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let session_header = session
        .map(|id| format!("Mcp-Session-Id: {id}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session_header}Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    client.write_all(request.as_bytes()).unwrap();
    let mut reader = BufReader::new(client);
    let mut head = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
        head.push_str(&line);
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (head, String::from_utf8(body).unwrap())
}

fn header_value(head: &str, wanted: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(wanted)
            .then(|| value.trim().to_owned())
    })
}

fn tables_file(name: &str) -> TempPath {
    let file = TempPath::new(name);
    let path = file.path().to_str().unwrap().to_owned();
    for statement in [
        "CREATE TABLE orders (id integer PRIMARY KEY, total integer)",
        "CREATE TABLE secrets (id integer PRIMARY KEY, word text)",
        "INSERT INTO secrets VALUES (1, 'swordfish')",
    ] {
        let created = eggdb(&["sql", &path, "--create", "-c", statement], None);
        assert!(created.status.success(), "{}", stderr(&created));
    }
    file
}

#[test]
fn mcp_tables_hides_unlisted_tables_from_every_session_of_the_mcp_endpoint() {
    let file = tables_file("serve-mcp-tables");
    let path = file.path().to_str().unwrap().to_owned();
    let token_file = TempPath::new("serve-mcp-tables-token");
    let token = "0123456789abcdef0123456789abcdef";
    std::fs::write(token_file.path(), format!("{token}\n")).unwrap();
    let mcp_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mcp_address = format!("127.0.0.1:{mcp_port}");
    let mut server = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args([
            "serve",
            &path,
            "--listen",
            "0",
            "--mcp-listen",
            &mcp_address,
            "--mcp-token-file",
            token_file.path().to_str().unwrap(),
            "--mcp-tables",
            "orders",
        ])
        .env("EGGDB_PASSWORD", "secret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("{\"listening\":"), "{first}");

    for _session in 0..2 {
        let (head, answer) = post_mcp(
            &mcp_address,
            token,
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"serve-test","version":"1.0"}}}"#,
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(answer.contains("(--tables)"), "{answer}");
        let session = header_value(&head, "mcp-session-id").expect("a session id");
        let (head, _) = post_mcp(
            &mcp_address,
            token,
            Some(&session),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert!(head.starts_with("HTTP/1.1 202"), "{head}");
        let (_, listed) = post_mcp(
            &mcp_address,
            token,
            Some(&session),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tables","arguments":{}}}"#,
        );
        assert!(listed.contains("\"orders\""), "{listed}");
        assert!(!listed.contains("secrets"), "{listed}");
        let (_, read) = post_mcp(
            &mcp_address,
            token,
            Some(&session),
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"SELECT word FROM secrets"}}}"#,
        );
        assert!(read.contains("\"isError\":true"), "{read}");
        assert!(read.contains("42P01"), "{read}");
        assert!(!read.contains("swordfish"), "{read}");
        let (_, tools) = post_mcp(
            &mcp_address,
            token,
            Some(&session),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#,
        );
        assert!(!tools.contains("\"kv_get\""), "{tools}");
    }
    stop(&mut server);
    let _ = server.wait();
    let _ = std::fs::remove_file(format!("{path}.mcp-audit.jsonl"));
}

#[test]
fn mcp_tables_needs_the_mcp_endpoint_and_names_that_exist() {
    let file = tables_file("serve-mcp-tables-refused");
    let path = file.path().to_str().unwrap().to_owned();
    let alone = eggdb(
        &["serve", &path, "--listen", "0", "--mcp-tables", "orders"],
        Some("secret"),
    );
    assert_eq!(alone.status.code(), Some(2), "{}", stderr(&alone));
    assert!(
        stderr(&alone).contains("--mcp-tables limits the MCP endpoint"),
        "{}",
        stderr(&alone)
    );
    let token_file = TempPath::new("serve-mcp-tables-refused-token");
    std::fs::write(token_file.path(), "0123456789abcdef0123456789abcdef\n").unwrap();
    let unknown = eggdb_within(
        &[
            "serve",
            &path,
            "--listen",
            "0",
            "--mcp-listen",
            "127.0.0.1:0",
            "--mcp-token-file",
            token_file.path().to_str().unwrap(),
            "--mcp-tables",
            "orders,nowhere",
        ],
        "secret",
        Duration::from_secs(30),
    );
    assert_ne!(unknown.status.code(), Some(0), "{}", stderr(&unknown));
    assert!(
        stderr(&unknown).contains("--mcp-tables names 'nowhere': no table, view or sequence in"),
        "{}",
        stderr(&unknown)
    );
}
