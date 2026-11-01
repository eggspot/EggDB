//! Signature algorithms and public keys as certificates and TLS name them, checking a signature
//! with any of them, and the DER form of an ECDSA signature (RFC 5280, RFC 4055, RFC 5758,
//! RFC 8017).

use eggdb_crypto::{
    EcdsaP256Signature, P256_ELEMENT_BYTES, P256PublicKey, RsaPublicKey, Sha1, Sha256, Sha384,
    Sha512, ecdsa_p256_verify, rsa_pkcs1v15_verify, rsa_pss_verify, sha1, sha256, sha384, sha512,
};

use crate::der::{DerError, DerReader, DerWriter, tag};
use crate::oid;

const PSS_DEFAULT_SALT_LENGTH: u64 = 20;
const PSS_TRAILER_FIELD_BC: u64 = 1;

/// The hash a signature is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureHash {
    /// SHA-1, refused in certificate chains and only checked where an old protocol needs it.
    Sha1,
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
}

impl SignatureHash {
    /// The digest of `message`.
    pub fn digest(self, message: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1(message).to_vec(),
            Self::Sha256 => sha256(message).to_vec(),
            Self::Sha384 => sha384(message).to_vec(),
            Self::Sha512 => sha512(message).to_vec(),
        }
    }

    /// The length of the digest in bytes.
    pub fn digest_bytes(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }
}

/// A signature algorithm: how a signature is made and checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// ECDSA over the hash; the curve is the key's.
    Ecdsa(SignatureHash),
    /// RSA with PKCS#1 v1.5 padding over the hash.
    RsaPkcs1(SignatureHash),
    /// RSA-PSS over the hash, with MGF1 over the same hash.
    RsaPss {
        /// The message and MGF1 hash.
        hash: SignatureHash,
        /// The salt length in bytes.
        salt_length: usize,
    },
    /// An algorithm EggDB does not check (such as Ed25519 or a PSS form with a different MGF1
    /// hash); a signature made with it never verifies.
    Unsupported,
}

impl SignatureAlgorithm {
    /// The hash the algorithm signs over, if it is supported.
    pub fn hash(self) -> Option<SignatureHash> {
        match self {
            Self::Ecdsa(hash) | Self::RsaPkcs1(hash) | Self::RsaPss { hash, .. } => Some(hash),
            Self::Unsupported => None,
        }
    }
}

/// A public key from a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectPublicKey {
    /// An elliptic-curve key on P-256.
    P256(P256PublicKey),
    /// An RSA key.
    Rsa(RsaPublicKey),
    /// A key of a type or curve EggDB does not use (such as P-384 or Ed25519).
    Unsupported,
}

/// Why a signature was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// The algorithm or the key type is one EggDB does not check.
    UnsupportedAlgorithm,
    /// The algorithm needs another type of key than the one given.
    KeyMismatch,
    /// The signature is not a valid signature of the message by the key.
    Invalid,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsupportedAlgorithm => "the signature algorithm or key type is not supported",
            Self::KeyMismatch => "the signature algorithm does not match the key type",
            Self::Invalid => "the signature is not valid",
        })
    }
}

impl std::error::Error for SignatureError {}

/// Checks that `signature` is `public_key`'s signature of `message` with `algorithm`. ECDSA
/// signatures are in their DER form, RSA signatures are the raw integer of the key's length.
pub fn verify_signature(
    algorithm: SignatureAlgorithm,
    public_key: &SubjectPublicKey,
    message: &[u8],
    signature: &[u8],
) -> Result<(), SignatureError> {
    let valid = match (algorithm, public_key) {
        (SignatureAlgorithm::Unsupported, _) | (_, SubjectPublicKey::Unsupported) => {
            return Err(SignatureError::UnsupportedAlgorithm);
        }
        (SignatureAlgorithm::Ecdsa(hash), SubjectPublicKey::P256(key)) => {
            ecdsa_signature_from_der(signature)
                .is_ok_and(|signature| ecdsa_p256_verify(key, &hash.digest(message), &signature))
        }
        (SignatureAlgorithm::RsaPkcs1(hash), SubjectPublicKey::Rsa(key)) => {
            verify_rsa_pkcs1(hash, key, message, signature)
        }
        (SignatureAlgorithm::RsaPss { hash, salt_length }, SubjectPublicKey::Rsa(key)) => {
            verify_rsa_pss(hash, salt_length, key, message, signature)
        }
        _ => return Err(SignatureError::KeyMismatch),
    };
    if valid {
        Ok(())
    } else {
        Err(SignatureError::Invalid)
    }
}

