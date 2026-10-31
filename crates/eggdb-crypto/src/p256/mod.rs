mod arithmetic;
mod point;

use crate::hash::HashFunction;
use crate::hmac::Hmac;
use arithmetic::{FieldElement, Scalar};
use point::{ProjectivePoint, curve_right_side, is_on_curve};

/// The length in bytes of a P-256 scalar (a private key, or either half of a signature) and of a
/// coordinate (a shared secret).
pub const P256_ELEMENT_BYTES: usize = 32;
/// The length in bytes of a public key in the uncompressed SEC 1 form: `0x04`, then x, then y.
pub const P256_UNCOMPRESSED_POINT_BYTES: usize = 1 + 2 * P256_ELEMENT_BYTES;
/// The length in bytes of a public key in the compressed SEC 1 form: `0x02` or `0x03`, then x.
pub const P256_COMPRESSED_POINT_BYTES: usize = 1 + P256_ELEMENT_BYTES;

const UNCOMPRESSED_TAG: u8 = 0x04;
const COMPRESSED_EVEN_TAG: u8 = 0x02;
const COMPRESSED_ODD_TAG: u8 = 0x03;

/// Why a P-256 key was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P256Error {
    /// The private key is zero or not below the group order n.
    PrivateKeyOutOfRange,
    /// The public key is not a SEC 1 point encoding of the right length and tag, or a coordinate
    /// is not below the field prime.
    PublicKeyEncoding,
    /// The public key's coordinates are not a point of the curve.
    PublicKeyNotOnCurve,
    /// The product was the point at infinity, which a valid key pair never gives.
    PointAtInfinity,
}

impl std::fmt::Display for P256Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PrivateKeyOutOfRange => "the P-256 private key is zero or not below the order",
            Self::PublicKeyEncoding => "the P-256 public key is not a valid SEC 1 point encoding",
            Self::PublicKeyNotOnCurve => "the P-256 public key is not a point of the curve",
            Self::PointAtInfinity => "the P-256 result is the point at infinity",
        })
    }
}

impl std::error::Error for P256Error {}

/// A P-256 public key: a point of the curve other than the point at infinity, checked when it is
/// read.
#[derive(Clone, Copy)]
pub struct P256PublicKey {
    x: FieldElement,
    y: FieldElement,
}

impl P256PublicKey {
    /// Reads a public key in the SEC 1 form (section 2.3.4): uncompressed (65 bytes, `0x04`) or
    /// compressed (33 bytes, `0x02` or `0x03`). The encoding of the point at infinity, a
    /// coordinate not below the prime, and a point off the curve are all refused.
    pub fn from_sec1(bytes: &[u8]) -> Result<Self, P256Error> {
        match bytes.split_first() {
            Some((&UNCOMPRESSED_TAG, coordinates))
                if coordinates.len() == 2 * P256_ELEMENT_BYTES =>
            {
                let (x, y) = coordinates.split_at(P256_ELEMENT_BYTES);
                let x = read_coordinate(x)?;
                let y = read_coordinate(y)?;
                if !is_on_curve(&x, &y) {
                    return Err(P256Error::PublicKeyNotOnCurve);
                }
                Ok(Self { x, y })
            }
            Some((&tag, x))
                if (tag == COMPRESSED_EVEN_TAG || tag == COMPRESSED_ODD_TAG)
                    && x.len() == P256_ELEMENT_BYTES =>
            {
                let x = read_coordinate(x)?;
                let root = curve_right_side(&x)
                    .square_root()
                    .ok_or(P256Error::PublicKeyNotOnCurve)?;
                let wants_odd = tag == COMPRESSED_ODD_TAG;
                let y = if root.is_odd() == wants_odd {
                    root
                } else {
                    root.negate()
                };
                Ok(Self { x, y })
            }
            _ => Err(P256Error::PublicKeyEncoding),
        }
    }

