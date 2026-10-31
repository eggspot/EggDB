//! The cipher suites EggDB speaks, the hash each one uses for its handshake and the AEAD that
//! protects its records. AES-GCM is portable constant-time code without CPU AES instructions, so
//! the ChaCha20-Poly1305 suites come first in the server's order (D23).

use eggdb_crypto::{
    AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AeadError, AesGcm, ChaCha20Poly1305, HashFunction, Hmac,
    Sha256, Sha384, hkdf_expand, hkdf_extract,
};

/// A protocol version as it appears on the wire.
pub mod version {
    /// TLS 1.2 (RFC 5246), also the legacy version field of every TLS 1.3 record.
    pub const TLS12: u16 = 0x0303;
    /// TLS 1.3 (RFC 8446).
    pub const TLS13: u16 = 0x0304;
    /// TLS 1.0, written in the record header of a first ClientHello by some clients.
    pub const TLS10: u16 = 0x0301;
}

/// The hash a cipher suite uses for its transcript, key schedule and Finished messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
}

impl HashKind {
    /// The digest length in bytes.
    pub fn digest_bytes(self) -> usize {
        match self {
            Self::Sha256 => Sha256::DIGEST_BYTES,
            Self::Sha384 => Sha384::DIGEST_BYTES,
        }
    }

    /// The digest of `bytes`.
    pub fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => Sha256::digest(bytes).as_ref().to_vec(),
            Self::Sha384 => Sha384::digest(bytes).as_ref().to_vec(),
        }
    }

    /// HMAC of `message` under `key`.
    pub fn hmac(self, key: &[u8], message: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => hmac_with::<Sha256>(key, message),
            Self::Sha384 => hmac_with::<Sha384>(key, message),
        }
    }

    /// HKDF-Extract (RFC 5869).
    pub fn hkdf_extract(self, salt: &[u8], input_key: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => hkdf_extract::<Sha256>(salt, input_key).as_ref().to_vec(),
            Self::Sha384 => hkdf_extract::<Sha384>(salt, input_key).as_ref().to_vec(),
        }
    }

    /// HKDF-Expand (RFC 5869) to `length` bytes; TLS never asks for more than 255 digests.
    pub fn hkdf_expand(self, pseudorandom_key: &[u8], info: &[u8], length: usize) -> Vec<u8> {
        let mut output = vec![0u8; length];
        let expanded = match self {
            Self::Sha256 => hkdf_expand::<Sha256>(pseudorandom_key, info, &mut output),
            Self::Sha384 => hkdf_expand::<Sha384>(pseudorandom_key, info, &mut output),
        };
        if expanded.is_err() {
            output.clear();
        }
        output
    }

    /// A running transcript hash of this kind.
    pub fn transcript(self) -> TranscriptHash {
        match self {
            Self::Sha256 => TranscriptHash::Sha256(Sha256::new()),
            Self::Sha384 => TranscriptHash::Sha384(Sha384::new()),
        }
    }
}

fn hmac_with<H: HashFunction>(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<H>::new(key);
    mac.update(message);
    mac.finish().as_ref().to_vec()
}

/// The hash of every handshake message so far, which can be read at any point without ending it.
#[derive(Debug, Clone)]
pub enum TranscriptHash {
    /// A SHA-256 transcript.
    Sha256(Sha256),
    /// A SHA-384 transcript.
    Sha384(Sha384),
}

impl TranscriptHash {
    /// Adds a whole handshake message, header included.
    pub fn update(&mut self, message: &[u8]) {
        match self {
            Self::Sha256(hash) => hash.update(message),
            Self::Sha384(hash) => hash.update(message),
        }
    }

    /// The hash of everything added so far.
    pub fn current(&self) -> Vec<u8> {
        match self {
            Self::Sha256(hash) => hash.clone().finish().as_ref().to_vec(),
            Self::Sha384(hash) => hash.clone().finish().as_ref().to_vec(),
        }
    }

    /// Which hash this transcript uses.
    pub fn kind(&self) -> HashKind {
        match self {
            Self::Sha256(_) => HashKind::Sha256,
            Self::Sha384(_) => HashKind::Sha384,
        }
    }
}

/// The record protection a cipher suite uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeadKind {
    /// AES-128-GCM.
    Aes128Gcm,
    /// AES-256-GCM.
    Aes256Gcm,
    /// ChaCha20-Poly1305.
    ChaCha20Poly1305,
}