fn verify_rsa_pkcs1(
    hash: SignatureHash,
    key: &RsaPublicKey,
    message: &[u8],
    signature: &[u8],
) -> bool {
    let digest = hash.digest(message);
    match hash {
        SignatureHash::Sha1 => rsa_pkcs1v15_verify::<Sha1>(key, &digest, signature),
        SignatureHash::Sha256 => rsa_pkcs1v15_verify::<Sha256>(key, &digest, signature),
        SignatureHash::Sha384 => rsa_pkcs1v15_verify::<Sha384>(key, &digest, signature),
        SignatureHash::Sha512 => rsa_pkcs1v15_verify::<Sha512>(key, &digest, signature),
    }
}

fn verify_rsa_pss(
    hash: SignatureHash,
    salt_length: usize,
    key: &RsaPublicKey,
    message: &[u8],
    signature: &[u8],
) -> bool {
    let digest = hash.digest(message);
    match hash {
        SignatureHash::Sha1 => rsa_pss_verify::<Sha1>(key, &digest, signature, salt_length),
        SignatureHash::Sha256 => rsa_pss_verify::<Sha256>(key, &digest, signature, salt_length),
        SignatureHash::Sha384 => rsa_pss_verify::<Sha384>(key, &digest, signature, salt_length),
        SignatureHash::Sha512 => rsa_pss_verify::<Sha512>(key, &digest, signature, salt_length),
    }
}

/// Reads an ECDSA P-256 signature from its DER form, `SEQUENCE { r INTEGER, s INTEGER }`
/// (RFC 3279 section 2.2.3). An r or s longer than 32 bytes is refused; range checks are the
/// verifier's.
pub fn ecdsa_signature_from_der(der: &[u8]) -> Result<EcdsaP256Signature, DerError> {
    let mut outer = DerReader::new(der);
    let mut sequence = outer.read_sequence()?;
    outer.finish()?;
    let r = fixed_width_scalar(sequence.read_unsigned_integer()?)?;
    let s = fixed_width_scalar(sequence.read_unsigned_integer()?)?;
    sequence.finish()?;
    Ok(EcdsaP256Signature { r, s })
}

/// The DER form of an ECDSA P-256 signature.
pub fn ecdsa_signature_to_der(signature: &EcdsaP256Signature) -> Vec<u8> {
    let mut writer = DerWriter::new();
    writer.write_sequence(|sequence| {
        sequence.write_unsigned_integer(&signature.r);
        sequence.write_unsigned_integer(&signature.s);
    });
    writer.into_bytes()
}

fn fixed_width_scalar(magnitude: &[u8]) -> Result<[u8; P256_ELEMENT_BYTES], DerError> {
    let start = P256_ELEMENT_BYTES
        .checked_sub(magnitude.len())
        .ok_or(DerError::IntegerEncoding)?;
    let mut scalar = [0u8; P256_ELEMENT_BYTES];
    scalar[start..].copy_from_slice(magnitude);
    Ok(scalar)
}

const ECDSA_ALGORITHMS: [(&[u8], SignatureHash); 3] = [
    (oid::ECDSA_WITH_SHA256, SignatureHash::Sha256),
    (oid::ECDSA_WITH_SHA384, SignatureHash::Sha384),
    (oid::ECDSA_WITH_SHA512, SignatureHash::Sha512),
];

const RSA_PKCS1_ALGORITHMS: [(&[u8], SignatureHash); 4] = [
    (oid::SHA1_WITH_RSA_ENCRYPTION, SignatureHash::Sha1),
    (oid::SHA256_WITH_RSA_ENCRYPTION, SignatureHash::Sha256),
    (oid::SHA384_WITH_RSA_ENCRYPTION, SignatureHash::Sha384),
    (oid::SHA512_WITH_RSA_ENCRYPTION, SignatureHash::Sha512),
];

const HASH_ALGORITHMS: [(&[u8], SignatureHash); 4] = [
    (oid::SHA1, SignatureHash::Sha1),
    (oid::SHA256, SignatureHash::Sha256),
    (oid::SHA384, SignatureHash::Sha384),
    (oid::SHA512, SignatureHash::Sha512),
];

fn find_hash(table: &[(&[u8], SignatureHash)], algorithm: &[u8]) -> Option<SignatureHash> {
    table
        .iter()
        .find(|(identifier, _)| *identifier == algorithm)
        .map(|&(_, hash)| hash)
}

/// Reads the parameters of an algorithm that takes none: absent or `NULL`.
pub(crate) fn read_absent_or_null_parameters(reader: &mut DerReader<'_>) -> Result<(), DerError> {
    if !reader.is_empty() {
        reader.read_null()?;
    }
    reader.finish()
}

