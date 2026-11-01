//! Certificate verification: building a chain from a peer's certificate to a trust anchor
//! (RFC 5280 section 6, the parts TLS needs) and matching a server name against the
//! certificate (RFC 6125, with the common-name fallback PostgreSQL's libpq also applies).

use std::net::IpAddr;

use crate::oid;
use crate::signature::SignatureHash;
use crate::x509::{
    BasicConstraints, Certificate, CertificateError, KeyUsage, SubjectAlternativeName,
    certificates_from_pem,
};

/// The most certificates a chain may have, the peer's own included.
pub const MAX_CHAIN_CERTIFICATES: usize = 8;
const MAX_SIGNATURE_CHECKS: usize = 32;
const MAX_DNS_NAME_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;

/// The certificates a verifier trusts as the roots of chains.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    anchors: Vec<Certificate>,
}

impl TrustStore {
    /// An empty store, which trusts nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store of every certificate in PEM `text`.
    pub fn from_pem(text: &str) -> Result<Self, CertificateError> {
        Ok(Self {
            anchors: certificates_from_pem(text)?,
        })
    }

    /// Trusts one more certificate.
    pub fn add(&mut self, certificate: Certificate) {
        self.anchors.push(certificate);
    }

    /// The trusted certificates.
    pub fn certificates(&self) -> &[Certificate] {
        &self.anchors
    }

    /// Whether the store trusts nothing.
    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }
}

/// What the peer's certificate is checked to be for (its extended key usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPurpose {
    /// A TLS server (`id-kp-serverAuth`).
    ServerAuthentication,
    /// A TLS client (`id-kp-clientAuth`).
    ClientAuthentication,
}

impl KeyPurpose {
    fn object_identifier(self) -> &'static [u8] {
        match self {
            Self::ServerAuthentication => oid::SERVER_AUTH,
            Self::ClientAuthentication => oid::CLIENT_AUTH,
        }
    }
}

/// The name a client used to reach a server, which the server's certificate must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerName {
    /// A DNS name, lower case, without a trailing dot.
    DnsName(String),
    /// An IP address.
    IpAddress(IpAddr),
}

/// A host that is neither an IP address nor a valid DNS name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidServerName(pub String);

impl std::fmt::Display for InvalidServerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} is neither an IP address nor a valid DNS name",
            self.0
        )
    }
}

impl std::error::Error for InvalidServerName {}

impl ServerName {
    /// Reads a host as a client was given it: an IPv4 or IPv6 address (optionally in
    /// brackets) or a DNS name (optionally with a trailing dot).
    pub fn parse(host: &str) -> Result<Self, InvalidServerName> {
        let unbracketed = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(address) = unbracketed.parse::<IpAddr>() {
            return Ok(Self::IpAddress(address));
        }
        let name = host.strip_suffix('.').unwrap_or(host);
        if is_valid_dns_name(name) {
            Ok(Self::DnsName(name.to_ascii_lowercase()))
        } else {
            Err(InvalidServerName(host.to_string()))
        }
    }
}

fn is_valid_dns_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DNS_NAME_BYTES
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= MAX_DNS_LABEL_BYTES
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

/// Why a certificate or chain was not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateVerificationError {
    /// The peer sent no certificate.
    EmptyChain,
    /// The peer sent more than [`MAX_CHAIN_CERTIFICATES`] certificates.
    ChainTooLong,
    /// A certificate is not valid yet.
    NotYetValid,
    /// A certificate has expired.
    Expired,
    /// No chain leads from the certificate to a trust anchor: the issuer is unknown or a
    /// signature does not verify.
    UnknownIssuer,
    /// A certificate that signed another is not a certificate authority.
    NotACertificateAuthority,
    /// A chain is longer than an issuer's path length constraint allows.
    PathLengthExceeded,
    /// A key usage extension forbids the use.
    KeyUsage,
    /// An extended key usage extension does not allow the purpose.
    ExtendedKeyUsage,
    /// A certificate has a critical extension EggDB does not read.
    UnhandledCriticalExtension,
    /// A certificate in the chain is signed with SHA-1.
    WeakSignatureAlgorithm,
    /// Finding a chain took more signature checks than any honest chain needs.
    PathSearchLimit,
    /// The certificate is not for the name the client connected to.
    NameMismatch,
}

impl std::fmt::Display for CertificateVerificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyChain => "the peer sent no certificate",
            Self::ChainTooLong => "the peer sent too many certificates",
            Self::NotYetValid => "a certificate in the chain is not valid yet",
            Self::Expired => "a certificate in the chain has expired",
            Self::UnknownIssuer => {
                "the certificate is not signed by a trusted certificate authority"
            }
            Self::NotACertificateAuthority => {
                "a certificate in the chain signed another without being a certificate authority"
            }
            Self::PathLengthExceeded => "the chain is longer than an issuer allows",
            Self::KeyUsage => "a certificate's key usage does not allow this use",
            Self::ExtendedKeyUsage => "the certificate is not for this purpose",
            Self::UnhandledCriticalExtension => {
                "a certificate has a critical extension that is not supported"
            }
            Self::WeakSignatureAlgorithm => "a certificate in the chain is signed with SHA-1",
            Self::PathSearchLimit => "the chain took too many signature checks",
            Self::NameMismatch => "the certificate is not for the host name connected to",
        })
    }
}

