//! EggDB's email store (D56, D892): mailboxes and messages kept on EggDB's key-value
//! transactions through [`eggdb_keyspace::Keyspace`] (database 0 of the keyspace the caller
//! gives, which the `eggdb` binary keeps in the collection `mail0`), mail received over SMTP
//! ([`serve_smtp`]) and read over IMAP ([`serve_imap`]) and JMAP ([`JmapEndpoint`]). Everything is written from the IETF
//! RFCs (RFC 3501, RFC 4315, RFC 8620, RFC 8621, RFC 5321, RFC 5322, RFC 2045, RFC 2046, RFC 2047, RFC 6851, RFC 7888); no other mail
//! server's code is used or read.

mod imap;
mod jmap;
mod json;
pub mod message;
pub mod mime;
mod push;
mod smtp;
mod smtp_client;
mod transport;
pub mod store;

pub use jmap::{Endpoint as JmapEndpoint, MAX_REQUEST_BYTES as MAX_JMAP_REQUEST_BYTES};

use eggdb_keyspace::Keyspace;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_secs(1);
const WRITE_LIMIT: Duration = Duration::from_secs(60);

/// What a mail listener needs.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The account's user name, for IMAP and JMAP sign-in.
    pub user: String,
    /// The SHA-256 of the account's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// The largest message accepted, in bytes.
    pub max_message_bytes: usize,
    /// The account's mail domain: its address is `user@domain`, and mail submitted over JMAP to
    /// this domain (or `localhost`) is delivered to the store.
    pub domain: String,
    /// The SMTP server (`host:port`) that JMAP submissions to other domains are relayed through;
    /// without one they are refused.
    pub relay: Option<String>,
    /// The certificate and key `STARTTLS` secures SMTP and IMAP connections with; IMAP then
    /// refuses to sign a client in before it has secured its connection.
    pub tls: Option<std::sync::Arc<eggdb_tls::ServerConfig>>,
}

