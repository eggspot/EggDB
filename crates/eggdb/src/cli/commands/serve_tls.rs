use crate::cli::args::{CommandName, ServeOptions};
use crate::cli::failure::Failure;
use crate::cli::render::{self, TimeStyle};
use eggdb_tls::private_key::{PrivateKey, PrivateKeyError};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::signature::{SignatureHash, SubjectPublicKey};
use eggdb_tls::x509::{Certificate, SubjectAlternativeName, certificates_from_pem};
use eggdb_tls::config::ServerConfigError;
use eggdb_tls::{MinimumVersion, ServerConfig};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

const SELF_SIGNED_DAYS: i64 = 365;
const SECONDS_PER_DAY: i64 = 86_400;
const POSTGRESQL_ALPN: &[u8] = b"postgresql";
const TDS_8_ALPN: &[u8] = b"tds/8.0";

pub(super) enum CertificateSource {
    Files { certificate: String, key: String },
    SelfSigned,
}

pub(super) struct TlsSetup {
    pub(super) source: CertificateSource,
    pub(super) chain_length: usize,
    pub(super) key_kind: &'static str,
    pub(super) names: Vec<String>,
    pub(super) not_after_unix_seconds: i64,
    pub(super) fingerprint_sha256: Vec<u8>,
    pub(super) minimum_version: MinimumVersion,
    pub(super) postgresql: Arc<ServerConfig>,
    pub(super) mysql: Arc<ServerConfig>,
    pub(super) tds: Arc<ServerConfig>,
}

impl TlsSetup {
    pub(super) fn fingerprint_hex(&self) -> String {
        self.fingerprint_sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(super) fn fingerprint_colons(&self) -> String {
        self.fingerprint_sha256
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    pub(super) fn version_text(&self) -> &'static str {
        match self.minimum_version {
            MinimumVersion::Tls12 => "TLS 1.2 or newer",
            MinimumVersion::Tls13 => "TLS 1.3",
        }
    }

    pub(super) fn minimum_version_number(&self) -> &'static str {
        match self.minimum_version {
            MinimumVersion::Tls12 => "1.2",
            MinimumVersion::Tls13 => "1.3",
        }
    }

    pub(super) fn not_after_date(&self) -> String {
        let micros = u64::try_from(self.not_after_unix_seconds).unwrap_or(0) * 1_000_000;
        render::utc_time(micros, TimeStyle::Readable)
            .chars()
            .take(10)
            .collect()
    }

    pub(super) fn not_after_rfc3339(&self) -> String {
        let micros = u64::try_from(self.not_after_unix_seconds).unwrap_or(0) * 1_000_000;
        render::utc_time(micros, TimeStyle::Rfc3339)
    }

    pub(super) fn first_dns_name(&self) -> Option<&str> {
        self.names
            .iter()
            .find(|name| name.parse::<IpAddr>().is_err())
            .map(String::as_str)
    }
}

fn usage(message: String) -> Failure {
    Failure::usage(message, Some(CommandName::Serve))
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

fn read_text(option: &str, path: &Path) -> Result<String, Failure> {
    std::fs::read_to_string(path)
        .map_err(|error| usage(format!("cannot read {option} {}: {error}", shown(path))))
}

fn load_chain(path: &Path) -> Result<Vec<Certificate>, Failure> {
    let text = read_text("--tls-certificate", path)?;
    let chain = certificates_from_pem(&text).map_err(|error| {
        usage(format!("--tls-certificate {}: {error}", shown(path)))
    })?;
    if chain.is_empty() {
        return Err(usage(format!(
            "--tls-certificate {} has no CERTIFICATE block: expected PEM, -----BEGIN CERTIFICATE-----",
            shown(path)
        )));
    }
    Ok(chain)
}

fn load_key(path: &Path) -> Result<PrivateKey, Failure> {
    let text = read_text("--tls-key", path)?;
    PrivateKey::from_pem(&text).map_err(|error| match error {
        PrivateKeyError::UnsupportedCurve | PrivateKeyError::UnsupportedAlgorithm => usage(format!(
            "--tls-key {}: the key is of a kind serve does not support (P-256 or RSA)",
            shown(path)
        )),
        _ => usage(format!(
            "--tls-key {} is not a PEM private key: expected PRIVATE KEY (PKCS#8), EC PRIVATE KEY (SEC1) or RSA PRIVATE KEY (PKCS#1), P-256 or RSA",
            shown(path)
        )),
    })
}

fn names_of(certificate: &Certificate) -> Vec<String> {
    let names: Vec<String> = certificate
        .subject_alternative_names()
        .iter()
        .map(|name| match name {
            SubjectAlternativeName::DnsName(name) => name.clone(),
            SubjectAlternativeName::IpAddress(address) => address.to_string(),
        })
        .collect();
    if names.is_empty() {
        certificate.subject().common_names().to_vec()
    } else {
        names
    }
}

fn self_signed(hosts: &[IpAddr]) -> Result<(Vec<Certificate>, PrivateKey), Failure> {
    let mut names: Vec<SubjectAlternativeName> = Vec::new();
    for host in hosts {
        let name = SubjectAlternativeName::IpAddress(*host);
        if !host.is_unspecified() && !names.contains(&name) {
            names.push(name);
        }
    }
    names.push(SubjectAlternativeName::DnsName("localhost".to_owned()));
    let now = now_unix_seconds();
    let request = SelfSignedCertificateRequest {
        common_name: "localhost".to_owned(),
        subject_alternative_names: names,
        not_before_unix_seconds: now - SECONDS_PER_DAY,
        not_after_unix_seconds: now + SELF_SIGNED_DAYS * SECONDS_PER_DAY,
    };
    let (certificate, key) =
        generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill)
            .map_err(|error| usage(format!("cannot make a self-signed certificate: {error:?}")))?;
    Ok((vec![certificate], key))
}