impl AeadKind {
    /// The key length in bytes.
    pub fn key_bytes(self) -> usize {
        match self {
            Self::Aes128Gcm => 16,
            Self::Aes256Gcm | Self::ChaCha20Poly1305 => 32,
        }
    }
}

/// An AEAD keyed for one direction of a connection.
pub enum RecordAead {
    /// AES-GCM with a 128- or 256-bit key.
    AesGcm(AesGcm),
    /// ChaCha20-Poly1305.
    ChaCha20Poly1305(ChaCha20Poly1305),
}

impl std::fmt::Debug for RecordAead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AesGcm(_) => "RecordAead::AesGcm",
            Self::ChaCha20Poly1305(_) => "RecordAead::ChaCha20Poly1305",
        })
    }
}

impl RecordAead {
    /// The AEAD of `kind` under `key`, or `None` when the key length is wrong for it.
    pub fn new(kind: AeadKind, key: &[u8]) -> Option<Self> {
        if key.len() != kind.key_bytes() {
            return None;
        }
        match kind {
            AeadKind::Aes128Gcm | AeadKind::Aes256Gcm => AesGcm::new(key).ok().map(Self::AesGcm),
            AeadKind::ChaCha20Poly1305 => {
                let key: &[u8; 32] = key.try_into().ok()?;
                Some(Self::ChaCha20Poly1305(ChaCha20Poly1305::new(key)))
            }
        }
    }

    /// Encrypts `buffer` in place and returns the tag.
    pub fn seal_in_place(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        buffer: &mut [u8],
    ) -> [u8; AEAD_TAG_BYTES] {
        match self {
            Self::AesGcm(aead) => aead.seal_in_place(nonce, associated_data, buffer),
            Self::ChaCha20Poly1305(aead) => aead.seal_in_place(nonce, associated_data, buffer),
        }
    }

    /// Checks `tag` and decrypts `buffer` in place only when it matches.
    pub fn open_in_place(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        buffer: &mut [u8],
        tag: &[u8; AEAD_TAG_BYTES],
    ) -> Result<(), AeadError> {
        match self {
            Self::AesGcm(aead) => aead.open_in_place(nonce, associated_data, buffer, tag),
            Self::ChaCha20Poly1305(aead) => {
                aead.open_in_place(nonce, associated_data, buffer, tag)
            }
        }
    }
}

/// How a TLS 1.2 cipher suite authenticates the server's key exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls12Authentication {
    /// An ECDSA certificate.
    Ecdsa,
    /// An RSA certificate.
    Rsa,
}

