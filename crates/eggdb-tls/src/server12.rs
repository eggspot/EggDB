//! The server side of a TLS 1.2 handshake (RFC 5246) with ECDHE key exchange (RFC 8422), AEAD
//! suites only, extended master secret when offered (RFC 7627), no resumption and no
//! renegotiation (RFC 5746).

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, Tls12Authentication, version};
use crate::config::{MaximumVersion, ServerConfig};
use crate::error::TlsError;
use crate::handshake::{
    CertificateMessage, ClientHello, DOWNGRADE_TO_TLS12_SENTINEL, DigitallySigned,
    EcdheServerKeyExchange, Extension, ServerHello, decode_ecdhe_client_key_exchange,
    extension_type, finished_message, handshake_message, handshake_type, message_body,
    named_group,
};
use crate::negotiation::{
    EphemeralKey, SUPPORTED_GROUPS, choose_application_protocol, choose_signature_scheme,
    scheme_algorithm,
};
use crate::private_key::FillRandom;
use crate::record::{Tls12RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer};
use crate::session::{Established, SessionInfo, Tls12Traffic, Traffic};
use crate::signature::SubjectPublicKey;
use crate::tls12::{extended_master_secret, finished_verify_data, key_block, master_secret};
use eggdb_crypto::constant_time_eq;

const EMPTY_RENEGOTIATION_INFO_SCSV: u16 = 0x00ff;

pub(crate) fn tls12_protection(
    suite: CipherSuite,
    key: &[u8],
    iv: &[u8],
) -> Result<Protection, TlsError> {
    Tls12RecordProtection::new(suite.aead(), key, iv)
        .map(Protection::Tls12)
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "TLS 1.2 key of the wrong length"))
}

