//! Private keys for TLS: P-256 and RSA keys read from PKCS#8 (RFC 5958), SEC 1 (RFC 5915) and
//! PKCS#1 (RFC 8017) DER or PEM, P-256 keys written as PKCS#8, and signing with either.

use eggdb_crypto::{
    P256_ELEMENT_BYTES, P256PrivateKey, P256PublicKey, RsaError, RsaPrivateKey, Sha1, Sha256,
    Sha384, Sha512, ecdsa_p256_sign, rsa_pkcs1v15_sign, rsa_pss_sign, sha1, sha256, sha384, sha512,
};

use crate::der::{DerError, DerReader, DerWriter, tag};
use crate::oid;
use crate::pem::{
    EC_PRIVATE_KEY_LABEL, ENCRYPTED_PRIVATE_KEY_LABEL, PRIVATE_KEY_LABEL, PemError,
    RSA_PRIVATE_KEY_LABEL, pem_decode_all, pem_encode,
};
use crate::signature::{
    SignatureAlgorithm, SignatureHash, SubjectPublicKey, ecdsa_signature_to_der,
    read_absent_or_null_parameters,
};

const PKCS8_MAX_VERSION: u64 = 1;
const PKCS1_MAX_VERSION: u64 = 1;
const PKCS1_MULTI_PRIME_VERSION: u64 = 1;
const SEC1_VERSION: u64 = 1;
const PKCS1_CRT_INTEGERS: usize = 5;
const P256_KEY_GENERATION_ATTEMPTS: usize = 16;

/// A source of unpredictable bytes, such as `eggdb_platform::secure_random::fill`.
pub type FillRandom<'a> = &'a mut dyn FnMut(&mut [u8]) -> std::io::Result<()>;

/// Why a private key was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateKeyError {
    /// The key is not valid DER of its structure.
    Der(DerError),
    /// The PEM text is not valid.
    Pem(PemError),
    /// The PEM text holds no private key block.
    NoPrivateKey,
    /// The key is encrypted; EggDB reads only unencrypted keys.
    Encrypted,
    /// The structure's version is not one EggDB reads.
    Version,
    /// A key type other than elliptic curve and RSA.
    UnsupportedAlgorithm,
    /// An elliptic-curve key on a curve other than P-256.
    UnsupportedCurve,
    /// The key's numbers are out of range or do not fit together.
    InvalidKey,
    /// The public key stored with the private key is not the private key's.
    PublicKeyMismatch,
}

impl std::fmt::Display for PrivateKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Der(error) => write!(f, "the private key is not valid DER: {error}"),
            Self::Pem(error) => write!(f, "the private key file is not valid PEM: {error}"),
            Self::NoPrivateKey => f.write_str("the file holds no private key block"),
            Self::Encrypted => f.write_str(
                "the private key is encrypted: decrypt it first, EggDB reads plain keys",
            ),
            Self::Version => f.write_str("the private key's format version is not supported"),
            Self::UnsupportedAlgorithm => {
                f.write_str("the private key is neither an elliptic-curve nor an RSA key")
            }
            Self::UnsupportedCurve => f.write_str("the elliptic-curve key is not on P-256"),
            Self::InvalidKey => f.write_str("the private key's numbers are not valid"),
            Self::PublicKeyMismatch => {
                f.write_str("the public key stored with the private key does not match it")
            }
        }
    }
}

impl std::error::Error for PrivateKeyError {}

impl From<DerError> for PrivateKeyError {
    fn from(error: DerError) -> Self {
        Self::Der(error)
    }
}

/// Why signing failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningError {
    /// The algorithm is not one EggDB signs with.
    UnsupportedAlgorithm,
    /// The algorithm needs another type of key.
    KeyMismatch,
    /// The RSA key cannot sign this way (for example, it is too short for the salt).
    Rsa(RsaError),
    /// The random source failed.
    Random(std::io::ErrorKind),
}

impl std::fmt::Display for SigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAlgorithm => f.write_str("the signature algorithm is not supported"),
            Self::KeyMismatch => f.write_str("the signature algorithm does not match the key"),
            Self::Rsa(error) => write!(f, "RSA signing failed: {error}"),
            Self::Random(kind) => write!(f, "the random source failed: {kind}"),
        }
    }
}

impl std::error::Error for SigningError {}

/// A private key EggDB signs TLS handshakes with. Its `Debug` output never shows the secret.
#[derive(Debug, Clone)]
pub enum PrivateKey {
    /// A P-256 key.
    P256(P256PrivateKey),
    /// An RSA key.
    Rsa(RsaPrivateKey),
}