/// Reads the contents of a signature `AlgorithmIdentifier`.
pub(crate) fn parse_signature_algorithm(contents: &[u8]) -> Result<SignatureAlgorithm, DerError> {
    let mut reader = DerReader::new(contents);
    let algorithm = reader.read_object_identifier()?;
    if let Some(hash) = find_hash(&ECDSA_ALGORITHMS, algorithm) {
        reader.finish()?;
        return Ok(SignatureAlgorithm::Ecdsa(hash));
    }
    if let Some(hash) = find_hash(&RSA_PKCS1_ALGORITHMS, algorithm) {
        read_absent_or_null_parameters(&mut reader)?;
        return Ok(SignatureAlgorithm::RsaPkcs1(hash));
    }
    if algorithm == oid::RSASSA_PSS {
        let parameters = reader.read_sequence()?;
        reader.finish()?;
        return parse_pss_parameters(parameters);
    }
    Ok(SignatureAlgorithm::Unsupported)
}

fn parse_pss_parameters(mut parameters: DerReader<'_>) -> Result<SignatureAlgorithm, DerError> {
    let hash = match parameters.read_optional(tag::context_constructed(0))? {
        Some(explicit) => read_explicit_hash_algorithm(explicit)?,
        None => Some(SignatureHash::Sha1),
    };
    let mask_hash = match parameters.read_optional(tag::context_constructed(1))? {
        Some(explicit) => read_explicit_mask_generation(explicit)?,
        None => Some(SignatureHash::Sha1),
    };
    let salt_length = match parameters.read_optional(tag::context_constructed(2))? {
        Some(explicit) => read_explicit_small_integer(explicit)?,
        None => PSS_DEFAULT_SALT_LENGTH,
    };
    let trailer_field = match parameters.read_optional(tag::context_constructed(3))? {
        Some(explicit) => read_explicit_small_integer(explicit)?,
        None => PSS_TRAILER_FIELD_BC,
    };
    parameters.finish()?;
    let salt_length = usize::try_from(salt_length).map_err(|_| DerError::IntegerEncoding)?;
    Ok(match (hash, mask_hash) {
        (Some(hash), Some(mask_hash))
            if hash == mask_hash && trailer_field == PSS_TRAILER_FIELD_BC =>
        {
            SignatureAlgorithm::RsaPss { hash, salt_length }
        }
        _ => SignatureAlgorithm::Unsupported,
    })
}

fn read_explicit_hash_algorithm(explicit: &[u8]) -> Result<Option<SignatureHash>, DerError> {
    let mut outer = DerReader::new(explicit);
    let identifier = outer.read(tag::SEQUENCE)?;
    outer.finish()?;
    parse_hash_algorithm(identifier)
}

fn read_explicit_mask_generation(explicit: &[u8]) -> Result<Option<SignatureHash>, DerError> {
    let mut outer = DerReader::new(explicit);
    let mut mask_generation = outer.read_sequence()?;
    outer.finish()?;
    let algorithm = mask_generation.read_object_identifier()?;
    let hash_identifier = mask_generation.read(tag::SEQUENCE)?;
    mask_generation.finish()?;
    if algorithm == oid::MGF1 {
        parse_hash_algorithm(hash_identifier)
    } else {
        Ok(None)
    }
}

fn read_explicit_small_integer(explicit: &[u8]) -> Result<u64, DerError> {
    let mut outer = DerReader::new(explicit);
    let value = outer.read_small_unsigned_integer()?;
    outer.finish()?;
    Ok(value)
}

fn parse_hash_algorithm(contents: &[u8]) -> Result<Option<SignatureHash>, DerError> {
    let mut reader = DerReader::new(contents);
    let algorithm = reader.read_object_identifier()?;
    read_absent_or_null_parameters(&mut reader)?;
    Ok(find_hash(&HASH_ALGORITHMS, algorithm))
}

