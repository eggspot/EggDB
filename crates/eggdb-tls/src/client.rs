//! The client side of the handshake: TLS 1.3 (RFC 8446) with a HelloRetryRequest when the
//! server wants another group, falling back to [`crate::client12`] for a TLS 1.2 server.

use std::io::{Read, Write};

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, version};
use crate::config::{ClientConfig, ServerVerification};
use crate::error::TlsError;
use crate::handshake::{
    CertificateEntry, CertificateMessage, CertificateRequest, ClientHello,
    DOWNGRADE_TO_TLS12_SENTINEL, DigitallySigned, EncryptedExtensions, Extension, KeyShareEntry,
    ServerHello, certificate_verify_content, extension_type, finished_message, handshake_type,
    message_body, message_hash_message, named_group,
};
use crate::key_schedule::{KeySchedule, finished_verify_data, traffic_keys};
use crate::negotiation::{
    EphemeralKey, SUPPORTED_SIGNATURE_SCHEMES, scheme_algorithm, scheme_fits_key, tls12_groups, tls13_groups,
};
use crate::private_key::FillRandom;
use crate::record::{Tls13RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer};
use crate::resumption::{
    ClientResumption, PSK_DHE_KE, StoredTicket, binder, fill_binder, now_unix_milliseconds,
    pre_shared_key_offer, resumption_master_secret, single_binder_list_bytes,
};
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
        return Err(TlsError::protocol(
            AlertDescription::DecodeError,
            "the server sent no certificate",
        ));
    }
    let chain = entries
        .iter()
        .map(|entry| Certificate::from_der(&entry.certificate))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            TlsError::protocol(
                AlertDescription::BadCertificate,
                "the server's certificate could not be parsed",
            )
        })?;
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
        .filter(|suite| !config.fips_only || suite.is_fips_approved())
        .filter(|suite| {
            if suite.is_tls13() {
                config.offers_tls13()
            } else {
                config.offers_tls12()
            }
        })
        .map(CipherSuite::id)
        .collect()
}

fn client_hello(
    config: &ClientConfig,
    random: [u8; 32],
    session_id: &[u8],
    shares: &[KeyShareEntry],
    cookie: Option<&[u8]>,
) -> ClientHello {
    let offer_tls12 = config.offers_tls12();
    let mut extensions = Vec::new();
    if let Some(name) = &config.server_name_indication {
        extensions.push(Extension::server_name(name));
    }
    let versions: Vec<u16> = [
        (config.offers_tls13(), version::TLS13),
        (offer_tls12, version::TLS12),
    ]
    .into_iter()
    .filter_map(|(offered, version)| offered.then_some(version))
    .collect();
    extensions.push(Extension::client_supported_versions(&versions));
    if config.offers_tls13() {
        extensions.push(Extension::supported_groups(tls13_groups(config.fips_only)));
    } else {
        extensions.push(Extension::supported_groups(tls12_groups(config.fips_only)));
    }
    extensions.push(Extension::signature_algorithms(
        &SUPPORTED_SIGNATURE_SCHEMES,
    ));
    extensions.push(Extension::client_key_shares(shares));
    if !config.application_protocols.is_empty() {
        let protocols: Vec<&[u8]> = config
            .application_protocols
            .iter()
            .map(Vec::as_slice)
            .collect();
        extensions.push(Extension::application_protocols(&protocols));
    }
    if let Some(cookie) = cookie {
        extensions.push(Extension {
            extension_type: extension_type::COOKIE,
            data: cookie.to_vec(),
        });
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

fn ticket_hash(ticket: &StoredTicket) -> Option<crate::cipher_suite::HashKind> {
    CipherSuite::from_id(ticket.cipher_suite)
        .filter(|suite| suite.is_tls13())
        .map(CipherSuite::hash)
}

fn hello_with_resumption(
    config: &ClientConfig,
    mut hello: ClientHello,
    ticket: Option<&StoredTicket>,
    transcript_before: Option<&crate::cipher_suite::TranscriptHash>,
) -> Vec<u8> {
    if config.session_store.is_some() && config.offers_tls13() {
        hello.extensions.push(Extension::psk_key_exchange_modes(&[PSK_DHE_KE]));
    }
    let Some((ticket, hash)) = ticket.and_then(|ticket| ticket_hash(ticket).map(|hash| (ticket, hash))) else {
        return hello.encode();
    };
    hello.extensions.push(pre_shared_key_offer(
        &ticket.ticket,
        ticket.obfuscated_age(now_unix_milliseconds()),
        hash.digest_bytes(),
    ));
    let mut message = hello.encode();
    let partial_length = message.len() - single_binder_list_bytes(hash.digest_bytes());
    let mut transcript = transcript_before.cloned().unwrap_or_else(|| hash.transcript());
    transcript.update(&message[..partial_length]);
    let made = binder(hash, &ticket.psk, &transcript.current());
    fill_binder(&mut message, &made);
    message
}

fn key_shares(ephemerals: &[EphemeralKey]) -> Vec<KeyShareEntry> {
    ephemerals
        .iter()
        .map(|ephemeral| KeyShareEntry {
            group: ephemeral.group(),
            key_exchange: ephemeral.public_key(),
        })
        .collect()
}

fn read_server_hello<S: Read + Write>(
    layer: &mut RecordLayer<S>,
) -> Result<(Vec<u8>, ServerHello), TlsError> {
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::SERVER_HELLO {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected a ServerHello",
        ));
    }
    let hello = ServerHello::decode(message_body(&message))?;
    Ok((message, hello))
}

