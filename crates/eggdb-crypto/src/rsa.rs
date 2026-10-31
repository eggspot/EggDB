use crate::bignum::{MontgomeryModulus, limbs_from_be_bytes, limbs_to_be_bytes};
use crate::constant_time::constant_time_eq;
use crate::hash::HashFunction;
use crate::sha1::Sha1;
use crate::sha256::Sha256;
use crate::sha512::{Sha384, Sha512};

/// The smallest RSA modulus accepted for verifying, in bits: anything smaller is broken.
pub const RSA_MIN_PUBLIC_MODULUS_BITS: usize = 1024;
/// The smallest RSA modulus accepted for signing, in bits (NIST SP 800-131A).
pub const RSA_MIN_PRIVATE_MODULUS_BITS: usize = 2048;
/// The largest RSA modulus accepted, in bits, so a hostile key cannot make one verification cost
/// seconds.
pub const RSA_MAX_MODULUS_BITS: usize = 8192;
/// The largest public exponent accepted, in bits, for the same reason; real keys use 65537.
pub const RSA_MAX_PUBLIC_EXPONENT_BITS: usize = 33;

const PKCS1_MINIMUM_PADDING_BYTES: usize = 8;
const PSS_ZERO_PREFIX: [u8; 8] = [0; 8];
const PSS_TRAILER: u8 = 0xbc;

/// Why an RSA key or operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsaError {
    /// The modulus is even, or its size is outside the accepted range.
    Modulus,
    /// The public exponent is even, below 3, too long, or not below the modulus.
    PublicExponent,
    /// The private exponent is zero, not below the modulus, or does not undo the public exponent.
    PrivateExponent,
    /// The digest is not as long as the hash function's output.
    DigestLength,
    /// The modulus is too small for this hash (and salt).
    ModulusTooSmallForEncoding,
}

impl std::fmt::Display for RsaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Modulus => "the RSA modulus is even or of an unsupported size",
            Self::PublicExponent => "the RSA public exponent is not valid",
            Self::PrivateExponent => "the RSA private exponent does not match the public key",
            Self::DigestLength => "the digest length does not match the hash function",
            Self::ModulusTooSmallForEncoding => "the RSA modulus is too small for this encoding",
        })
    }
}

impl std::error::Error for RsaError {}

/// A hash function that RSASSA-PKCS1-v1_5 can sign with: the DER `DigestInfo` prefix that names
/// it (RFC 8017 section 9.2, note 1).
pub trait RsaPkcs1Hash: HashFunction {
    /// The DER encoding of `DigestInfo` up to the digest itself.
    const DIGEST_INFO_PREFIX: &'static [u8];
}

impl RsaPkcs1Hash for Sha1 {
    const DIGEST_INFO_PREFIX: &'static [u8] = &[
        0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
    ];
}

impl RsaPkcs1Hash for Sha256 {
    const DIGEST_INFO_PREFIX: &'static [u8] = &[
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];
}

impl RsaPkcs1Hash for Sha384 {
    const DIGEST_INFO_PREFIX: &'static [u8] = &[
        0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02,
        0x05, 0x00, 0x04, 0x30,
    ];
}

impl RsaPkcs1Hash for Sha512 {
    const DIGEST_INFO_PREFIX: &'static [u8] = &[
        0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03,
        0x05, 0x00, 0x04, 0x40,
    ];
}

/// An RSA public key: the modulus n and the public exponent e (RFC 8017 section 3.1).
#[derive(Clone)]
pub struct RsaPublicKey {
    modulus: MontgomeryModulus,
    exponent: Vec<u64>,
}

impl RsaPublicKey {
    /// Reads a public key from the big-endian modulus and public exponent, as an X.509
    /// certificate's `RSAPublicKey` carries them (leading zero bytes allowed). The modulus must be
    /// odd and from [`RSA_MIN_PUBLIC_MODULUS_BITS`] to [`RSA_MAX_MODULUS_BITS`] bits; the
    /// exponent odd, at least 3, and at most [`RSA_MAX_PUBLIC_EXPONENT_BITS`] bits.
    pub fn new(modulus: &[u8], public_exponent: &[u8]) -> Result<Self, RsaError> {
        let modulus = MontgomeryModulus::new(modulus).ok_or(RsaError::Modulus)?;
        if !(RSA_MIN_PUBLIC_MODULUS_BITS..=RSA_MAX_MODULUS_BITS).contains(&modulus.bits()) {
            return Err(RsaError::Modulus);
        }
        let exponent =
            limbs_from_be_bytes(public_exponent, 1).ok_or(RsaError::PublicExponent)?;
        let value = exponent.first().copied().unwrap_or(0);
        if value & 1 == 0 || value < 3 || value >> RSA_MAX_PUBLIC_EXPONENT_BITS != 0 {
            return Err(RsaError::PublicExponent);
        }
        Ok(Self { modulus, exponent })
    }

