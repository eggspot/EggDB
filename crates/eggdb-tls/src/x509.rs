//! X.509 certificates (RFC 5280): a strict parser for the fields TLS needs (names, validity,
//! public key, subject alternative names, basic constraints, key usage and extended key usage)
//! and the check of a certificate's signature by its issuer's key.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use eggdb_crypto::{P256PublicKey, RsaPublicKey};

use crate::der::{DerElement, DerError, DerReader, tag};
use crate::oid;
use crate::pem::{CERTIFICATE_LABEL, PemError, pem_decode_all, pem_encode};
use crate::signature::{
    SignatureAlgorithm, SignatureError, SubjectPublicKey, parse_signature_algorithm,
    read_absent_or_null_parameters, verify_signature,
};
use crate::x509_time::{parse_generalized_time, parse_utc_time};

const VERSION_1: u64 = 0;
const VERSION_3: u64 = 2;
const DNS_NAME_TAG: u8 = tag::context_primitive(2);
const IP_ADDRESS_TAG: u8 = tag::context_primitive(7);
const ISSUER_UNIQUE_ID_TAG: u8 = tag::context_primitive(1);
const SUBJECT_UNIQUE_ID_TAG: u8 = tag::context_primitive(2);
const KEY_USAGE_MAX_BYTES: usize = 2;

/// Why a certificate was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateError {
    /// The certificate is not valid DER of the structure RFC 5280 defines.
    Der(DerError),
    /// The PEM text around the certificate is not valid.
    Pem(PemError),
    /// The PEM text holds no `CERTIFICATE` block.
    NoCertificates,
    /// The version is not 1, 2 or 3, or extensions appear in a certificate older than 3.
    Version,
    /// The signature algorithm inside the signed part differs from the one outside it.
    SignatureAlgorithmMismatch,
    /// A validity time that is not a valid `UTCTime` or `GeneralizedTime` in UTC.
    Time,
    /// A name attribute whose text cannot be read.
    Name,
    /// The public key is malformed, or an RSA key outside the sizes EggDB accepts.
    PublicKey,
    /// The same extension appears twice.
    DuplicateExtension,
    /// An extension EggDB reads has a malformed value.
    Extension,
}

impl std::fmt::Display for CertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Der(error) => write!(f, "the certificate is not valid DER: {error}"),
            Self::Pem(error) => write!(f, "the certificate file is not valid PEM: {error}"),
            Self::NoCertificates => f.write_str("the file holds no CERTIFICATE block"),
            Self::Version => f.write_str("the certificate's version is not supported"),
            Self::SignatureAlgorithmMismatch => {
                f.write_str("the certificate names two different signature algorithms")
            }
            Self::Time => f.write_str("the certificate's validity time is not valid"),
            Self::Name => f.write_str("a name in the certificate cannot be read"),
            Self::PublicKey => f.write_str("the certificate's public key is not valid"),
            Self::DuplicateExtension => f.write_str("the certificate repeats an extension"),
            Self::Extension => f.write_str("a certificate extension is malformed"),
        }
    }
}

impl std::error::Error for CertificateError {}

impl From<DerError> for CertificateError {
    fn from(error: DerError) -> Self {
        Self::Der(error)
    }
}

/// A name in a subject alternative name extension that TLS checks a server against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectAlternativeName {
    /// A `dNSName`, possibly with a wildcard first label (`*.example.com`).
    DnsName(String),
    /// An `iPAddress`.
    IpAddress(IpAddr),
}

/// The basic constraints extension: whether the key may sign certificates and how many
/// intermediate certificates may follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicConstraints {
    /// Whether the certificate belongs to a certificate authority.
    pub certificate_authority: bool,
    /// The most intermediate certificates that may follow this one in a chain.
    pub path_length: Option<u64>,
}

/// The key usage extension's flags (RFC 5280 section 4.2.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyUsage(u16);

