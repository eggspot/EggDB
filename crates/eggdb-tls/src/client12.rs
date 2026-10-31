//! The client side of a TLS 1.2 handshake (RFC 5246) with ECDHE key exchange (RFC 8422), for
//! servers that do not speak TLS 1.3.

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, Tls12Authentication, version};
use crate::client::check_server_certificates;
use crate::config::ClientConfig;
use crate::error::TlsError;
use crate::handshake::{
    CertificateMessage, EcdheServerKeyExchange, ServerHello, ecdhe_client_key_exchange_message,
    extension_type, finished_message, handshake_type, message_body,
};
use crate::negotiation::{
    EphemeralKey, SUPPORTED_GROUPS, SUPPORTED_SIGNATURE_SCHEMES, scheme_algorithm, scheme_fits_key,
};
use crate::private_key::FillRandom;
use crate::record::content_type;
use crate::record_layer::RecordLayer;
use crate::server12::tls12_protection;
use crate::session::{Established, SessionInfo, Tls12Traffic, Traffic};
use crate::signature::{SubjectPublicKey, verify_signature};
use crate::tls12::{extended_master_secret, finished_verify_data, key_block, master_secret};
use crate::record::Tls12RecordProtection;
use eggdb_crypto::constant_time_eq;

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ClientConfig,
    client_hello_message: Vec<u8>,
    server_hello_message: Vec<u8>,
    server_hello: ServerHello,
    client_random: [u8; 32],
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let suite = CipherSuite::from_id(server_hello.cipher_suite)
        .filter(|suite| !suite.is_tls13())
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose a suite the client did not offer"))?;
    if server_hello.compression_method != 0 {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose compression"));
    }
    if let Some(info) = server_hello.extension(extension_type::RENEGOTIATION_INFO)
        && info != [0]
    {
        return Err(TlsError::protocol(AlertDescription::HandshakeFailure, "the server's renegotiation_info is not empty"));
    }
    let extended_master = server_hello.extension(extension_type::EXTENDED_MASTER_SECRET).is_some();
    let application_protocol = server_hello.application_protocol()?;
    if let Some(protocol) = &application_protocol
        && !config.application_protocols.contains(protocol)
    {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose an application protocol the client did not offer"));
    }
    let hash = suite.hash();
    let mut transcript = hash.transcript();
    transcript.update(&client_hello_message);
    transcript.update(&server_hello_message);
    layer.set_record_version(version::TLS12);

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::CERTIFICATE {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the server's Certificate"));
    }
    let certificates = CertificateMessage::decode_tls12(message_body(&message))?;
    let chain = check_server_certificates(config, &certificates.entries)?;
    let leaf_key = chain[0].public_key();
    let key_matches_suite = matches!(
        (suite.tls12_authentication(), leaf_key),
        (Some(Tls12Authentication::Ecdsa), SubjectPublicKey::P256(_)) | (Some(Tls12Authentication::Rsa), SubjectPublicKey::Rsa(_))
    );
    if !key_matches_suite {
        return Err(TlsError::protocol(AlertDescription::UnsupportedCertificate, "the certificate's key does not match the cipher suite"));
    }

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::SERVER_KEY_EXCHANGE {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected ServerKeyExchange"));
    }
    let key_exchange = EcdheServerKeyExchange::decode(message_body(&message))?;
    if !SUPPORTED_GROUPS.contains(&key_exchange.group) {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose a group the client did not offer"));
    }
    let scheme = key_exchange.signed.scheme;
    let algorithm = scheme_algorithm(scheme, false)
        .filter(|_| SUPPORTED_SIGNATURE_SCHEMES.contains(&scheme) && scheme_fits_key(scheme, leaf_key))
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server signed with a scheme the client did not offer"))?;
    let mut signed_content = client_random.to_vec();
    signed_content.extend_from_slice(&server_hello.random);
    signed_content.extend_from_slice(&EcdheServerKeyExchange::params(key_exchange.group, &key_exchange.public_key));
    verify_signature(algorithm, leaf_key, &signed_content, &key_exchange.signed.signature)
        .map_err(|_| TlsError::protocol(AlertDescription::DecryptError, "the server's key exchange signature does not verify"))?;

    let (mut message_type, mut message) = layer.read_handshake_message(Some(&mut transcript))?;
    let certificate_requested = message_type == handshake_type::CERTIFICATE_REQUEST;
    if certificate_requested {
        (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    }
    if message_type != handshake_type::SERVER_HELLO_DONE || !message_body(&message).is_empty() {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected ServerHelloDone"));
    }

    let ephemeral = EphemeralKey::generate(key_exchange.group, &mut *fill_random)?
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "unsupported group chosen"))?;
    let pre_master_secret = ephemeral
        .shared_secret(&key_exchange.public_key)
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server's key exchange is not a valid public key"))?;
    if certificate_requested {
        let empty = CertificateMessage::default().encode_tls12();
        transcript.update(&empty);
        layer.queue(content_type::HANDSHAKE, &empty)?;
    }
    let client_key_exchange = ecdhe_client_key_exchange_message(&ephemeral.public_key());
    transcript.update(&client_key_exchange);
    layer.queue(content_type::HANDSHAKE, &client_key_exchange)?;
    let master = if extended_master {
        extended_master_secret(hash, &pre_master_secret, &transcript.current())
    } else {
        master_secret(hash, &pre_master_secret, &client_random, &server_hello.random)
    };
    let iv_bytes = Tls12RecordProtection::iv_bytes(suite.aead());
    let keys = key_block(hash, &master, &client_random, &server_hello.random, suite.aead().key_bytes(), iv_bytes);

    layer.queue_change_cipher_spec();
    layer.set_write_protection(tls12_protection(suite, &keys.client_key, &keys.client_iv)?);
    let finished = finished_message(&finished_verify_data(hash, &master, true, &transcript.current()));
    transcript.update(&finished);
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;

    layer.read_change_cipher_spec()?;
    layer.set_read_protection(tls12_protection(suite, &keys.server_key, &keys.server_iv)?)?;
    let expected = finished_verify_data(hash, &master, false, &transcript.current());
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the server's Finished"));
    }
    if !constant_time_eq(message_body(&message), &expected) {
        return Err(TlsError::protocol(AlertDescription::DecryptError, "the server's Finished does not verify"));
    }

    Ok(Established {
        info: SessionInfo {
            version: version::TLS12,
            cipher_suite: suite,
            application_protocol,
            server_name: config.server_name_indication.clone(),
            server_certificate: chain.first().cloned(),
            peer_certificates: chain,
        },
        traffic: Traffic::Tls12(Tls12Traffic {
            hash,
            master_secret: master,
            client_random,
            server_random: server_hello.random,
        }),
    })
}
