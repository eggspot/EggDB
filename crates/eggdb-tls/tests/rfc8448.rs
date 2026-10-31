use eggdb_crypto::{RsaPublicKey, x25519_public_key, x25519_shared_secret};
use eggdb_tls::alert::{Alert, AlertDescription};
use eggdb_tls::cipher_suite::{AeadKind, CipherSuite, HashKind};
use eggdb_tls::handshake::{
    CertificateMessage, ClientHello, DigitallySigned, EncryptedExtensions, HandshakeBuffer,
    NewSessionTicket, ServerHello, certificate_verify_content, finished_message, handshake_type,
    message_body, named_group, signature_scheme,
};
use eggdb_tls::key_schedule::{
    KeySchedule, TrafficKeys, finished_verify_data, resumption_pre_shared_key, traffic_keys,
};
use eggdb_tls::record::{
    MAX_TLS13_CIPHERTEXT_BYTES, RECORD_HEADER_BYTES, RecordHeader, Tls13RecordProtection,
    content_type,
};
use eggdb_tls::signature::{SignatureAlgorithm, SignatureHash, SubjectPublicKey, verify_signature};

const VECTORS: &str = include_str!("data/rfc8448_simple_1rtt.txt");

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

fn value(name: &str) -> Vec<u8> {
    VECTORS
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let (key, data) = line.split_once(' ')?;
            (key == name).then(|| hex(data))
        })
        .unwrap_or_else(|| panic!("no vector named {name}"))
}

fn array32(name: &str) -> [u8; 32] {
    value(name).try_into().expect("32 bytes")
}

fn transcript(messages: &[&str]) -> Vec<u8> {
    let mut hash = HashKind::Sha256.transcript();
    for name in messages {
        hash.update(&value(name));
    }
    hash.current()
}

fn protection(key_name: &str, iv_name: &str) -> Tls13RecordProtection {
    let keys = TrafficKeys { key: value(key_name), iv: value(iv_name).try_into().expect("12 bytes") };
    Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys).expect("AES-128 key")
}

fn open_record(protection: &mut Tls13RecordProtection, record: &[u8]) -> (u8, Vec<u8>) {
    let header_bytes: [u8; RECORD_HEADER_BYTES] = record[..RECORD_HEADER_BYTES].try_into().expect("header");
    let header = RecordHeader::parse(&header_bytes, MAX_TLS13_CIPHERTEXT_BYTES).expect("header parses");
    assert_eq!(header.length, record.len() - RECORD_HEADER_BYTES);
    let mut payload = record[RECORD_HEADER_BYTES..].to_vec();
    let inner_type = protection.open(&header, &mut payload).expect("record opens");
    (inner_type, payload)
}

#[test]
fn the_x25519_shares_agree_on_the_shared_secret() {
    let client_private = array32("client_x25519_private");
    let server_private = array32("server_x25519_private");
    assert_eq!(x25519_public_key(&client_private).to_vec(), value("client_x25519_public"));
    assert_eq!(x25519_public_key(&server_private).to_vec(), value("server_x25519_public"));
    let client_view = x25519_shared_secret(&client_private, &array32("server_x25519_public")).expect("secret");
    let server_view = x25519_shared_secret(&server_private, &array32("client_x25519_public")).expect("secret");
    assert_eq!(client_view, server_view);
    assert_eq!(client_view.to_vec(), value("shared_secret"));
}

#[test]
fn the_hellos_decode_and_encode_byte_for_byte() {
    let client_hello_message = value("client_hello");
    let client_hello = ClientHello::decode(message_body(&client_hello_message)).expect("ClientHello");
    assert_eq!(client_hello.encode(), client_hello_message);
    assert_eq!(client_hello.supported_versions(), Ok(vec![0x0304]));
    let shares = client_hello.key_shares().expect("key shares");
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].group, named_group::X25519);
    assert_eq!(shares[0].key_exchange, value("client_x25519_public"));
    assert_eq!(client_hello.server_name(), Ok(Some("server".to_owned())));
    assert!(client_hello.cipher_suites.contains(&CipherSuite::Tls13Aes128GcmSha256.id()));

    let server_hello_message = value("server_hello");
    let server_hello = ServerHello::decode(message_body(&server_hello_message)).expect("ServerHello");
    assert_eq!(server_hello.encode(), server_hello_message);
    assert!(!server_hello.is_hello_retry_request());
    assert_eq!(server_hello.selected_version(), Ok(Some(0x0304)));
    assert_eq!(CipherSuite::from_id(server_hello.cipher_suite), Some(CipherSuite::Tls13Aes128GcmSha256));
    let share = server_hello.key_share().expect("key share").expect("present");
    assert_eq!(share.key_exchange, value("server_x25519_public"));
}