pub(crate) fn handshake<S: Read + Write>(
    layer: &mut RecordLayer<S>,
    config: &ClientConfig,
    fill_random: FillRandom<'_>,
) -> Result<Established, TlsError> {
    if !config.offers_tls13() && !config.offers_tls12() {
        return Err(TlsError::protocol(
            AlertDescription::ProtocolVersion,
            "the client is set up to offer no TLS version",
        ));
    }
    let mut random = [0u8; 32];
    fill_random(&mut random)?;
    let mut session_id = vec![0u8; 32];
    fill_random(&mut session_id)?;
    let first_groups: &[u16] = match (config.offers_tls13(), config.fips_only) {
        (true, false) => &[named_group::X25519_MLKEM768, named_group::X25519],
        (true, true) => &[named_group::X25519_MLKEM768, named_group::SECP256R1],
        (false, false) => &[named_group::X25519],
        (false, true) => &[named_group::SECP256R1],
    };
    let mut ephemerals = Vec::with_capacity(first_groups.len());
    for &group in first_groups {
        ephemerals.push(EphemeralKey::generate(group, &mut *fill_random)?.ok_or(
            TlsError::protocol(AlertDescription::InternalError, "a key exchange group is unavailable"),
        )?);
    }
    let offered_suites_now = offered_suites(config);
    let mut ticket = config
        .session_store
        .as_ref()
        .filter(|_| config.offers_tls13())
        .and_then(|store| store.take_for(config.server_name_indication.as_deref(), now_unix_milliseconds()))
        .filter(|ticket| offered_suites_now.contains(&ticket.cipher_suite));
    let mut hello_message = hello_with_resumption(
        config,
        client_hello(config, random, &session_id, &key_shares(&ephemerals), None),
        ticket.as_ref(),
        None,
    );
    layer.queue(content_type::HANDSHAKE, &hello_message)?;
    layer.flush()?;

    let (mut server_hello_message, mut server_hello) = read_server_hello(layer)?;
    let mut retry_transcript = None;
    let mut sent_change_cipher_spec = false;
    if server_hello.is_hello_retry_request() {
        let suite = CipherSuite::from_id(server_hello.cipher_suite)
            .filter(|suite| suite.is_tls13())
            .ok_or(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the retry names a suite the client did not offer",
            ))?;
        let group = server_hello.retry_group()?.unwrap_or(0);
        if server_hello.selected_version()? != Some(version::TLS13)
            || !tls13_groups(config.fips_only).contains(&group)
            || ephemerals.iter().any(|ephemeral| ephemeral.group() == group)
        {
            return Err(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the HelloRetryRequest asks for nothing new",
            ));
        }
        let mut transcript = suite.hash().transcript();
        transcript.update(&message_hash_message(&suite.hash().digest(&hello_message)));
        transcript.update(&server_hello_message);
        ephemerals = vec![EphemeralKey::generate(group, &mut *fill_random)?.ok_or(
            TlsError::protocol(AlertDescription::InternalError, "unsupported group requested"),
        )?];
        let cookie = server_hello
            .extension(extension_type::COOKIE)
            .map(<[u8]>::to_vec);
        if ticket.as_ref().and_then(ticket_hash) != Some(suite.hash()) {
            ticket = None;
        }
        hello_message = hello_with_resumption(
            config,
            client_hello(
                config,
                random,
                &session_id,
                &key_shares(&ephemerals),
                cookie.as_deref(),
            ),
            ticket.as_ref(),
            Some(&transcript),
        );
        transcript.update(&hello_message);
        layer.queue_change_cipher_spec();
        sent_change_cipher_spec = true;
        layer.queue(content_type::HANDSHAKE, &hello_message)?;
        layer.flush()?;
        layer.ignore_change_cipher_spec = true;
        (server_hello_message, server_hello) = read_server_hello(layer)?;
        if server_hello.is_hello_retry_request() || server_hello.cipher_suite != suite.id() {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "a second HelloRetryRequest or a changed suite",
            ));
        }
        retry_transcript = Some(transcript);
    }

    match server_hello.selected_version()? {
        Some(version::TLS13) => {}
        Some(_) => {
            return Err(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the server selected a version the client did not offer",
            ));
        }
        None => {
            let tls12_allowed = config.offers_tls12()
                && retry_transcript.is_none()
                && server_hello.legacy_version == version::TLS12;
            if !tls12_allowed {
                return Err(TlsError::protocol(
                    AlertDescription::ProtocolVersion,
                    "the server does not speak TLS 1.3",
                ));
            }
            if config.offers_tls13() && server_hello.random[24..] == DOWNGRADE_TO_TLS12_SENTINEL {
                return Err(TlsError::protocol(
                    AlertDescription::IllegalParameter,
                    "the server signalled a downgrade from TLS 1.3",
                ));
            }
            return crate::client12::handshake(
                layer,
                config,
                hello_message,
                server_hello_message,
                server_hello,
                random,
                fill_random,
            );
        }
    }

    let suite = CipherSuite::from_id(server_hello.cipher_suite)
        .filter(|suite| suite.is_tls13())
        .ok_or(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the server chose a suite the client did not offer",
        ))?;
    if server_hello.legacy_session_id_echo != session_id || server_hello.compression_method != 0 {
        return Err(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the ServerHello does not echo the session or uses compression",
        ));
    }
    let hash = suite.hash();
    let mut transcript = retry_transcript.unwrap_or_else(|| {
        let mut fresh = hash.transcript();
        fresh.update(&hello_message);
        fresh
    });
    transcript.update(&server_hello_message);
    let server_share = server_hello.key_share()?.ok_or(TlsError::protocol(
        AlertDescription::IllegalParameter,
        "the ServerHello carries no key share",
    ))?;
    let ephemeral = ephemerals
        .iter()
        .find(|ephemeral| ephemeral.group() == server_share.group)
        .ok_or(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the server's key share is not in a group the client sent",
        ))?;
    let shared_secret =
        ephemeral
            .shared_secret(&server_share.key_exchange)
            .ok_or(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the server's key share is not a valid public key",
            ))?;

    let resumed_ticket = match server_hello.extension(extension_type::PRE_SHARED_KEY) {
        None => None,
        Some(selected) => match &ticket {
            Some(offered) if selected == [0, 0] && offered.cipher_suite == suite.id() => Some(offered),
            _ => {
                return Err(TlsError::protocol(
                    AlertDescription::IllegalParameter,
                    "the server selected a pre-shared key the client did not offer",
                ));
            }
        },
    };
    let mut schedule = KeySchedule::new(hash, resumed_ticket.map(|offered| offered.psk.as_slice()));
    let handshake_secrets = schedule.enter_handshake(&shared_secret, &transcript.current());
    let protection = |secret: &[u8]| {
        Tls13RecordProtection::new(suite.aead(), &traffic_keys(hash, suite.aead(), secret))
            .map(Protection::Tls13)
            .ok_or(TlsError::protocol(
                AlertDescription::InternalError,
                "traffic key of the wrong length",
            ))
    };
    layer.set_read_protection(protection(&handshake_secrets.server)?)?;
    layer.ignore_change_cipher_spec = true;
    if !sent_change_cipher_spec {
        layer.queue_change_cipher_spec();
    }
    layer.set_write_protection(protection(&handshake_secrets.client)?);

    let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
    if message_type != handshake_type::ENCRYPTED_EXTENSIONS {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected EncryptedExtensions",
        ));
    }
    let encrypted_extensions = EncryptedExtensions::decode(message_body(&message))?;
    let application_protocol = encrypted_extensions.application_protocol()?;
    if let Some(protocol) = &application_protocol
        && !config.application_protocols.contains(protocol)
    {
        return Err(TlsError::protocol(
            AlertDescription::IllegalParameter,
            "the server chose an application protocol the client did not offer",
        ));
    }

    let (chain, certificate_request_context) = if let Some(resumed) = resumed_ticket {
        (resumed.server_certificates.clone(), None)
    } else {
        let (mut message_type, mut message) = layer.read_handshake_message(Some(&mut transcript))?;
        let mut certificate_request_context = None;
        if message_type == handshake_type::CERTIFICATE_REQUEST {
            certificate_request_context =
                Some(CertificateRequest::decode(message_body(&message))?.request_context);
            (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
        }
        if message_type != handshake_type::CERTIFICATE {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "expected the server's Certificate",
            ));
        }
        let certificate_message = CertificateMessage::decode_tls13(message_body(&message))?;
        if !certificate_message.request_context.is_empty() {
            return Err(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the server's Certificate has a request context",
            ));
        }
        let chain = check_server_certificates(config, &certificate_message.entries)?;
        let before_verify = transcript.current();

        let (message_type, message) = layer.read_handshake_message(Some(&mut transcript))?;
        if message_type != handshake_type::CERTIFICATE_VERIFY {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "expected CertificateVerify",
            ));
        }
        let signed = DigitallySigned::decode(message_body(&message))?;
        let leaf_key = chain[0].public_key();
        let algorithm = scheme_algorithm(signed.scheme, true)
            .filter(|_| {
                SUPPORTED_SIGNATURE_SCHEMES.contains(&signed.scheme)
                    && scheme_fits_key(signed.scheme, leaf_key)
            })
            .ok_or(TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the server signed with a scheme the client did not offer",
            ))?;
        verify_signature(
            algorithm,
            leaf_key,
            &certificate_verify_content(true, &before_verify),
            &signed.signature,
        )
        .map_err(|_| {
            TlsError::protocol(
                AlertDescription::DecryptError,
                "the server's CertificateVerify does not verify",
            )
        })?;
        (chain, certificate_request_context)
    };

    let expected = finished_verify_data(hash, &handshake_secrets.server, &transcript.current());
    let (message_type, message) = layer.read_handshake_message(None)?;
    if message_type != handshake_type::FINISHED {
        return Err(TlsError::protocol(
            AlertDescription::UnexpectedMessage,
            "expected the server's Finished",
        ));
    }
    if !constant_time_eq(message_body(&message), &expected) {
        return Err(TlsError::protocol(
            AlertDescription::DecryptError,
            "the server's Finished does not verify",
        ));
    }
    transcript.update(&message);
    let (application_secrets, exporter_master_secret) =
        schedule.enter_application(&transcript.current());

    if let Some(request_context) = certificate_request_context {
        let empty = CertificateMessage {
            request_context,
            entries: Vec::new(),
        }
        .encode_tls13();
        transcript.update(&empty);
        layer.queue(content_type::HANDSHAKE, &empty)?;
    }
    let finished = finished_message(&finished_verify_data(
        hash,
        &handshake_secrets.client,
        &transcript.current(),
    ));
    layer.queue(content_type::HANDSHAKE, &finished)?;
    layer.flush()?;
    layer.set_write_protection(protection(&application_secrets.client)?);
    transcript.update(&finished);
    let resumption = config.session_store.as_ref().map(|store| ClientResumption {
        store: std::sync::Arc::clone(store),
        hash,
        cipher_suite: suite.id(),
        resumption_master_secret: resumption_master_secret(hash, schedule.stage_secret(), &transcript.current()),
        server_name: config.server_name_indication.clone(),
        server_certificates: chain.clone(),
    });
    let resumed = resumed_ticket.is_some();
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
            key_exchange_group: server_share.group,
            resumed,
        },
        traffic: Traffic::Tls13(Tls13Traffic {
            hash,
            aead: suite.aead(),
            read_secret: application_secrets.server,
            write_secret: application_secrets.client,
            exporter_master_secret,
        }),
        resumption,
    })
}
