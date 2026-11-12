//! The choices both sides of a handshake make from what the other offers: signature schemes,
//! key exchange groups and application protocols, and the ephemeral keys of the key exchange.

use crate::handshake::{named_group, signature_scheme};
use crate::private_key::{FillRandom, PrivateKey, generate_p256_private_key};
use crate::signature::{SignatureAlgorithm, SignatureHash, SubjectPublicKey};
use eggdb_crypto::{
    MLKEM_KEY_GENERATION_SEED_BYTES, MLKEM768_CIPHERTEXT_BYTES, MLKEM768_ENCAPSULATION_KEY_BYTES,
    P256PublicKey, X25519_KEY_BYTES, mlkem768_decapsulate, mlkem768_encapsulate,
    mlkem768_key_pair, p256_shared_secret, x25519_public_key, x25519_shared_secret,
};

/// The key exchange groups EggDB supports in TLS 1.2, in its order of preference.
pub const SUPPORTED_GROUPS: [u16; 2] = [named_group::X25519, named_group::SECP256R1];

/// The key exchange groups EggDB supports in TLS 1.3, in its order of preference: the hybrid
/// post-quantum group first.
pub const TLS13_GROUPS: [u16; 3] = [
    named_group::X25519_MLKEM768,
    named_group::X25519,
    named_group::SECP256R1,
];

/// The TLS 1.3 groups of the FIPS-only mode: ML-KEM-768 is the approved part of the hybrid, and
/// P-256 is approved on its own.
pub const FIPS_TLS13_GROUPS: [u16; 2] = [named_group::X25519_MLKEM768, named_group::SECP256R1];

/// The TLS 1.2 groups of the FIPS-only mode.
pub const FIPS_TLS12_GROUPS: [u16; 1] = [named_group::SECP256R1];

/// The TLS 1.3 groups in order of preference, the FIPS-only ones when `fips_only` is set.
pub fn tls13_groups(fips_only: bool) -> &'static [u16] {
    if fips_only { &FIPS_TLS13_GROUPS } else { &TLS13_GROUPS }
}

/// The TLS 1.2 groups in order of preference, the FIPS-only ones when `fips_only` is set.
pub fn tls12_groups(fips_only: bool) -> &'static [u16] {
    if fips_only { &FIPS_TLS12_GROUPS } else { &SUPPORTED_GROUPS }
}

/// The bytes of a client's `X25519MLKEM768` key share: the ML-KEM-768 encapsulation key, then
/// the X25519 public key.
pub const X25519_MLKEM768_CLIENT_SHARE_BYTES: usize =
    MLKEM768_ENCAPSULATION_KEY_BYTES + X25519_KEY_BYTES;

/// The bytes of a server's `X25519MLKEM768` key share: the ML-KEM-768 ciphertext, then the
/// X25519 public key.
pub const X25519_MLKEM768_SERVER_SHARE_BYTES: usize = MLKEM768_CIPHERTEXT_BYTES + X25519_KEY_BYTES;

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
    let pss = |hash: SignatureHash| {
        Some(SignatureAlgorithm::RsaPss {
            hash,
            salt_length: hash.digest_bytes(),
        })
    };
    match scheme {
        signature_scheme::ECDSA_SECP256R1_SHA256 => {
            Some(SignatureAlgorithm::Ecdsa(SignatureHash::Sha256))
        }
        signature_scheme::RSA_PSS_RSAE_SHA256 => pss(SignatureHash::Sha256),
        signature_scheme::RSA_PSS_RSAE_SHA384 => pss(SignatureHash::Sha384),
        signature_scheme::RSA_PSS_RSAE_SHA512 => pss(SignatureHash::Sha512),
        signature_scheme::RSA_PKCS1_SHA256 if !tls13 => {
            Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256))
        }
        signature_scheme::RSA_PKCS1_SHA384 if !tls13 => {
            Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha384))
        }
        signature_scheme::RSA_PKCS1_SHA512 if !tls13 => {
            Some(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha512))
        }
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
    let usable = |scheme: &u16| {
        scheme_fits_key(*scheme, &public_key) && scheme_algorithm(*scheme, tls13).is_some()
    };
    if offered.is_empty() && !tls13 {
        return [
            signature_scheme::ECDSA_SECP256R1_SHA256,
            signature_scheme::RSA_PKCS1_SHA256,
        ]
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
    /// A client's `X25519MLKEM768` keys: the ML-KEM-768 key pair and an X25519 private scalar.
    X25519MlKem768 {
        /// The ML-KEM-768 encapsulation key the share carries.
        encapsulation: Vec<u8>,
        /// The ML-KEM-768 decapsulation key.
        decapsulation: Vec<u8>,
        /// The X25519 private scalar.
        scalar: [u8; X25519_KEY_BYTES],
    },
}

/// What a server answers to a client's key share: the share it sends back and the shared secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerKeyExchange {
    /// The server's key share.
    pub share: Vec<u8>,
    /// The shared secret both sides now hold.
    pub shared_secret: Vec<u8>,
}