/// Writes the `AlgorithmIdentifier` of `ecdsa-with-SHA256`, which takes no parameters.
pub(crate) fn write_ecdsa_sha256_algorithm(writer: &mut DerWriter) {
    writer.write_sequence(|algorithm| algorithm.write_object_identifier(oid::ECDSA_WITH_SHA256));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecdsa_der_round_trips_and_keeps_leading_zeros() {
        let mut signature = EcdsaP256Signature {
            r: [0x80; P256_ELEMENT_BYTES],
            s: [0x01; P256_ELEMENT_BYTES],
        };
        signature.s[0] = 0;
        let der = ecdsa_signature_to_der(&signature);
        assert_eq!(der[0], tag::SEQUENCE);
        assert_eq!(ecdsa_signature_from_der(&der), Ok(signature));
    }

    #[test]
    fn ecdsa_der_refuses_long_and_negative_integers() {
        let mut writer = DerWriter::new();
        writer.write_sequence(|sequence| {
            sequence.write_unsigned_integer(&[0x01; 33]);
            sequence.write_unsigned_integer(&[0x01]);
        });
        assert_eq!(
            ecdsa_signature_from_der(&writer.into_bytes()),
            Err(DerError::IntegerEncoding)
        );
        assert_eq!(
            ecdsa_signature_from_der(&[0x30, 0x06, 0x02, 0x01, 0x81, 0x02, 0x01, 0x01]),
            Err(DerError::IntegerEncoding)
        );
        assert!(ecdsa_signature_from_der(&[0x30, 0x03, 0x02, 0x01, 0x01]).is_err());
        assert!(
            ecdsa_signature_from_der(&[0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x00])
                .is_err()
        );
    }

    #[test]
    fn signature_algorithms_are_read_with_their_parameters() {
        let ecdsa = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
        assert_eq!(
            parse_signature_algorithm(&ecdsa),
            Ok(SignatureAlgorithm::Ecdsa(SignatureHash::Sha256))
        );
        let mut ecdsa_with_null = ecdsa.to_vec();
        ecdsa_with_null.extend_from_slice(&[0x05, 0x00]);
        assert_eq!(
            parse_signature_algorithm(&ecdsa_with_null),
            Err(DerError::TrailingData)
        );
        let mut rsa = vec![0x06, 0x09];
        rsa.extend_from_slice(oid::SHA256_WITH_RSA_ENCRYPTION);
        assert_eq!(
            parse_signature_algorithm(&rsa),
            Ok(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256))
        );
        rsa.extend_from_slice(&[0x05, 0x00]);
        assert_eq!(
            parse_signature_algorithm(&rsa),
            Ok(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256))
        );
        let ed25519 = [0x06, 0x03, 0x2b, 0x65, 0x70];
        assert_eq!(
            parse_signature_algorithm(&ed25519),
            Ok(SignatureAlgorithm::Unsupported)
        );
    }

    fn pss_parameters(hash: &[u8], mask_hash: &[u8], salt_length: u64) -> Vec<u8> {
        let mut writer = DerWriter::new();
        writer.write_object_identifier(oid::RSASSA_PSS);
        writer.write_sequence(|parameters| {
            parameters.write_constructed(tag::context_constructed(0), |explicit| {
                explicit.write_sequence(|identifier| identifier.write_object_identifier(hash));
            });
            parameters.write_constructed(tag::context_constructed(1), |explicit| {
                explicit.write_sequence(|mask| {
                    mask.write_object_identifier(oid::MGF1);
                    mask.write_sequence(|identifier| {
                        identifier.write_object_identifier(mask_hash);
                        identifier.write_null();
                    });
                });
            });
            parameters.write_constructed(tag::context_constructed(2), |explicit| {
                explicit.write_small_unsigned_integer(salt_length);
            });
        });
        writer.into_bytes()
    }

    #[test]
    fn pss_parameters_name_the_hash_and_salt_length() {
        assert_eq!(
            parse_signature_algorithm(&pss_parameters(oid::SHA256, oid::SHA256, 32)),
            Ok(SignatureAlgorithm::RsaPss {
                hash: SignatureHash::Sha256,
                salt_length: 32
            })
        );
        assert_eq!(
            parse_signature_algorithm(&pss_parameters(oid::SHA384, oid::SHA256, 48)),
            Ok(SignatureAlgorithm::Unsupported)
        );
        let mut defaults = vec![0x06, 0x09];
        defaults.extend_from_slice(oid::RSASSA_PSS);
        defaults.extend_from_slice(&[0x30, 0x00]);
        assert_eq!(
            parse_signature_algorithm(&defaults),
            Ok(SignatureAlgorithm::RsaPss {
                hash: SignatureHash::Sha1,
                salt_length: 20
            })
        );
    }

    #[test]
    fn mismatched_and_unsupported_keys_are_reported() {
        assert_eq!(
            verify_signature(
                SignatureAlgorithm::Ecdsa(SignatureHash::Sha256),
                &SubjectPublicKey::Unsupported,
                b"message",
                b"signature"
            ),
            Err(SignatureError::UnsupportedAlgorithm)
        );
        assert_eq!(
            verify_signature(
                SignatureAlgorithm::Unsupported,
                &SubjectPublicKey::Unsupported,
                b"message",
                b"signature"
            ),
            Err(SignatureError::UnsupportedAlgorithm)
        );
    }
}