/// A cipher suite EggDB can negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherSuite {
    /// `TLS_AES_128_GCM_SHA256` (TLS 1.3).
    Tls13Aes128GcmSha256,
    /// `TLS_AES_256_GCM_SHA384` (TLS 1.3).
    Tls13Aes256GcmSha384,
    /// `TLS_CHACHA20_POLY1305_SHA256` (TLS 1.3).
    Tls13ChaCha20Poly1305Sha256,
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` (TLS 1.2).
    EcdheEcdsaAes128GcmSha256,
    /// `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` (TLS 1.2).
    EcdheEcdsaAes256GcmSha384,
    /// `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` (TLS 1.2).
    EcdheEcdsaChaCha20Poly1305Sha256,
    /// `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` (TLS 1.2).
    EcdheRsaAes128GcmSha256,
    /// `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384` (TLS 1.2).
    EcdheRsaAes256GcmSha384,
    /// `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256` (TLS 1.2).
    EcdheRsaChaCha20Poly1305Sha256,
}

impl CipherSuite {
    /// Every suite, in the server's order of preference.
    pub const ALL: [Self; 9] = [
        Self::Tls13ChaCha20Poly1305Sha256,
        Self::Tls13Aes128GcmSha256,
        Self::Tls13Aes256GcmSha384,
        Self::EcdheEcdsaChaCha20Poly1305Sha256,
        Self::EcdheRsaChaCha20Poly1305Sha256,
        Self::EcdheEcdsaAes128GcmSha256,
        Self::EcdheRsaAes128GcmSha256,
        Self::EcdheEcdsaAes256GcmSha384,
        Self::EcdheRsaAes256GcmSha384,
    ];

    /// The suite's two-byte identifier.
    pub fn id(self) -> u16 {
        match self {
            Self::Tls13Aes128GcmSha256 => 0x1301,
            Self::Tls13Aes256GcmSha384 => 0x1302,
            Self::Tls13ChaCha20Poly1305Sha256 => 0x1303,
            Self::EcdheEcdsaAes128GcmSha256 => 0xc02b,
            Self::EcdheEcdsaAes256GcmSha384 => 0xc02c,
            Self::EcdheRsaAes128GcmSha256 => 0xc02f,
            Self::EcdheRsaAes256GcmSha384 => 0xc030,
            Self::EcdheRsaChaCha20Poly1305Sha256 => 0xcca8,
            Self::EcdheEcdsaChaCha20Poly1305Sha256 => 0xcca9,
        }
    }

    /// The suite with identifier `id`, if EggDB speaks it.
    pub fn from_id(id: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|suite| suite.id() == id)
    }

    /// Whether the suite belongs to TLS 1.3.
    pub fn is_tls13(self) -> bool {
        matches!(
            self,
            Self::Tls13Aes128GcmSha256
                | Self::Tls13Aes256GcmSha384
                | Self::Tls13ChaCha20Poly1305Sha256
        )
    }

    /// The hash of the handshake.
    pub fn hash(self) -> HashKind {
        match self {
            Self::Tls13Aes256GcmSha384
            | Self::EcdheEcdsaAes256GcmSha384
            | Self::EcdheRsaAes256GcmSha384 => HashKind::Sha384,
            _ => HashKind::Sha256,
        }
    }

    /// The record protection.
    pub fn aead(self) -> AeadKind {
        match self {
            Self::Tls13Aes128GcmSha256
            | Self::EcdheEcdsaAes128GcmSha256
            | Self::EcdheRsaAes128GcmSha256 => AeadKind::Aes128Gcm,
            Self::Tls13Aes256GcmSha384
            | Self::EcdheEcdsaAes256GcmSha384
            | Self::EcdheRsaAes256GcmSha384 => AeadKind::Aes256Gcm,
            Self::Tls13ChaCha20Poly1305Sha256
            | Self::EcdheEcdsaChaCha20Poly1305Sha256
            | Self::EcdheRsaChaCha20Poly1305Sha256 => AeadKind::ChaCha20Poly1305,
        }
    }

    /// How a TLS 1.2 suite authenticates the server, or `None` for a TLS 1.3 suite.
    pub fn tls12_authentication(self) -> Option<Tls12Authentication> {
        match self {
            Self::EcdheEcdsaAes128GcmSha256
            | Self::EcdheEcdsaAes256GcmSha384
            | Self::EcdheEcdsaChaCha20Poly1305Sha256 => Some(Tls12Authentication::Ecdsa),
            Self::EcdheRsaAes128GcmSha256
            | Self::EcdheRsaAes256GcmSha384
            | Self::EcdheRsaChaCha20Poly1305Sha256 => Some(Tls12Authentication::Rsa),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_suite_round_trips_through_its_identifier() {
        for suite in CipherSuite::ALL {
            assert_eq!(CipherSuite::from_id(suite.id()), Some(suite));
        }
        assert_eq!(CipherSuite::from_id(0x0000), None);
        assert_eq!(CipherSuite::from_id(0x009c), None);
    }

    #[test]
    fn chacha_suites_come_first_for_each_version() {
        assert_eq!(CipherSuite::ALL[0], CipherSuite::Tls13ChaCha20Poly1305Sha256);
        let first_tls12 = CipherSuite::ALL.into_iter().find(|suite| !suite.is_tls13());
        assert_eq!(first_tls12.map(CipherSuite::aead), Some(AeadKind::ChaCha20Poly1305));
    }

    #[test]
    fn an_aead_refuses_a_key_of_the_wrong_length() {
        assert!(RecordAead::new(AeadKind::Aes128Gcm, &[0; 32]).is_none());
        assert!(RecordAead::new(AeadKind::ChaCha20Poly1305, &[0; 16]).is_none());
        assert!(RecordAead::new(AeadKind::Aes256Gcm, &[0; 32]).is_some());
    }

    #[test]
    fn transcript_reads_do_not_end_the_hash() {
        let mut transcript = HashKind::Sha256.transcript();
        transcript.update(b"abc");
        assert_eq!(transcript.current(), HashKind::Sha256.digest(b"abc"));
        transcript.update(b"def");
        assert_eq!(transcript.current(), HashKind::Sha256.digest(b"abcdef"));
    }
}
