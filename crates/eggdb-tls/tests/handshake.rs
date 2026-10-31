use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use eggdb_tls::alert::AlertDescription;
use eggdb_tls::cipher_suite::{CipherSuite, version};
use eggdb_tls::config::{ClientConfig, MaximumVersion, MinimumVersion, ServerConfig, ServerVerification};
use eggdb_tls::error::TlsError;
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::stream::TlsStream;
use eggdb_tls::verify::{ServerName, TrustStore};
use eggdb_tls::x509::{Certificate, SubjectAlternativeName};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .expect("clock after 1970")
}

fn server_setup(name: &str) -> (ServerConfig, Certificate) {
    let request = SelfSignedCertificateRequest {
        common_name: name.to_owned(),
        subject_alternative_names: vec![SubjectAlternativeName::DnsName(name.to_owned())],
        not_before_unix_seconds: now() - 86_400,
        not_after_unix_seconds: now() + 365 * 86_400,
    };
    let (certificate, key) = generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill)
        .expect("self-signed certificate");
    let config = ServerConfig::new(vec![certificate.clone()], key).expect("server config");
    (config, certificate)
}

fn trusting(certificate: &Certificate) -> TrustStore {
    let mut trust = TrustStore::new();
    trust.add(certificate.clone());
    trust
}

fn full_verification(certificate: &Certificate, name: &str) -> ClientConfig {
    ClientConfig::new(
        ServerVerification::Full {
            trust: trusting(certificate),
            server_name: ServerName::parse(name).expect("name"),
        },
        name,
    )
}

fn connected_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let client = TcpStream::connect(address).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    (server, client)
}

fn run_handshake(
    server_config: ServerConfig,
    client_config: ClientConfig,
) -> (Result<TlsStream<TcpStream>, TlsError>, Result<TlsStream<TcpStream>, TlsError>) {
    let (server_socket, client_socket) = connected_pair();
    let server = thread::spawn(move || TlsStream::accept(server_socket, &server_config));
    let client = TlsStream::connect(client_socket, &client_config);
    let server = server.join().expect("server thread");
    (server, client)
}

#[test]
fn a_client_and_server_exchange_data_over_tls13_both_ways() {
    let (server_config, certificate) = server_setup("db.example.com");
    let client_config = full_verification(&certificate, "db.example.com");
    let (server_socket, client_socket) = connected_pair();
    let server = thread::spawn(move || {
        let mut stream = TlsStream::accept(server_socket, &server_config).expect("server handshake");
        let mut request = [0u8; 5];
        stream.read_exact(&mut request).expect("read");
        assert_eq!(&request, b"hello");
        let large = vec![0x5a_u8; 100_000];
        stream.write_all(&large).expect("write");
        stream.close().expect("close");
        stream.info().server_name.clone()
    });
    let mut client = TlsStream::connect(client_socket, &client_config).expect("client handshake");
    assert_eq!(client.info().version, version::TLS13);
    assert_eq!(client.info().cipher_suite, CipherSuite::Tls13ChaCha20Poly1305Sha256);
    client.write_all(b"hello").expect("write");
    let mut received = Vec::new();
    client.read_to_end(&mut received).expect("read to close_notify");
    assert_eq!(received.len(), 100_000);
    assert!(received.iter().all(|byte| *byte == 0x5a));
    assert_eq!(server.join().expect("server"), Some("db.example.com".to_owned()));
}

#[test]
fn both_sides_agree_on_the_channel_binding_and_exported_keys() {
    let (server_config, certificate) = server_setup("db.example.com");
    let client_config = full_verification(&certificate, "db.example.com");
    let (server, client) = run_handshake(server_config, client_config);
    let (server, client) = (server.expect("server"), client.expect("client"));
    assert_eq!(server.info().tls_server_end_point(), client.info().tls_server_end_point());
    assert_eq!(server.info().tls_server_end_point().map(|hash| hash.len()), Some(32));
    assert_eq!(
        server.export_keying_material(b"EXPORTER-test", Some(b"context"), 32),
        client.export_keying_material(b"EXPORTER-test", Some(b"context"), 32)
    );
}

