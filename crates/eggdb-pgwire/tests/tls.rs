mod common;

use common::{DATABASE, PASSWORD, TestServer, USER, cstring};
use eggdb_crypto::{base64_decode, base64_encode, hmac_sha256, pbkdf2_hmac_sha256, sha256};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::x509::SubjectAlternativeName;
use eggdb_tls::{ClientConfig, ServerConfig, ServerVerification, TlsStream};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const SSL_REQUEST_CODE: u32 = 80_877_103;
const PROTOCOL_3_0: u32 = 196_608;

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
    let mut config = ServerConfig::new(vec![certificate], key).expect("TLS config");
    config.application_protocols = vec![b"postgresql".to_vec()];
    TestServer::start_with_tls(Arc::new(config))
}

fn tcp(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(address).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    stream
}

fn client_config(application_protocols: Vec<Vec<u8>>) -> ClientConfig {
    let mut config = ClientConfig::new(ServerVerification::EncryptionOnly, "localhost");
    config.application_protocols = application_protocols;
    config
}

fn after_ssl_request(address: SocketAddr) -> TlsStream<TcpStream> {
    let mut stream = tcp(address);
    let mut request = 8u32.to_be_bytes().to_vec();
    request.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&request).expect("SSLRequest");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("answer");
    assert_eq!(answer[0], b'S');
    TlsStream::connect(stream, &client_config(Vec::new())).expect("TLS handshake")
}

fn send_startup(stream: &mut impl Write) {
    let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
    for (name, value) in [("user", USER), ("database", DATABASE)] {
        body.extend_from_slice(&cstring(name));
        body.extend_from_slice(&cstring(value));
    }
    body.push(0);
    let mut packet = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&body);
    stream.write_all(&packet).expect("startup");
}

fn send(stream: &mut impl Write, tag: u8, body: &[u8]) {
    let mut message = vec![tag];
    message.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    message.extend_from_slice(body);
    stream.write_all(&message).expect("send");
}

fn read_message(stream: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; length - 4];
    stream.read_exact(&mut body).ok()?;
    Some((header[0], body))
}

fn scram_login(stream: &mut (impl Read + Write), mechanism: &str, binding: Option<&[u8]>) -> (u8, Vec<u8>) {
    let (tag, body) = read_message(stream).expect("authentication request");
    assert_eq!(tag, b'R');
    assert_eq!(&body[..4], &10i32.to_be_bytes());
    assert_eq!(&body[4..], b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
    let gs2_header = if mechanism.ends_with("-PLUS") { "p=tls-server-end-point,," } else { "y,," };
    let client_nonce = "fyko+d2lbbFgONRv9qkxdawL";
    let client_first_bare = format!("n=,r={client_nonce}");
    let client_first = format!("{gs2_header}{client_first_bare}");
    let mut initial = cstring(mechanism);
    initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    initial.extend_from_slice(client_first.as_bytes());
    send(stream, b'p', &initial);
    let (tag, body) = read_message(stream).expect("SASL continue");
    if tag == b'E' {
        return (tag, body);
    }
    let server_first = String::from_utf8(body[4..].to_vec()).expect("text");
    let mut nonce = "";
    let mut salt = Vec::new();
    let mut iterations = 0;
    for attribute in server_first.split(',') {
        if let Some(value) = attribute.strip_prefix("r=") {
            nonce = value;
        } else if let Some(value) = attribute.strip_prefix("s=") {
            salt = base64_decode(value.as_bytes()).expect("salt");
        } else if let Some(value) = attribute.strip_prefix("i=") {
            iterations = value.parse().expect("iterations");
        }
    }
    let mut salted = [0u8; 32];
    pbkdf2_hmac_sha256(PASSWORD.as_bytes(), &salt, iterations, &mut salted);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256(&client_key);
    let mut channel = gs2_header.as_bytes().to_vec();
    channel.extend_from_slice(binding.unwrap_or_default());
    let without_proof = format!("c={},r={nonce}", base64_encode(&channel));
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(key, signed)| key ^ signed).collect();
    send(stream, b'p', format!("{without_proof},p={}", base64_encode(&proof)).as_bytes());
    let (tag, body) = read_message(stream).expect("SASL final");
    if tag == b'E' {
        return (tag, body);
    }
    read_message(stream).expect("authentication ok")
}

fn query_one(stream: &mut (impl Read + Write)) -> String {
    loop {
        let (tag, _) = read_message(stream).expect("startup messages");
        if tag == b'Z' {
            break;
        }
    }
    send(stream, b'Q', &cstring("SELECT 40 + 2"));
    loop {
        let (tag, body) = read_message(stream).expect("query messages");
        if tag == b'D' {
            let length = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
            return String::from_utf8(body[6..6 + length].to_vec()).expect("text");
        }
        assert_ne!(tag, b'E', "the query failed");
    }
}

#[test]
fn ssl_request_then_scram_plus_logs_in_and_runs_a_query() {
    let server = tls_server();
    let mut stream = after_ssl_request(server.address);
    let binding = stream.info().tls_server_end_point().expect("channel binding");
    send_startup(&mut stream);
    let (tag, body) = scram_login(&mut stream, "SCRAM-SHA-256-PLUS", Some(&binding));
    assert_eq!(tag, b'R', "{}", String::from_utf8_lossy(&body));
    assert_eq!(&body[..4], &0i32.to_be_bytes());
    assert_eq!(query_one(&mut stream), "42");
}

#[test]
fn a_wrong_channel_binding_is_refused() {
    let server = tls_server();
    let mut stream = after_ssl_request(server.address);
    send_startup(&mut stream);
    let (tag, _) = scram_login(&mut stream, "SCRAM-SHA-256-PLUS", Some(&[0u8; 32]));
    assert_eq!(tag, b'E');
}

#[test]
fn a_client_claiming_no_server_binding_support_is_refused_over_tls() {
    let server = tls_server();
    let mut stream = after_ssl_request(server.address);
    send_startup(&mut stream);
    let (tag, _) = scram_login(&mut stream, "SCRAM-SHA-256", None);
    assert_eq!(tag, b'E');
}

#[test]
fn direct_tls_with_the_postgresql_protocol_logs_in() {
    let server = tls_server();
    let mut stream =
        TlsStream::connect(tcp(server.address), &client_config(vec![b"postgresql".to_vec()])).expect("direct TLS");
    assert_eq!(stream.info().application_protocol, Some(b"postgresql".to_vec()));
    let binding = stream.info().tls_server_end_point().expect("channel binding");
    send_startup(&mut stream);
    let (tag, _) = scram_login(&mut stream, "SCRAM-SHA-256-PLUS", Some(&binding));
    assert_eq!(tag, b'R');
    assert_eq!(query_one(&mut stream), "42");
}

#[test]
fn direct_tls_without_alpn_is_closed() {
    let server = tls_server();
    let mut stream = TlsStream::connect(tcp(server.address), &client_config(Vec::new())).expect("handshake");
    send_startup(&mut stream);
    assert!(read_message(&mut stream).is_none());
}

#[test]
fn a_server_without_tls_still_answers_n() {
    let server = TestServer::start(4);
    let mut stream = tcp(server.address);
    let mut request = 8u32.to_be_bytes().to_vec();
    request.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&request).expect("SSLRequest");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("answer");
    assert_eq!(answer[0], b'N');
}