impl std::error::Error for CertificateVerificationError {}

/// Checks that `chain` (the peer's certificate first, then the intermediates it sent, in any
/// order) leads to a certificate in `trust_store`, that every certificate on the way is valid
/// at `now_unix_seconds` and allowed to sign the next, and that the peer's certificate is for
/// `purpose`. A peer certificate that is itself in the store is trusted directly, as a
/// self-signed server certificate is.
pub fn verify_certificate_chain(
    chain: &[Certificate],
    trust_store: &TrustStore,
    purpose: KeyPurpose,
    now_unix_seconds: i64,
) -> Result<(), CertificateVerificationError> {
    let (end_entity, intermediates) = chain
        .split_first()
        .ok_or(CertificateVerificationError::EmptyChain)?;
    if chain.len() > MAX_CHAIN_CERTIFICATES {
        return Err(CertificateVerificationError::ChainTooLong);
    }
    check_validity(end_entity, now_unix_seconds)?;
    check_end_entity(end_entity, purpose)?;
    if trust_store
        .anchors
        .iter()
        .any(|anchor| anchor.der() == end_entity.der())
    {
        return Ok(());
    }
    let mut search = PathSearch {
        intermediates,
        used: vec![false; intermediates.len()],
        trust_store,
        purpose,
        now_unix_seconds,
        signature_checks_left: MAX_SIGNATURE_CHECKS,
    };
    search.find_issuer(end_entity, 0)
}

/// Checks that `certificate` is for `server_name`: a DNS name matches a `dNSName` entry
/// (case-insensitively, a `*` first label matching exactly one label), an IP address matches
/// an `iPAddress` entry. Only when the certificate has no DNS or IP entries at all is its single
/// common name checked the same way.
pub fn verify_server_name(
    certificate: &Certificate,
    server_name: &ServerName,
) -> Result<(), CertificateVerificationError> {
    let names = certificate.subject_alternative_names();
    let matched = if names.is_empty() {
        match certificate.subject().common_names() {
            [common_name] => common_name_matches(common_name, server_name),
            _ => false,
        }
    } else {
        names.iter().any(|name| match (name, server_name) {
            (SubjectAlternativeName::DnsName(pattern), ServerName::DnsName(host)) => {
                dns_name_matches(pattern, host)
            }
            (SubjectAlternativeName::IpAddress(address), ServerName::IpAddress(host)) => {
                address == host
            }
            _ => false,
        })
    };
    if matched {
        Ok(())
    } else {
        Err(CertificateVerificationError::NameMismatch)
    }
}

fn common_name_matches(common_name: &str, server_name: &ServerName) -> bool {
    match server_name {
        ServerName::DnsName(host) => dns_name_matches(common_name, host),
        ServerName::IpAddress(host) => common_name
            .parse::<IpAddr>()
            .is_ok_and(|address| address == *host),
    }
}

fn dns_name_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.strip_suffix('.').unwrap_or(pattern);
    if let Some(suffix) = pattern.strip_prefix("*.") {
        let Some((first_label, rest)) = host.split_once('.') else {
            return false;
        };
        return !first_label.is_empty()
            && suffix.contains('.')
            && rest.eq_ignore_ascii_case(suffix);
    }
    pattern.eq_ignore_ascii_case(host)
}

fn check_validity(
    certificate: &Certificate,
    now_unix_seconds: i64,
) -> Result<(), CertificateVerificationError> {
    if now_unix_seconds < certificate.not_before_unix_seconds() {
        return Err(CertificateVerificationError::NotYetValid);
    }
    if now_unix_seconds > certificate.not_after_unix_seconds() {
        return Err(CertificateVerificationError::Expired);
    }
    Ok(())
}

fn check_purpose(
    certificate: &Certificate,
    purpose: KeyPurpose,
) -> Result<(), CertificateVerificationError> {
    match certificate.extended_key_usage() {
        Some(purposes)
            if !purposes.iter().any(|allowed| {
                allowed == purpose.object_identifier() || allowed == oid::ANY_EXTENDED_KEY_USAGE
            }) =>
        {
            Err(CertificateVerificationError::ExtendedKeyUsage)
        }
        _ => Ok(()),
    }
}

fn check_end_entity(
    certificate: &Certificate,
    purpose: KeyPurpose,
) -> Result<(), CertificateVerificationError> {
    if certificate.has_unhandled_critical_extension() {
        return Err(CertificateVerificationError::UnhandledCriticalExtension);
    }
    if certificate
        .key_usage()
        .is_some_and(|usage| !usage.contains(KeyUsage::DIGITAL_SIGNATURE))
    {
        return Err(CertificateVerificationError::KeyUsage);
    }
    check_purpose(certificate, purpose)
}