#[test]
fn the_key_schedule_matches_every_published_secret() {
    let mut schedule = KeySchedule::new(HashKind::Sha256, None);
    assert_eq!(schedule.stage_secret(), value("early_secret"));
    let hello_hash = transcript(&["client_hello", "server_hello"]);
    assert_eq!(hello_hash, value("hello_hash"));
    let handshake = schedule.enter_handshake(&value("shared_secret"), &hello_hash);
    assert_eq!(schedule.stage_secret(), value("handshake_secret"));
    assert_eq!(handshake.client, value("client_handshake_traffic_secret"));
    assert_eq!(handshake.server, value("server_handshake_traffic_secret"));

    let server_keys = traffic_keys(HashKind::Sha256, AeadKind::Aes128Gcm, &handshake.server);
    assert_eq!(server_keys.key, value("server_handshake_key"));
    assert_eq!(server_keys.iv.to_vec(), value("server_handshake_iv"));
    let client_keys = traffic_keys(HashKind::Sha256, AeadKind::Aes128Gcm, &handshake.client);
    assert_eq!(client_keys.key, value("client_handshake_key"));
    assert_eq!(client_keys.iv.to_vec(), value("client_handshake_iv"));

    let before_server_finished = transcript(&[
        "client_hello",
        "server_hello",
        "encrypted_extensions",
        "certificate",
        "certificate_verify",
    ]);
    let server_verify_data = finished_verify_data(HashKind::Sha256, &handshake.server, &before_server_finished);
    assert_eq!(finished_message(&server_verify_data), value("server_finished"));

    let server_finished_hash = transcript(&[
        "client_hello",
        "server_hello",
        "encrypted_extensions",
        "certificate",
        "certificate_verify",
        "server_finished",
    ]);
    assert_eq!(server_finished_hash, value("server_finished_hash"));
    let (application, exporter) = schedule.enter_application(&server_finished_hash);
    assert_eq!(schedule.stage_secret(), value("master_secret"));
    assert_eq!(application.client, value("client_application_traffic_secret"));
    assert_eq!(application.server, value("server_application_traffic_secret"));
    assert_eq!(exporter, value("exporter_master_secret"));
    let server_application = traffic_keys(HashKind::Sha256, AeadKind::Aes128Gcm, &application.server);
    assert_eq!(server_application.key, value("server_application_key"));
    assert_eq!(server_application.iv.to_vec(), value("server_application_iv"));
    let client_application = traffic_keys(HashKind::Sha256, AeadKind::Aes128Gcm, &application.client);
    assert_eq!(client_application.key, value("client_application_key"));
    assert_eq!(client_application.iv.to_vec(), value("client_application_iv"));

    let client_verify_data = finished_verify_data(HashKind::Sha256, &handshake.client, &server_finished_hash);
    assert_eq!(finished_message(&client_verify_data), value("client_finished"));
    let client_finished_hash = transcript(&[
        "client_hello",
        "server_hello",
        "encrypted_extensions",
        "certificate",
        "certificate_verify",
        "server_finished",
        "client_finished",
    ]);
    assert_eq!(client_finished_hash, value("client_finished_hash"));
    let resumption = schedule.resumption_master_secret(&client_finished_hash);
    assert_eq!(resumption, value("resumption_master_secret"));
    let ticket_message = value("new_session_ticket");
    let ticket = NewSessionTicket::decode(message_body(&ticket_message)).expect("NewSessionTicket");
    assert_eq!(ticket.encode(), ticket_message);
    assert_eq!(resumption_pre_shared_key(HashKind::Sha256, &resumption, &ticket.nonce), value("resumption_psk"));
}