impl std::fmt::Debug for ServerKeyExchange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerKeyExchange")
            .field("share_bytes", &self.share.len())
            .finish_non_exhaustive()
    }
}

/// The server's side of the key exchange in `group` with the client's `client_share`: `Ok(None)`
/// when the group is unsupported or the client's share is malformed or of low order.
pub fn server_key_exchange(
    group: u16,
    client_share: &[u8],
    fill_random: FillRandom<'_>,
) -> std::io::Result<Option<ServerKeyExchange>> {
    if group != named_group::X25519_MLKEM768 {
        let Some(ephemeral) = EphemeralKey::generate(group, fill_random)? else {
            return Ok(None);
        };
        return Ok(ephemeral
            .shared_secret(client_share)
            .map(|shared_secret| ServerKeyExchange {
                share: ephemeral.public_key(),
                shared_secret,
            }));
    }
    if client_share.len() != X25519_MLKEM768_CLIENT_SHARE_BYTES {
        return Ok(None);
    }
    let (encapsulation, x25519_peer) = client_share.split_at(MLKEM768_ENCAPSULATION_KEY_BYTES);
    let mut message = [0u8; 32];
    fill_random(&mut message)?;
    let mut scalar = [0u8; X25519_KEY_BYTES];
    fill_random(&mut scalar)?;
    let Ok((ciphertext, mlkem_secret)) = mlkem768_encapsulate(encapsulation, &message) else {
        return Ok(None);
    };
    let Ok(peer): Result<&[u8; X25519_KEY_BYTES], _> = x25519_peer.try_into() else {
        return Ok(None);
    };
    let Ok(x25519_secret) = x25519_shared_secret(&scalar, peer) else {
        return Ok(None);
    };
    let mut share = ciphertext;
    share.extend_from_slice(&x25519_public_key(&scalar));
    let mut shared_secret = mlkem_secret.to_vec();
    shared_secret.extend_from_slice(&x25519_secret);
    Ok(Some(ServerKeyExchange {
        share,
        shared_secret,
    }))
}

