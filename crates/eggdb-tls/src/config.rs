//! What a TLS server or client is set up with: certificates, keys, trust and protocols.

use crate::private_key::PrivateKey;
use crate::signature::SubjectPublicKey;
use crate::verify::{ServerName, TrustStore};
use crate::x509::Certificate;

/// Why a server's certificate and key cannot be used together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerConfigError {
    /// No certificate was given.
    EmptyChain,
    /// The private key does not belong to the first certificate.
    KeyMismatch,
    /// The certificate's key is of a kind TLS cannot sign handshakes with here.
    UnsupportedKey,
}

impl std::fmt::Display for ServerConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyChain => "no TLS certificate was given",
            Self::KeyMismatch => "the TLS private key does not belong to the certificate",
            Self::UnsupportedKey => "the TLS certificate's key is neither P-256 nor RSA",
        })
    }
}

impl std::error::Error for ServerConfigError {}

/// The lowest protocol version a side accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MinimumVersion {
    /// TLS 1.2 and TLS 1.3, the default: drivers such as older JDBC and .NET builds still need
    /// TLS 1.2.
    #[default]
    Tls12,
    /// TLS 1.3 only.
    Tls13,
}

/// The highest protocol version a side offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaximumVersion {
    /// TLS 1.2 only, for a peer or middlebox that breaks on TLS 1.3.
    Tls12,
    /// Up to TLS 1.3, the default.
    #[default]
    Tls13,
}

/// A server's TLS setup.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    chain: Vec<Certificate>,
    private_key: PrivateKey,
    /// The application protocols the server accepts with ALPN, in its order of preference.
    pub application_protocols: Vec<Vec<u8>>,
    /// The lowest version accepted.
    pub minimum_version: MinimumVersion,
    /// The highest version offered.
    pub maximum_version: MaximumVersion,
}

impl ServerConfig {
    /// A setup serving `chain` (leaf first) signed for with `private_key`.
    pub fn new(chain: Vec<Certificate>, private_key: PrivateKey) -> Result<Self, ServerConfigError> {
        let leaf = chain.first().ok_or(ServerConfigError::EmptyChain)?;
        let matches = match (leaf.public_key(), private_key.public_key()) {
            (SubjectPublicKey::P256(certified), SubjectPublicKey::P256(held)) => *certified == held,
            (SubjectPublicKey::Rsa(certified), SubjectPublicKey::Rsa(held)) => *certified == held,
            (SubjectPublicKey::Unsupported, _) | (_, SubjectPublicKey::Unsupported) => {
                return Err(ServerConfigError::UnsupportedKey);
            }
            _ => false,
        };
        if !matches {
            return Err(ServerConfigError::KeyMismatch);
        }
        Ok(Self {
            chain,
            private_key,
            application_protocols: Vec::new(),
            minimum_version: MinimumVersion::default(),
            maximum_version: MaximumVersion::default(),
        })
    }

    /// The certificate chain, leaf first.
    pub fn chain(&self) -> &[Certificate] {
        &self.chain
    }

    /// The private key.
    pub fn private_key(&self) -> &PrivateKey {
        &self.private_key
    }
}

/// How a client checks the server's certificate.
#[derive(Debug, Clone)]
pub enum ServerVerification {
    /// The chain must lead to `trust` and the certificate must name `server_name` (libpq's
    /// `verify-full`).
    Full {
        /// The trusted roots.
        trust: TrustStore,
        /// The name the client connected to.
        server_name: ServerName,
    },
    /// The chain must lead to `trust`; the name is not checked (libpq's `verify-ca`).
    CertificateAuthority {
        /// The trusted roots.
        trust: TrustStore,
    },
    /// The certificate is not checked, only that the server holds its key (libpq's `require`,
    /// SqlClient's `TrustServerCertificate`): encrypted, but open to an active attacker.
    EncryptionOnly,
}

/// A client's TLS setup.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// How the server's certificate is checked.
    pub verification: ServerVerification,
    /// The host name sent in `server_name`, if any (never an IP address).
    pub server_name_indication: Option<String>,
    /// The application protocols offered with ALPN, in the client's order of preference.
    pub application_protocols: Vec<Vec<u8>>,
    /// The lowest version accepted.
    pub minimum_version: MinimumVersion,
    /// The highest version offered.
    pub maximum_version: MaximumVersion,
}

impl ClientConfig {
    /// A client that checks the server's certificate the given way and sends `host` as its
    /// server name when it is a DNS name.
    pub fn new(verification: ServerVerification, host: &str) -> Self {
        let server_name_indication = match ServerName::parse(host) {
            Ok(ServerName::DnsName(name)) => Some(name),
            _ => None,
        };
        Self {
            verification,
            server_name_indication,
            application_protocols: Vec::new(),
            minimum_version: MinimumVersion::default(),
            maximum_version: MaximumVersion::default(),
        }
    }

    pub(crate) fn offers_tls13(&self) -> bool {
        self.maximum_version == MaximumVersion::Tls13
    }

    pub(crate) fn offers_tls12(&self) -> bool {
        self.minimum_version == MinimumVersion::Tls12
    }
}
