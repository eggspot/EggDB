use std::io::{Cursor, Read, Write};

use eggdb_tls::cipher_suite::AeadKind;
use eggdb_tls::handshake::{
    CertificateMessage, ClientHello, EcdheServerKeyExchange, EncryptedExtensions, HandshakeBuffer,
    NewSessionTicket, ServerHello, message_body,
};
use eggdb_tls::key_schedule::TrafficKeys;
use eggdb_tls::record::{RECORD_HEADER_BYTES, RecordHeader, Tls12RecordProtection, Tls13RecordProtection};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::x509::SubjectAlternativeName;
use eggdb_tls::{ServerConfig, TlsStream};

const VECTORS: &str = include_str!("data/rfc8448_simple_1rtt.txt");

fn value(name: &str) -> Vec<u8> {
    let line = VECTORS
        .lines()
        .find(|line| line.split_once(' ').is_some_and(|(key, _)| key == name))
        .expect("vector");
    let text = line.split_once(' ').expect("value").1;
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

struct Mutations {
    state: u64,
}

impl Mutations {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn mutate(&mut self, original: &[u8]) -> Vec<u8> {
        let mut bytes = original.to_vec();
        let changes = 1 + (self.next() % 4) as usize;
        for _ in 0..changes {
            match self.next() % 4 {
                0 if !bytes.is_empty() => {
                    let position = (self.next() as usize) % bytes.len();
                    bytes[position] = self.next() as u8;
                }
                1 if !bytes.is_empty() => {
                    let position = (self.next() as usize) % bytes.len();
                    bytes.truncate(position);
                }
                2 => {
                    let position = (self.next() as usize) % (bytes.len() + 1);
                    bytes.insert(position, self.next() as u8);
                }
                _ if !bytes.is_empty() => {
                    let position = (self.next() as usize) % bytes.len();
                    bytes[position] ^= 0x80;
                }
                _ => {}
            }
        }
        bytes
    }
}

#[test]
fn mutated_handshake_messages_never_panic_a_decoder() {
    let client_hello = value("client_hello");
    let server_hello = value("server_hello");
    let certificate = value("certificate");
    let encrypted_extensions = value("encrypted_extensions");
    let ticket = value("new_session_ticket");
    let mut mutations = Mutations::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..5_000 {
        let mutated = mutations.mutate(message_body(&client_hello));
        if let Ok(hello) = ClientHello::decode(&mutated) {
            let _ = hello.server_name();
            let _ = hello.supported_versions();
            let _ = hello.supported_groups();
            let _ = hello.signature_algorithms();
            let _ = hello.key_shares();
            let _ = hello.application_protocols();
            let _ = hello.psk_key_exchange_modes();
            let _ = hello.encode();
        }
        if let Ok(hello) = ServerHello::decode(&mutations.mutate(message_body(&server_hello))) {
            let _ = hello.selected_version();
            let _ = hello.key_share();
            let _ = hello.retry_group();
            let _ = hello.application_protocol();
        }
        let _ = CertificateMessage::decode_tls13(&mutations.mutate(message_body(&certificate)));
        let _ = CertificateMessage::decode_tls12(&mutations.mutate(message_body(&certificate)));
        if let Ok(extensions) = EncryptedExtensions::decode(&mutations.mutate(message_body(&encrypted_extensions))) {
            let _ = extensions.application_protocol();
        }
        let _ = NewSessionTicket::decode(&mutations.mutate(message_body(&ticket)));
        let _ = EcdheServerKeyExchange::decode(&mutations.mutate(&[3, 0, 0x1d, 4, 1, 2, 3, 4, 8, 4, 0, 1, 9]));
    }
}

#[test]
fn random_records_never_panic_record_protection() {
    let keys = TrafficKeys { key: vec![1u8; 16], iv: [2u8; 12] };
    let mut mutations = Mutations::new(0x1234_5678_9abc_def1);
    let sample = value("server_flight_record");
    for _ in 0..2_000 {
        let mutated = mutations.mutate(&sample);
        let Some(header_bytes) = mutated.get(..RECORD_HEADER_BYTES) else {
            continue;
        };
        let header_bytes: [u8; RECORD_HEADER_BYTES] = header_bytes.try_into().expect("five bytes");
        let Ok(header) = RecordHeader::parse(&header_bytes, 1 << 15) else {
            continue;
        };
        let mut tls13 = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys).expect("key");
        let mut payload = mutated[RECORD_HEADER_BYTES..].to_vec();
        let _ = tls13.open(&header, &mut payload);
        let mut tls12 = Tls12RecordProtection::new(AeadKind::Aes128Gcm, &[1u8; 16], &[2u8; 4]).expect("key");
        let mut payload = mutated[RECORD_HEADER_BYTES..].to_vec();
        let _ = tls12.open(&header, &mut payload);
    }
}

#[test]
fn random_fragments_never_panic_the_handshake_buffer() {
    let mut mutations = Mutations::new(0x0bad_cafe_f00d_d00d);
    for _ in 0..2_000 {
        let mut buffer = HandshakeBuffer::new();
        for _ in 0..8 {
            let length = (mutations.next() % 40) as usize;
            let fragment: Vec<u8> = (0..length).map(|_| mutations.next() as u8).collect();
            if buffer.push(&fragment).is_err() {
                break;
            }
            while buffer.next_message().is_some() {}
        }
    }
}

struct Scripted {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl Read for Scripted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer)
    }
}

impl Write for Scripted {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_server_given_mutated_client_hellos_fails_cleanly() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .expect("clock");
    let request = SelfSignedCertificateRequest {
        common_name: "server".to_owned(),
        subject_alternative_names: vec![SubjectAlternativeName::DnsName("server".to_owned())],
        not_before_unix_seconds: now - 60,
        not_after_unix_seconds: now + 3_600,
    };
    let (certificate, key) =
        generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill).expect("certificate");
    let config = ServerConfig::new(vec![certificate], key).expect("config");
    let record = value("client_hello_record");
    let mut mutations = Mutations::new(0x5eed_5eed_5eed_5eed);
    for round in 0..300 {
        let input = if round == 0 { record.clone() } else { mutations.mutate(&record) };
        let stream = Scripted { input: Cursor::new(input), output: Vec::new() };
        let outcome = TlsStream::accept(stream, &config);
        assert!(outcome.is_err(), "a lone ClientHello can never finish a handshake");
    }
}
