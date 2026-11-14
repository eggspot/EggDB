use super::serve_tls::TlsSetup;
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use eggdb_tls::{ServerConfig, TlsStream};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
const RECORD_WAIT: Duration = Duration::from_millis(200);
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const PIECE_BYTES: usize = 16 * 1024;

struct Relay {
    flag: &'static str,
    public: TcpListener,
    inner: SocketAddr,
}

/// The listeners whose clients reach them over TLS from outside this machine: each public
/// listener ends TLS and hands the plaintext to its protocol server on a loopback port.
pub(super) struct TlsRelays {
    relays: Vec<Relay>,
}

fn retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

fn cannot_listen(what: &str, address: SocketAddr, error: std::io::Error) -> Failure {
    Failure::database(
        format!("cannot listen for {what} on {address}: {error}"),
        Exit::DatabaseError,
    )
}

impl TlsRelays {
    pub(super) fn new() -> Self {
        TlsRelays { relays: Vec::new() }
    }

    /// The listener a protocol server accepts on: `address` itself on a loopback address, or,
    /// when `address` faces other machines, a loopback port behind a TLS relay on `address`.
    /// Without TLS, a listener outside loopback is refused.
    pub(super) fn listener(
        &mut self,
        address: SocketAddr,
        flag: &'static str,
        what: &str,
        tls: Option<&TlsSetup>,
    ) -> Result<TcpListener, Failure> {
        if address.ip().is_loopback() {
            return TcpListener::bind(address).map_err(|error| cannot_listen(what, address, error));
        }
        if tls.is_none() {
            return Err(Failure::usage(
                format!(
                    "{flag} {address}: clients outside this machine connect over TLS; give --tls-certificate and --tls-key, or --tls-self-signed, or listen on a loopback address"
                ),
                Some(CommandName::Serve),
            ));
        }
        let public = TcpListener::bind(address).map_err(|error| cannot_listen(what, address, error))?;
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let inner_listener =
            TcpListener::bind(loopback).map_err(|error| cannot_listen(what, loopback, error))?;
        let inner = inner_listener
            .local_addr()
            .map_err(|error| cannot_listen(what, loopback, error))?;
        self.relays.push(Relay {
            flag,
            public,
            inner,
        });
        Ok(inner_listener)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.relays.is_empty()
    }

    /// The flags whose listeners are relayed, for the startup lines.
    pub(super) fn relayed_flags(&self) -> Vec<&'static str> {
        self.relays.iter().map(|relay| relay.flag).collect()
    }

    /// Accepts on every public listener until `stop` says so, each connection on a thread of its own.
    pub(super) fn run<'scope, 'env>(
        &'env self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        config: &'env Arc<ServerConfig>,
        stop: &'env (dyn Fn() -> bool + Sync),
    ) {
        for relay in &self.relays {
            scope.spawn(move || accept_loop(relay, config, stop));
        }
    }
}

