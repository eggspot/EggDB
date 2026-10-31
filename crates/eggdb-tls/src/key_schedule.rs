//! The TLS 1.3 key schedule (RFC 8446 section 7): HKDF-Expand-Label, Derive-Secret, the early,
//! handshake and master secrets, traffic keys and IVs, Finished values and key updates.

use crate::cipher_suite::{AeadKind, HashKind};
use crate::codec::Writer;
use eggdb_crypto::AEAD_NONCE_BYTES;

const LABEL_PREFIX: &[u8] = b"tls13 ";

/// HKDF-Expand-Label: `secret` expanded under `"tls13 " + label` and `context` to `length` bytes.
pub fn hkdf_expand_label(
    hash: HashKind,
    secret: &[u8],
    label: &[u8],
    context: &[u8],
    length: usize,
) -> Vec<u8> {
    let mut info = Writer::new();
    info.write_u16(u16::try_from(length).unwrap_or(u16::MAX));
    info.write_vector_u8(|full_label| {
        full_label.write_bytes(LABEL_PREFIX);
        full_label.write_bytes(label);
    });
    info.write_vector_u8(|context_field| context_field.write_bytes(context));
    hash.hkdf_expand(secret, &info.into_bytes(), length)
}

/// Derive-Secret: `secret` expanded under `label` and an already computed transcript hash.
pub fn derive_secret(
    hash: HashKind,
    secret: &[u8],
    label: &[u8],
    transcript_hash: &[u8],
) -> Vec<u8> {
    hkdf_expand_label(hash, secret, label, transcript_hash, hash.digest_bytes())
}

/// The write key and IV of one direction, derived from its traffic secret.
#[derive(Clone, PartialEq, Eq)]
pub struct TrafficKeys {
    /// The AEAD key.
    pub key: Vec<u8>,
    /// The per-connection IV that sequence numbers are XORed into.
    pub iv: [u8; AEAD_NONCE_BYTES],
}

impl std::fmt::Debug for TrafficKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TrafficKeys { .. }")
    }
}

/// The key and IV `aead` uses for records protected under `traffic_secret`.
pub fn traffic_keys(hash: HashKind, aead: AeadKind, traffic_secret: &[u8]) -> TrafficKeys {
    let key = hkdf_expand_label(hash, traffic_secret, b"key", &[], aead.key_bytes());
    let iv_bytes = hkdf_expand_label(hash, traffic_secret, b"iv", &[], AEAD_NONCE_BYTES);
    let mut iv = [0u8; AEAD_NONCE_BYTES];
    if iv_bytes.len() == AEAD_NONCE_BYTES {
        iv.copy_from_slice(&iv_bytes);
    }
    TrafficKeys { key, iv }
}

/// The `verify_data` of a Finished message sent under `base_key` (a handshake traffic secret)
/// over the transcript hash up to, but not including, that Finished.
pub fn finished_verify_data(hash: HashKind, base_key: &[u8], transcript_hash: &[u8]) -> Vec<u8> {
    let finished_key = hkdf_expand_label(hash, base_key, b"finished", &[], hash.digest_bytes());
    hash.hmac(&finished_key, transcript_hash)
}

/// The next application traffic secret after a KeyUpdate.
pub fn next_traffic_secret(hash: HashKind, traffic_secret: &[u8]) -> Vec<u8> {
    hkdf_expand_label(hash, traffic_secret, b"traffic upd", &[], hash.digest_bytes())
}

/// The client's and server's traffic secrets of one stage of the handshake.
#[derive(Clone, PartialEq, Eq)]
pub struct TrafficSecrets {
    /// The secret protecting what the client sends.
    pub client: Vec<u8>,
    /// The secret protecting what the server sends.
    pub server: Vec<u8>,
}

impl std::fmt::Debug for TrafficSecrets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TrafficSecrets { .. }")
    }
}

/// The key schedule of one connection, advanced stage by stage as the handshake goes on.
pub struct KeySchedule {
    hash: HashKind,
    stage_secret: Vec<u8>,
}

impl std::fmt::Debug for KeySchedule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeySchedule")
            .field("hash", &self.hash)
            .finish_non_exhaustive()
    }
}

