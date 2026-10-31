mod common;

use common::{LoginOptions, TestServer, batch_body, errors, login7_body, parse_tokens, prelogin_body, rows};
use eggdb_tdswire::messages::{ENCRYPT_NOT_SUP, ENCRYPT_OFF, ENCRYPT_ON};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::x509::SubjectAlternativeName;
use eggdb_tls::{ClientConfig, ServerConfig, ServerVerification, TlsStream};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const PRELOGIN: u8 = 0x12;
const LOGIN7: u8 = 0x10;
const SQL_BATCH: u8 = 0x01;
const HEADER_BYTES: usize = 8;

fn tls_config() -> ServerConfig {
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
    ServerConfig::new(vec![certificate], key).expect("TLS config")
}

struct PreloginFramed {
    tcp: TcpStream,
    framed: bool,
    left_in_packet: usize,
    pending: Vec<u8>,
}

impl Read for PreloginFramed {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.framed {
            return self.tcp.read(buffer);
        }
        while self.left_in_packet == 0 {
            let mut header = [0u8; HEADER_BYTES];
            self.tcp.read_exact(&mut header)?;
            assert_eq!(header[0], PRELOGIN);
            self.left_in_packet = usize::from(u16::from_be_bytes([header[2], header[3]])) - HEADER_BYTES;
        }
        let wanted = buffer.len().min(self.left_in_packet);
        let read = self.tcp.read(&mut buffer[..wanted])?;
        self.left_in_packet -= read;
        Ok(read)
    }
}

impl Write for PreloginFramed {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.framed {
            self.pending.extend_from_slice(buffer);
            Ok(buffer.len())
        } else {
            self.tcp.write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.framed && !self.pending.is_empty() {
            let mut packet = vec![PRELOGIN, 1];
            packet.extend_from_slice(&((self.pending.len() + HEADER_BYTES) as u16).to_be_bytes());
            packet.extend_from_slice(&[0, 0, 1, 0]);
            packet.append(&mut self.pending);
            self.tcp.write_all(&packet)?;
        }
        self.tcp.flush()
    }
}

fn send_message(stream: &mut impl Write, kind: u8, body: &[u8]) {
    let mut packet = vec![kind, 1];
    packet.extend_from_slice(&((body.len() + HEADER_BYTES) as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 0, 1, 0]);
    packet.extend_from_slice(body);
    stream.write_all(&packet).expect("send");
    stream.flush().expect("flush");
}

fn read_message(stream: &mut impl Read) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut header = [0u8; HEADER_BYTES];
        stream.read_exact(&mut header).ok()?;
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let start = body.len();
        body.resize(start + length - HEADER_BYTES, 0);
        stream.read_exact(&mut body[start..]).ok()?;
        if header[1] & 1 != 0 {
            return Some(body);
        }
    }
}

fn prelogin_encryption(answer: &[u8]) -> Option<u8> {
    let mut at = 0;
    while *answer.get(at)? != 0xFF {
        let token = answer[at];
        let offset = usize::from(u16::from_be_bytes([answer[at + 1], answer[at + 2]]));
        if token == 1 {
            return answer.get(offset).copied();
        }
        at += 5;
    }
    None
}

fn connect(server: &TestServer, encryption: u8) -> (TcpStream, u8) {
    let mut tcp = TcpStream::connect(server.address).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    send_message(&mut tcp, PRELOGIN, &prelogin_body(encryption));
    let answer = read_message(&mut tcp).expect("PRELOGIN answer");
    (tcp, prelogin_encryption(&answer).expect("encryption option"))
}

fn tls_in_prelogin(tcp: TcpStream) -> TlsStream<PreloginFramed> {
    let transport = PreloginFramed { tcp, framed: true, left_in_packet: 0, pending: Vec::new() };
    let mut tls = TlsStream::connect(transport, &ClientConfig::new(ServerVerification::EncryptionOnly, "localhost"))
        .expect("TLS handshake in PRELOGIN packets");
    tls.get_mut().framed = false;
    tls
}

fn assert_runs_a_batch(stream: &mut (impl Read + Write)) {
    send_message(stream, SQL_BATCH, &batch_body("SELECT 6 * 7"));
    let tokens = parse_tokens(&read_message(stream).expect("batch result"));
    assert!(errors(&tokens).is_empty(), "{:?}", errors(&tokens));
    assert_eq!(rows(&tokens), vec![vec![Some("42".to_owned())]]);
}

#[test]
fn a_client_asking_for_encryption_logs_in_and_runs_batches_over_tls() {
    let server = TestServer::start_with_tls(Arc::new(tls_config()));
    let (tcp, answer) = connect(&server, ENCRYPT_ON);
    assert_eq!(answer, ENCRYPT_ON);
    let mut tls = tls_in_prelogin(tcp);
    send_message(&mut tls, LOGIN7, &login7_body(&LoginOptions::default()));
    let tokens = parse_tokens(&read_message(&mut tls).expect("login answer"));
    assert!(errors(&tokens).is_empty(), "{:?}", errors(&tokens));
    assert_runs_a_batch(&mut tls);
}

#[test]
fn a_loopback_client_with_encryption_off_encrypts_only_its_login() {
    let server = TestServer::start_with_tls(Arc::new(tls_config()));
    let (tcp, answer) = connect(&server, ENCRYPT_OFF);
    assert_eq!(answer, ENCRYPT_OFF);
    let mut tls = tls_in_prelogin(tcp);
    send_message(&mut tls, LOGIN7, &login7_body(&LoginOptions::default()));
    let mut tcp = tls.into_inner().tcp;
    let tokens = parse_tokens(&read_message(&mut tcp).expect("plaintext login answer"));
    assert!(errors(&tokens).is_empty(), "{:?}", errors(&tokens));
    assert_runs_a_batch(&mut tcp);
}

#[test]
fn a_loopback_client_without_encryption_stays_in_plaintext() {
    let server = TestServer::start_with_tls(Arc::new(tls_config()));
    let (mut tcp, answer) = connect(&server, ENCRYPT_NOT_SUP);
    assert_eq!(answer, ENCRYPT_NOT_SUP);
    send_message(&mut tcp, LOGIN7, &login7_body(&LoginOptions::default()));
    let tokens = parse_tokens(&read_message(&mut tcp).expect("login answer"));
    assert!(errors(&tokens).is_empty());
    assert_runs_a_batch(&mut tcp);
}

#[test]
fn tds_8_strict_starts_with_tls_and_alpn() {
    let mut config = tls_config();
    config.application_protocols = vec![b"tds/8.0".to_vec()];
    let server = TestServer::start_with_tls(Arc::new(config));
    let tcp = TcpStream::connect(server.address).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(20))).expect("timeout");
    let mut client = ClientConfig::new(ServerVerification::EncryptionOnly, "localhost");
    client.application_protocols = vec![b"tds/8.0".to_vec()];
    let mut tls = TlsStream::connect(tcp, &client).expect("TLS first");
    send_message(&mut tls, PRELOGIN, &prelogin_body(ENCRYPT_ON));
    read_message(&mut tls).expect("PRELOGIN answer inside TLS");
    send_message(&mut tls, LOGIN7, &login7_body(&LoginOptions::default()));
    let tokens = parse_tokens(&read_message(&mut tls).expect("login answer"));
    assert!(errors(&tokens).is_empty(), "{:?}", errors(&tokens));
    assert_runs_a_batch(&mut tls);
}