fn accept_loop(relay: &Relay, config: &Arc<ServerConfig>, stop: &(dyn Fn() -> bool + Sync)) {
    if relay.public.set_nonblocking(true).is_err() {
        return;
    }
    while !stop() {
        match relay.public.accept() {
            Ok((client, _)) => {
                let config = Arc::clone(config);
                let inner = relay.inner;
                let _ = std::thread::Builder::new()
                    .name("eggdb-tls-relay".to_owned())
                    .spawn(move || relay_connection(client, &config, inner));
            }
            Err(error) if retryable(&error) => std::thread::sleep(ACCEPT_POLL),
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
    }
}

fn relay_connection(client: TcpStream, config: &ServerConfig, inner: SocketAddr) {
    if client.set_nonblocking(false).is_err() || client.set_read_timeout(Some(HANDSHAKE_WAIT)).is_err() {
        return;
    }
    let Ok(raw) = client.try_clone() else {
        return;
    };
    let Ok(tls) = TlsStream::accept(client, config) else {
        return;
    };
    let Ok(server) = TcpStream::connect(inner) else {
        return;
    };
    let _ = server.set_nodelay(true);
    let _ = raw.set_nodelay(true);
    let Ok(server_reader) = server.try_clone() else {
        return;
    };
    let tls = Arc::new(Mutex::new(tls));
    let outbound = {
        let tls = Arc::clone(&tls);
        std::thread::Builder::new()
            .name("eggdb-tls-relay-out".to_owned())
            .spawn(move || pump_to_client(server_reader, &tls))
    };
    pump_to_server(&raw, &tls, server);
    if let Ok(outbound) = outbound {
        let _ = outbound.join();
    }
}

fn has_buffered_input(tls: &Mutex<TlsStream<TcpStream>>) -> Option<bool> {
    tls.lock().ok().map(|stream| stream.has_buffered_input())
}

fn wait_for_bytes(raw: &TcpStream) -> bool {
    if raw.set_read_timeout(None).is_err() {
        return false;
    }
    loop {
        match raw.peek(&mut [0u8; 1]) {
            Ok(_) => return true,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

fn pump_to_server(raw: &TcpStream, tls: &Mutex<TlsStream<TcpStream>>, mut server: TcpStream) {
    let mut buffer = vec![0u8; PIECE_BYTES];
    loop {
        match has_buffered_input(tls) {
            None => break,
            Some(false) if !wait_for_bytes(raw) => break,
            Some(_) => {}
        }
        if raw.set_read_timeout(Some(RECORD_WAIT)).is_err() {
            break;
        }
        let read = match tls.lock() {
            Ok(mut stream) => stream.read(&mut buffer),
            Err(_) => break,
        };
        match read {
            Ok(0) => break,
            Ok(count) => {
                if server.write_all(&buffer[..count]).is_err() {
                    break;
                }
            }
            Err(error) if retryable(&error) => std::thread::yield_now(),
            Err(_) => break,
        }
    }
    let _ = server.shutdown(Shutdown::Write);
}

fn pump_to_client(mut server: TcpStream, tls: &Mutex<TlsStream<TcpStream>>) {
    let mut buffer = vec![0u8; PIECE_BYTES];
    loop {
        let count = match server.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let Ok(mut stream) = tls.lock() else {
            break;
        };
        if stream.write_all(&buffer[..count]).is_err() {
            break;
        }
    }
    if let Ok(mut stream) = tls.lock() {
        let _ = stream.close();
        let _ = stream.get_ref().shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
    use eggdb_tls::x509::SubjectAlternativeName;
    use eggdb_tls::{ClientConfig, ServerConfig, ServerVerification};

    fn config() -> (Arc<ServerConfig>, eggdb_tls::x509::Certificate) {
        let request = SelfSignedCertificateRequest {
            common_name: "localhost".to_owned(),
            subject_alternative_names: vec![SubjectAlternativeName::DnsName("localhost".to_owned())],
            not_before_unix_seconds: 0,
            not_after_unix_seconds: 4_000_000_000,
        };
        let (certificate, key) =
            generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill)
                .expect("a self-signed certificate");
        let config = ServerConfig::new(vec![certificate.clone()], key).expect("a server config");
        (Arc::new(config), certificate)
    }

    #[test]
    fn a_relayed_connection_carries_bytes_both_ways_and_server_pushes() {
        let (config, certificate) = config();
        let inner_listener = TcpListener::bind("127.0.0.1:0").expect("inner");
        let inner = inner_listener.local_addr().expect("inner address");
        let public = TcpListener::bind("127.0.0.1:0").expect("public");
        let public_address = public.local_addr().expect("public address");
        let echo = std::thread::spawn(move || {
            let (mut connection, _) = inner_listener.accept().expect("accept");
            connection.write_all(b"hello\n").expect("push");
            let mut line = [0u8; 4];
            connection.read_exact(&mut line).expect("read");
            connection.write_all(&line).expect("echo");
        });
        let relay_config = Arc::clone(&config);
        let relay = std::thread::spawn(move || {
            let (client, _) = public.accept().expect("accept public");
            relay_connection(client, &relay_config, inner);
        });
        let mut trust = eggdb_tls::verify::TrustStore::default();
        trust.add(certificate);
        let client_config = ClientConfig::new(ServerVerification::CertificateAuthority { trust }, "localhost");
        let socket = TcpStream::connect(public_address).expect("connect");
        let mut tls = TlsStream::connect(socket, &client_config).expect("handshake");
        let mut pushed = [0u8; 6];
        tls.read_exact(&mut pushed).expect("pushed");
        assert_eq!(&pushed, b"hello\n");
        tls.write_all(b"ping").expect("send");
        let mut echoed = [0u8; 4];
        tls.read_exact(&mut echoed).expect("echoed");
        assert_eq!(&echoed, b"ping");
        echo.join().expect("echo thread");
        let _ = tls.close();
        drop(tls);
        relay.join().expect("relay thread");
    }

    #[test]
    fn a_listener_outside_loopback_without_tls_is_refused() {
        let mut relays = TlsRelays::new();
        let address: SocketAddr = "0.0.0.0:0".parse().expect("address");
        assert!(relays.listener(address, "--resp-listen", "RESP", None).is_err());
        assert!(relays.is_empty());
    }
}
