//! The server side of the handshake: TLS 1.3 (RFC 8446), with a HelloRetryRequest when the
//! client's key shares miss EggDB's groups, and TLS 1.2 handed to [`crate::server12`].

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, version};
use crate::config::{MaximumVersion, MinimumVersion, ServerConfig};
use crate::error::TlsError;
use crate::handshake::{
    CertificateMessage, ClientHello, DigitallySigned, EncryptedExtensions, Extension,
    HELLO_RETRY_REQUEST_RANDOM, KeyShareEntry, ServerHello, certificate_verify_content,
    extension_type, finished_message, handshake_type, message_body, message_hash_message,
};
use crate::key_schedule::{KeySchedule, finished_verify_data, traffic_keys};
use crate::negotiation::{
    EphemeralKey, SUPPORTED_GROUPS, choose_application_protocol, choose_signature_scheme,
    scheme_algorithm,
};
use crate::private_key::FillRandom;
use crate::record::{Tls13RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer};
use crate::session::{Established, SessionInfo, Tls13Traffic, Traffic};
use eggdb_crypto::constant_time_eq;

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ServerConfig,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::CLIENT_HELLO {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "the client did not start with a ClientHello"));
    }
    let hello = ClientHello::decode(message_body(&message))?;
    if !hello.compression_methods.contains(&0) {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the client offers no null compression"));
    }
    let versions = hello.supported_versions()?;
    if versions.contains(&version::TLS13) && config.maximum_version == MaximumVersion::Tls13 {
        return tls13(layer, config, message, hello, fill_random);
    }
    let offers_tls12 = if versions.is_empty() {
        hello.legacy_version >= version::TLS12
    } else {
        versions.contains(&version::TLS12)
    };
    if config.minimum_version == MinimumVersion::Tls12 && offers_tls12 {
        return crate::server12::handshake(layer, config, message, hello, fill_random);
    }
    Err(TlsError::protocol(AlertDescription::ProtocolVersion, "the client offers no TLS version EggDB accepts"))
}

pub(crate) fn choose_suite(offered: &[u16], tls13: bool) -> Option<CipherSuite> {
    CipherSuite::ALL
        .into_iter()
        .filter(|suite| suite.is_tls13() == tls13)
        .find(|suite| offered.contains(&suite.id()))
}

fn preferred_share(hello: &ClientHello) -> Result<Option<KeyShareEntry>, TlsError> {
    let shares = hello.key_shares()?;
    Ok(SUPPORTED_GROUPS
        .into_iter()
        .find_map(|group| shares.iter().find(|share| share.group == group).cloned()))
}

