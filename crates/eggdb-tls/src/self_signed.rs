//! Self-signed server certificates, so `eggdb serve` can offer TLS without a certificate
//! authority: a P-256 key, signed with ECDSA and SHA-256, valid for the names it is asked for.
//! A client trusts it by having the certificate itself as its trust anchor.

use std::net::IpAddr;

use eggdb_crypto::{P256PrivateKey, P256PublicKey, Sha256, ecdsa_p256_sign, sha256};

use crate::der::{DerWriter, tag};
use crate::oid;
use crate::private_key::{FillRandom, PrivateKey, generate_p256_private_key};
use crate::signature::{ecdsa_signature_to_der, write_ecdsa_sha256_algorithm};
use crate::x509::{Certificate, CertificateError, SubjectAlternativeName};
use crate::x509_time::encode_certificate_time;

/// The length of a generated serial number in bytes (RFC 5280 allows up to 20).
pub const SELF_SIGNED_SERIAL_NUMBER_BYTES: usize = 16;
const VERSION_3: u64 = 2;
const KEY_IDENTIFIER_BYTES: usize = 20;
const DNS_NAME_TAG: u8 = tag::context_primitive(2);
const IP_ADDRESS_TAG: u8 = tag::context_primitive(7);

/// What a self-signed certificate says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfSignedCertificateRequest {
    /// The subject's and issuer's common name.
    pub common_name: String,
    /// The DNS names and IP addresses clients may reach the server by.
    pub subject_alternative_names: Vec<SubjectAlternativeName>,
    /// The first valid second, in seconds since the Unix epoch.
    pub not_before_unix_seconds: i64,
    /// The last valid second, in seconds since the Unix epoch.
    pub not_after_unix_seconds: i64,
}

/// Why a self-signed certificate could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfSignedCertificateError {
    /// The common name is empty.
    EmptyCommonName,
    /// A DNS name is empty or not ASCII (an internationalized name must be given in its
    /// `xn--` form).
    DnsName(String),
    /// The validity ends before it starts, or a time is outside the years 0 to 9999.
    Validity,
    /// The random source failed.
    Random(std::io::ErrorKind),
    /// The certificate written did not read back (a bug, reported rather than panicking).
    Certificate(CertificateError),
}

impl std::fmt::Display for SelfSignedCertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCommonName => f.write_str("the certificate needs a common name"),
            Self::DnsName(name) => write!(
                f,
                "{name:?} is not a DNS name a certificate can hold: give it in ASCII (xn-- form)"
            ),
            Self::Validity => f.write_str("the certificate's validity period is not valid"),
            Self::Random(kind) => write!(f, "the random source failed: {kind}"),
            Self::Certificate(error) => {
                write!(f, "the generated certificate did not read back: {error}")
            }
        }
    }
}

impl std::error::Error for SelfSignedCertificateError {}

/// A self-signed certificate for `request`, signed by `private_key`, with `serial_number`
/// (big-endian, positive). The same inputs always give the same certificate.
pub fn self_signed_certificate(
    request: &SelfSignedCertificateRequest,
    private_key: &P256PrivateKey,
    serial_number: &[u8; SELF_SIGNED_SERIAL_NUMBER_BYTES],
) -> Result<Certificate, SelfSignedCertificateError> {
    if request.common_name.is_empty() {
        return Err(SelfSignedCertificateError::EmptyCommonName);
    }
    for name in &request.subject_alternative_names {
        if let SubjectAlternativeName::DnsName(dns_name) = name
            && (dns_name.is_empty() || !dns_name.is_ascii())
        {
            return Err(SelfSignedCertificateError::DnsName(dns_name.clone()));
        }
    }
    if request.not_after_unix_seconds < request.not_before_unix_seconds {
        return Err(SelfSignedCertificateError::Validity);
    }
    let not_before = encode_certificate_time(request.not_before_unix_seconds)
        .ok_or(SelfSignedCertificateError::Validity)?;
    let not_after = encode_certificate_time(request.not_after_unix_seconds)
        .ok_or(SelfSignedCertificateError::Validity)?;
    let public_key = private_key.public_key();

    let mut to_be_signed = DerWriter::new();
    to_be_signed.write_sequence(|tbs| {
        tbs.write_constructed(tag::context_constructed(0), |version| {
            version.write_small_unsigned_integer(VERSION_3);
        });
        tbs.write_unsigned_integer(serial_number);
        write_ecdsa_sha256_algorithm(tbs);
        write_common_name(tbs, &request.common_name);
        tbs.write_sequence(|validity| {
            validity.write(not_before.0, not_before.1.as_bytes());
            validity.write(not_after.0, not_after.1.as_bytes());
        });
        write_common_name(tbs, &request.common_name);
        write_p256_subject_public_key_info(tbs, &public_key);
        tbs.write_constructed(tag::context_constructed(3), |explicit| {
            explicit.write_sequence(|extensions| {
                write_extensions(extensions, request, &public_key);
            });
        });
    });
    let to_be_signed = to_be_signed.into_bytes();
    let signature = ecdsa_p256_sign::<Sha256>(private_key, &sha256(&to_be_signed));

    let mut certificate = DerWriter::new();
    certificate.write_sequence(|sequence| {
        sequence.write_encoded(&to_be_signed);
        write_ecdsa_sha256_algorithm(sequence);
        sequence.write_bit_string(&ecdsa_signature_to_der(&signature));
    });
    Certificate::from_der(&certificate.into_bytes())
        .map_err(SelfSignedCertificateError::Certificate)
}