    /// The uncompressed SEC 1 form: `0x04`, then x and y, each 32 bytes big-endian. This is the
    /// form TLS key shares and X.509 certificates carry.
    pub fn to_sec1_uncompressed(&self) -> [u8; P256_UNCOMPRESSED_POINT_BYTES] {
        let mut bytes = [0u8; P256_UNCOMPRESSED_POINT_BYTES];
        bytes[0] = UNCOMPRESSED_TAG;
        bytes[1..1 + P256_ELEMENT_BYTES].copy_from_slice(&self.x.to_be_bytes());
        bytes[1 + P256_ELEMENT_BYTES..].copy_from_slice(&self.y.to_be_bytes());
        bytes
    }

    fn point(&self) -> ProjectivePoint {
        ProjectivePoint::from_affine(self.x, self.y)
    }
}

fn read_coordinate(bytes: &[u8]) -> Result<FieldElement, P256Error> {
    let bytes: &[u8; P256_ELEMENT_BYTES] = bytes
        .try_into()
        .map_err(|_| P256Error::PublicKeyEncoding)?;
    FieldElement::from_be_bytes(bytes).ok_or(P256Error::PublicKeyEncoding)
}

impl PartialEq for P256PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.x.equals(&other.x) && self.y.equals(&other.y)
    }
}

impl Eq for P256PublicKey {}

impl std::fmt::Debug for P256PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("P256PublicKey(")?;
        for byte in self.to_sec1_uncompressed() {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

/// A P-256 private key: a scalar from 1 to n - 1, with its public key computed once when it is
/// read. Its `Debug` output never shows the scalar.
#[derive(Clone)]
pub struct P256PrivateKey {
    scalar: Scalar,
    public_key: P256PublicKey,
}

impl P256PrivateKey {
    /// Reads a private key from its 32 big-endian bytes (SEC 1 section 2.3.7), refusing zero and
    /// any value not below the group order n. To make a new key, fill 32 bytes from a secure
    /// random source and call this again on the rare refusal.
    pub fn from_bytes(bytes: &[u8; P256_ELEMENT_BYTES]) -> Result<Self, P256Error> {
        let scalar = Scalar::from_be_bytes(bytes)
            .filter(|scalar| !scalar.is_zero())
            .ok_or(P256Error::PrivateKeyOutOfRange)?;
        let (x, y) = ProjectivePoint::generator()
            .multiply(bytes)
            .to_affine()
            .ok_or(P256Error::PointAtInfinity)?;
        Ok(Self {
            scalar,
            public_key: P256PublicKey { x, y },
        })
    }

    /// The private key's 32 big-endian bytes, for storing it (PKCS#8, SEC 1).
    pub fn to_bytes(&self) -> [u8; P256_ELEMENT_BYTES] {
        self.scalar.to_be_bytes()
    }

    /// The public key: the private scalar times the generator.
    pub fn public_key(&self) -> P256PublicKey {
        self.public_key
    }
}

impl std::fmt::Debug for P256PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P256PrivateKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// The ECDH shared secret of `private_key` and the peer's `public_key` (SEC 1 section 3.3.1):
/// the x coordinate of their product, 32 bytes big-endian, as TLS uses it. The multiplication
/// does the same steps and memory accesses whatever the private key.
pub fn p256_shared_secret(
    private_key: &P256PrivateKey,
    public_key: &P256PublicKey,
) -> Result<[u8; P256_ELEMENT_BYTES], P256Error> {
    public_key
        .point()
        .multiply(&private_key.scalar.to_be_bytes())
        .to_affine()
        .map(|(x, _)| x.to_be_bytes())
        .ok_or(P256Error::PointAtInfinity)
}

/// An ECDSA signature over P-256: the two scalars r and s, each 32 bytes big-endian. TLS and
/// X.509 carry it DER-encoded, WebAuthn and JOSE as `r || s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcdsaP256Signature {
    /// The x coordinate of the nonce point, reduced modulo n.
    pub r: [u8; P256_ELEMENT_BYTES],
    /// The proof that the signer knew the private key.
    pub s: [u8; P256_ELEMENT_BYTES],
}

