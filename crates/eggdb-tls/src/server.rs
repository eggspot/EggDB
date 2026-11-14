//! The server side of the handshake: TLS 1.3 (RFC 8446), with a HelloRetryRequest when the
//! client's key shares miss EggDB's groups, and TLS 1.2 handed to [`crate::server12`].

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, TranscriptHash, version};
use crate::config::{ClientAuthentication, MaximumVersion, MinimumVersion, ServerConfig};
use crate::error::TlsError;
use crate::handshake::{
    CertificateMessage, CertificateRequest, ClientHello, DigitallySigned, EncryptedExtensions, Extension,
    HELLO_RETRY_REQUEST_RANDOM, KeyShareEntry, ServerHello, certificate_verify_content,
    extension_type, finished_message, handshake_type, message_body, message_hash_message,
};
use crate::key_schedule::{KeySchedule, finished_verify_data, traffic_keys};
use crate::negotiation::{
    SUPPORTED_SIGNATURE_SCHEMES, ServerKeyExchange, choose_application_protocol,
    choose_signature_scheme, scheme_algorithm, scheme_fits_key, server_key_exchange, tls13_groups,
};
use crate::signature::verify_signature;
use crate::verify::{KeyPurpose, verify_certificate_chain};
use crate::x509::Certificate;
use crate::private_key::FillRandom;
use crate::record::{Tls13RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer};
use crate::resumption::{
    OfferedPsks, PSK_DHE_KE, TicketState, binder_matches, new_session_ticket_message,
    now_unix_milliseconds, partial_client_hello, resumption_master_secret, selected_identity,
    ticket_is_fresh, ticket_psk,
};
use crate::session::{Established, SessionInfo, Tls13Traffic, Traffic};
use eggdb_crypto::constant_time_eq;

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ServerConfig,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::CLIENT_HELLO {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "the client did not start with a ClientHello",
        ));
    }
    let hello = ClientHello::decode(message_body(&message))?;
    if !hello.compression_methods.contains(&0) {
        return Err(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the client offers no null compression",
        ));
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
        if matches!(config.client_authentication, ClientAuthentication::Required { .. }) {
            return Err(TlsError::protocol(
                AlertDescription::ProtocolVersion,
                "client certificates are checked over TLS 1.3 only",
            ));
        }
        return crate::server12::handshake(layer, config, message, hello, fill_random);
    }
    Err(TlsError::protocol(
        AlertDescription::ProtocolVersion,
        "the client offers no TLS version EggDB accepts",
    ))
}

pub(crate) fn choose_suite(offered: &[u16], tls13: bool, fips_only: bool) -> Option<CipherSuite> {
    CipherSuite::ALL
        .into_iter()
        .filter(|suite| suite.is_tls13() == tls13 && (!fips_only || suite.is_fips_approved()))
        .find(|suite| offered.contains(&suite.id()))
}

fn preferred_share(hello: &ClientHello, fips_only: bool) -> Result<Option<KeyShareEntry>, TlsError> {
    let shares = hello.key_shares()?;
    Ok(tls13_groups(fips_only)
        .iter()
        .find_map(|group| shares.iter().find(|share| share.group == *group).cloned()))
}

fn resumable_psk(
    config: &ServerConfig,
    hello: &ClientHello,
    hello_message: &[u8],
    transcript_before_hello: &TranscriptHash,
    suite: CipherSuite,
) -> Result<Option<Vec<u8>>, TlsError> {
    let Some(tickets) = &config.session_tickets else {
        return Ok(None);
    };
    if !matches!(config.client_authentication, ClientAuthentication::None) {
        return Ok(None);
    }
    let Some(data) = hello.extension(extension_type::PRE_SHARED_KEY) else {
        return Ok(None);
    };
    if hello.extensions.last().map(|extension| extension.extension_type) != Some(extension_type::PRE_SHARED_KEY) {
        return Err(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "pre_shared_key is not the ClientHello's last extension",
        ));
    }
    if !hello.psk_key_exchange_modes()?.contains(&PSK_DHE_KE) {
        return Ok(None);
    }
    let offered = OfferedPsks::decode(data)?;
    let (Some(identity), Some(binder)) = (offered.identities.first(), offered.binders.first()) else {
        return Ok(None);
    };
    let Some(state) = tickets.open(&identity.identity) else {
        return Ok(None);
    };
    let same_name = hello.server_name()?.unwrap_or_default() == state.server_name;
    if state.cipher_suite != suite.id()
        || !same_name
        || !ticket_is_fresh(&state, identity.obfuscated_ticket_age, now_unix_milliseconds())
    {
        return Ok(None);
    }
    let partial = partial_client_hello(hello_message, &offered).ok_or(TlsError::protocol(
        AlertDescription::DecodeError,
        "the PSK binders are longer than the ClientHello",
    ))?;
    let mut partial_transcript = transcript_before_hello.clone();
    partial_transcript.update(partial);
    if !binder_matches(suite.hash(), &state.psk, &partial_transcript.current(), binder) {
        return Err(TlsError::protocol(
            AlertDescription::DecryptError,
            "the PSK binder does not verify",
        ));
    }
    Ok(Some(state.psk))
}