/// A new P-256 key and a self-signed certificate for it, with a random serial number, both
/// from `fill_random`.
pub fn generate_self_signed_certificate(
    request: &SelfSignedCertificateRequest,
    fill_random: FillRandom<'_>,
) -> Result<(Certificate, PrivateKey), SelfSignedCertificateError> {
    let private_key = generate_p256_private_key(&mut *fill_random)
        .map_err(|error| SelfSignedCertificateError::Random(error.kind()))?;
    let mut serial_number = [0u8; SELF_SIGNED_SERIAL_NUMBER_BYTES];
    fill_random(&mut serial_number)
        .map_err(|error| SelfSignedCertificateError::Random(error.kind()))?;
    serial_number[0] = (serial_number[0] & 0x7f) | 0x40;
    let certificate = self_signed_certificate(request, &private_key, &serial_number)?;
    Ok((certificate, PrivateKey::P256(private_key)))
}

fn write_common_name(writer: &mut DerWriter, common_name: &str) {
    writer.write_sequence(|name| {
        name.write_constructed(tag::SET, |relative_name| {
            relative_name.write_sequence(|attribute| {
                attribute.write_object_identifier(oid::COMMON_NAME);
                attribute.write(tag::UTF8_STRING, common_name.as_bytes());
            });
        });
    });
}

fn write_p256_subject_public_key_info(writer: &mut DerWriter, public_key: &P256PublicKey) {
    writer.write_sequence(|info| {
        info.write_sequence(|algorithm| {
            algorithm.write_object_identifier(oid::EC_PUBLIC_KEY);
            algorithm.write_object_identifier(oid::PRIME256V1);
        });
        info.write_bit_string(&public_key.to_sec1_uncompressed());
    });
}

fn der_of(build: impl FnOnce(&mut DerWriter)) -> Vec<u8> {
    let mut writer = DerWriter::new();
    build(&mut writer);
    writer.into_bytes()
}

fn write_extension(writer: &mut DerWriter, identifier: &[u8], critical: bool, value: &[u8]) {
    writer.write_sequence(|extension| {
        extension.write_object_identifier(identifier);
        if critical {
            extension.write_boolean(true);
        }
        extension.write_octet_string(value);
    });
}

fn write_extensions(
    writer: &mut DerWriter,
    request: &SelfSignedCertificateRequest,
    public_key: &P256PublicKey,
) {
    let end_entity = der_of(|value| value.write_sequence(|_| {}));
    write_extension(writer, oid::BASIC_CONSTRAINTS, true, &end_entity);
    let purposes = der_of(|value| {
        value.write_sequence(|purposes| {
            purposes.write_object_identifier(oid::SERVER_AUTH);
            purposes.write_object_identifier(oid::CLIENT_AUTH);
        });
    });
    write_extension(writer, oid::EXTENDED_KEY_USAGE, false, &purposes);
    if !request.subject_alternative_names.is_empty() {
        let names = der_of(|value| {
            value.write_sequence(|names| {
                for name in &request.subject_alternative_names {
                    match name {
                        SubjectAlternativeName::DnsName(dns_name) => {
                            names.write(DNS_NAME_TAG, dns_name.as_bytes());
                        }
                        SubjectAlternativeName::IpAddress(IpAddr::V4(address)) => {
                            names.write(IP_ADDRESS_TAG, &address.octets());
                        }
                        SubjectAlternativeName::IpAddress(IpAddr::V6(address)) => {
                            names.write(IP_ADDRESS_TAG, &address.octets());
                        }
                    }
                }
            });
        });
        write_extension(writer, oid::SUBJECT_ALT_NAME, false, &names);
    }
    let key_digest = sha256(&public_key.to_sec1_uncompressed());
    let key_identifier =
        der_of(|value| value.write_octet_string(&key_digest[..KEY_IDENTIFIER_BYTES]));
    write_extension(writer, oid::SUBJECT_KEY_IDENTIFIER, false, &key_identifier);
}