impl KeySchedule {
    /// A schedule starting from the early secret: HKDF-Extract of a pre-shared key, or of zeros
    /// when there is none.
    pub fn new(hash: HashKind, pre_shared_key: Option<&[u8]>) -> Self {
        let zeros = vec![0u8; hash.digest_bytes()];
        let input = pre_shared_key.unwrap_or(&zeros);
        Self { hash, stage_secret: hash.hkdf_extract(&zeros, input) }
    }

    /// The hash this schedule uses.
    pub fn hash(&self) -> HashKind {
        self.hash
    }

    /// The secret of the current stage (early, then handshake, then master).
    pub fn stage_secret(&self) -> &[u8] {
        &self.stage_secret
    }

    fn advance(&mut self, input_key: &[u8]) {
        let empty_hash = self.hash.digest(&[]);
        let salt = derive_secret(self.hash, &self.stage_secret, b"derived", &empty_hash);
        self.stage_secret = self.hash.hkdf_extract(&salt, input_key);
    }

    /// Moves to the handshake secret with the (EC)DHE shared secret and returns the handshake
    /// traffic secrets over the transcript hash of ClientHello..ServerHello.
    pub fn enter_handshake(&mut self, shared_secret: &[u8], hello_hash: &[u8]) -> TrafficSecrets {
        self.advance(shared_secret);
        TrafficSecrets {
            client: derive_secret(self.hash, &self.stage_secret, b"c hs traffic", hello_hash),
            server: derive_secret(self.hash, &self.stage_secret, b"s hs traffic", hello_hash),
        }
    }

    /// Moves to the master secret and returns the application traffic secrets and the exporter
    /// secret over the transcript hash of ClientHello..server Finished.
    pub fn enter_application(&mut self, server_finished_hash: &[u8]) -> (TrafficSecrets, Vec<u8>) {
        let zeros = vec![0u8; self.hash.digest_bytes()];
        self.advance(&zeros);
        let secrets = TrafficSecrets {
            client: derive_secret(
                self.hash,
                &self.stage_secret,
                b"c ap traffic",
                server_finished_hash,
            ),
            server: derive_secret(
                self.hash,
                &self.stage_secret,
                b"s ap traffic",
                server_finished_hash,
            ),
        };
        let exporter = derive_secret(self.hash, &self.stage_secret, b"exp master", server_finished_hash);
        (secrets, exporter)
    }

    /// The resumption master secret over the transcript hash of ClientHello..client Finished.
    pub fn resumption_master_secret(&self, client_finished_hash: &[u8]) -> Vec<u8> {
        derive_secret(self.hash, &self.stage_secret, b"res master", client_finished_hash)
    }
}

/// The pre-shared key a NewSessionTicket with `ticket_nonce` stands for.
pub fn resumption_pre_shared_key(
    hash: HashKind,
    resumption_master_secret: &[u8],
    ticket_nonce: &[u8],
) -> Vec<u8> {
    hkdf_expand_label(
        hash,
        resumption_master_secret,
        b"resumption",
        ticket_nonce,
        hash.digest_bytes(),
    )
}

/// TLS-Exporter (RFC 8446 section 7.5): `length` bytes of keying material for `label` and
/// `context`, from the exporter master secret.
pub fn export_keying_material(
    hash: HashKind,
    exporter_master_secret: &[u8],
    label: &[u8],
    context: &[u8],
    length: usize,
) -> Vec<u8> {
    let empty_hash = hash.digest(&[]);
    let secret = derive_secret(hash, exporter_master_secret, label, &empty_hash);
    hkdf_expand_label(hash, &secret, b"exporter", &hash.digest(context), length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_label_info_has_the_rfc_8446_layout() {
        let secret = [7u8; 32];
        let direct = hkdf_expand_label(HashKind::Sha256, &secret, b"key", &[], 16);
        let mut info = vec![0x00, 0x10, 9];
        info.extend_from_slice(b"tls13 key");
        info.push(0);
        assert_eq!(direct, HashKind::Sha256.hkdf_expand(&secret, &info, 16));
    }

    #[test]
    fn a_key_update_changes_the_secret() {
        let secret = [1u8; 48];
        let next = next_traffic_secret(HashKind::Sha384, &secret);
        assert_eq!(next.len(), 48);
        assert_ne!(next, secret);
    }
}