#[test]
fn a_wrong_name_or_unknown_issuer_fails_the_client_and_alerts_the_server() {
    let (server_config, certificate) = server_setup("db.example.com");
    let client_config = full_verification(&certificate, "other.example.com");
    let (server, client) = run_handshake(server_config, client_config);
    assert!(matches!(client, Err(TlsError::Certificate(_))));
    assert!(matches!(server, Err(TlsError::PeerAlert(alert)) if alert.description() == Some(AlertDescription::BadCertificate)));

    let (server_config, _) = server_setup("db.example.com");
    let (_, stranger) = server_setup("db.example.com");
    let client_config = full_verification(&stranger, "db.example.com");
    let (server, client) = run_handshake(server_config, client_config);
    assert!(matches!(client, Err(TlsError::Certificate(_))));
    assert!(matches!(server, Err(TlsError::PeerAlert(alert)) if alert.description() == Some(AlertDescription::UnknownCa)));
}

#[test]
fn encryption_only_accepts_any_certificate_the_server_holds_the_key_for() {
    let (server_config, _) = server_setup("db.example.com");
    let client_config = ClientConfig::new(ServerVerification::EncryptionOnly, "127.0.0.1");
    assert_eq!(client_config.server_name_indication, None);
    let (server, client) = run_handshake(server_config, client_config);
    assert!(server.is_ok());
    assert_eq!(client.expect("client").info().server_name, None);
}

#[test]
fn application_protocols_are_agreed_or_the_handshake_fails() {
    let (mut server_config, certificate) = server_setup("db.example.com");
    server_config.application_protocols = vec![b"postgresql".to_vec()];
    let mut client_config = full_verification(&certificate, "db.example.com");
    client_config.application_protocols = vec![b"postgresql".to_vec()];
    let (server, client) = run_handshake(server_config.clone(), client_config.clone());
    assert_eq!(server.expect("server").info().application_protocol, Some(b"postgresql".to_vec()));
    assert_eq!(client.expect("client").info().application_protocol, Some(b"postgresql".to_vec()));

    client_config.application_protocols = vec![b"h2".to_vec()];
    let (server, client) = run_handshake(server_config, client_config);
    assert!(matches!(server, Err(TlsError::Protocol { alert: AlertDescription::NoApplicationProtocol, .. })));
    assert!(client.is_err());
}

#[test]
fn key_updates_keep_the_connection_working() {
    let (server_config, certificate) = server_setup("db.example.com");
    let client_config = full_verification(&certificate, "db.example.com");
    let (server_socket, client_socket) = connected_pair();
    let server = thread::spawn(move || {
        let mut stream = TlsStream::accept(server_socket, &server_config).expect("server handshake");
        let mut buffer = [0u8; 6];
        stream.read_exact(&mut buffer).expect("first read");
        stream.write_all(b"after-1").expect("write");
        stream.read_exact(&mut buffer).expect("second read");
        stream.write_all(b"after-2").expect("write");
        buffer
    });
    let mut client = TlsStream::connect(client_socket, &client_config).expect("client handshake");
    client.update_keys(true).expect("key update");
    client.write_all(b"first.").expect("write");
    let mut reply = [0u8; 7];
    client.read_exact(&mut reply).expect("reply");
    assert_eq!(&reply, b"after-1");
    client.update_keys(false).expect("key update");
    client.write_all(b"second").expect("write");
    client.read_exact(&mut reply).expect("reply");
    assert_eq!(&reply, b"after-2");
    assert_eq!(&server.join().expect("server"), b"second");
}

#[test]
fn a_tls13_only_server_serves_a_client_that_offers_both_versions() {
    let (mut server_config, certificate) = server_setup("db.example.com");
    server_config.minimum_version = MinimumVersion::Tls13;
    let client_config = full_verification(&certificate, "db.example.com");
    let (server, client) = run_handshake(server_config, client_config);
    assert!(server.is_ok());
    assert!(client.is_ok());
}