pub(crate) fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Accepts connections on `listener` until `stop`, serving each on its own thread with
/// `serve`, at most `config.max_connections` at once.
pub(crate) fn accept_loop(
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    name: &str,
    serve: &(dyn Fn(TcpStream) -> io::Result<()> + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_POLL)).is_err()
                || stream.set_write_timeout(Some(WRITE_LIMIT)).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new().name(name.to_owned()).spawn_scoped(scope, move || {
                let _ = serve(stream);
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

/// Each recipient's outcome of a relayed message: the relay's reply, or why it refused.
pub type RelayOutcome = Vec<(String, Result<String, String>)>;

/// Sends `message` from `from` to `recipients` through the SMTP server at `relay`, greeting it
/// as `helo` (RFC 5321).
pub fn relay_message(relay: &str, helo: &str, from: &str, recipients: &[String], message: &[u8]) -> Result<RelayOutcome, String> {
    smtp_client::relay(relay, helo, from, recipients, message)
}

/// Stores `message` in the INBOX of `keyspace`'s mail store, as delivered mail.
pub fn deliver_local(keyspace: &(dyn Keyspace + Sync), message: &[u8]) -> Result<(), String> {
    keyspace.write(0, &mut |keys| store::append(keys, store::INBOX, &[], now_seconds(), message).map(drop).map_err(|error| error.0))
}

/// Receives mail over SMTP on `listener` until `stop`, storing every accepted message in the
/// `INBOX` of `keyspace`'s database 0. It relays nothing.
pub fn serve_smtp(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    accept_loop(listener, config, stop, "eggdb-smtp", &|stream| smtp::serve_connection(keyspace, config, stream, stop))
}

/// Serves the store over IMAP4rev1 on `listener` until `stop`, to clients signing in as
/// `config.user`.
pub fn serve_imap(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    accept_loop(listener, config, stop, "eggdb-imap", &|stream| imap::serve_connection(keyspace, config, stream, stop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn mail_received_over_smtp_lands_in_the_inbox() {
        let keyspace = MemoryKeyspace::default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
            tls: None,
        };
        let stopped = AtomicBool::new(false);
        let stop = || stopped.load(Ordering::Acquire);
        std::thread::scope(|scope| {
            scope.spawn(|| serve_smtp(&keyspace, &listener, &config, &stop));
            let stream = TcpStream::connect(address).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut expect = |reader: &mut BufReader<TcpStream>, code: &str| loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with(code), "{line}");
                if line.as_bytes().get(3) == Some(&b' ') {
                    break;
                }
            };
            expect(&mut reader, "220");
            for (command, code) in [
                ("EHLO client.example", "250"),
                ("MAIL FROM:<ann@example.org>", "250"),
                ("RCPT TO:<eggdb@localhost>", "250"),
                ("DATA", "354"),
                ("Subject: hello\r\n\r\n..dotted\r\n.", "250"),
                ("QUIT", "221"),
            ] {
                writer.write_all(format!("{command}\r\n").as_bytes()).unwrap();
                expect(&mut reader, code);
            }
            stopped.store(true, Ordering::Release);
        });
        let mut stored = Vec::new();
        keyspace
            .read(0, &mut |keys| {
                stored = store::messages(keys, store::INBOX).map_err(|error| error.0)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(stored.len(), 1);
        let raw = String::from_utf8_lossy(&stored[0].raw).into_owned();
        assert!(raw.starts_with("Received: from client.example"), "{raw}");
        assert!(raw.contains("Delivered-To: eggdb@localhost\r\n"), "{raw}");
        assert!(raw.ends_with("Subject: hello\r\n\r\n.dotted\r\n"), "{raw}");
    }

    fn self_signed() -> std::sync::Arc<eggdb_tls::ServerConfig> {
        use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
        use eggdb_tls::x509::SubjectAlternativeName;
        let now = now_seconds() as i64;
        let request = SelfSignedCertificateRequest {
            common_name: "localhost".to_owned(),
            subject_alternative_names: vec![SubjectAlternativeName::DnsName("localhost".to_owned())],
            not_before_unix_seconds: now - 86_400,
            not_after_unix_seconds: now + 86_400,
        };
        let (certificate, key) = generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill).unwrap();
        std::sync::Arc::new(eggdb_tls::ServerConfig::new(vec![certificate], key).unwrap())
    }

    #[test]
    fn imap_refuses_sign_in_until_starttls_secures_the_connection() {
        let keyspace = MemoryKeyspace::default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
            tls: Some(self_signed()),
        };
        let stopped = AtomicBool::new(false);
        let stop = || stopped.load(Ordering::Acquire);
        std::thread::scope(|scope| {
            scope.spawn(|| serve_imap(&keyspace, &listener, &config, &stop));
            let stream = TcpStream::connect(address).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("STARTTLS") && line.contains("LOGINDISABLED"), "{line}");
            writer.write_all(b"a1 LOGIN eggdb secret\r\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("a1 NO [PRIVACYREQUIRED]"), "{line}");
            writer.write_all(b"a2 STARTTLS\r\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("a2 OK"), "{line}");
            let client = eggdb_tls::ClientConfig::new(eggdb_tls::ServerVerification::EncryptionOnly, "localhost");
            let secured = eggdb_tls::TlsStream::connect(reader.into_inner(), &client).unwrap();
            let mut secured = BufReader::new(secured);
            secured.get_mut().write_all(b"a3 LOGIN eggdb secret\r\n").unwrap();
            let mut answer = String::new();
            secured.read_line(&mut answer).unwrap();
            assert!(answer.starts_with("a3 OK") && !answer.contains("LOGINDISABLED"), "{answer}");
            stopped.store(true, Ordering::Release);
        });
    }

    #[test]
    fn imap_clients_sign_in_list_fetch_flag_and_expunge() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                store::append(keys, store::INBOX, &[], 0, b"From: Ann <ann@example.org>\r\nSubject: First\r\n\r\nHello\r\n").map_err(|error| error.0)?;
                store::append(keys, store::INBOX, &[], 0, b"Subject: Second\r\n\r\nBye\r\n").map(drop).map_err(|error| error.0)
            })
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
            tls: None,
        };
        let stopped = AtomicBool::new(false);
        let stop = || stopped.load(Ordering::Acquire);
        std::thread::scope(|scope| {
            scope.spawn(|| serve_imap(&keyspace, &listener, &config, &stop));
            let stream = TcpStream::connect(address).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut until_tagged = |reader: &mut BufReader<TcpStream>, tag: &str| {
                let mut seen = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    seen.push_str(&line);
                    if line.starts_with(tag) || line.is_empty() {
                        return seen;
                    }
                }
            };
            until_tagged(&mut reader, "* OK");
            let mut ask = |command: &str, tag: &str| {
                writer.write_all(format!("{tag} {command}\r\n").as_bytes()).unwrap();
                until_tagged(&mut reader, tag)
            };
            assert!(ask("LOGIN eggdb wrong", "a1").contains("a1 NO"));
            assert!(ask("LOGIN eggdb secret", "a2").contains("a2 OK"));
            let selected = ask("SELECT INBOX", "a3");
            assert!(selected.contains("* 2 EXISTS"), "{selected}");
            let fetched = ask("FETCH 1 (UID FLAGS ENVELOPE BODY[TEXT])", "a4");
            assert!(fetched.contains("\"First\""), "{fetched}");
            assert!(fetched.contains("Hello"), "{fetched}");
            assert!(fetched.contains("\\Seen"), "{fetched}");
            assert!(ask("SEARCH UNSEEN", "a5").contains("* SEARCH 2\r\n"));
            assert!(ask("STORE 2 +FLAGS (\\Deleted)", "a6").contains("\\Deleted"));
            assert!(ask("EXPUNGE", "a7").contains("* 2 EXPUNGE"));
            assert!(ask("CREATE Archive", "a8").contains("a8 OK"));
            assert!(ask("UID MOVE 1 Archive", "a9").contains("* 1 EXPUNGE"));
            assert!(ask("STATUS Archive (MESSAGES UIDNEXT)", "b1").contains("MESSAGES 1 UIDNEXT 2"));
            assert!(ask("LOGOUT", "b2").contains("b2 OK"));
            stopped.store(true, Ordering::Release);
        });
    }
}