/// The TLS setup `options` ask for, or `None` when they ask for none.
pub(super) fn tls_setup(
    options: &ServeOptions,
    listen_hosts: &[IpAddr],
) -> Result<Option<TlsSetup>, Failure> {
    let (chain, key, source) = match (
        &options.tls_certificate,
        &options.tls_key,
        options.tls_self_signed,
    ) {
        (Some(_), _, true) => {
            return Err(usage(
                "--tls-self-signed and --tls-certificate exclude each other: use your files, or let serve make a certificate".to_owned(),
            ));
        }
        (None, Some(_), true) => {
            return Err(usage(
                "--tls-self-signed and --tls-key exclude each other: use your files, or let serve make a certificate".to_owned(),
            ));
        }
        (None, Some(_), false) => {
            return Err(usage(
                "--tls-key needs --tls-certificate: the private key goes with its certificate chain, in two PEM files".to_owned(),
            ));
        }
        (Some(_), None, false) => {
            return Err(usage(
                "--tls-certificate needs --tls-key: the certificate chain goes with its private key, in two PEM files".to_owned(),
            ));
        }
        (None, None, false) => {
            if options.tls_min_version.is_some() {
                return Err(usage(
                    "--tls-min-version needs TLS: pass --tls-certificate and --tls-key, or --tls-self-signed".to_owned(),
                ));
            }
            return Ok(None);
        }
        (None, None, true) => {
            let (chain, key) = self_signed(listen_hosts)?;
            (chain, key, CertificateSource::SelfSigned)
        }
        (Some(certificate), Some(key), false) => (
            load_chain(certificate)?,
            load_key(key)?,
            CertificateSource::Files {
                certificate: shown(certificate),
                key: shown(key),
            },
        ),
    };
    let key_kind = match key.public_key() {
        SubjectPublicKey::P256(_) => "P-256",
        _ => "RSA",
    };
    let leaf = chain[0].clone();
    let chain_length = chain.len();
    let mut config = ServerConfig::new(chain, key).map_err(|error| match (error, &source) {
        (ServerConfigError::KeyMismatch, CertificateSource::Files { certificate, key }) => usage(format!(
            "--tls-key {key} does not match --tls-certificate {certificate}: its public key is not the one in the first certificate"
        )),
        (error, _) => usage(format!("cannot use the TLS certificate: {error}")),
    })?;
    let minimum_version = options.tls_min_version.unwrap_or_default();
    config.minimum_version = minimum_version;
    let with_protocol = |protocol: Option<&[u8]>| {
        let mut config = config.clone();
        config.application_protocols = protocol.map(|name| vec![name.to_vec()]).unwrap_or_default();
        Arc::new(config)
    };
    Ok(Some(TlsSetup {
        source,
        chain_length,
        key_kind,
        names: names_of(&leaf),
        not_after_unix_seconds: leaf.not_after_unix_seconds(),
        fingerprint_sha256: SignatureHash::Sha256.digest(leaf.der()),
        minimum_version,
        postgresql: with_protocol(Some(POSTGRESQL_ALPN)),
        mysql: with_protocol(None),
        tds: with_protocol(Some(TDS_8_ALPN)),
    }))
}