impl KeyUsage {
    /// `digitalSignature`, needed to sign TLS handshakes.
    pub const DIGITAL_SIGNATURE: Self = Self(0x8000);
    /// `keyEncipherment`.
    pub const KEY_ENCIPHERMENT: Self = Self(0x8000 >> 2);
    /// `keyAgreement`.
    pub const KEY_AGREEMENT: Self = Self(0x8000 >> 4);
    /// `keyCertSign`, needed to sign certificates.
    pub const KEY_CERT_SIGN: Self = Self(0x8000 >> 5);
    /// `cRLSign`.
    pub const CRL_SIGN: Self = Self(0x8000 >> 6);

    /// Whether every flag of `usage` is set.
    pub fn contains(self, usage: KeyUsage) -> bool {
        self.0 & usage.0 == usage.0
    }
}

/// A distinguished name: its DER form, which chains compare, and its common names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistinguishedName {
    encoded: Vec<u8>,
    common_names: Vec<String>,
}

impl DistinguishedName {
    fn parse(element: DerElement<'_>) -> Result<Self, CertificateError> {
        let mut relative_names = DerReader::new(element.contents);
        let mut common_names = Vec::new();
        while !relative_names.is_empty() {
            let mut attributes = relative_names.read_constructed(tag::SET)?;
            if attributes.is_empty() {
                return Err(CertificateError::Name);
            }
            while !attributes.is_empty() {
                let mut attribute = attributes.read_sequence()?;
                let attribute_type = attribute.read_object_identifier()?;
                let value = attribute.read_element()?;
                attribute.finish()?;
                if attribute_type == oid::COMMON_NAME {
                    let text = decode_directory_string(value.tag, value.contents)
                        .ok_or(CertificateError::Name)?;
                    common_names.push(text);
                }
            }
        }
        Ok(Self {
            encoded: element.encoded.to_vec(),
            common_names,
        })
    }

    /// The name's DER encoding.
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    /// The text of every common name (`CN`) attribute, in order.
    pub fn common_names(&self) -> &[String] {
        &self.common_names
    }
}

fn decode_directory_string(string_tag: u8, bytes: &[u8]) -> Option<String> {
    match string_tag {
        tag::UTF8_STRING | tag::PRINTABLE_STRING | tag::IA5_STRING => {
            String::from_utf8(bytes.to_vec()).ok()
        }
        tag::TELETEX_STRING => Some(bytes.iter().map(|&byte| char::from(byte)).collect()),
        tag::BMP_STRING if bytes.len() % 2 == 0 => char::decode_utf16(
            bytes
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]])),
        )
        .collect::<Result<String, _>>()
        .ok(),
        tag::UNIVERSAL_STRING if bytes.len() % 4 == 0 => bytes
            .chunks_exact(4)
            .map(|quad| char::from_u32(u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]])))
            .collect(),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Extensions {
    subject_alternative_names: Vec<SubjectAlternativeName>,
    basic_constraints: Option<BasicConstraints>,
    key_usage: Option<KeyUsage>,
    extended_key_usage: Option<Vec<Vec<u8>>>,
    unhandled_critical_extension: bool,
}

impl Extensions {
    fn parse(explicit: &[u8]) -> Result<Self, CertificateError> {
        let mut outer = DerReader::new(explicit);
        let mut list = outer.read_sequence()?;
        outer.finish()?;
        if list.is_empty() {
            return Err(CertificateError::Extension);
        }
        let mut extensions = Self::default();
        let mut seen: Vec<&[u8]> = Vec::new();
        while !list.is_empty() {
            let mut extension = list.read_sequence()?;
            let identifier = extension.read_object_identifier()?;
            let critical = if extension.peek_tag() == Some(tag::BOOLEAN) {
                extension.read_boolean()?
            } else {
                false
            };
            let value = extension.read_octet_string()?;
            extension.finish()?;
            if seen.contains(&identifier) {
                return Err(CertificateError::DuplicateExtension);
            }
            seen.push(identifier);
            extensions.read_extension(identifier, critical, value)?;
        }
        Ok(extensions)
    }

