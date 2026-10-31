//! The client side of the handshake: TLS 1.3 (RFC 8446) with a HelloRetryRequest when the
//! server wants another group, falling back to [`crate::client12`] for a TLS 1.2 server.

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, version};
use crate::config::{ClientConfig, MinimumVersion, ServerVerification};
use crate::error::TlsError;
use crate::handshake::{
    CertificateEntry, CertificateMessage, CertificateRequest, ClientHello, DOWNGRADE_TO_TLS12_SENTINEL,
    DigitallySigned, EncryptedExtensions, Extension, KeyShareEntry, ServerHello,
    certificate_verify_content, extension_type, finished_message, handshake_type, message_body,
    message_hash_message, named_group,
};
use crate::key_schedule::{KeySchedule, finished_verify_data, traffic_keys};
use crate::negotiation::{
    EphemeralKey, SUPPORTED_GROUPS, SUPPORTED_SIGNATURE_SCHEMES, scheme_algorithm, scheme_fits_key,
};
use crate::private_key::FillRandom;
use crate::record::{Tls13RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer};
use crate::session::{Established, SessionInfo, Tls13Traffic, Traffic};
use crate::signature::verify_signature;
use crate::verify::{KeyPurpose, verify_certificate_chain, verify_server_name};
use crate::x509::Certificate;
use eggdb_crypto::constant_time_eq;

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Parses the server's chain and checks it the way `config` asks.
pub(crate) fn check_server_certificates(
    config: &ClientConfig,
    entries: &[CertificateEntry],
) -> Result<Vec<Certificate>, TlsError> {
    if entries.is_empty() {
        return Err(TlsError::protocol(AlertDescription::DecodeError, "the server sent no certificate"));
    }
    let chain = entries
        .iter()
        .map(|entry| Certificate::from_der(&entry.certificate))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsError::protocol(AlertDescription::BadCertificate, "the server's certificate could not be parsed"))?;
    match &config.verification {
        ServerVerification::Full { trust, server_name } => {
            verify_certificate_chain(&chain, trust, KeyPurpose::ServerAuthentication, unix_now())?;
            verify_server_name(&chain[0], server_name)?;
        }
        ServerVerification::CertificateAuthority { trust } => {
            verify_certificate_chain(&chain, trust, KeyPurpose::ServerAuthentication, unix_now())?;
        }
        ServerVerification::EncryptionOnly => {}
    }
    Ok(chain)
}

fn offered_suites(config: &ClientConfig) -> Vec<u16> {
    CipherSuite::ALL
        .into_iter()
        .filter(|suite| suite.is_tls13() || config.minimum_version == MinimumVersion::Tls12)
        .map(CipherSuite::id)
        .collect()
}

fn client_hello(
    config: &ClientConfig,
    random: [u8; 32],
    session_id: &[u8],
    share: KeyShareEntry,
    cookie: Option<&[u8]>,
) -> ClientHello {
    let offer_tls12 = config.minimum_version == MinimumVersion::Tls12;
    let mut extensions = Vec::new();
    if let Some(name) = &config.server_name_indication {
        extensions.push(Extension::server_name(name));
    }
    let versions: &[u16] = if offer_tls12 { &[version::TLS13, version::TLS12] } else { &[version::TLS13] };
    extensions.push(Extension::client_supported_versions(versions));
    extensions.push(Extension::supported_groups(&SUPPORTED_GROUPS));
    extensions.push(Extension::signature_algorithms(&SUPPORTED_SIGNATURE_SCHEMES));
    extensions.push(Extension::client_key_shares(&[share]));
    if !config.application_protocols.is_empty() {
        let protocols: Vec<&[u8]> = config.application_protocols.iter().map(Vec::as_slice).collect();
        extensions.push(Extension::application_protocols(&protocols));
    }
    if let Some(cookie) = cookie {
        extensions.push(Extension { extension_type: extension_type::COOKIE, data: cookie.to_vec() });
    }
    if offer_tls12 {
        extensions.push(Extension::uncompressed_point_format());
        extensions.push(Extension::empty(extension_type::EXTENDED_MASTER_SECRET));
        extensions.push(Extension::empty_renegotiation_info());
    }
    ClientHello {
        legacy_version: version::TLS12,
        random,
        legacy_session_id: session_id.to_vec(),
        cipher_suites: offered_suites(config),
        compression_methods: vec![0],
        extensions,
    }
}

