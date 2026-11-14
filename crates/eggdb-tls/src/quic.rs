//! TLS 1.3 for QUIC (RFC 9001 section 4): the server's side of the handshake without a record
//! layer. QUIC carries the handshake messages in CRYPTO frames at three encryption levels and
//! protects packets itself, so the handshake takes the bytes of each level as they arrive and
//! answers with the bytes to send at each level and the secrets QUIC derives its packet keys
//! from. Resumption, early data and HelloRetryRequest are not offered over QUIC yet: a client
//! must send a key share in a group EggDB uses.

use crate::alert::AlertDescription;
use crate::cipher_suite::{CipherSuite, version};
use crate::config::ServerConfig;
use crate::error::TlsError;
use crate::handshake::{
    CertificateMessage, ClientHello, DigitallySigned, EncryptedExtensions, Extension, KeyShareEntry,
    ServerHello, certificate_verify_content, extension_type, finished_message, handshake_type,
    message_body,
};
use crate::key_schedule::{KeySchedule, finished_verify_data};
use crate::negotiation::{choose_application_protocol, choose_signature_scheme, scheme_algorithm, server_key_exchange};
use crate::private_key::FillRandom;
use eggdb_crypto::constant_time_eq;

/// The `quic_transport_parameters` extension (RFC 9001 section 8.2).
pub const QUIC_TRANSPORT_PARAMETERS: u16 = 0x39;
const HANDSHAKE_HEADER_BYTES: usize = 4;
const MOST_HANDSHAKE_BYTES: usize = 64 * 1024;

/// QUIC's encryption levels, each with its own keys and its own CRYPTO stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncryptionLevel {
    /// Initial packets, keyed from the client's first destination connection ID.
    Initial,
    /// Handshake packets.
    Handshake,
    /// 1-RTT packets.
    Application,
}

impl EncryptionLevel {
    fn index(self) -> usize {
        match self {
            Self::Initial => 0,
            Self::Handshake => 1,
            Self::Application => 2,
        }
    }
}

/// What the handshake asks QUIC to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuicHandshakeEvent {
    /// Send `data` in CRYPTO frames at `level`.
    Send {
        /// The encryption level.
        level: EncryptionLevel,
        /// The handshake bytes.
        data: Vec<u8>,
    },
    /// Derive the packet keys of `level` from these secrets.
    Secrets {
        /// The encryption level the secrets are for.
        level: EncryptionLevel,
        /// The secret protecting what the client sends.
        client: Vec<u8>,
        /// The secret protecting what the server sends.
        server: Vec<u8>,
    },
    /// The client's Finished verified: the handshake is complete.
    Complete {
        /// The application protocol agreed with ALPN.
        application_protocol: Vec<u8>,
        /// The client's transport parameters, as sent.
        peer_transport_parameters: Vec<u8>,
        /// The host name the client asked for, if any.
        server_name: Option<String>,
    },
}

enum Stage {
    AwaitingClientHello,
    AwaitingClientFinished {
        expected_finished: Vec<u8>,
        application_protocol: Vec<u8>,
        peer_transport_parameters: Vec<u8>,
        server_name: Option<String>,
    },
    Complete,
}

/// The server side of a QUIC connection's TLS 1.3 handshake.
pub struct QuicServerHandshake<'c> {
    config: &'c ServerConfig,
    transport_parameters: Vec<u8>,
    stage: Stage,
    suite: Option<CipherSuite>,
    buffers: [Vec<u8>; 3],
}

impl std::fmt::Debug for QuicServerHandshake<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuicServerHandshake")
            .field("suite", &self.suite)
            .finish_non_exhaustive()
    }
}