    fn read_extension(
        &mut self,
        identifier: &[u8],
        critical: bool,
        value: &[u8],
    ) -> Result<(), CertificateError> {
        if identifier == oid::SUBJECT_ALT_NAME {
            self.subject_alternative_names = parse_subject_alternative_names(value)?;
        } else if identifier == oid::BASIC_CONSTRAINTS {
            self.basic_constraints = Some(parse_basic_constraints(value)?);
        } else if identifier == oid::KEY_USAGE {
            self.key_usage = Some(parse_key_usage(value)?);
        } else if identifier == oid::EXTENDED_KEY_USAGE {
            self.extended_key_usage = Some(parse_extended_key_usage(value)?);
        } else if critical {
            self.unhandled_critical_extension = true;
        }
        Ok(())
    }
}

fn single_element(value: &[u8]) -> Result<DerReader<'_>, CertificateError> {
    let mut outer = DerReader::new(value);
    let inner = outer.read_sequence()?;
    outer.finish()?;
    Ok(inner)
}

fn parse_subject_alternative_names(
    value: &[u8],
) -> Result<Vec<SubjectAlternativeName>, CertificateError> {
    let mut names = single_element(value)?;
    let mut parsed = Vec::new();
    while !names.is_empty() {
        let name = names.read_element()?;
        match name.tag {
            DNS_NAME_TAG => {
                let text = std::str::from_utf8(name.contents)
                    .ok()
                    .filter(|text| text.is_ascii())
                    .ok_or(CertificateError::Extension)?;
                parsed.push(SubjectAlternativeName::DnsName(text.to_string()));
            }
            IP_ADDRESS_TAG => {
                let address = parse_ip_address(name.contents).ok_or(CertificateError::Extension)?;
                parsed.push(SubjectAlternativeName::IpAddress(address));
            }
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_ip_address(bytes: &[u8]) -> Option<IpAddr> {
    if let Ok(octets) = <[u8; 4]>::try_from(bytes) {
        return Some(IpAddr::V4(Ipv4Addr::from(octets)));
    }
    <[u8; 16]>::try_from(bytes)
        .ok()
        .map(|octets| IpAddr::V6(Ipv6Addr::from(octets)))
}

fn parse_basic_constraints(value: &[u8]) -> Result<BasicConstraints, CertificateError> {
    let mut constraints = single_element(value)?;
    let certificate_authority = if constraints.peek_tag() == Some(tag::BOOLEAN) {
        constraints.read_boolean()?
    } else {
        false
    };
    let path_length = if constraints.peek_tag() == Some(tag::INTEGER) {
        Some(constraints.read_small_unsigned_integer()?)
    } else {
        None
    };
    constraints.finish()?;
    Ok(BasicConstraints {
        certificate_authority,
        path_length,
    })
}

fn parse_key_usage(value: &[u8]) -> Result<KeyUsage, CertificateError> {
    let mut outer = DerReader::new(value);
    let flags = outer.read_flag_bit_string()?;
    outer.finish()?;
    if flags.len() > KEY_USAGE_MAX_BYTES {
        return Err(CertificateError::Extension);
    }
    let first = flags.first().copied().unwrap_or(0);
    let second = flags.get(1).copied().unwrap_or(0);
    Ok(KeyUsage(u16::from_be_bytes([first, second])))
}

fn parse_extended_key_usage(value: &[u8]) -> Result<Vec<Vec<u8>>, CertificateError> {
    let mut purposes = single_element(value)?;
    let mut parsed = Vec::new();
    while !purposes.is_empty() {
        parsed.push(purposes.read_object_identifier()?.to_vec());
    }
    if parsed.is_empty() {
        return Err(CertificateError::Extension);
    }
    Ok(parsed)
}

fn parse_subject_public_key_info(contents: &[u8]) -> Result<SubjectPublicKey, CertificateError> {
    let mut info = DerReader::new(contents);
    let mut algorithm = info.read_sequence()?;
    let key_bits = info.read_bit_string()?;
    info.finish()?;
    let algorithm_identifier = algorithm.read_object_identifier()?;
    if algorithm_identifier == oid::EC_PUBLIC_KEY {
        if algorithm.peek_tag() != Some(tag::OBJECT_IDENTIFIER) {
            return Ok(SubjectPublicKey::Unsupported);
        }
        let curve = algorithm.read_object_identifier()?;
        algorithm.finish()?;
        if curve != oid::PRIME256V1 {
            return Ok(SubjectPublicKey::Unsupported);
        }
        return P256PublicKey::from_sec1(key_bits)
            .map(SubjectPublicKey::P256)
            .map_err(|_| CertificateError::PublicKey);
    }
    if algorithm_identifier == oid::RSA_ENCRYPTION {
        read_absent_or_null_parameters(&mut algorithm)?;
        let mut key = single_element(key_bits)?;
        let modulus = key.read_unsigned_integer()?;
        let public_exponent = key.read_unsigned_integer()?;
        key.finish()?;
        return RsaPublicKey::new(modulus, public_exponent)
            .map(SubjectPublicKey::Rsa)
            .map_err(|_| CertificateError::PublicKey);
    }
    Ok(SubjectPublicKey::Unsupported)
}

fn read_validity_time(reader: &mut DerReader<'_>) -> Result<i64, CertificateError> {
    let element = reader.read_element()?;
    let seconds = match element.tag {
        tag::UTC_TIME => parse_utc_time(element.contents),
        tag::GENERALIZED_TIME => parse_generalized_time(element.contents),
        _ => None,
    };
    seconds.ok_or(CertificateError::Time)
}

fn read_version(tbs: &mut DerReader<'_>) -> Result<u64, CertificateError> {
    let Some(explicit) = tbs.read_optional(tag::context_constructed(0))? else {
        return Ok(VERSION_1);
    };
    let mut reader = DerReader::new(explicit);
    let version = reader.read_small_unsigned_integer()?;
    reader.finish()?;
    if version > VERSION_3 {
        return Err(CertificateError::Version);
    }
    Ok(version)
}

/// A parsed X.509 certificate. It keeps its DER, so it can be sent in a TLS handshake as it
/// was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    der: Vec<u8>,
    to_be_signed: Vec<u8>,
    serial_number: Vec<u8>,
    signature_algorithm: SignatureAlgorithm,
    signature: Vec<u8>,
    issuer: DistinguishedName,
    subject: DistinguishedName,
    not_before: i64,
    not_after: i64,
    subject_public_key_info: Vec<u8>,
    public_key: SubjectPublicKey,
    extensions: Extensions,
}

impl Certificate {
    /// Reads a certificate from its DER encoding. Unsupported key types and signature
    /// algorithms are not errors here (they are [`SubjectPublicKey::Unsupported`] and
    /// [`SignatureAlgorithm::Unsupported`]), so a file of trust anchors with such certificates
    /// still loads; malformed input is.
    pub fn from_der(der: &[u8]) -> Result<Self, CertificateError> {
        let mut outer = DerReader::new(der);
        let mut certificate = outer.read_sequence()?;
        outer.finish()?;
        let to_be_signed = certificate.read_tagged_element(tag::SEQUENCE)?;
        let outer_algorithm = certificate.read_tagged_element(tag::SEQUENCE)?;
        let signature = certificate.read_bit_string()?;
        certificate.finish()?;

        let mut tbs = DerReader::new(to_be_signed.contents);
        let version = read_version(&mut tbs)?;
        let serial_number = tbs.read(tag::INTEGER)?;
        if serial_number.is_empty() {
            return Err(CertificateError::Der(DerError::IntegerEncoding));
        }
        let inner_algorithm = tbs.read_tagged_element(tag::SEQUENCE)?;
        if inner_algorithm.encoded != outer_algorithm.encoded {
            return Err(CertificateError::SignatureAlgorithmMismatch);
        }
        let signature_algorithm = parse_signature_algorithm(inner_algorithm.contents)?;
        let issuer = DistinguishedName::parse(tbs.read_tagged_element(tag::SEQUENCE)?)?;
        let mut validity = tbs.read_sequence()?;
        let not_before = read_validity_time(&mut validity)?;
        let not_after = read_validity_time(&mut validity)?;
        validity.finish()?;
        let subject = DistinguishedName::parse(tbs.read_tagged_element(tag::SEQUENCE)?)?;
        let subject_public_key_info = tbs.read_tagged_element(tag::SEQUENCE)?;
        let public_key = parse_subject_public_key_info(subject_public_key_info.contents)?;
        for unique_identifier in [ISSUER_UNIQUE_ID_TAG, SUBJECT_UNIQUE_ID_TAG] {
            if tbs.read_optional(unique_identifier)?.is_some() && version == VERSION_1 {
                return Err(CertificateError::Version);
            }
        }
        let extensions = match tbs.read_optional(tag::context_constructed(3))? {
            Some(explicit) if version == VERSION_3 => Extensions::parse(explicit)?,
            Some(_) => return Err(CertificateError::Version),
            None => Extensions::default(),
        };
        tbs.finish()?;

        Ok(Self {
            der: der.to_vec(),
            to_be_signed: to_be_signed.encoded.to_vec(),
            serial_number: serial_number.to_vec(),
            signature_algorithm,
            signature: signature.to_vec(),
            issuer,
            subject,
            not_before,
            not_after,
            subject_public_key_info: subject_public_key_info.encoded.to_vec(),
            public_key,
            extensions,
        })
    }

    /// The certificate's DER encoding, as it was read.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The certificate as PEM text.
    pub fn to_pem(&self) -> String {
        pem_encode(CERTIFICATE_LABEL, &self.der)
    }

    /// The signed part, `TBSCertificate`, with its tag and length.
    pub fn to_be_signed(&self) -> &[u8] {
        &self.to_be_signed
    }

    /// The serial number's `INTEGER` contents (two's complement, as the issuer wrote it).
    pub fn serial_number(&self) -> &[u8] {
        &self.serial_number
    }

    /// How the issuer signed the certificate.
    pub fn signature_algorithm(&self) -> SignatureAlgorithm {
        self.signature_algorithm
    }

    /// The issuer's signature.
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }

    /// The name of the issuer.
    pub fn issuer(&self) -> &DistinguishedName {
        &self.issuer
    }

    /// The name of the subject.
    pub fn subject(&self) -> &DistinguishedName {
        &self.subject
    }

    /// The first second the certificate is valid, in seconds since the Unix epoch.
    pub fn not_before_unix_seconds(&self) -> i64 {
        self.not_before
    }

    /// The last second the certificate is valid, in seconds since the Unix epoch.
    pub fn not_after_unix_seconds(&self) -> i64 {
        self.not_after
    }

    /// The subject's public key.
    pub fn public_key(&self) -> &SubjectPublicKey {
        &self.public_key
    }

    /// The `SubjectPublicKeyInfo` with its tag and length.
    pub fn subject_public_key_info(&self) -> &[u8] {
        &self.subject_public_key_info
    }

    /// The DNS names and IP addresses of the subject alternative name extension (empty when
    /// there is none).
    pub fn subject_alternative_names(&self) -> &[SubjectAlternativeName] {
        &self.extensions.subject_alternative_names
    }

    /// The basic constraints extension, if present.
    pub fn basic_constraints(&self) -> Option<BasicConstraints> {
        self.extensions.basic_constraints
    }

    /// The key usage extension, if present.
    pub fn key_usage(&self) -> Option<KeyUsage> {
        self.extensions.key_usage
    }

    /// The purposes of the extended key usage extension (constants of [`crate::oid`]), if
    /// present.
    pub fn extended_key_usage(&self) -> Option<&[Vec<u8>]> {
        self.extensions.extended_key_usage.as_deref()
    }

    /// Whether the certificate has a critical extension EggDB does not read (such as name
    /// constraints), which means it must not be trusted (RFC 5280 section 4.2).
    pub fn has_unhandled_critical_extension(&self) -> bool {
        self.extensions.unhandled_critical_extension
    }

    /// Checks the certificate's signature with its issuer's public key.
    pub fn verify_signed_by(
        &self,
        issuer_public_key: &SubjectPublicKey,
    ) -> Result<(), SignatureError> {
        verify_signature(
            self.signature_algorithm,
            issuer_public_key,
            &self.to_be_signed,
            &self.signature,
        )
    }
}

/// Every certificate in PEM `text`, in order. Blocks with other labels (such as a private key
/// in the same file) are skipped; text with no certificate at all is an error.
pub fn certificates_from_pem(text: &str) -> Result<Vec<Certificate>, CertificateError> {
    let certificates = pem_decode_all(text)
        .map_err(CertificateError::Pem)?
        .into_iter()
        .filter(|block| block.label == CERTIFICATE_LABEL)
        .map(|block| Certificate::from_der(&block.contents))
        .collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(CertificateError::NoCertificates);
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_strings_decode_every_string_type() {
        assert_eq!(
            decode_directory_string(tag::UTF8_STRING, "caf\u{e9}".as_bytes()),
            Some("caf\u{e9}".to_string())
        );
        assert_eq!(
            decode_directory_string(tag::TELETEX_STRING, &[0x63, 0xe9]),
            Some("c\u{e9}".to_string())
        );
        assert_eq!(
            decode_directory_string(tag::BMP_STRING, &[0x00, 0x41, 0x00, 0xe9]),
            Some("A\u{e9}".to_string())
        );
        assert_eq!(decode_directory_string(tag::BMP_STRING, &[0x00]), None);
        assert_eq!(
            decode_directory_string(tag::BMP_STRING, &[0xd8, 0x00]),
            None
        );
        assert_eq!(
            decode_directory_string(tag::UNIVERSAL_STRING, &[0, 0, 0, 0x41]),
            Some("A".to_string())
        );
        assert_eq!(
            decode_directory_string(tag::UNIVERSAL_STRING, &[0, 0x11, 0, 0]),
            None
        );
        assert_eq!(decode_directory_string(tag::UTF8_STRING, &[0xff]), None);
        assert_eq!(decode_directory_string(tag::INTEGER, b"1"), None);
    }

    #[test]
    fn key_usage_flags_follow_the_bit_order() {
        assert_eq!(
            parse_key_usage(&[0x03, 0x02, 0x07, 0x80]),
            Ok(KeyUsage::DIGITAL_SIGNATURE)
        );
        let ca = parse_key_usage(&[0x03, 0x02, 0x01, 0x06]).unwrap();
        assert!(ca.contains(KeyUsage::KEY_CERT_SIGN));
        assert!(ca.contains(KeyUsage::CRL_SIGN));
        assert!(!ca.contains(KeyUsage::DIGITAL_SIGNATURE));
        assert!(parse_key_usage(&[0x03, 0x02, 0x07, 0x81]).is_err());
        assert!(parse_key_usage(&[0x03, 0x04, 0x00, 0x80, 0x00, 0x00]).is_err());
    }

    #[test]
    fn subject_alternative_names_keep_dns_and_ip_entries() {
        let value = [
            0x30, 0x18, 0x82, 0x06, b'a', b'.', b't', b'e', b's', b't', 0x87, 0x04, 192, 0, 2, 1,
            0x81, 0x03, b'a', b'@', b'b', 0x87, 0x03, 1, 2, 3,
        ];
        assert_eq!(
            parse_subject_alternative_names(&value),
            Err(CertificateError::Extension)
        );
        let value = [
            0x30, 0x13, 0x82, 0x06, b'a', b'.', b't', b'e', b's', b't', 0x87, 0x04, 192, 0, 2, 1,
            0x81, 0x03, b'a', b'@', b'b',
        ];
        assert_eq!(
            parse_subject_alternative_names(&value),
            Ok(vec![
                SubjectAlternativeName::DnsName("a.test".to_string()),
                SubjectAlternativeName::IpAddress(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
            ])
        );
    }

    #[test]
    fn basic_constraints_default_to_an_end_entity() {
        assert_eq!(
            parse_basic_constraints(&[0x30, 0x00]),
            Ok(BasicConstraints {
                certificate_authority: false,
                path_length: None
            })
        );
        assert_eq!(
            parse_basic_constraints(&[0x30, 0x06, 0x01, 0x01, 0xff, 0x02, 0x01, 0x00]),
            Ok(BasicConstraints {
                certificate_authority: true,
                path_length: Some(0)
            })
        );
        assert!(parse_basic_constraints(&[0x30, 0x03, 0x01, 0x01, 0x01]).is_err());
    }
}
