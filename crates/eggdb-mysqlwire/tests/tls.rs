mod common;

use common::{DATABASE, PASSWORD, TestServer, USER, handshake_response};
use eggdb_mysqlwire::packets::CLIENT_SSL;
use eggdb_mysqlwire::{CACHING_SHA2_PASSWORD, NONCE_BYTES, caching_sha2_response};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::x509::SubjectAlternativeName;
use eggdb_tls::{ClientConfig, ServerConfig, ServerVerification, TlsStream};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

fn tls_server() -> TestServer {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .expect("clock");
    let request = SelfSignedCertificateRequest {
        common_name: "localhost".to_owned(),
        subject_alternative_names: vec![SubjectAlternativeName::DnsName("localhost".to_owned())],
        not_before_unix_seconds: now - 60,
        not_after_unix_seconds: now + 3_600,
    };
    let (certificate, key) =
        generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill).expect("certificate");
    TestServer::start_with_tls(Arc::new(ServerConfig::new(vec![certificate], key).expect("TLS config")))
}

fn write_packet(stream: &mut impl Write, sequence: u8, payload: &[u8]) {
    let mut packet = (payload.len() as u32).to_le_bytes()[..3].to_vec();
    packet.push(sequence);
    packet.extend_from_slice(payload);
    stream.write_all(&packet).expect("write");
}

fn read_packet(stream: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).ok()?;
    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).ok()?;
    Some((header[3], payload))
}

fn server_greeting(stream: &mut TcpStream) -> ([u8; NONCE_BYTES], u32) {
    let (_, body) = read_packet(stream).expect("handshake");
    assert_eq!(body[0], 10);
    let version_end = body[1..].iter().position(|&byte| byte == 0).expect("version") + 1;
    let mut at = version_end + 1 + 4;
    let mut nonce = [0u8; NONCE_BYTES];
    nonce[..8].copy_from_slice(&body[at..at + 8]);
    at += 9;
    let lower = u16::from_le_bytes([body[at], body[at + 1]]);
    at += 2 + 1 + 2;
    let upper = u16::from_le_bytes([body[at], body[at + 1]]);
    at += 2 + 1 + 10;
    nonce[8..].copy_from_slice(&body[at..at + 12]);
    (nonce, u32::from(lower) | u32::from(upper) << 16)
}

#[test]
fn the_login_and_queries_run_over_tls_after_an_ssl_request() {
    let server = tls_server();
    let mut tcp = TcpStream::connect(server.address).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    let (nonce, capabilities) = server_greeting(&mut tcp);
    assert_ne!(capabilities & CLIENT_SSL, 0, "a server with TLS offers CLIENT_SSL");

    let full = handshake_response(USER, &caching_sha2_response(PASSWORD.as_bytes(), &nonce), DATABASE, CACHING_SHA2_PASSWORD);
    let mut flags = u32::from_le_bytes([full[0], full[1], full[2], full[3]]) | CLIENT_SSL;
    let mut ssl_request = full[..32].to_vec();
    ssl_request[..4].copy_from_slice(&flags.to_le_bytes());
    write_packet(&mut tcp, 1, &ssl_request);
    let mut tls = TlsStream::connect(tcp, &ClientConfig::new(ServerVerification::EncryptionOnly, "localhost"))
        .expect("TLS handshake");

    let mut response = full;
    flags |= CLIENT_SSL;
    response[..4].copy_from_slice(&flags.to_le_bytes());
    write_packet(&mut tls, 2, &response);
    let (_, ok) = read_packet(&mut tls).expect("login answer");
    assert_eq!(ok[0], 0x00, "login over TLS succeeds: {ok:?}");

    let mut query = vec![0x03];
    query.extend_from_slice(b"SELECT 6 * 7");
    write_packet(&mut tls, 0, &query);
    let mut saw_row = false;
    while let Some((_, packet)) = read_packet(&mut tls) {
        if packet.len() > 1 && &packet[1..] == b"42" {
            saw_row = true;
        }
        if packet[0] == 0xFE && packet.len() < 9 && saw_row {
            break;
        }
        assert_ne!(packet[0], 0xFF, "the query failed");
    }
    assert!(saw_row);
}

#[test]
fn a_server_without_tls_does_not_offer_it() {
    let server = TestServer::start(4);
    let mut tcp = TcpStream::connect(server.address).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    let (_, capabilities) = server_greeting(&mut tcp);
    assert_eq!(capabilities & CLIENT_SSL, 0);
}