#[test]
fn the_server_flight_opens_into_its_four_messages() {
    let mut server_handshake = protection("server_handshake_key", "server_handshake_iv");
    let (inner_type, payload) = open_record(&mut server_handshake, &value("server_flight_record"));
    assert_eq!(inner_type, content_type::HANDSHAKE);
    assert_eq!(payload, value("server_flight_payload"));

    let mut buffer = HandshakeBuffer::new();
    buffer.push(&payload).expect("fragment");
    let mut messages = Vec::new();
    while let Some(message) = buffer.next_message() {
        messages.push(message);
    }
    let types: Vec<u8> = messages.iter().map(|(message_type, _)| *message_type).collect();
    assert_eq!(
        types,
        [
            handshake_type::ENCRYPTED_EXTENSIONS,
            handshake_type::CERTIFICATE,
            handshake_type::CERTIFICATE_VERIFY,
            handshake_type::FINISHED
        ]
    );
    let extensions = EncryptedExtensions::decode(message_body(&messages[0].1)).expect("EncryptedExtensions");
    assert_eq!(extensions.encode(), messages[0].1);
    let certificate = CertificateMessage::decode_tls13(message_body(&messages[1].1)).expect("Certificate");
    assert_eq!(certificate.entries.len(), 1);
    assert_eq!(certificate.encode_tls13(), messages[1].1);
    let verify = DigitallySigned::decode(message_body(&messages[2].1)).expect("CertificateVerify");
    assert_eq!(verify.scheme, signature_scheme::RSA_PSS_RSAE_SHA256);
    assert_eq!(verify.encode_certificate_verify(), messages[2].1);
}

#[test]
fn the_server_certificate_verify_signature_verifies_with_the_published_rsa_key() {
    let verify_message = value("certificate_verify");
    let verify = DigitallySigned::decode(message_body(&verify_message)).expect("CertificateVerify");
    let key = RsaPublicKey::new(&value("server_rsa_modulus"), &value("server_rsa_public_exponent")).expect("RSA key");
    let hash = transcript(&["client_hello", "server_hello", "encrypted_extensions", "certificate"]);
    let content = certificate_verify_content(true, &hash);
    let algorithm = SignatureAlgorithm::RsaPss { hash: SignatureHash::Sha256, salt_length: 32 };
    let public_key = SubjectPublicKey::Rsa(key);
    assert_eq!(verify_signature(algorithm, &public_key, &content, &verify.signature), Ok(()));
    let client_content = certificate_verify_content(false, &hash);
    assert!(verify_signature(algorithm, &public_key, &client_content, &verify.signature).is_err());
}

#[test]
fn sealing_reproduces_the_published_records_exactly() {
    let mut client_handshake = protection("client_handshake_key", "client_handshake_iv");
    let record = client_handshake.seal(content_type::HANDSHAKE, &value("client_finished")).expect("seal");
    assert_eq!(record, value("client_finished_record"));

    let mut client_application = protection("client_application_key", "client_application_iv");
    let record = client_application
        .seal(content_type::APPLICATION_DATA, &value("client_application_data"))
        .expect("seal");
    assert_eq!(record, value("client_application_data_record"));
    let record = client_application
        .seal(content_type::ALERT, &AlertDescription::CloseNotify.encode())
        .expect("seal");
    assert_eq!(record, value("client_alert_record"));

    let mut server_handshake = protection("server_handshake_key", "server_handshake_iv");
    let record = server_handshake
        .seal(content_type::HANDSHAKE, &value("server_flight_payload"))
        .expect("seal");
    assert_eq!(record, value("server_flight_record"));
}

#[test]
fn the_application_records_open_in_sequence() {
    let mut server_application = protection("server_application_key", "server_application_iv");
    let (inner_type, ticket) = open_record(&mut server_application, &value("new_session_ticket_record"));
    assert_eq!(inner_type, content_type::HANDSHAKE);
    assert_eq!(ticket, value("new_session_ticket"));
    let (inner_type, data) = open_record(&mut server_application, &value("server_application_data_record"));
    assert_eq!(inner_type, content_type::APPLICATION_DATA);
    assert_eq!(data, value("server_application_data"));
    let (inner_type, alert) = open_record(&mut server_application, &value("server_alert_record"));
    assert_eq!(inner_type, content_type::ALERT);
    assert!(Alert::decode(&alert).expect("alert").is_close_notify());

    let mut out_of_order = protection("server_application_key", "server_application_iv");
    let record = value("server_application_data_record");
    let header_bytes: [u8; RECORD_HEADER_BYTES] = record[..RECORD_HEADER_BYTES].try_into().expect("header");
    let header = RecordHeader::parse(&header_bytes, MAX_TLS13_CIPHERTEXT_BYTES).expect("header");
    let mut payload = record[RECORD_HEADER_BYTES..].to_vec();
    assert!(out_of_order.open(&header, &mut payload).is_err());
}