    /// The modulus n, big-endian without leading zero bytes.
    pub fn modulus(&self) -> Vec<u8> {
        self.modulus.to_be_bytes()
    }

    /// The public exponent e, big-endian without leading zero bytes.
    pub fn public_exponent(&self) -> Vec<u8> {
        let bytes = limbs_to_be_bytes(&self.exponent, 8);
        let first_nonzero = bytes.iter().position(|&byte| byte != 0).unwrap_or(0);
        bytes[first_nonzero..].to_vec()
    }

    /// The size of the modulus in bits.
    pub fn modulus_bits(&self) -> usize {
        self.modulus.bits()
    }

    /// The length of every signature by this key in bytes: the modulus length k.
    pub fn signature_bytes(&self) -> usize {
        self.modulus.byte_length()
    }

    fn encoded_message(&self, signature: &[u8]) -> Option<Vec<u8>> {
        if signature.len() != self.signature_bytes() {
            return None;
        }
        let value = self.modulus.read_below(signature)?;
        let power = self
            .modulus
            .power_public(&self.modulus.to_montgomery(&value), &self.exponent);
        Some(limbs_to_be_bytes(
            &self.modulus.from_montgomery(&power),
            self.signature_bytes(),
        ))
    }
}

impl PartialEq for RsaPublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.modulus() == other.modulus() && self.exponent == other.exponent
    }
}

impl Eq for RsaPublicKey {}

impl std::fmt::Debug for RsaPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaPublicKey")
            .field("modulus_bits", &self.modulus_bits())
            .field("public_exponent", &self.exponent.first().copied().unwrap_or(0))
            .finish()
    }
}

/// An RSA private key: the public key and the private exponent d. Signing exponentiates by d
/// with the same steps and memory accesses whatever its value. Its `Debug` output never shows d.
#[derive(Clone)]
pub struct RsaPrivateKey {
    public_key: RsaPublicKey,
    private_exponent: Vec<u64>,
}

impl RsaPrivateKey {
    /// Reads a private key from its big-endian modulus, public exponent and private exponent
    /// (the first three integers of PKCS#1's `RSAPrivateKey`). The modulus must be at least
    /// [`RSA_MIN_PRIVATE_MODULUS_BITS`] bits, and the key is checked by raising a value to d and
    /// then e, which must give the value back.
    pub fn new(
        modulus: &[u8],
        public_exponent: &[u8],
        private_exponent: &[u8],
    ) -> Result<Self, RsaError> {
        let public_key = RsaPublicKey::new(modulus, public_exponent)?;
        if public_key.modulus_bits() < RSA_MIN_PRIVATE_MODULUS_BITS {
            return Err(RsaError::Modulus);
        }
        let private_exponent = public_key
            .modulus
            .read_below(private_exponent)
            .filter(|limbs| limbs.iter().any(|&limb| limb != 0))
            .ok_or(RsaError::PrivateExponent)?;
        let key = Self {
            public_key,
            private_exponent,
        };
        let probe = [2u8];
        let signed = key
            .private_operation(&probe)
            .ok_or(RsaError::PrivateExponent)?;
        let recovered = key
            .public_key
            .encoded_message(&signed)
            .ok_or(RsaError::PrivateExponent)?;
        if recovered.iter().rev().skip(1).any(|&byte| byte != 0) || recovered.last() != Some(&2) {
            return Err(RsaError::PrivateExponent);
        }
        Ok(key)
    }

    /// The public key.
    pub fn public_key(&self) -> &RsaPublicKey {
        &self.public_key
    }

    fn private_operation(&self, message: &[u8]) -> Option<Vec<u8>> {
        let modulus = &self.public_key.modulus;
        let value = modulus.read_below(message)?;
        let power = modulus.power_secret(&modulus.to_montgomery(&value), &self.private_exponent);
        Some(limbs_to_be_bytes(
            &modulus.from_montgomery(&power),
            self.public_key.signature_bytes(),
        ))
    }
}

impl std::fmt::Debug for RsaPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaPrivateKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

fn pkcs1v15_encode<H: RsaPkcs1Hash>(digest: &[u8], length: usize) -> Result<Vec<u8>, RsaError> {
    if digest.len() != H::DIGEST_BYTES {
        return Err(RsaError::DigestLength);
    }
    let digest_info_length = H::DIGEST_INFO_PREFIX.len() + digest.len();
    if length < digest_info_length + PKCS1_MINIMUM_PADDING_BYTES + 3 {
        return Err(RsaError::ModulusTooSmallForEncoding);
    }
    let mut encoded = vec![0xffu8; length];
    encoded[0] = 0x00;
    encoded[1] = 0x01;
    let digest_info_start = length - digest_info_length;
    encoded[digest_info_start - 1] = 0x00;
    encoded[digest_info_start..length - digest.len()].copy_from_slice(H::DIGEST_INFO_PREFIX);
    encoded[length - digest.len()..].copy_from_slice(digest);
    Ok(encoded)
}

