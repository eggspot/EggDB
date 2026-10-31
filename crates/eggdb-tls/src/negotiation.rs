//! The choices both sides of a handshake make from what the other offers: signature schemes,
//! key exchange groups and application protocols, and the ephemeral keys of the key exchange.

use crate::handshake::{named_group, signature_scheme};
use crate::private_key::{FillRandom, PrivateKey, generate_p256_private_key};
use crate::signature::{SignatureAlgorithm, SignatureHash, SubjectPublicKey};
use eggdb_crypto::{
    P256PublicKey, X25519_KEY_BYTES, p256_shared_secret, x25519_public_key, x25519_shared_secret,
};

/// The key exchange groups EggDB supports, in its order of preference.
pub const SUPPORTED_GROUPS: [u16; 2] = [named_group::X25519, named_group::SECP256R1];

/// The signature schemes a client offers and accepts, in its order of preference.
pub const SUPPORTED_SIGNATURE_SCHEMES: [u16; 7] = [
    signature_scheme::ECDSA_SECP256R1_SHA256,
    signature_scheme::RSA_PSS_RSAE_SHA256,
    signature_scheme::RSA_PSS_RSAE_SHA384,
    signature_scheme::RSA_PSS_RSAE_SHA512,
    signature_scheme::RSA_PKCS1_SHA256,
    signature_scheme::RSA_PKCS1_SHA384,
    signature_scheme::RSA_PKCS1_SHA512,
];

/// The signature algorithm of a TLS signature scheme. TLS 1.3 forbids PKCS#1 v1.5 signatures in
/// the handshake, so they map to an algorithm only for TLS 1.2.
pub fn scheme_algorithm(scheme: u16, tls13: bool) -> Option<SignatureAlgorithm> {
    let pss = |hash: SignatureHash| Some(SignatureAlgorithm::RsaPss { hash, salt_length: hash.digest_bytes() });
    match scheme {
        signature_scheme::ECDSA_SECP256R1_SHA256 => Some(SignatureAlgorithm::Ecdsa(SignatureHash::Sha256)),
        signature_scheme::RSA_PSS_RSAE_SHA256 => pss(SignatureHash::Sha256),
        signature_scheme::RSA_PSS_RSAE_SHA384 => pss(SignatureHash::Sha384),
        signature_scheme::RSA_PSS_RSAE_SHA512 => pss(SignatureHash::Sha512),
        signature_scheme::RSA_PKCS1_SHA256 if !tls13 => Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256)),
        signature_scheme::RSA_PKCS1_SHA384 if !tls13 => Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha384)),
        signature_scheme::RSA_PKCS1_SHA512 if !tls13 => Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha512)),
        _ => None,
    }
}

/// Whether `scheme` can sign or verify with a key of this kind.
pub fn scheme_fits_key(scheme: u16, key: &SubjectPublicKey) -> bool {
    match key {
        SubjectPublicKey::P256(_) => scheme == signature_scheme::ECDSA_SECP256R1_SHA256,
        SubjectPublicKey::Rsa(_) => matches!(
            scheme,
            signature_scheme::RSA_PSS_RSAE_SHA256
                | signature_scheme::RSA_PSS_RSAE_SHA384
                | signature_scheme::RSA_PSS_RSAE_SHA512
                | signature_scheme::RSA_PKCS1_SHA256
                | signature_scheme::RSA_PKCS1_SHA384
                | signature_scheme::RSA_PKCS1_SHA512
        ),
        SubjectPublicKey::Unsupported => false,
    }
}

/// The scheme a signer with `key` uses: the first of the peer's `offered` schemes the key can
/// produce. A TLS 1.2 peer that sent no `signature_algorithms` gets SHA-256 (RFC 5246 section
/// 7.4.1.4.1 would allow SHA-1, which EggDB never signs with).
pub fn choose_signature_scheme(key: &PrivateKey, offered: &[u16], tls13: bool) -> Option<u16> {
    let public_key = key.public_key();
    let usable = |scheme: &u16| scheme_fits_key(*scheme, &public_key) && scheme_algorithm(*scheme, tls13).is_some();
    if offered.is_empty() && !tls13 {
        return [signature_scheme::ECDSA_SECP256R1_SHA256, signature_scheme::RSA_PKCS1_SHA256]
            .into_iter()
            .find(usable);
    }
    offered.iter().copied().find(usable)
}

/// Both sides offered application protocols and none match, which ends the handshake with
/// `no_application_protocol` (RFC 7301).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoCommonApplicationProtocol;

/// The server's application protocol: its first protocol the client also offered, or `None`
/// when either side offers none.
pub fn choose_application_protocol(
    server_protocols: &[Vec<u8>],
    client_protocols: &[Vec<u8>],
) -> Result<Option<Vec<u8>>, NoCommonApplicationProtocol> {
    if server_protocols.is_empty() || client_protocols.is_empty() {
        return Ok(None);
    }
    server_protocols
        .iter()
        .find(|protocol| client_protocols.contains(protocol))
        .cloned()
        .map(Some)
        .ok_or(NoCommonApplicationProtocol)
}