fn next_message(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>, TlsError> {
    if buffer.len() < HANDSHAKE_HEADER_BYTES {
        return Ok(None);
    }
    let length = (usize::from(buffer[1]) << 16) | (usize::from(buffer[2]) << 8) | usize::from(buffer[3]);
    if length > MOST_HANDSHAKE_BYTES {
        return Err(TlsError::protocol(
            AlertDescription::DecodeError,
            "a handshake message longer than EggDB reads",
        ));
    }
    if buffer.len() < HANDSHAKE_HEADER_BYTES + length {
        return Ok(None);
    }
    Ok(Some(buffer.drain(..HANDSHAKE_HEADER_BYTES + length).collect()))
}

impl<'c> QuicServerHandshake<'c> {
    /// A handshake serving `config`'s certificate, sending `transport_parameters` (already
    /// encoded by QUIC) in its EncryptedExtensions. `config` must name the application
    /// protocols it serves: QUIC requires ALPN.
    pub fn new(config: &'c ServerConfig, transport_parameters: Vec<u8>) -> Self {
        Self {
            config,
            transport_parameters,
            stage: Stage::AwaitingClientHello,
            suite: None,
            buffers: [Vec::new(), Vec::new(), Vec::new()],
        }
    }

    /// The cipher suite once the ClientHello chose one: its hash and AEAD give QUIC's packet keys.
    pub fn suite(&self) -> Option<CipherSuite> {
        self.suite
    }

    /// Whether the client's Finished has verified.
    pub fn is_complete(&self) -> bool {
        matches!(self.stage, Stage::Complete)
    }

    /// Takes CRYPTO data received at `level`, in order, and returns what QUIC must do.
    pub fn receive(
        &mut self,
        level: EncryptionLevel,
        data: &[u8],
        fill_random: FillRandom<'_>,
    ) -> Result<Vec<QuicHandshakeEvent>, TlsError> {
        let buffer = &mut self.buffers[level.index()];
        if buffer.len().saturating_add(data.len()) > MOST_HANDSHAKE_BYTES + HANDSHAKE_HEADER_BYTES {
            return Err(TlsError::protocol(
                AlertDescription::DecodeError,
                "more handshake data than EggDB reads",
            ));
        }
        buffer.extend_from_slice(data);
        let mut events = Vec::new();
        while let Some(message) = next_message(&mut self.buffers[level.index()])? {
            let awaiting_hello = matches!(self.stage, Stage::AwaitingClientHello);
            let awaiting_finished = matches!(self.stage, Stage::AwaitingClientFinished { .. });
            match level {
                EncryptionLevel::Initial if awaiting_hello => {
                    events.extend(self.answer_client_hello(&message, &mut *fill_random)?);
                }
                EncryptionLevel::Handshake if awaiting_finished => {
                    events.push(self.check_client_finished(&message)?);
                }
                _ => {
                    return Err(TlsError::protocol(
                        AlertDescription::UnexpectedMessage,
                        "a handshake message at the wrong encryption level or time",
                    ));
                }
            }
        }
        Ok(events)
    }

    fn answer_client_hello(
        &mut self,
        message: &[u8],
        fill_random: FillRandom<'_>,
    ) -> Result<Vec<QuicHandshakeEvent>, TlsError> {
        if message[0] != handshake_type::CLIENT_HELLO {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "the client did not start with a ClientHello",
            ));
        }
        let hello = ClientHello::decode(message_body(message))?;
        if !hello.supported_versions()?.contains(&version::TLS13) {
            return Err(TlsError::protocol(
                AlertDescription::ProtocolVersion,
                "QUIC needs TLS 1.3",
            ));
        }
        let suite = crate::server::choose_suite(&hello.cipher_suites, true, self.config.fips_only).ok_or(
            TlsError::protocol(AlertDescription::HandshakeFailure, "no TLS 1.3 cipher suite in common"),
        )?;
        let share = crate::server::preferred_share(&hello, self.config.fips_only)?.ok_or(TlsError::protocol(
            AlertDescription::HandshakeFailure,
            "the QUIC client sent no key share in a group EggDB uses",
        ))?;
        let peer_transport_parameters = hello
            .extension(QUIC_TRANSPORT_PARAMETERS)
            .ok_or(TlsError::protocol(
                AlertDescription::MissingExtension,
                "the QUIC client sent no transport parameters",
            ))?
            .to_vec();
        let application_protocol = choose_application_protocol(
            &self.config.application_protocols,
            &hello.application_protocols()?,
        )
        .ok()
        .flatten()
        .ok_or(TlsError::protocol(
            AlertDescription::NoApplicationProtocol,
            "no application protocol in common, which QUIC requires",
        ))?;
        let scheme = choose_signature_scheme(self.config.private_key(), &hello.signature_algorithms()?, true).ok_or(
            TlsError::protocol(AlertDescription::HandshakeFailure, "no signature scheme in common"),
        )?;
        let algorithm = scheme_algorithm(scheme, true).ok_or(TlsError::protocol(
            AlertDescription::InternalError,
            "the chosen signature scheme has no algorithm",
        ))?;
        let server_name = hello.server_name()?;
        let hash = suite.hash();
        let mut transcript = hash.transcript();
        transcript.update(message);

        let exchange = server_key_exchange(share.group, &share.key_exchange, &mut *fill_random)?.ok_or(
            TlsError::protocol(
                AlertDescription::IllegalParameter,
                "the client's key share is not a valid public key",
            ),
        )?;
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
                Extension::server_key_share(&KeyShareEntry {
                    group: share.group,
                    key_exchange: exchange.share,
                }),
            ],
        }
        .encode();
        transcript.update(&server_hello);
        let mut schedule = KeySchedule::new(hash, None);
        let handshake_secrets = schedule.enter_handshake(&exchange.shared_secret, &transcript.current());

        let mut extensions = vec![
            Extension::application_protocols(&[application_protocol.as_slice()]),
            Extension {
                extension_type: QUIC_TRANSPORT_PARAMETERS,
                data: self.transport_parameters.clone(),
            },
        ];
        if server_name.is_some() {
            extensions.push(Extension::empty(extension_type::SERVER_NAME));
        }
        let mut flight = EncryptedExtensions { extensions }.encode();
        transcript.update(&flight);
        let chain: Vec<&[u8]> = self.config.chain().iter().map(|certificate| certificate.der()).collect();
        let certificate = CertificateMessage::from_chain(&chain).encode_tls13();
        transcript.update(&certificate);
        flight.extend_from_slice(&certificate);
        let content = certificate_verify_content(true, &transcript.current());
        let signature = self
            .config
            .private_key()
            .sign(algorithm, &content, &mut *fill_random)
            .map_err(|_| TlsError::protocol(AlertDescription::InternalError, "signing the handshake failed"))?;
        let certificate_verify = DigitallySigned { scheme, signature }.encode_certificate_verify();
        transcript.update(&certificate_verify);
        flight.extend_from_slice(&certificate_verify);
        let finished = finished_message(&finished_verify_data(hash, &handshake_secrets.server, &transcript.current()));
        transcript.update(&finished);
        flight.extend_from_slice(&finished);
        let (application_secrets, _exporter) = schedule.enter_application(&transcript.current());
        let expected_finished = finished_verify_data(hash, &handshake_secrets.client, &transcript.current());

        self.suite = Some(suite);
        self.stage = Stage::AwaitingClientFinished {
            expected_finished,
            application_protocol,
            peer_transport_parameters,
            server_name,
        };
        Ok(vec![
            QuicHandshakeEvent::Send {
                level: EncryptionLevel::Initial,
                data: server_hello,
            },
            QuicHandshakeEvent::Secrets {
                level: EncryptionLevel::Handshake,
                client: handshake_secrets.client,
                server: handshake_secrets.server,
            },
            QuicHandshakeEvent::Send {
                level: EncryptionLevel::Handshake,
                data: flight,
            },
            QuicHandshakeEvent::Secrets {
                level: EncryptionLevel::Application,
                client: application_secrets.client,
                server: application_secrets.server,
            },
        ])
    }

    fn check_client_finished(&mut self, message: &[u8]) -> Result<QuicHandshakeEvent, TlsError> {
        let Stage::AwaitingClientFinished {
            expected_finished,
            application_protocol,
            peer_transport_parameters,
            server_name,
        } = std::mem::replace(&mut self.stage, Stage::Complete)
        else {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "no Finished was expected"));
        };
        if message[0] != handshake_type::FINISHED {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "QUIC clients do not send certificates to EggDB: expected the client's Finished",
            ));
        }
        if !constant_time_eq(message_body(message), &expected_finished) {
            return Err(TlsError::protocol(
                AlertDescription::DecryptError,
                "the client's Finished does not verify",
            ));
        }
        Ok(QuicHandshakeEvent::Complete {
            application_protocol,
            peer_transport_parameters,
            server_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cipher_suite::HashKind;
    use crate::handshake::{ClientHello, ServerHello, named_group};
    use crate::negotiation::{EphemeralKey, SUPPORTED_SIGNATURE_SCHEMES};
    use crate::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
    use crate::x509::SubjectAlternativeName;

    fn config() -> ServerConfig {
        let request = SelfSignedCertificateRequest {
            common_name: "quic.example".to_owned(),
            subject_alternative_names: vec![SubjectAlternativeName::DnsName("quic.example".to_owned())],
            not_before_unix_seconds: 0,
            not_after_unix_seconds: 4_000_000_000,
        };
        let (certificate, key) =
            generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill).expect("certificate");
        let mut config = ServerConfig::new(vec![certificate], key).expect("config");
        config.application_protocols = vec![b"h3".to_vec()];
        config
    }

    fn split_messages(mut bytes: Vec<u8>) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        while let Some(message) = next_message(&mut bytes).expect("a message") {
            messages.push(message);
        }
        messages
    }

    #[test]
    fn a_quic_handshake_hands_over_each_level_s_secrets_and_completes() {
        let config = config();
        let mut fill = eggdb_platform::secure_random::fill;
        let mut server = QuicServerHandshake::new(&config, vec![0x01, 0x02, 0x40, 0x64]);
        let ephemeral = EphemeralKey::generate(named_group::X25519, &mut fill).expect("random").expect("x25519");
        let hello = ClientHello {
            legacy_version: version::TLS12,
            random: [7; 32],
            legacy_session_id: Vec::new(),
            cipher_suites: vec![CipherSuite::Tls13Aes128GcmSha256.id()],
            compression_methods: vec![0],
            extensions: vec![
                Extension::server_name("quic.example"),
                Extension::client_supported_versions(&[version::TLS13]),
                Extension::supported_groups(&[named_group::X25519]),
                Extension::signature_algorithms(&SUPPORTED_SIGNATURE_SCHEMES),
                Extension::client_key_shares(&[KeyShareEntry {
                    group: named_group::X25519,
                    key_exchange: ephemeral.public_key(),
                }]),
                Extension::application_protocols(&[b"h3".as_slice()]),
                Extension { extension_type: QUIC_TRANSPORT_PARAMETERS, data: vec![0x04, 0x01, 0x10] },
            ],
        }
        .encode();
        let (first, second) = hello.split_at(10);
        assert!(server.receive(EncryptionLevel::Initial, first, &mut fill).expect("part").is_empty());
        let events = server.receive(EncryptionLevel::Initial, second, &mut fill).expect("the server's flight");
        assert_eq!(events.len(), 4);
        let QuicHandshakeEvent::Send { level: EncryptionLevel::Initial, data: server_hello } = &events[0] else {
            panic!("expected the ServerHello first: {events:?}");
        };
        let QuicHandshakeEvent::Send { level: EncryptionLevel::Handshake, data: flight } = &events[2] else {
            panic!("expected the Handshake flight third");
        };
        let QuicHandshakeEvent::Secrets { level: EncryptionLevel::Handshake, client: client_secret, .. } = &events[1] else {
            panic!("expected the Handshake secrets second");
        };
        let hash = HashKind::Sha256;
        let parsed = ServerHello::decode(message_body(server_hello)).expect("server hello");
        let share = parsed.key_share().expect("share").expect("a key share");
        let shared = ephemeral.shared_secret(&share.key_exchange).expect("shared secret");
        let mut transcript = hash.transcript();
        transcript.update(&hello);
        transcript.update(server_hello);
        let mut schedule = KeySchedule::new(hash, None);
        let secrets = schedule.enter_handshake(&shared, &transcript.current());
        assert_eq!(&secrets.client, client_secret);
        for message in split_messages(flight.clone()) {
            transcript.update(&message);
        }
        let finished = finished_message(&finished_verify_data(hash, &secrets.client, &transcript.current()));
        let done = server.receive(EncryptionLevel::Handshake, &finished, &mut fill).expect("the client's Finished");
        assert_eq!(
            done,
            vec![QuicHandshakeEvent::Complete {
                application_protocol: b"h3".to_vec(),
                peer_transport_parameters: vec![0x04, 0x01, 0x10],
                server_name: Some("quic.example".to_owned()),
            }]
        );
        assert!(server.is_complete());
    }

    #[test]
    fn a_quic_client_without_transport_parameters_or_alpn_is_refused() {
        let config = config();
        let mut fill = eggdb_platform::secure_random::fill;
        let ephemeral = EphemeralKey::generate(named_group::X25519, &mut fill).expect("random").expect("x25519");
        let hello = |extra: Vec<Extension>| {
            let mut extensions = vec![
                Extension::client_supported_versions(&[version::TLS13]),
                Extension::supported_groups(&[named_group::X25519]),
                Extension::signature_algorithms(&SUPPORTED_SIGNATURE_SCHEMES),
                Extension::client_key_shares(&[KeyShareEntry {
                    group: named_group::X25519,
                    key_exchange: ephemeral.public_key(),
                }]),
            ];
            extensions.extend(extra);
            ClientHello {
                legacy_version: version::TLS12,
                random: [1; 32],
                legacy_session_id: Vec::new(),
                cipher_suites: vec![CipherSuite::Tls13Aes128GcmSha256.id()],
                compression_methods: vec![0],
                extensions,
            }
            .encode()
        };
        let without_parameters = hello(vec![Extension::application_protocols(&[b"h3".as_slice()])]);
        assert!(QuicServerHandshake::new(&config, Vec::new())
            .receive(EncryptionLevel::Initial, &without_parameters, &mut fill)
            .is_err());
        let without_alpn = hello(vec![Extension { extension_type: QUIC_TRANSPORT_PARAMETERS, data: Vec::new() }]);
        assert!(QuicServerHandshake::new(&config, Vec::new())
            .receive(EncryptionLevel::Initial, &without_alpn, &mut fill)
            .is_err());
    }
}