impl PrivateKey {
    /// Reads the first private key in PEM `text`: a `PRIVATE KEY` (PKCS#8), `EC PRIVATE KEY`
    /// (SEC 1) or `RSA PRIVATE KEY` (PKCS#1) block. Other blocks, such as certificates or
    /// `EC PARAMETERS`, are skipped.
    pub fn from_pem(text: &str) -> Result<Self, PrivateKeyError> {
        for block in pem_decode_all(text).map_err(PrivateKeyError::Pem)? {
            match block.label.as_str() {
                PRIVATE_KEY_LABEL => return Self::from_pkcs8_der(&block.contents),
                EC_PRIVATE_KEY_LABEL => return Self::from_sec1_der(&block.contents),
                RSA_PRIVATE_KEY_LABEL => return Self::from_pkcs1_der(&block.contents),
                ENCRYPTED_PRIVATE_KEY_LABEL => return Err(PrivateKeyError::Encrypted),
                _ => {}
            }
        }
        Err(PrivateKeyError::NoPrivateKey)
    }

    /// Reads an unencrypted PKCS#8 `PrivateKeyInfo` (or `OneAsymmetricKey`) holding a P-256 or
    /// RSA key.
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, PrivateKeyError> {
        let mut outer = DerReader::new(der);
        let mut info = outer.read_sequence()?;
        outer.finish()?;
        if info.read_small_unsigned_integer()? > PKCS8_MAX_VERSION {
            return Err(PrivateKeyError::Version);
        }
        let mut algorithm = info.read_sequence()?;
        let private_key = info.read_octet_string()?;
        while !info.is_empty() {
            info.read_element()?;
        }
        let algorithm_identifier = algorithm.read_object_identifier()?;
        if algorithm_identifier == oid::EC_PUBLIC_KEY {
            let curve = algorithm.read_object_identifier()?;
            algorithm.finish()?;
            if curve != oid::PRIME256V1 {
                return Err(PrivateKeyError::UnsupportedCurve);
            }
            return parse_ec_private_key(private_key).map(Self::P256);
        }
        if algorithm_identifier == oid::RSA_ENCRYPTION {
            read_absent_or_null_parameters(&mut algorithm)?;
            return Self::from_pkcs1_der(private_key);
        }
        Err(PrivateKeyError::UnsupportedAlgorithm)
    }

    /// Reads a SEC 1 `ECPrivateKey` on P-256.
    pub fn from_sec1_der(der: &[u8]) -> Result<Self, PrivateKeyError> {
        parse_ec_private_key(der).map(Self::P256)
    }

    /// Reads a PKCS#1 `RSAPrivateKey`. Only n, e and d are kept; the primes and CRT values must
    /// be well-formed integers but are not used.
    pub fn from_pkcs1_der(der: &[u8]) -> Result<Self, PrivateKeyError> {
        let mut outer = DerReader::new(der);
        let mut key = outer.read_sequence()?;
        outer.finish()?;
        let version = key.read_small_unsigned_integer()?;
        if version > PKCS1_MAX_VERSION {
            return Err(PrivateKeyError::Version);
        }
        let modulus = key.read_unsigned_integer()?;
        let public_exponent = key.read_unsigned_integer()?;
        let private_exponent = key.read_unsigned_integer()?;
        for _ in 0..PKCS1_CRT_INTEGERS {
            key.read_unsigned_integer()?;
        }
        if version == PKCS1_MULTI_PRIME_VERSION {
            key.read_sequence()?;
        }
        key.finish()?;
        RsaPrivateKey::new(modulus, public_exponent, private_exponent)
            .map(Self::Rsa)
            .map_err(|_| PrivateKeyError::InvalidKey)
    }

    /// The matching public key.
    pub fn public_key(&self) -> SubjectPublicKey {
        match self {
            Self::P256(key) => SubjectPublicKey::P256(key.public_key()),
            Self::Rsa(key) => SubjectPublicKey::Rsa(key.public_key().clone()),
        }
    }

    /// The key as PKCS#8 DER, for P-256 keys; `None` for RSA keys, whose primes EggDB does not
    /// keep.
    pub fn to_pkcs8_der(&self) -> Option<Vec<u8>> {
        match self {
            Self::P256(key) => Some(p256_private_key_to_pkcs8_der(key)),
            Self::Rsa(_) => None,
        }
    }

    /// The key as a PEM `PRIVATE KEY` block, for P-256 keys.
    pub fn to_pkcs8_pem(&self) -> Option<String> {
        self.to_pkcs8_der()
            .map(|der| pem_encode(PRIVATE_KEY_LABEL, &der))
    }