fn suite_for_key(offered: &[u16], key: &SubjectPublicKey) -> Option<CipherSuite> {
    let authentication = match key {
        SubjectPublicKey::P256(_) => Tls12Authentication::Ecdsa,
        SubjectPublicKey::Rsa(_) => Tls12Authentication::Rsa,
        SubjectPublicKey::Unsupported => return None,
    };
    CipherSuite::ALL
        .into_iter()
        .filter(|suite| suite.tls12_authentication() == Some(authentication))
        .find(|suite| offered.contains(&suite.id()))
}

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ServerConfig,
    client_hello_message: Vec<u8>,
    hello: ClientHello,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let suite = suite_for_key(&hello.cipher_suites, &config.private_key().public_key())
        .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no TLS 1.2 cipher suite in common"))?;
    let hash = suite.hash();
    let renegotiation_signalled = match hello.extension(extension_type::RENEGOTIATION_INFO) {
        Some([0]) => true,
        Some(_) => return Err(TlsError::protocol(AlertDescription::HandshakeFailure, "renegotiation is not supported")),
        None => hello.cipher_suites.contains(&EMPTY_RENEGOTIATION_INFO_SCSV),
    };
    let extended_master = hello.extension(extension_type::EXTENDED_MASTER_SECRET).is_some();
    let point_formats = hello.extension(extension_type::EC_POINT_FORMATS);
    if let Some(formats) = point_formats
        && !formats.get(1..).is_some_and(|list| list.contains(&0))
    {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the client cannot read uncompressed points"));
    }
    let client_groups = hello.supported_groups()?;
    let group = if client_groups.is_empty() {
        named_group::SECP256R1
    } else {
        SUPPORTED_GROUPS
            .into_iter()
            .find(|group| client_groups.contains(group))
            .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no key exchange group in common"))?
    };
    let scheme = choose_signature_scheme(config.private_key(), &hello.signature_algorithms()?, false)
        .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no signature scheme in common"))?;
    let algorithm = scheme_algorithm(scheme, false)
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "the chosen signature scheme has no algorithm"))?;
    let application_protocol = choose_application_protocol(&config.application_protocols, &hello.application_protocols()?)
        .map_err(|_| TlsError::protocol(AlertDescription::NoApplicationProtocol, "no application protocol in common"))?;
    let server_name = hello.server_name()?;

    let mut transcript = hash.transcript();
    transcript.update(&client_hello_message);
    let mut server_random = [0u8; 32];
    fill_random(&mut server_random)?;
    if config.maximum_version == MaximumVersion::Tls13 {
        server_random[24..].copy_from_slice(&DOWNGRADE_TO_TLS12_SENTINEL);
    }

    let mut extensions = Vec::new();
    if renegotiation_signalled {
        extensions.push(Extension::empty_renegotiation_info());
    }
    if extended_master {
        extensions.push(Extension::empty(extension_type::EXTENDED_MASTER_SECRET));
    }
    if point_formats.is_some() {
        extensions.push(Extension::uncompressed_point_format());
    }
    if let Some(protocol) = &application_protocol {
        extensions.push(Extension::application_protocols(&[protocol.as_slice()]));
    }
    if server_name.is_some() {
        extensions.push(Extension::empty(extension_type::SERVER_NAME));
    }
    let server_hello = ServerHello {
        legacy_version: version::TLS12,
        random: server_random,
        legacy_session_id_echo: Vec::new(),
        cipher_suite: suite.id(),
        compression_method: 0,
        extensions,
    }
    .encode();
    transcript.update(&server_hello);
    layer.queue(content_type::HANDSHAKE, &server_hello)?;

    let chain: Vec<&[u8]> = config.chain().iter().map(|certificate| certificate.der()).collect();
    let certificate = CertificateMessage::from_chain(&chain).encode_tls12();
    transcript.update(&certificate);
    layer.queue(content_type::HANDSHAKE, &certificate)?;

    let ephemeral = EphemeralKey::generate(group, &mut *fill_random)?
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "unsupported group chosen"))?;
    let public_key = ephemeral.public_key();
    let mut signed_content = hello.random.to_vec();
    signed_content.extend_from_slice(&server_random);
    signed_content.extend_from_slice(&EcdheServerKeyExchange::params(group, &public_key));
    let signature = config
        .private_key()
        .sign(algorithm, &signed_content, &mut *fill_random)
        .map_err(|_| TlsError::protocol(AlertDescription::InternalError, "signing the key exchange failed"))?;
    let key_exchange = EcdheServerKeyExchange { group, public_key, signed: DigitallySigned { scheme, signature } }.encode();
    transcript.update(&key_exchange);
    layer.queue(content_type::HANDSHAKE, &key_exchange)?;
    let done = handshake_message(handshake_type::SERVER_HELLO_DONE, &[]);
    transcript.update(&done);
    layer.queue(content_type::HANDSHAKE, &done)?;
    layer.flush()?;

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::CLIENT_KEY_EXCHANGE {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected ClientKeyExchange"));
    }
    let client_public_key = decode_ecdhe_client_key_exchange(message_body(&message))?;
    let pre_master_secret = ephemeral
        .shared_secret(&client_public_key)
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the client's key exchange is not a valid public key"))?;
    let master = if extended_master {
        extended_master_secret(hash, &pre_master_secret, &transcript.current())
    } else {
        master_secret(hash, &pre_master_secret, &hello.random, &server_random)
    };
    let iv_bytes = Tls12RecordProtection::iv_bytes(suite.aead());
    let keys = key_block(hash, &master, &hello.random, &server_random, suite.aead().key_bytes(), iv_bytes);

    layer.read_change_cipher_spec()?;
    layer.set_read_protection(tls12_protection(suite, &keys.client_key, &keys.client_iv)?)?;
    let expected = finished_verify_data(hash, &master, true, &transcript.current());
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the client's Finished"));
    }
    if !constant_time_eq(message_body(&message), &expected) {
        return Err(TlsError::protocol(AlertDescription::DecryptError, "the client's Finished does not verify"));
    }
    transcript.update(&message);

    layer.queue_change_cipher_spec();
    layer.set_write_protection(tls12_protection(suite, &keys.server_key, &keys.server_iv)?);
    let finished = finished_message(&finished_verify_data(hash, &master, false, &transcript.current()));
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;

    Ok(Established {
        info: SessionInfo {
            version: version::TLS12,
            cipher_suite: suite,
            application_protocol,
            server_name,
            peer_certificates: Vec::new(),
            server_certificate: config.chain().first().cloned(),
        },
        traffic: Traffic::Tls12(Tls12Traffic {
            hash,
            master_secret: master,
            client_random: hello.random,
            server_random,
        }),
    })
}