/// Signs `digest`, the hash of the message, with ECDSA over P-256 (FIPS 186-5 section 6.4.1),
/// taking the nonce deterministically from the key and the digest by HMAC-DRBG over `H`
/// (RFC 6979 section 3.2), so no random source is needed and the same input always gives the
/// same signature. `H` should be the hash that made `digest`. Every step that touches the
/// private key or the nonce takes the same time whatever their values.
pub fn ecdsa_p256_sign<H: HashFunction>(
    private_key: &P256PrivateKey,
    digest: &[u8],
) -> EcdsaP256Signature {
    let message = Scalar::from_digest(digest);
    let mut nonces = Rfc6979Nonces::<H>::new(&private_key.to_bytes(), &message.to_be_bytes());
    loop {
        let candidate = nonces.next_candidate();
        let signature = Scalar::from_be_bytes(&candidate)
            .filter(|nonce| !nonce.is_zero())
            .and_then(|nonce| sign_with_nonce(&private_key.scalar, &message, &nonce));
        if let Some(signature) = signature {
            return signature;
        }
        nonces.reject_candidate();
    }
}

fn sign_with_nonce(
    private_scalar: &Scalar,
    message: &Scalar,
    nonce: &Scalar,
) -> Option<EcdsaP256Signature> {
    let (x, _) = ProjectivePoint::generator()
        .multiply(&nonce.to_be_bytes())
        .to_affine()?;
    let r = Scalar::from_be_bytes_reduced(&x.to_be_bytes());
    if r.is_zero() {
        return None;
    }
    let s = nonce
        .invert()
        .multiply(&message.add(&r.multiply(private_scalar)));
    if s.is_zero() {
        return None;
    }
    Some(EcdsaP256Signature {
        r: r.to_be_bytes(),
        s: s.to_be_bytes(),
    })
}

/// Checks an ECDSA P-256 `signature` of `digest` by `public_key` (FIPS 186-5 section 6.4.2).
/// An r or s that is zero or not below n is refused.
pub fn ecdsa_p256_verify(
    public_key: &P256PublicKey,
    digest: &[u8],
    signature: &EcdsaP256Signature,
) -> bool {
    let (Some(r), Some(s)) = (
        Scalar::from_be_bytes(&signature.r),
        Scalar::from_be_bytes(&signature.s),
    ) else {
        return false;
    };
    if r.is_zero() || s.is_zero() {
        return false;
    }
    let message = Scalar::from_digest(digest);
    let s_inverse = s.invert();
    let generator_part =
        ProjectivePoint::generator().multiply(&message.multiply(&s_inverse).to_be_bytes());
    let key_part = public_key
        .point()
        .multiply(&r.multiply(&s_inverse).to_be_bytes());
    let Some((x, _)) = generator_part.add(&key_part).to_affine() else {
        return false;
    };
    Scalar::from_be_bytes_reduced(&x.to_be_bytes()).equals(&r)
}

struct Rfc6979Nonces<H: HashFunction> {
    key: H::Digest,
    value: H::Digest,
}

impl<H: HashFunction> Rfc6979Nonces<H> {
    fn new(private_key: &[u8; P256_ELEMENT_BYTES], message: &[u8; P256_ELEMENT_BYTES]) -> Self {
        let mut value = H::digest(&[]);
        value.as_mut().fill(0x01);
        let mut key = value;
        key.as_mut().fill(0x00);
        let mut nonces = Self { key, value };
        for separator in [0x00u8, 0x01] {
            nonces.key = nonces.mac(&[
                nonces.value.as_ref(),
                &[separator][..],
                &private_key[..],
                &message[..],
            ]);
            nonces.value = nonces.mac(&[nonces.value.as_ref()]);
        }
        nonces
    }

    fn mac(&self, parts: &[&[u8]]) -> H::Digest {
        let mut mac = Hmac::<H>::new(self.key.as_ref());
        for part in parts {
            mac.update(part);
        }
        mac.finish()
    }

    fn next_candidate(&mut self) -> [u8; P256_ELEMENT_BYTES] {
        let mut candidate = [0u8; P256_ELEMENT_BYTES];
        let mut filled = 0;
        while filled < P256_ELEMENT_BYTES {
            self.value = self.mac(&[self.value.as_ref()]);
            let block = self.value.as_ref();
            let taken = (P256_ELEMENT_BYTES - filled).min(block.len());
            if taken == 0 {
                break;
            }
            candidate[filled..filled + taken].copy_from_slice(&block[..taken]);
            filled += taken;
        }
        candidate
    }