fn tls13<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ServerConfig,
    first_message: Vec<u8>,
    first_hello: ClientHello,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    let suite = choose_suite(&first_hello.cipher_suites, true, config.fips_only).ok_or(TlsError::protocol(
        AlertDescription::HandshakeFailure,
        "no TLS 1.3 cipher suite in common",
    ))?;
    let hash = suite.hash();
    let mut transcript = hash.transcript();
    let mut sent_change_cipher_spec = false;
    let (hello, hello_message, share) = match preferred_share(&first_hello, config.fips_only)? {
        Some(share) => (first_hello, first_message, share),
        None => {
            let groups = first_hello.supported_groups()?;
            let group = tls13_groups(config.fips_only)
                .iter()
                .copied()
                .find(|group| groups.contains(group))
                .ok_or(TlsError::protocol(
                    AlertDescription::HandshakeFailure,
                    "no key exchange group in common",
                ))?;
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
                return Err(TlsError::protocol(
                    AlertDescription::UnexpectedMessage,
                    "expected a second ClientHello",
                ));
            }
            let second_hello = ClientHello::decode(message_body(&second_message))?;
            let shares = second_hello.key_shares()?;
            let consistent = second_hello.legacy_session_id == first_hello.legacy_session_id
                && second_hello.cipher_suites.contains(&suite.id())
                && second_hello.supported_versions()?.contains(&version::TLS13)
                && shares.len() == 1
                && shares[0].group == group;
            if !consistent {
                return Err(TlsError::protocol(
                    AlertDescription::IllegalParameter,
                    "the second ClientHello does not answer the retry",
                ));
            }
            let share = shares[0].clone();
            (second_hello, second_message, share)
        }
    };
    let resumed_psk = resumable_psk(config, &hello, &hello_message, &transcript, suite)?;
    transcript.update(&hello_message);

    let signing = match resumed_psk {
        Some(_) => None,
        None => {
            let scheme = choose_signature_scheme(config.private_key(), &hello.signature_algorithms()?, true)
                .ok_or(TlsError::protocol(
                    AlertDescription::HandshakeFailure,
                    "no signature scheme in common",
                ))?;
            let algorithm = scheme_algorithm(scheme, true).ok_or(TlsError::protocol(
                AlertDescription::InternalError,
                "the chosen signature scheme has no algorithm",
            ))?;
            Some((scheme, algorithm))
        }
    };
    let application_protocol = choose_application_protocol(
        &config.application_protocols,
        &hello.application_protocols()?,
    )
    .map_err(|_| {
        TlsError::protocol(
            AlertDescription::NoApplicationProtocol,
            "no application protocol in common",
        )
    })?;
    let server_name = hello.server_name()?;

    let ServerKeyExchange {
        share: server_share,
        shared_secret,
    } = server_key_exchange(share.group, &share.key_exchange, &mut *fill_random)?.ok_or(
        TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the client's key share is not a valid public key",
        ),
    )?;
    let mut random = [0u8; 32];
    fill_random(&mut random)?;
    let mut server_extensions = vec![
        Extension::server_supported_version(version::TLS13),
        Extension::server_key_share(&KeyShareEntry {
            group: share.group,
            key_exchange: server_share,
        }),
    ];
    if resumed_psk.is_some() {
        server_extensions.push(Extension {
            extension_type: extension_type::PRE_SHARED_KEY,
            data: selected_identity(0),
        });
    }
    let server_hello = ServerHello {
        legacy_version: version::TLS12,
        random,
        legacy_session_id_echo: hello.legacy_session_id.clone(),
        cipher_suite: suite.id(),
        compression_method: 0,
        extensions: server_extensions,
    }
    .encode();
    transcript.update(&server_hello);
    layer.queue(content_type::HANDSHAKE, &server_hello)?;
    if !sent_change_cipher_spec && !hello.legacy_session_id.is_empty() {
        layer.queue_change_cipher_spec();
    }

    let mut schedule = KeySchedule::new(hash, resumed_psk.as_deref());
    let handshake_secrets = schedule.enter_handshake(&shared_secret, &transcript.current());
    let protection = |secret: &[u8]| {
        Tls13RecordProtection::new(suite.aead(), &traffic_keys(hash, suite.aead(), secret))
            .map(Protection::Tls13)
            .ok_or(TlsError::protocol(
                AlertDescription::InternalError,
                "traffic key of the wrong length",
            ))
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

    let client_trust = match &config.client_authentication {
        _ if resumed_psk.is_some() => None,
        ClientAuthentication::None => None,
        ClientAuthentication::Optional { trust } => Some((trust, false)),
        ClientAuthentication::Required { trust } => Some((trust, true)),
    };
    if client_trust.is_some() {
        let request = CertificateRequest {
            request_context: Vec::new(),
            extensions: vec![Extension::signature_algorithms(&SUPPORTED_SIGNATURE_SCHEMES)],
        }
        .encode();
        transcript.update(&request);
        layer.queue(content_type::HANDSHAKE, &request)?;
    }

    if let Some((scheme, algorithm)) = signing {
        let chain: Vec<&[u8]> = config
            .chain()
            .iter()
            .map(|certificate| certificate.der())
            .collect();
        let certificate = CertificateMessage::from_chain(&chain).encode_tls13();
        transcript.update(&certificate);
        layer.queue(content_type::HANDSHAKE, &certificate)?;

        let content = certificate_verify_content(true, &transcript.current());
        let signature = config
            .private_key()
            .sign(algorithm, &content, &mut *fill_random)
            .map_err(|_| {
                TlsError::protocol(
                    AlertDescription::InternalError,
                    "signing the handshake failed",
                )
            })?;
        let certificate_verify = DigitallySigned { scheme, signature }.encode_certificate_verify();
        transcript.update(&certificate_verify);
        layer.queue(content_type::HANDSHAKE, &certificate_verify)?;
    }

    let finished = finished_message(&finished_verify_data(
        hash,
        &handshake_secrets.server,
        &transcript.current(),
    ));
    transcript.update(&finished);
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;

    let (application_secrets, exporter_master_secret) =
        schedule.enter_application(&transcript.current());
    layer.set_write_protection(protection(&application_secrets.server)?);

    let peer_certificates = match client_trust {
        Some((trust, required)) => read_client_certificate(layer, &mut transcript, trust, required)?,
        None => Vec::new(),
    };
    let expected = finished_verify_data(hash, &handshake_secrets.client, &transcript.current());
    let (message_type, client_finished) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected the client's Finished",
        ));
    }
    if !constant_time_eq(message_body(&client_finished), &expected) {
        return Err(TlsError::protocol(
            AlertDescription::DecryptError,
            "the client's Finished does not verify",
        ));
    }
    layer.set_read_protection(protection(&application_secrets.client)?)?;
    layer.ignore_change_cipher_spec = false;
    transcript.update(&client_finished);

    if let Some(tickets) = &config.session_tickets
        && matches!(config.client_authentication, ClientAuthentication::None)
        && hello.psk_key_exchange_modes()?.contains(&PSK_DHE_KE)
    {
        let resumption = resumption_master_secret(hash, schedule.stage_secret(), &transcript.current());
        let mut age_add = [0u8; 4];
        fill_random(&mut age_add)?;
        let state = TicketState {
            cipher_suite: suite.id(),
            issued_unix_milliseconds: now_unix_milliseconds(),
            age_add: u32::from_be_bytes(age_add),
            server_name: server_name.clone().unwrap_or_default(),
            psk: ticket_psk(hash, &resumption),
        };
        let ticket = tickets.seal(&state, &mut *fill_random)?;
        layer.queue(
            content_type::HANDSHAKE,
            &new_session_ticket_message(state.age_add, &ticket),
        )?;
        layer.flush()?;
    }

    Ok(Established {
        info: SessionInfo {
            version: version::TLS13,
            cipher_suite: suite,
            application_protocol,
            server_name,
            peer_certificates,
            server_certificate: config.chain().first().cloned(),
            key_exchange_group: share.group,
            resumed: resumed_psk.is_some(),
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

fn read_client_certificate<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    transcript: &mut crate::cipher_suite::TranscriptHash,
    trust: &crate::verify::TrustStore,
    required: bool,
) -> Result<Vec<Certificate>, TlsError> {
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::CERTIFICATE {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected the client's Certificate",
        ));
    }
    transcript.update(&message);
    let certificate_message = CertificateMessage::decode_tls13(message_body(&message))?;
    if !certificate_message.request_context.is_empty() {
        return Err(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the client's Certificate has a request context",
        ));
    }
    if certificate_message.entries.is_empty() {
        if required {
            return Err(TlsError::protocol(
                AlertDescription::CertificateRequired,
                "the client sent no certificate",
            ));
        }
        return Ok(Vec::new());
    }
    let chain = certificate_message
        .entries
        .iter()
        .map(|entry| Certificate::from_der(&entry.certificate))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            TlsError::protocol(
                AlertDescription::BadCertificate,
                "the client's certificate could not be parsed",
            )
        })?;
    verify_certificate_chain(
        &chain,
        trust,
        KeyPurpose::ClientAuthentication,
        crate::client::unix_now(),
    )?;
    let before_verify = transcript.current();
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::CERTIFICATE_VERIFY {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected the client's CertificateVerify",
        ));
    }
    transcript.update(&message);
    let signed = DigitallySigned::decode(message_body(&message))?;
    let leaf_key = chain[0].public_key();
    let algorithm = scheme_algorithm(signed.scheme, true)
        .filter(|_| {
            SUPPORTED_SIGNATURE_SCHEMES.contains(&signed.scheme)
                && scheme_fits_key(signed.scheme, leaf_key)
        })
        .ok_or(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the client signed with a scheme the server did not offer",
        ))?;
    verify_signature(
        algorithm,
        leaf_key,
        &certificate_verify_content(false, &before_verify),
        &signed.signature,
    )
    .map_err(|_| {
        TlsError::protocol(
            AlertDescription::DecryptError,
            "the client's CertificateVerify does not verify",
        )
    })?;
    Ok(chain)
}