/// An ephemeral key of the key exchange.
pub enum EphemeralKey {
    /// An X25519 private scalar.
    X25519([u8; X25519_KEY_BYTES]),
    /// A P-256 private key.
    P256(eggdb_crypto::P256PrivateKey),
}

impl std::fmt::Debug for EphemeralKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::X25519(_) => "EphemeralKey::X25519",
            Self::P256(_) => "EphemeralKey::P256",
        })
    }
}

impl EphemeralKey {
    /// A new key in `group`, or `None` for a group EggDB does not support.
    pub fn generate(group: u16, fill_random: FillRandom<'_>) -> std::io::Result<Option<Self>> {
        match group {
            named_group::X25519 => {
                let mut scalar = [0u8; X25519_KEY_BYTES];
                fill_random(&mut scalar)?;
                Ok(Some(Self::X25519(scalar)))
            }
            named_group::SECP256R1 => Ok(Some(Self::P256(generate_p256_private_key(fill_random)?))),
            _ => Ok(None),
        }
    }

    /// The group of this key.
    pub fn group(&self) -> u16 {
        match self {
            Self::X25519(_) => named_group::X25519,
            Self::P256(_) => named_group::SECP256R1,
        }
    }

    /// The public key as a key share carries it: 32 bytes for X25519, the uncompressed point for
    /// P-256.
    pub fn public_key(&self) -> Vec<u8> {
        match self {
            Self::X25519(scalar) => x25519_public_key(scalar).to_vec(),
            Self::P256(key) => key.public_key().to_sec1_uncompressed().to_vec(),
        }
    }

    /// The shared secret with the peer's public key, or `None` when that key is malformed, not
    /// on the curve, or of low order.
    pub fn shared_secret(&self, peer_public_key: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::X25519(scalar) => {
                let peer: &[u8; X25519_KEY_BYTES] = peer_public_key.try_into().ok()?;
                x25519_shared_secret(scalar, peer).ok().map(|secret| secret.to_vec())
            }
            Self::P256(key) => {
                if peer_public_key.first() != Some(&0x04) {
                    return None;
                }
                let peer = P256PublicKey::from_sec1(peer_public_key).ok()?;
                p256_shared_secret(key, &peer).ok().map(|secret| secret.to_vec())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_random(byte: u8) -> impl FnMut(&mut [u8]) -> std::io::Result<()> {
        move |bytes: &mut [u8]| {
            bytes.fill(byte);
            Ok(())
        }
    }

    #[test]
    fn both_groups_agree_on_a_shared_secret() {
        for group in SUPPORTED_GROUPS {
            let mut first_random = fixed_random(3);
            let mut second_random = fixed_random(5);
            let first = EphemeralKey::generate(group, &mut first_random).unwrap().unwrap();
            let second = EphemeralKey::generate(group, &mut second_random).unwrap().unwrap();
            assert_eq!(first.group(), group);
            let one = first.shared_secret(&second.public_key()).unwrap();
            let other = second.shared_secret(&first.public_key()).unwrap();
            assert_eq!(one, other);
        }
    }

    #[test]
    fn malformed_and_low_order_peer_keys_are_refused() {
        let mut random = fixed_random(9);
        let x25519 = EphemeralKey::generate(named_group::X25519, &mut random).unwrap().unwrap();
        assert!(x25519.shared_secret(&[0u8; 32]).is_none());
        assert!(x25519.shared_secret(&[1u8; 31]).is_none());
        let p256 = EphemeralKey::generate(named_group::SECP256R1, &mut random).unwrap().unwrap();
        assert!(p256.shared_secret(&[4u8; 65]).is_none());
        let compressed = {
            let mut point = p256.public_key();
            point.truncate(33);
            point[0] = 0x02;
            point
        };
        assert!(p256.shared_secret(&compressed).is_none());
        assert!(EphemeralKey::generate(0x0018, &mut random).unwrap().is_none());
    }

    #[test]
    fn application_protocols_follow_the_server_order() {
        let server = vec![b"postgresql".to_vec(), b"other".to_vec()];
        let client = vec![b"other".to_vec(), b"postgresql".to_vec()];
        assert_eq!(choose_application_protocol(&server, &client), Ok(Some(b"postgresql".to_vec())));
        assert_eq!(choose_application_protocol(&server, &[]), Ok(None));
        assert_eq!(choose_application_protocol(&server, &[b"h2".to_vec()]), Err(NoCommonApplicationProtocol));
    }

    #[test]
    fn tls13_never_uses_pkcs1_signatures() {
        assert!(scheme_algorithm(signature_scheme::RSA_PKCS1_SHA256, true).is_none());
        assert!(scheme_algorithm(signature_scheme::RSA_PKCS1_SHA256, false).is_some());
        assert_eq!(
            scheme_algorithm(signature_scheme::RSA_PSS_RSAE_SHA384, true),
            Some(SignatureAlgorithm::RsaPss { hash: SignatureHash::Sha384, salt_length: 48 })
        );
    }
}