    /// Signs `message` with `algorithm`. ECDSA signatures come out in DER form and need no
    /// randomness (RFC 6979); RSA-PSS takes its salt from `fill_random`.
    pub fn sign(
        &self,
        algorithm: SignatureAlgorithm,
        message: &[u8],
        fill_random: FillRandom<'_>,
    ) -> Result<Vec<u8>, SigningError> {
        match (algorithm, self) {
            (SignatureAlgorithm::Unsupported, _) => Err(SigningError::UnsupportedAlgorithm),
            (SignatureAlgorithm::Ecdsa(hash), Self::P256(key)) => {
                Ok(ecdsa_signature_to_der(&sign_ecdsa(hash, key, message)))
            }
            (SignatureAlgorithm::RsaPkcs1(hash), Self::Rsa(key)) => {
                sign_rsa_pkcs1(hash, key, message).map_err(SigningError::Rsa)
            }
            (SignatureAlgorithm::RsaPss { hash, salt_length }, Self::Rsa(key)) => {
                if salt_length > key.public_key().signature_bytes() {
                    return Err(SigningError::Rsa(RsaError::ModulusTooSmallForEncoding));
                }
                let mut salt = vec![0u8; salt_length];
                fill_random(&mut salt).map_err(|error| SigningError::Random(error.kind()))?;
                sign_rsa_pss(hash, key, message, &salt).map_err(SigningError::Rsa)
            }
            _ => Err(SigningError::KeyMismatch),
        }
    }
}

/// A new random P-256 private key from `fill_random`.
pub fn generate_p256_private_key(fill_random: FillRandom<'_>) -> std::io::Result<P256PrivateKey> {
    for _ in 0..P256_KEY_GENERATION_ATTEMPTS {
        let mut scalar = [0u8; P256_ELEMENT_BYTES];
        fill_random(&mut scalar)?;
        if let Ok(key) = P256PrivateKey::from_bytes(&scalar) {
            return Ok(key);
        }
    }
    Err(std::io::Error::other(
        "the random source gave no valid P-256 private key",
    ))
}

fn sign_ecdsa(
    hash: SignatureHash,
    key: &P256PrivateKey,
    message: &[u8],
) -> eggdb_crypto::EcdsaP256Signature {
    match hash {
        SignatureHash::Sha1 => ecdsa_p256_sign::<Sha1>(key, &sha1(message)),
        SignatureHash::Sha256 => ecdsa_p256_sign::<Sha256>(key, &sha256(message)),
        SignatureHash::Sha384 => ecdsa_p256_sign::<Sha384>(key, &sha384(message)),
        SignatureHash::Sha512 => ecdsa_p256_sign::<Sha512>(key, &sha512(message)),
    }
}

fn sign_rsa_pkcs1(
    hash: SignatureHash,
    key: &RsaPrivateKey,
    message: &[u8],
) -> Result<Vec<u8>, RsaError> {
    let digest = hash.digest(message);
    match hash {
        SignatureHash::Sha1 => rsa_pkcs1v15_sign::<Sha1>(key, &digest),
        SignatureHash::Sha256 => rsa_pkcs1v15_sign::<Sha256>(key, &digest),
        SignatureHash::Sha384 => rsa_pkcs1v15_sign::<Sha384>(key, &digest),
        SignatureHash::Sha512 => rsa_pkcs1v15_sign::<Sha512>(key, &digest),
    }
}