struct PathSearch<'a> {
    intermediates: &'a [Certificate],
    used: Vec<bool>,
    trust_store: &'a TrustStore,
    purpose: KeyPurpose,
    now_unix_seconds: i64,
    signature_checks_left: usize,
}

impl PathSearch<'_> {
    fn find_issuer(
        &mut self,
        certificate: &Certificate,
        intermediates_below: usize,
    ) -> Result<(), CertificateVerificationError> {
        if certificate.signature_algorithm().hash() == Some(SignatureHash::Sha1) {
            return Err(CertificateVerificationError::WeakSignatureAlgorithm);
        }
        let mut first_error = None;
        let trust_store = self.trust_store;
        for anchor in &trust_store.anchors {
            if anchor.subject() != certificate.issuer() || !self.signed_by(certificate, anchor)? {
                continue;
            }
            match self.check_issuer(anchor, intermediates_below, true) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        let intermediates = self.intermediates;
        for (index, candidate) in intermediates.iter().enumerate() {
            if self.used[index]
                || candidate.subject() != certificate.issuer()
                || !self.signed_by(certificate, candidate)?
            {
                continue;
            }
            if let Err(error) = self.check_issuer(candidate, intermediates_below, false) {
                first_error.get_or_insert(error);
                continue;
            }
            self.used[index] = true;
            match self.find_issuer(candidate, intermediates_below + 1) {
                Ok(()) => return Ok(()),
                Err(CertificateVerificationError::PathSearchLimit) => {
                    return Err(CertificateVerificationError::PathSearchLimit);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
            self.used[index] = false;
        }
        Err(first_error.unwrap_or(CertificateVerificationError::UnknownIssuer))
    }

    fn signed_by(
        &mut self,
        certificate: &Certificate,
        issuer: &Certificate,
    ) -> Result<bool, CertificateVerificationError> {
        self.signature_checks_left = self
            .signature_checks_left
            .checked_sub(1)
            .ok_or(CertificateVerificationError::PathSearchLimit)?;
        Ok(certificate.verify_signed_by(issuer.public_key()).is_ok())
    }

    fn check_issuer(
        &self,
        issuer: &Certificate,
        intermediates_below: usize,
        is_trust_anchor: bool,
    ) -> Result<(), CertificateVerificationError> {
        check_validity(issuer, self.now_unix_seconds)?;
        if issuer.has_unhandled_critical_extension() {
            return Err(CertificateVerificationError::UnhandledCriticalExtension);
        }
        match issuer.basic_constraints() {
            Some(BasicConstraints {
                certificate_authority: true,
                path_length,
            }) => {
                let below = u64::try_from(intermediates_below).unwrap_or(u64::MAX);
                if path_length.is_some_and(|limit| below > limit) {
                    return Err(CertificateVerificationError::PathLengthExceeded);
                }
            }
            None if is_trust_anchor => {}
            _ => return Err(CertificateVerificationError::NotACertificateAuthority),
        }
        if issuer
            .key_usage()
            .is_some_and(|usage| !usage.contains(KeyUsage::KEY_CERT_SIGN))
        {
            return Err(CertificateVerificationError::KeyUsage);
        }
        if is_trust_anchor {
            Ok(())
        } else {
            check_purpose(issuer, self.purpose)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_normalized() {
        assert_eq!(
            ServerName::parse("DB.Example.COM."),
            Ok(ServerName::DnsName("db.example.com".to_string()))
        );
        assert_eq!(
            ServerName::parse("[2001:db8::1]"),
            Ok(ServerName::IpAddress("2001:db8::1".parse().unwrap()))
        );
        assert_eq!(
            ServerName::parse("192.0.2.10"),
            Ok(ServerName::IpAddress("192.0.2.10".parse().unwrap()))
        );
        for invalid in [
            "",
            ".",
            "a..b",
            "-a.b",
            "a-.b",
            "a b",
            "*.example.com",
            "caf\u{e9}.test",
        ] {
            assert!(ServerName::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(ServerName::parse(&format!("{}a", "a.".repeat(127))).is_err());
    }

    #[test]
    fn wildcards_match_exactly_one_first_label() {
        assert!(dns_name_matches("*.example.com", "db.example.com"));
        assert!(dns_name_matches("*.Example.com.", "db.example.com"));
        assert!(!dns_name_matches("*.example.com", "example.com"));
        assert!(!dns_name_matches("*.example.com", "a.db.example.com"));
        assert!(!dns_name_matches("*.com", "example.com"));
        assert!(!dns_name_matches("db*.example.com", "db1.example.com"));
        assert!(!dns_name_matches("*", "localhost"));
        assert!(dns_name_matches("DB.example.com", "db.example.com"));
        assert!(!dns_name_matches("db.example.com", "db.example.org"));
    }
}