/// Signs `digest` with RSASSA-PKCS1-v1_5 (RFC 8017 section 8.2.1), the RSA signature of TLS 1.2
/// and of most X.509 certificates. The signature is [`RsaPublicKey::signature_bytes`] long.
pub fn rsa_pkcs1v15_sign<H: RsaPkcs1Hash>(
    private_key: &RsaPrivateKey,
    digest: &[u8],
) -> Result<Vec<u8>, RsaError> {
    let encoded = pkcs1v15_encode::<H>(digest, private_key.public_key.signature_bytes())?;
    private_key
        .private_operation(&encoded)
        .ok_or(RsaError::ModulusTooSmallForEncoding)
}

/// Checks an RSASSA-PKCS1-v1_5 `signature` of `digest` (RFC 8017 section 8.2.2), by encoding the
/// digest again and comparing, so only the one valid encoding is accepted.
pub fn rsa_pkcs1v15_verify<H: RsaPkcs1Hash>(
    public_key: &RsaPublicKey,
    digest: &[u8],
    signature: &[u8],
) -> bool {
    let Ok(expected) = pkcs1v15_encode::<H>(digest, public_key.signature_bytes()) else {
        return false;
    };
    public_key
        .encoded_message(signature)
        .is_some_and(|encoded| constant_time_eq(&encoded, &expected))
}

fn mgf1_xor<H: HashFunction>(seed: &[u8], output: &mut [u8]) {
    for (counter, chunk) in (0u32..).zip(output.chunks_mut(H::DIGEST_BYTES.max(1))) {
        let mut hash = H::new();
        hash.update(seed);
        hash.update(&counter.to_be_bytes());
        let mask = hash.finish();
        for (byte, mask) in chunk.iter_mut().zip(mask.as_ref()) {
            *byte ^= mask;
        }
    }
}

fn pss_message_hash<H: HashFunction>(digest: &[u8], salt: &[u8]) -> H::Digest {
    let mut hash = H::new();
    hash.update(&PSS_ZERO_PREFIX);
    hash.update(digest);
    hash.update(salt);
    hash.finish()
}

fn unused_top_bits_mask(encoded_bits: usize, encoded_length: usize) -> u8 {
    0xffu8 >> (8 * encoded_length - encoded_bits)
}

fn pss_encode<H: HashFunction>(
    digest: &[u8],
    salt: &[u8],
    encoded_bits: usize,
) -> Result<Vec<u8>, RsaError> {
    if digest.len() != H::DIGEST_BYTES {
        return Err(RsaError::DigestLength);
    }
    let length = encoded_bits.div_ceil(8);
    if length < H::DIGEST_BYTES + salt.len() + 2 {
        return Err(RsaError::ModulusTooSmallForEncoding);
    }
    let message_hash = pss_message_hash::<H>(digest, salt);
    let data_block_length = length - H::DIGEST_BYTES - 1;
    let mut encoded = vec![0u8; length];
    let salt_start = data_block_length - salt.len();
    encoded[salt_start - 1] = 0x01;
    encoded[salt_start..data_block_length].copy_from_slice(salt);
    mgf1_xor::<H>(message_hash.as_ref(), &mut encoded[..data_block_length]);
    encoded[0] &= unused_top_bits_mask(encoded_bits, length);
    encoded[data_block_length..length - 1].copy_from_slice(message_hash.as_ref());
    encoded[length - 1] = PSS_TRAILER;
    Ok(encoded)
}

fn pss_encoding_matches<H: HashFunction>(
    digest: &[u8],
    encoded: &[u8],
    encoded_bits: usize,
    salt_length: usize,
) -> bool {
    let length = encoded.len();
    if digest.len() != H::DIGEST_BYTES || length < H::DIGEST_BYTES + salt_length + 2 {
        return false;
    }
    if encoded.last() != Some(&PSS_TRAILER) {
        return false;
    }
    let data_block_length = length - H::DIGEST_BYTES - 1;
    let (masked_data_block, rest) = encoded.split_at(data_block_length);
    let message_hash = &rest[..H::DIGEST_BYTES];
    let top_mask = unused_top_bits_mask(encoded_bits, length);
    if masked_data_block[0] & !top_mask != 0 {
        return false;
    }
    let mut data_block = masked_data_block.to_vec();
    mgf1_xor::<H>(message_hash, &mut data_block);
    data_block[0] &= top_mask;
    let padding_length = data_block_length - salt_length - 1;
    if data_block[..padding_length].iter().any(|&byte| byte != 0)
        || data_block[padding_length] != 0x01
    {
        return false;
    }
    let salt = &data_block[padding_length + 1..];
    constant_time_eq(
        pss_message_hash::<H>(digest, salt).as_ref(),
        message_hash,
    )
}