#[test]
fn garbage_instead_of_a_client_hello_is_refused_without_panicking() {
    let (server_config, _) = server_setup("db.example.com");
    for garbage in [&b"GET / HTTP/1.1\r\n\r\n"[..], &[22, 3, 3, 0, 4, 1, 0, 0, 9], &[22, 3, 3, 0xff, 0xff], &[21, 3, 3, 0, 2, 2, 40]] {
        let (server_socket, mut client_socket) = connected_pair();
        let config = server_config.clone();
        let server = thread::spawn(move || TlsStream::accept(server_socket, &config));
        client_socket.write_all(garbage).expect("write");
        client_socket.shutdown(std::net::Shutdown::Write).expect("shutdown");
        assert!(server.join().expect("server thread must not panic").is_err());
    }
}

fn exchange_one_message(server_config: ServerConfig, client_config: ClientConfig) -> (u16, CipherSuite) {
    let (server_socket, client_socket) = connected_pair();
    let server = thread::spawn(move || {
        let mut stream = TlsStream::accept(server_socket, &server_config).expect("server handshake");
        let mut buffer = [0u8; 4];
        stream.read_exact(&mut buffer).expect("read");
        stream.write_all(&buffer).expect("echo");
        stream.close().expect("close");
    });
    let mut client = TlsStream::connect(client_socket, &client_config).expect("client handshake");
    client.write_all(b"ping").expect("write");
    let mut echoed = Vec::new();
    client.read_to_end(&mut echoed).expect("read");
    assert_eq!(echoed, b"ping");
    server.join().expect("server");
    (client.info().version, client.info().cipher_suite)
}

#[test]
fn a_tls12_only_client_gets_tls12_with_an_ecdsa_suite() {
    let (server_config, certificate) = server_setup("db.example.com");
    let mut client_config = full_verification(&certificate, "db.example.com");
    client_config.maximum_version = MaximumVersion::Tls12;
    let (negotiated, suite) = exchange_one_message(server_config, client_config);
    assert_eq!(negotiated, version::TLS12);
    assert_eq!(suite, CipherSuite::EcdheEcdsaChaCha20Poly1305Sha256);
}

#[test]
fn a_tls12_only_server_serves_a_client_that_offers_both_versions() {
    let (mut server_config, certificate) = server_setup("db.example.com");
    server_config.maximum_version = MaximumVersion::Tls12;
    let client_config = full_verification(&certificate, "db.example.com");
    let (negotiated, _) = exchange_one_message(server_config, client_config);
    assert_eq!(negotiated, version::TLS12);
}

#[test]
fn a_client_requiring_tls13_refuses_a_tls12_only_server() {
    let (mut server_config, certificate) = server_setup("db.example.com");
    server_config.maximum_version = MaximumVersion::Tls12;
    let mut client_config = full_verification(&certificate, "db.example.com");
    client_config.minimum_version = MinimumVersion::Tls13;
    let (server, client) = run_handshake(server_config, client_config);
    assert!(server.is_err());
    assert!(matches!(client, Err(TlsError::PeerAlert(_)) | Err(TlsError::Protocol { .. })));
}

#[test]
fn tls12_exports_the_same_keying_material_on_both_sides() {
    let (server_config, certificate) = server_setup("db.example.com");
    let mut client_config = full_verification(&certificate, "db.example.com");
    client_config.maximum_version = MaximumVersion::Tls12;
    let (server, client) = run_handshake(server_config, client_config);
    let (server, client) = (server.expect("server"), client.expect("client"));
    assert_eq!(
        server.export_keying_material(b"EXPORTER-test", None, 48),
        client.export_keying_material(b"EXPORTER-test", None, 48)
    );
    assert_eq!(server.info().tls_server_end_point(), client.info().tls_server_end_point());
}