fn sign_rsa_pss(
    hash: SignatureHash,
    key: &RsaPrivateKey,
    message: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, RsaError> {
    let digest = hash.digest(message);
    match hash {
        SignatureHash::Sha1 => rsa_pss_sign::<Sha1>(key, &digest, salt),
        SignatureHash::Sha256 => rsa_pss_sign::<Sha256>(key, &digest, salt),
        SignatureHash::Sha384 => rsa_pss_sign::<Sha384>(key, &digest, salt),
        SignatureHash::Sha512 => rsa_pss_sign::<Sha512>(key, &digest, salt),
    }
}

fn parse_ec_private_key(der: &[u8]) -> Result<P256PrivateKey, PrivateKeyError> {
    let mut outer = DerReader::new(der);
    let mut key = outer.read_sequence()?;
    outer.finish()?;
    if key.read_small_unsigned_integer()? != SEC1_VERSION {
        return Err(PrivateKeyError::Version);
    }
    let scalar_bytes = key.read_octet_string()?;
    if let Some(parameters) = key.read_optional(tag::context_constructed(0))? {
        let mut curve = DerReader::new(parameters);
        if curve.read_object_identifier()? != oid::PRIME256V1 {
            return Err(PrivateKeyError::UnsupportedCurve);
        }
        curve.finish()?;
    }
    let stated_public_key = key.read_optional(tag::context_constructed(1))?;
    key.finish()?;
    let start = P256_ELEMENT_BYTES
        .checked_sub(scalar_bytes.len())
        .ok_or(PrivateKeyError::InvalidKey)?;
    let mut scalar = [0u8; P256_ELEMENT_BYTES];
    scalar[start..].copy_from_slice(scalar_bytes);
    let private_key =
        P256PrivateKey::from_bytes(&scalar).map_err(|_| PrivateKeyError::InvalidKey)?;
    if let Some(explicit) = stated_public_key {
        let mut reader = DerReader::new(explicit);
        let point = reader.read_bit_string()?;
        reader.finish()?;
        let stated = P256PublicKey::from_sec1(point).map_err(|_| PrivateKeyError::InvalidKey)?;
        if stated != private_key.public_key() {
            return Err(PrivateKeyError::PublicKeyMismatch);
        }
    }
    Ok(private_key)
}

fn p256_private_key_to_pkcs8_der(key: &P256PrivateKey) -> Vec<u8> {
    let mut ec_private_key = DerWriter::new();
    ec_private_key.write_sequence(|sequence| {
        sequence.write_small_unsigned_integer(SEC1_VERSION);
        sequence.write_octet_string(&key.to_bytes());
        sequence.write_constructed(tag::context_constructed(1), |public_key| {
            public_key.write_bit_string(&key.public_key().to_sec1_uncompressed());
        });
    });
    let ec_private_key = ec_private_key.into_bytes();
    let mut writer = DerWriter::new();
    writer.write_sequence(|info| {
        info.write_small_unsigned_integer(0);
        info.write_sequence(|algorithm| {
            algorithm.write_object_identifier(oid::EC_PUBLIC_KEY);
            algorithm.write_object_identifier(oid::PRIME256V1);
        });
        info.write_octet_string(&ec_private_key);
    });
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_random(bytes: &mut [u8]) -> std::io::Result<()> {
        bytes.iter_mut().for_each(|byte| *byte = 0x5a);
        Ok(())
    }

    #[test]
    fn generated_p256_keys_round_trip_through_pkcs8() {
        let key = generate_p256_private_key(&mut fixed_random).unwrap();
        let private_key = PrivateKey::P256(key.clone());
        let pem = private_key.to_pkcs8_pem().unwrap();
        let PrivateKey::P256(read_back) = PrivateKey::from_pem(&pem).unwrap() else {
            panic!("a P-256 key was read back as another type");
        };
        assert_eq!(read_back.to_bytes(), key.to_bytes());
    }

    #[test]
    fn a_random_source_of_zeros_gives_no_key() {
        fn zeros(bytes: &mut [u8]) -> std::io::Result<()> {
            bytes.fill(0);
            Ok(())
        }
        assert!(generate_p256_private_key(&mut zeros).is_err());
    }

    #[test]
    fn a_stated_public_key_must_match() {
        let key = generate_p256_private_key(&mut fixed_random).unwrap();
        let other = P256PrivateKey::from_bytes(&[0x11; P256_ELEMENT_BYTES]).unwrap();
        let mut writer = DerWriter::new();
        writer.write_sequence(|sequence| {
            sequence.write_small_unsigned_integer(SEC1_VERSION);
            sequence.write_octet_string(&key.to_bytes());
            sequence.write_constructed(tag::context_constructed(1), |public_key| {
                public_key.write_bit_string(&other.public_key().to_sec1_uncompressed());
            });
        });
        assert_eq!(
            PrivateKey::from_sec1_der(&writer.into_bytes()).err(),
            Some(PrivateKeyError::PublicKeyMismatch)
        );
    }

    #[test]
    fn encrypted_and_missing_keys_are_reported() {
        let encrypted = pem_encode(ENCRYPTED_PRIVATE_KEY_LABEL, &[0x30, 0x00]);
        assert_eq!(
            PrivateKey::from_pem(&encrypted).err(),
            Some(PrivateKeyError::Encrypted)
        );
        let certificate_only = pem_encode("CERTIFICATE", &[0x30, 0x00]);
        assert_eq!(
            PrivateKey::from_pem(&certificate_only).err(),
            Some(PrivateKeyError::NoPrivateKey)
        );
    }

    #[test]
    fn ecdsa_signing_needs_a_p256_key() {
        let key = PrivateKey::P256(generate_p256_private_key(&mut fixed_random).unwrap());
        assert_eq!(
            key.sign(
                SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256),
                b"message",
                &mut fixed_random
            ),
            Err(SigningError::KeyMismatch)
        );
        assert_eq!(
            key.sign(
                SignatureAlgorithm::Unsupported,
                b"message",
                &mut fixed_random
            ),
            Err(SigningError::UnsupportedAlgorithm)
        );
    }
}