fn pss_encoded_bits(public_key: &RsaPublicKey) -> usize {
    public_key.modulus_bits() - 1
}

/// Signs `digest` with RSASSA-PSS (RFC 8017 section 8.1.1) using `H` for the message hash and
/// MGF1, and `salt` (TLS 1.3 wants random bytes as long as the digest; the caller supplies them,
/// so this crate needs no random source). The signature is [`RsaPublicKey::signature_bytes`]
/// long.
pub fn rsa_pss_sign<H: HashFunction>(
    private_key: &RsaPrivateKey,
    digest: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, RsaError> {
    let encoded = pss_encode::<H>(digest, salt, pss_encoded_bits(&private_key.public_key))?;
    private_key
        .private_operation(&encoded)
        .ok_or(RsaError::ModulusTooSmallForEncoding)
}

/// Checks an RSASSA-PSS `signature` of `digest` (RFC 8017 section 8.1.2) using `H` for the
/// message hash and MGF1, with a salt of exactly `salt_length` bytes.
pub fn rsa_pss_verify<H: HashFunction>(
    public_key: &RsaPublicKey,
    digest: &[u8],
    signature: &[u8],
    salt_length: usize,
) -> bool {
    let encoded_bits = pss_encoded_bits(public_key);
    let encoded_length = encoded_bits.div_ceil(8);
    let Some(message) = public_key.encoded_message(signature) else {
        return false;
    };
    let (high, encoded) = message.split_at(message.len() - encoded_length);
    high.iter().all(|&byte| byte == 0)
        && pss_encoding_matches::<H>(digest, encoded, encoded_bits, salt_length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::sha256;

    #[test]
    fn the_pkcs1_encoding_has_the_rfc_8017_layout() {
        let digest = sha256(b"abc");
        let encoded = pkcs1v15_encode::<Sha256>(&digest, 256).unwrap();
        assert_eq!(encoded[..2], [0x00, 0x01]);
        let separator = 256 - 32 - Sha256::DIGEST_INFO_PREFIX.len() - 1;
        assert!(encoded[2..separator].iter().all(|&byte| byte == 0xff));
        assert_eq!(encoded[separator], 0x00);
        assert_eq!(encoded[256 - 32..], digest);
        assert_eq!(
            pkcs1v15_encode::<Sha256>(&digest[..31], 256).unwrap_err(),
            RsaError::DigestLength
        );
        assert_eq!(
            pkcs1v15_encode::<Sha512>(&[0; 64], 64).unwrap_err(),
            RsaError::ModulusTooSmallForEncoding
        );
    }

    #[test]
    fn a_pss_encoding_is_accepted_and_a_changed_one_is_not() {
        let digest = sha256(b"abc");
        let salt = [7u8; 32];
        for encoded_bits in [2047usize, 2048, 2041] {
            let encoded = pss_encode::<Sha256>(&digest, &salt, encoded_bits).unwrap();
            assert!(pss_encoding_matches::<Sha256>(&digest, &encoded, encoded_bits, 32));
            assert!(!pss_encoding_matches::<Sha256>(&digest, &encoded, encoded_bits, 31));
            let mut changed = encoded.clone();
            changed[10] ^= 1;
            assert!(!pss_encoding_matches::<Sha256>(&digest, &changed, encoded_bits, 32));
        }
    }

    #[test]
    fn invalid_public_keys_are_refused() {
        let mut modulus = vec![0xffu8; 256];
        assert_eq!(
            RsaPublicKey::new(&modulus, &[0x01, 0x00, 0x01]).map(|key| key.modulus_bits()),
            Ok(2048)
        );
        assert_eq!(RsaPublicKey::new(&modulus, &[2]).unwrap_err(), RsaError::PublicExponent);
        assert_eq!(RsaPublicKey::new(&modulus, &[1]).unwrap_err(), RsaError::PublicExponent);
        assert_eq!(
            RsaPublicKey::new(&modulus, &[0x03, 0, 0, 0, 1]).unwrap_err(),
            RsaError::PublicExponent
        );
        assert_eq!(RsaPublicKey::new(&modulus[..64], &[3]).unwrap_err(), RsaError::Modulus);
        modulus[255] = 0xfe;
        assert_eq!(RsaPublicKey::new(&modulus, &[3]).unwrap_err(), RsaError::Modulus);
        assert_eq!(RsaPublicKey::new(&[], &[3]).unwrap_err(), RsaError::Modulus);
    }
}