fn read_server_hello<S: Read + Write>(layer: &mut RecordLayer<S>) -> Result<(Vec<u8>, ServerHello), TlsError> {
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::SERVER_HELLO {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected a ServerHello"));
    }
    let hello = ServerHello::decode(message_body(&message))?;
    Ok((message, hello))
}

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ClientConfig,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let mut random = [0u8; 32];
    fill_random(&mut random)?;
    let mut session_id = vec![0u8; 32];
    fill_random(&mut session_id)?;
    let mut ephemeral = EphemeralKey::generate(named_group::X25519, &mut *fill_random)?
        .ok_or(TlsError::protocol(AlertDescription::InternalError, "X25519 is unavailable"))?;
    let share = KeyShareEntry { group: ephemeral.group(), key_exchange: ephemeral.public_key() };
    let mut hello_message = client_hello(config, random, &session_id, share, None).encode();
    layer.queue(content_type::HANDSHAKE, &hello_message)?;
    layer.flush()?;

    let (mut server_hello_message, mut server_hello) = read_server_hello(layer)?;
    let mut retry_transcript = None;
    let mut sent_change_cipher_spec = false;
    if server_hello.is_hello_retry_request() {
        let suite = CipherSuite::from_id(server_hello.cipher_suite)
            .filter(|suite| suite.is_tls13())
            .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the retry names a suite the client did not offer"))?;
        let group = server_hello.retry_group()?.unwrap_or(0);
        if server_hello.selected_version()? != Some(version::TLS13)
            || !SUPPORTED_GROUPS.contains(&group)
            || group == ephemeral.group()
        {
            return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the HelloRetryRequest asks for nothing new"));
        }
        let mut transcript = suite.hash().transcript();
        transcript.update(&message_hash_message(&suite.hash().digest(&hello_message)));
        transcript.update(&server_hello_message);
        ephemeral = EphemeralKey::generate(group, &mut *fill_random)?
            .ok_or(TlsError::protocol(AlertDescription::InternalError, "unsupported group requested"))?;
        let share = KeyShareEntry { group, key_exchange: ephemeral.public_key() };
        let cookie = server_hello.extension(extension_type::COOKIE).map(<[u8]>::to_vec);
        hello_message = client_hello(config, random, &session_id, share, cookie.as_deref()).encode();
        transcript.update(&hello_message);
        layer.queue_change_cipher_spec();
        sent_change_cipher_spec = true;
        layer.queue(content_type::HANDSHAKE, &hello_message)?;
        layer.flush()?;
        layer.ignore_change_cipher_spec = true;
        (server_hello_message, server_hello) = read_server_hello(layer)?;
        if server_hello.is_hello_retry_request() || server_hello.cipher_suite != suite.id() {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "a second HelloRetryRequest or a changed suite"));
        }
        retry_transcript = Some(transcript);
    }

    match server_hello.selected_version()? {
        Some(version::TLS13) => {}
        Some(_) => return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server selected a version the client did not offer")),
        None => {
            let tls12_allowed = config.minimum_version == MinimumVersion::Tls12
                && retry_transcript.is_none()
                && server_hello.legacy_version == version::TLS12;
            if !tls12_allowed {
                return Err(TlsError::protocol(AlertDescription::ProtocolVersion, "the server does not speak TLS 1.3"));
            }
            if server_hello.random[24..] == DOWNGRADE_TO_TLS12_SENTINEL {
                return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server signalled a downgrade from TLS 1.3"));
            }
            return crate::client12::handshake(layer, config, hello_message, server_hello_message, server_hello, random, fill_random);
        }
    }

    let suite = CipherSuite::from_id(server_hello.cipher_suite)
        .filter(|suite| suite.is_tls13())
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose a suite the client did not offer"))?;
    if server_hello.legacy_session_id_echo != session_id || server_hello.compression_method != 0 {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the ServerHello does not echo the session or uses compression"));
    }
    let hash = suite.hash();
    let mut transcript = retry_transcript.unwrap_or_else(|| {
        let mut fresh = hash.transcript();
        fresh.update(&hello_message);
        fresh
    });
    transcript.update(&server_hello_message);
    let server_share = server_hello
        .key_share()?
        .filter(|share| share.group == ephemeral.group())
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server's key share is not in the client's group"))?;
    let shared_secret = ephemeral
        .shared_secret(&server_share.key_exchange)
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server's key share is not a valid public key"))?;

    let mut schedule = KeySchedule::new(hash, None);
    let handshake_secrets = schedule.enter_handshake(&shared_secret, &transcript.current());
    let protection = |secret: &[u8]| {
        Tls13RecordProtection::new(suite.aead(), &traffic_keys(hash, suite.aead(), secret))
            .map(Protection::Tls13)
            .ok_or(TlsError::protocol(AlertDescription::InternalError, "traffic key of the wrong length"))
    };
    layer.set_read_protection(protection(&handshake_secrets.server)?)?;
    layer.ignore_change_cipher_spec = true;
    if !sent_change_cipher_spec {
        layer.queue_change_cipher_spec();
    }
    layer.set_write_protection(protection(&handshake_secrets.client)?);

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::ENCRYPTED_EXTENSIONS {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected EncryptedExtensions"));
    }
    let encrypted_extensions = EncryptedExtensions::decode(message_body(&message))?;
    let application_protocol = encrypted_extensions.application_protocol()?;
    if let Some(protocol) = &application_protocol
        && !config.application_protocols.contains(protocol)
    {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server chose an application protocol the client did not offer"));
    }

    let (mut message_type, mut message) = layer.read_handshake_message(Some(&mut transcript))?;
    let mut certificate_request_context = None;
    if message_type == handshake_type::CERTIFICATE_REQUEST {
        certificate_request_context = Some(CertificateRequest::decode(message_body(&message))?.request_context);
        (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    }
    if message_type != handshake_type::CERTIFICATE {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the server's Certificate"));
    }
    let certificate_message = CertificateMessage::decode_tls13(message_body(&message))?;
    if !certificate_message.request_context.is_empty() {
        return Err(TlsError::protocol(AlertDescription::IllegalParameter, "the server's Certificate has a request context"));
    }
    let chain = check_server_certificates(config, &certificate_message.entries)?;
    let before_verify = transcript.current();

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::CERTIFICATE_VERIFY {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected CertificateVerify"));
    }
    let signed = DigitallySigned::decode(message_body(&message))?;
    let leaf_key = chain[0].public_key();
    let algorithm = scheme_algorithm(signed.scheme, true)
        .filter(|_| SUPPORTED_SIGNATURE_SCHEMES.contains(&signed.scheme) && scheme_fits_key(signed.scheme, leaf_key))
        .ok_or(TlsError::protocol(AlertDescription::IllegalParameter, "the server signed with a scheme the client did not offer"))?;
    verify_signature(algorithm, leaf_key, &certificate_verify_content(true, &before_verify), &signed.signature)
        .map_err(|_| TlsError::protocol(AlertDescription::DecryptError, "the server's CertificateVerify does not verify"))?;

    let expected = finished_verify_data(hash, &handshake_secrets.server, &transcript.current());
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected the server's Finished"));
    }
    if !constant_time_eq(message_body(&message), &expected) {
        return Err(TlsError::protocol(AlertDescription::DecryptError, "the server's Finished does not verify"));
    }
    transcript.update(&message);
    let (application_secrets, exporter_master_secret) = schedule.enter_application(&transcript.current());

    if let Some(request_context) = certificate_request_context {
        let empty = CertificateMessage { request_context, entries: Vec::new() }.encode_tls13();
        transcript.update(&empty);
        layer.queue(content_type::HANDSHAKE, &empty)?;
    }
    let finished = finished_message(&finished_verify_data(hash, &handshake_secrets.client, &transcript.current()));
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;
    layer.set_write_protection(protection(&application_secrets.client)?);
    layer.set_read_protection(protection(&application_secrets.server)?)?;
    layer.ignore_change_cipher_spec = false;

    Ok(Established {
        info: SessionInfo {
            version: version::TLS13,
            cipher_suite: suite,
            application_protocol,
            server_name: config.server_name_indication.clone(),
            server_certificate: chain.first().cloned(),
            peer_certificates: chain,
        },
        traffic: Traffic::Tls13(Tls13Traffic {
            hash,
            aead: suite.aead(),
            read_secret: application_secrets.server,
            write_secret: application_secrets.client,
            exporter_master_secret,
        }),
    })
}