impl std::fmt::Debug for EphemeralKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::X25519(_) => "EphemeralKey::X25519",
            Self::P256(_) => "EphemeralKey::P256",
            Self::X25519MlKem768 { .. } => "EphemeralKey::X25519MlKem768",
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
            named_group::X25519_MLKEM768 => {
                let mut seed = [0u8; MLKEM_KEY_GENERATION_SEED_BYTES];
                fill_random(&mut seed)?;
                let mut scalar = [0u8; X25519_KEY_BYTES];
                fill_random(&mut scalar)?;
                let (encapsulation, decapsulation) = mlkem768_key_pair(&seed);
                Ok(Some(Self::X25519MlKem768 {
                    encapsulation,
                    decapsulation,
                    scalar,
                }))
            }
            _ => Ok(None),
        }
    }

    /// The group of this key.
    pub fn group(&self) -> u16 {
        match self {
            Self::X25519(_) => named_group::X25519,
            Self::P256(_) => named_group::SECP256R1,
            Self::X25519MlKem768 { .. } => named_group::X25519_MLKEM768,
        }
    }

    /// The public key as a key share carries it: 32 bytes for X25519, the uncompressed point for
    /// P-256, and the encapsulation key then the X25519 public key for `X25519MLKEM768`.
    pub fn public_key(&self) -> Vec<u8> {
        match self {
            Self::X25519(scalar) => x25519_public_key(scalar).to_vec(),
            Self::P256(key) => key.public_key().to_sec1_uncompressed().to_vec(),
            Self::X25519MlKem768 {
                encapsulation,
                scalar,
                ..
            } => {
                let mut share = encapsulation.clone();
                share.extend_from_slice(&x25519_public_key(scalar));
                share
            }
        }
    }

    /// The shared secret with the peer's public key, or `None` when that key is malformed, not
    /// on the curve, or of low order.
    pub fn shared_secret(&self, peer_public_key: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::X25519(scalar) => {
                let peer: &[u8; X25519_KEY_BYTES] = peer_public_key.try_into().ok()?;
                x25519_shared_secret(scalar, peer)
                    .ok()
                    .map(|secret| secret.to_vec())
            }
            Self::P256(key) => {
                if peer_public_key.first() != Some(&0x04) {
                    return None;
                }
                let peer = P256PublicKey::from_sec1(peer_public_key).ok()?;
                p256_shared_secret(key, &peer)
                    .ok()
                    .map(|secret| secret.to_vec())
            }
            Self::X25519MlKem768 {
                decapsulation,
                scalar,
                ..
            } => {
                if peer_public_key.len() != X25519_MLKEM768_SERVER_SHARE_BYTES {
                    return None;
                }
                let (ciphertext, x25519_peer) = peer_public_key.split_at(MLKEM768_CIPHERTEXT_BYTES);
                let mlkem_secret = mlkem768_decapsulate(decapsulation, ciphertext).ok()?;
                let peer: &[u8; X25519_KEY_BYTES] = x25519_peer.try_into().ok()?;
                let x25519_secret = x25519_shared_secret(scalar, peer).ok()?;
                let mut secret = mlkem_secret.to_vec();
                secret.extend_from_slice(&x25519_secret);
                Some(secret)
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
            let first = EphemeralKey::generate(group, &mut first_random)
                .unwrap()
                .unwrap();
            let second = EphemeralKey::generate(group, &mut second_random)
                .unwrap()
                .unwrap();
            assert_eq!(first.group(), group);
            let one = first.shared_secret(&second.public_key()).unwrap();
            let other = second.shared_secret(&first.public_key()).unwrap();
            assert_eq!(one, other);
        }
    }

    #[test]
    fn the_hybrid_group_agrees_on_a_64_byte_secret() {
        let mut client_random = fixed_random(7);
        let client = EphemeralKey::generate(named_group::X25519_MLKEM768, &mut client_random)
            .unwrap()
            .unwrap();
        assert_eq!(client.group(), named_group::X25519_MLKEM768);
        let share = client.public_key();
        assert_eq!(share.len(), X25519_MLKEM768_CLIENT_SHARE_BYTES);
        let mut server_random = fixed_random(11);
        let answer = server_key_exchange(named_group::X25519_MLKEM768, &share, &mut server_random)
            .unwrap()
            .unwrap();
        assert_eq!(answer.share.len(), X25519_MLKEM768_SERVER_SHARE_BYTES);
        assert_eq!(answer.shared_secret.len(), 64);
        assert_eq!(client.shared_secret(&answer.share), Some(answer.shared_secret));
        assert!(client.shared_secret(&[1u8; 32]).is_none());
        let mut truncated = share.clone();
        truncated.pop();
        assert!(
            server_key_exchange(named_group::X25519_MLKEM768, &truncated, &mut server_random)
                .unwrap()
                .is_none()
        );
        let mut low_order = share;
        low_order[MLKEM768_ENCAPSULATION_KEY_BYTES..].fill(0);
        assert!(
            server_key_exchange(named_group::X25519_MLKEM768, &low_order, &mut server_random)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_classic_groups_answer_through_server_key_exchange() {
        for group in SUPPORTED_GROUPS {
            let mut client_random = fixed_random(3);
            let client = EphemeralKey::generate(group, &mut client_random)
                .unwrap()
                .unwrap();
            let mut server_random = fixed_random(5);
            let answer = server_key_exchange(group, &client.public_key(), &mut server_random)
                .unwrap()
                .unwrap();
            assert_eq!(client.shared_secret(&answer.share), Some(answer.shared_secret));
        }
        let mut random = fixed_random(1);
        assert!(server_key_exchange(0x0018, &[4; 65], &mut random).unwrap().is_none());
    }

    #[test]
    fn malformed_and_low_order_peer_keys_are_refused() {
        let mut random = fixed_random(9);
        let x25519 = EphemeralKey::generate(named_group::X25519, &mut random)
            .unwrap()
            .unwrap();
        assert!(x25519.shared_secret(&[0u8; 32]).is_none());
        assert!(x25519.shared_secret(&[1u8; 31]).is_none());
        let p256 = EphemeralKey::generate(named_group::SECP256R1, &mut random)
            .unwrap()
            .unwrap();
        assert!(p256.shared_secret(&[4u8; 65]).is_none());
        let compressed = {
            let mut point = p256.public_key();
            point.truncate(33);
            point[0] = 0x02;
            point
        };
        assert!(p256.shared_secret(&compressed).is_none());
        assert!(
            EphemeralKey::generate(0x0018, &mut random)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn application_protocols_follow_the_server_order() {
        let server = vec![b"postgresql".to_vec(), b"other".to_vec()];
        let client = vec![b"other".to_vec(), b"postgresql".to_vec()];
        assert_eq!(
            choose_application_protocol(&server, &client),
            Ok(Some(b"postgresql".to_vec()))
        );
        assert_eq!(choose_application_protocol(&server, &[]), Ok(None));
        assert_eq!(
            choose_application_protocol(&server, &[b"h2".to_vec()]),
            Err(NoCommonApplicationProtocol)
        );
    }

    #[test]
    fn tls13_never_uses_pkcs1_signatures() {
        assert!(scheme_algorithm(signature_scheme::RSA_PKCS1_SHA256, true).is_none());
        assert!(scheme_algorithm(signature_scheme::RSA_PKCS1_SHA256, false).is_some());
        assert_eq!(
            scheme_algorithm(signature_scheme::RSA_PSS_RSAE_SHA384, true),
            Some(SignatureAlgorithm::RsaPss {
                hash: SignatureHash::Sha384,
                salt_length: 48
            })
        );
    }
}