fn tls13<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ServerConfig,
    first_message: Vec<u8>,
    first_hello: ClientHello,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let suite = choose_suite(&first_hello.cipher_suites, true)
        .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no TLS 1.3 cipher suite in common"))?;
    let hash = suite.hash();
    let mut transcript = hash.transcript();
    let mut sent_change_cipher_spec = false;
    let (hello, hello_message, share) = match preferred_share(&first_hello)? {
        Some(share) => (first_hello, first_message, share),
        None => {
            let groups = first_hello.supported_groups()?;
            let group = SUPPORTED_GROUPS
                .into_iter()
                .find(|group| groups.contains(group))
                .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no key exchange group in common"))?;
            transcript.update(&message_hash_message(&hash.digest(&first_message)));
            let retry = ServerHello {
                legacy_version: version::TLS12,
                random: HELLO_RETRY_REQUEST_RANDOM,
                legacy_session_id_echo: first_hello.legacy_session_id.clone(),
                cipher_suite: suite.id(),
                compression_method: 0,
                extensions: vec![
                    Extension::server_supported_version(version::TLS13),
                    Extension::retry_key_share(group),
                ],
            }
            .encode();
            transcript.update(&retry);
            layer.queue(content_type::HANDSHAKE, &retry)?;
            if !first_hello.legacy_session_id.is_empty() {
                layer.queue_change_cipher_spec();
                sent_change_cipher_spec = true;
            }
            layer.flush()?;
            layer.ignore_change_cipher_spec = true;
            let (message_type, second_message) = layer.read_handshake_message(None)?;
            if message_type != handshake_type::CLIENT_HELLO {
                return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected a second ClientHello"));
            }
            let second_hello = ClientHello::decode(message_body(&second_message))?;
            let shares = second_hello.key_shares()?;
            let consistent = second_hello.legacy_session_id == first_hello.legacy_session_id
                && second_hello.cipher_suites.contains(&suite.id())
                && second_hello.supported_versions()?.contains(&version::TLS13)
                && shares.len() == 1
                && shares[0].group == group;
            if !consistent {
                return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the second ClientHello does not answer the retry"));
            }
            let share = shares[0].clone();
            (second_hello, second_message, share)
        }
    };
    transcript.update(&hello_message);

    let scheme = choose_signature_scheme(config.private_key(), &hello.signature_algorithms()?, true)
        .ok_or(TlsError::protocol(AlertDescription::HandshakeFailure, "no signature scheme in common"))?;
    let algorithm = scheme_algorithm(scheme, true)
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "the chosen signature scheme has no algorithm"))?;
    let application_protocol = choose_application_protocol(&config.application_protocols, &hello.application_protocols()?)
        .map_err(|_| TlsError::protocol(AlertDescription::NoApplicationProtocol, "no application protocol in common"))?;
    let server_name = hello.server_name()?;

    let ephemeral = EphemeralKey::generate(share.group, &mut *fill_random)?
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "unsupported group chosen"))?;
    let shared_secret = ephemeral
        .shared_secret(&share.key_exchange)
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the client's key share is not a valid public key"))?;
    let mut random = [0u8; 32];
    fill_random(&mut random)?;
    let server_hello = ServerHello {
        legacy_version: version::TLS12,
        random,
        legacy_session_id_echo: hello.legacy_session_id.clone(),
        cipher_suite: suite.id(),
        compression_method: 0,
        extensions: vec![
            Extension::server_supported_version(version::TLS13),
            Extension::server_key_share(&KeyShareEntry { group: share.group, key_exchange: ephemeral.public_key() }),
        ],
    }
    .encode();
    transcript.update(&server_hello);
    layer.queue(content_type::HANDSHAKE, &server_hello)?;
    if !sent_change_cipher_spec && !hello.legacy_session_id.is_empty() {
        layer.queue_change_cipher_spec();
    }

    let mut schedule = KeySchedule::new(hash, None);
    let handshake_secrets = schedule.enter_handshake(&shared_secret, &transcript.current());
    let protection = |secret: &[u8]| {
        Tls13RecordProtection::new(suite.aead(), &traffic_keys(hash, suite.aead(), secret))
            .map(Protection::Tls13)
            .ok_or(TlsError::protocol(AlertDescription::InternalError, "traffic key of the wrong length"))
    };
    layer.set_write_protection(protection(&handshake_secrets.server)?);
    layer.set_read_protection(protection(&handshake_secrets.client)?)?;
    layer.ignore_change_cipher_spec = true;

    let mut extensions = Vec::new();
    if let Some(protocol) = &application_protocol {
        extensions.push(Extension::application_protocols(&[protocol.as_slice()]));
    }
    if server_name.is_some() {
        extensions.push(Extension::empty(extension_type::SERVER_NAME));
    }
    let encrypted_extensions = EncryptedExtensions { extensions }.encode();
    transcript.update(&encrypted_extensions);
    layer.queue(content_type::HANDSHAKE, &encrypted_extensions)?;

    let chain: Vec<&[u8]> = config.chain().iter().map(|certificate| certificate.der()).collect();
    let certificate = CertificateMessage::from_chain(&chain).encode_tls13();
    transcript.update(&certificate);
    layer.queue(content_type::HANDSHAKE, &certificate)?;

    let content = certificate_verify_content(true, &transcript.current());
    let signature = config
        .private_key()
        .sign(algorithm, &content, &mut *fill_random)
        .map_err(|_| TlsError::protocol(AlertDescription::InternalError, "signing the handshake failed"))?;
    let certificate_verify = DigitallySigned { scheme, signature }.encode_certificate_verify();
    transcript.update(&certificate_verify);
    layer.queue(content_type::HANDSHAKE, &certificate_verify)?;

    let finished = finished_message(&finished_verify_data(hash, &handshake_secrets.server, &transcript.current()));
    transcript.update(&finished);
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;

    let (application_secrets, exporter_master_secret) = schedule.enter_application(&transcript.current());
    layer.set_write_protection(protection(&application_secrets.server)?);

    let expected = finished_verify_data(hash, &handshake_secrets.client, &transcript.current());
    let (message_type, client_finished) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the client's Finished"));
    }
    if !constant_time_eq(message_body(&client_finished), &expected) {
        return Err(TlsError::protocol(AlertDescription::DecryptError, "the client's Finished does not verify"));
    }
    layer.set_read_protection(protection(&application_secrets.client)?)?;
    layer.ignore_change_cipher_spec = false;

    Ok(Established {
        info: SessionInfo {
            version: version::TLS13,
            cipher_suite: suite,
            application_protocol,
            server_name,
            peer_certificates: Vec::new(),
            server_certificate: config.chain().first().cloned(),
        },
        traffic: Traffic::Tls13(Tls13Traffic {
            hash,
            aead: suite.aead(),
            read_secret: application_secrets.client,
            write_secret: application_secrets.server,
            exporter_master_secret,
        }),
    })
}
