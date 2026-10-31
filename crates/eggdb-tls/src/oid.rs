//! The object identifiers EggDB's TLS reads and writes, as their DER content bytes (the
//! encoded arcs), so a parsed identifier is compared with `==`.

/// `rsaEncryption` (1.2.840.113549.1.1.1), an RSA public key.
pub const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// `sha1WithRSAEncryption` (1.2.840.113549.1.1.5).
pub const SHA1_WITH_RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
/// `id-mgf1` (1.2.840.113549.1.1.8).
pub const MGF1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08];
/// `id-RSASSA-PSS` (1.2.840.113549.1.1.10).
pub const RSASSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
/// `sha256WithRSAEncryption` (1.2.840.113549.1.1.11).
pub const SHA256_WITH_RSA_ENCRYPTION: &[u8] =
    &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
/// `sha384WithRSAEncryption` (1.2.840.113549.1.1.12).
pub const SHA384_WITH_RSA_ENCRYPTION: &[u8] =
    &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
/// `sha512WithRSAEncryption` (1.2.840.113549.1.1.13).
pub const SHA512_WITH_RSA_ENCRYPTION: &[u8] =
    &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
/// `id-ecPublicKey` (1.2.840.10045.2.1), an elliptic-curve public key.
pub const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// `prime256v1` (1.2.840.10045.3.1.7), the curve P-256.
pub const PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// `secp384r1` (1.3.132.0.34), the curve P-384.
pub const SECP384R1: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// `ecdsa-with-SHA256` (1.2.840.10045.4.3.2).
pub const ECDSA_WITH_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// `ecdsa-with-SHA384` (1.2.840.10045.4.3.3).
pub const ECDSA_WITH_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
/// `ecdsa-with-SHA512` (1.2.840.10045.4.3.4).
pub const ECDSA_WITH_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
/// `id-Ed25519` (1.3.101.112).
pub const ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// `id-sha1` (1.3.14.3.2.26).
pub const SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
/// `id-sha256` (2.16.840.1.101.3.4.2.1).
pub const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
/// `id-sha384` (2.16.840.1.101.3.4.2.2).
pub const SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
/// `id-sha512` (2.16.840.1.101.3.4.2.3).
pub const SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
/// `id-at-commonName` (2.5.4.3).
pub const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// `id-at-countryName` (2.5.4.6).
pub const COUNTRY_NAME: &[u8] = &[0x55, 0x04, 0x06];
/// `id-at-organizationName` (2.5.4.10).
pub const ORGANIZATION_NAME: &[u8] = &[0x55, 0x04, 0x0a];
/// `id-ce-subjectKeyIdentifier` (2.5.29.14).
pub const SUBJECT_KEY_IDENTIFIER: &[u8] = &[0x55, 0x1d, 0x0e];
/// `id-ce-keyUsage` (2.5.29.15).
pub const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// `id-ce-subjectAltName` (2.5.29.17).
pub const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
/// `id-ce-basicConstraints` (2.5.29.19).
pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// `id-ce-nameConstraints` (2.5.29.30).
pub const NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];
/// `id-ce-authorityKeyIdentifier` (2.5.29.35).
pub const AUTHORITY_KEY_IDENTIFIER: &[u8] = &[0x55, 0x1d, 0x23];
/// `id-ce-extKeyUsage` (2.5.29.37).
pub const EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// `anyExtendedKeyUsage` (2.5.29.37.0).
pub const ANY_EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25, 0x00];
/// `id-kp-serverAuth` (1.3.6.1.5.5.7.3.1).
pub const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
/// `id-kp-clientAuth` (1.3.6.1.5.5.7.3.2).
pub const CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];