    fn reject_candidate(&mut self) {
        self.key = self.mac(&[self.value.as_ref(), &[0x00u8][..]]);
        self.value = self.mac(&[self.value.as_ref()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::Sha256;

    fn scalar_bytes(value: u8) -> [u8; P256_ELEMENT_BYTES] {
        let mut bytes = [0u8; P256_ELEMENT_BYTES];
        bytes[P256_ELEMENT_BYTES - 1] = value;
        bytes
    }

    #[test]
    fn one_times_the_generator_is_the_generator() {
        let key = P256PrivateKey::from_bytes(&scalar_bytes(1)).unwrap();
        let encoded = key.public_key().to_sec1_uncompressed();
        assert_eq!(encoded[0], UNCOMPRESSED_TAG);
        assert_eq!(encoded[1..33], point::GENERATOR_X);
        assert_eq!(encoded[33..], point::GENERATOR_Y);
    }

    #[test]
    fn zero_and_the_order_are_not_private_keys() {
        assert_eq!(
            P256PrivateKey::from_bytes(&[0; 32]).unwrap_err(),
            P256Error::PrivateKeyOutOfRange
        );
        assert_eq!(
            P256PrivateKey::from_bytes(&[0xff; 32]).unwrap_err(),
            P256Error::PrivateKeyOutOfRange
        );
    }

    #[test]
    fn a_compressed_key_reads_as_its_uncompressed_form() {
        let public = P256PrivateKey::from_bytes(&scalar_bytes(7))
            .unwrap()
            .public_key();
        let uncompressed = public.to_sec1_uncompressed();
        let mut compressed = [0u8; P256_COMPRESSED_POINT_BYTES];
        compressed[0] = COMPRESSED_EVEN_TAG + (uncompressed[64] & 1);
        compressed[1..].copy_from_slice(&uncompressed[1..33]);
        assert_eq!(P256PublicKey::from_sec1(&compressed).unwrap(), public);
        assert_eq!(P256PublicKey::from_sec1(&uncompressed).unwrap(), public);
    }

    #[test]
    fn hostile_public_key_encodings_are_refused_without_panicking() {
        let valid = P256PrivateKey::from_bytes(&scalar_bytes(3))
            .unwrap()
            .public_key()
            .to_sec1_uncompressed();
        for length in 0..valid.len() {
            assert!(P256PublicKey::from_sec1(&valid[..length]).is_err());
        }
        for tag in [0x00u8, 0x01, 0x05, 0x06, 0x07, 0xff] {
            let mut bytes = valid;
            bytes[0] = tag;
            assert!(P256PublicKey::from_sec1(&bytes).is_err());
        }
        let mut off_curve = valid;
        off_curve[64] ^= 1;
        assert_eq!(
            P256PublicKey::from_sec1(&off_curve).unwrap_err(),
            P256Error::PublicKeyNotOnCurve
        );
        let mut too_big = valid;
        too_big[1..33].fill(0xff);
        assert_eq!(
            P256PublicKey::from_sec1(&too_big).unwrap_err(),
            P256Error::PublicKeyEncoding
        );
    }

    #[test]
    fn both_sides_of_an_exchange_agree() {
        let alice = P256PrivateKey::from_bytes(&scalar_bytes(11)).unwrap();
        let bob = P256PrivateKey::from_bytes(&scalar_bytes(29)).unwrap();
        assert_eq!(
            p256_shared_secret(&alice, &bob.public_key()).unwrap(),
            p256_shared_secret(&bob, &alice.public_key()).unwrap()
        );
    }

    #[test]
    fn a_signature_verifies_and_a_changed_one_does_not() {
        let key = P256PrivateKey::from_bytes(&scalar_bytes(42)).unwrap();
        let digest = crate::sha256::sha256(b"EggDB");
        let signature = ecdsa_p256_sign::<Sha256>(&key, &digest);
        assert!(ecdsa_p256_verify(&key.public_key(), &digest, &signature));
        let mut changed = signature;
        changed.s[31] ^= 1;
        assert!(!ecdsa_p256_verify(&key.public_key(), &digest, &changed));
        let other = P256PrivateKey::from_bytes(&scalar_bytes(43)).unwrap();
        assert!(!ecdsa_p256_verify(&other.public_key(), &digest, &signature));
    }
}
