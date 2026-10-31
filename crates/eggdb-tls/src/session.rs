//! What a finished handshake agreed on: the version, suite, application protocol, names and
//! certificates, and the secrets the connection keeps for key updates and exporters.

use crate::cipher_suite::{AeadKind, CipherSuite, HashKind};
use crate::key_schedule::export_keying_material;
use crate::signature::SignatureHash;
use crate::x509::Certificate;

/// What the handshake agreed on, for the application.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The protocol version, [`crate::cipher_suite::version::TLS13`] or `TLS12`.
    pub version: u16,
    /// The cipher suite.
    pub cipher_suite: CipherSuite,
    /// The application protocol agreed with ALPN, if any.
    pub application_protocol: Option<Vec<u8>>,
    /// The host name the client asked for with `server_name`, if any.
    pub server_name: Option<String>,
    /// The peer's certificate chain, leaf first (empty on a server, which asks for none).
    pub peer_certificates: Vec<Certificate>,
    /// The server's own certificate: the peer's leaf on a client, the served leaf on a server.
    pub server_certificate: Option<Certificate>,
}

impl SessionInfo {
    /// The `tls-server-end-point` channel binding (RFC 5929): the hash of the server's
    /// certificate with its signature's hash, SHA-256 for SHA-1 and MD5. SCRAM-SHA-256-PLUS
    /// binds authentication to it.
    pub fn tls_server_end_point(&self) -> Option<Vec<u8>> {
        let certificate = self.server_certificate.as_ref()?;
        let hash = match certificate.signature_algorithm().hash() {
            Some(SignatureHash::Sha384) => SignatureHash::Sha384,
            Some(SignatureHash::Sha512) => SignatureHash::Sha512,
            _ => SignatureHash::Sha256,
        };
        Some(hash.digest(certificate.der()))
    }
}

/// The TLS 1.3 secrets a connection keeps after its handshake.
pub(crate) struct Tls13Traffic {
    pub(crate) hash: HashKind,
    pub(crate) aead: AeadKind,
    pub(crate) read_secret: Vec<u8>,
    pub(crate) write_secret: Vec<u8>,
    pub(crate) exporter_master_secret: Vec<u8>,
}

impl std::fmt::Debug for Tls13Traffic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tls13Traffic").field("hash", &self.hash).finish_non_exhaustive()
    }
}

/// The TLS 1.2 secrets a connection keeps after its handshake.
pub(crate) struct Tls12Traffic {
    pub(crate) hash: HashKind,
    pub(crate) master_secret: Vec<u8>,
    pub(crate) client_random: [u8; 32],
    pub(crate) server_random: [u8; 32],
}

impl std::fmt::Debug for Tls12Traffic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Tls12Traffic").field("hash", &self.hash).finish_non_exhaustive()
    }
}

/// The secrets of an established connection.
#[derive(Debug)]
pub(crate) enum Traffic {
    Tls13(Tls13Traffic),
    Tls12(Tls12Traffic),
}

impl Traffic {
    pub(crate) fn export(&self, label: &[u8], context: Option<&[u8]>, length: usize) -> Vec<u8> {
        match self {
            Self::Tls13(traffic) => export_keying_material(
                traffic.hash,
                &traffic.exporter_master_secret,
                label,
                context.unwrap_or(&[]),
                length,
            ),
            Self::Tls12(traffic) => {
                let mut seed = traffic.client_random.to_vec();
                seed.extend_from_slice(&traffic.server_random);
                if let Some(context) = context {
                    seed.extend_from_slice(&(context.len() as u16).to_be_bytes());
                    seed.extend_from_slice(context);
                }
                crate::tls12::prf(traffic.hash, &traffic.master_secret, label, &seed, length)
            }
        }
    }
}

/// A finished handshake: what it agreed on and the secrets it keeps.
#[derive(Debug)]
pub(crate) struct Established {
    pub(crate) info: SessionInfo,
    pub(crate) traffic: Traffic,
}
