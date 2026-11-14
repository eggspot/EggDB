//! QUIC packet protection (RFC 9001 section 5): the Initial secrets derived from the client's
//! first destination connection ID, the packet keys of a traffic secret, AEAD protection of the
//! payload, and header protection of the first byte and the packet number.

use crate::QuicCodecError;
use crate::packet::decode_packet_number;
use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, Aes, AesGcm, ChaCha20, ChaCha20Poly1305};
use eggdb_tls::cipher_suite::{AeadKind, HashKind};
use eggdb_tls::key_schedule::hkdf_expand_label;

/// The salt of QUIC version 1's Initial secrets (RFC 9001 section 5.2).
pub const INITIAL_SALT_V1: [u8; 20] = [
    0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8, 0x0c, 0xad, 0xcc, 0xbb,
    0x7f, 0x0a,
];
const SAMPLE_BYTES: usize = 16;
const MASK_BYTES: usize = 5;
const SAMPLE_OFFSET_FROM_PACKET_NUMBER: usize = 4;

/// The client's and server's Initial secrets for a connection whose client chose
/// `destination_connection_id` (RFC 9001 section 5.2).
pub fn initial_secrets(destination_connection_id: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let hash = HashKind::Sha256;
    let initial = hash.hkdf_extract(&INITIAL_SALT_V1, destination_connection_id);
    let client = hkdf_expand_label(hash, &initial, b"client in", &[], hash.digest_bytes());
    let server = hkdf_expand_label(hash, &initial, b"server in", &[], hash.digest_bytes());
    (client, server)
}

enum Sealer {
    AesGcm(AesGcm),
    ChaCha(ChaCha20Poly1305),
}

enum HeaderProtection {
    Aes(Aes),
    ChaCha([u8; 32]),
}

impl HeaderProtection {
    fn mask(&self, sample: &[u8; SAMPLE_BYTES]) -> [u8; MASK_BYTES] {
        let mut mask = [0u8; MASK_BYTES];
        match self {
            Self::Aes(cipher) => {
                let mut block = *sample;
                cipher.encrypt_block(&mut block);
                mask.copy_from_slice(&block[..MASK_BYTES]);
            }
            Self::ChaCha(key) => {
                let counter = u32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&sample[4..]);
                ChaCha20::new(key, &nonce).apply_keystream(counter, &mut mask);
            }
        }
        mask
    }
}

/// The keys that protect one direction's packets at one encryption level.
pub struct PacketKeys {
    sealer: Sealer,
    iv: [u8; AEAD_NONCE_BYTES],
    header: HeaderProtection,
}

impl std::fmt::Debug for PacketKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PacketKeys { .. }")
    }
}

/// The key, IV and header protection key `aead` uses under `secret` (labels `quic key`,
/// `quic iv` and `quic hp`), as bytes.
pub fn derive_key_material(hash: HashKind, aead: AeadKind, secret: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let key = hkdf_expand_label(hash, secret, b"quic key", &[], aead.key_bytes());
    let iv = hkdf_expand_label(hash, secret, b"quic iv", &[], AEAD_NONCE_BYTES);
    let header = hkdf_expand_label(hash, secret, b"quic hp", &[], aead.key_bytes());
    (key, iv, header)
}

/// The next generation of a 1-RTT secret after a key update (label `quic ku`).
pub fn next_secret(hash: HashKind, secret: &[u8]) -> Vec<u8> {
    hkdf_expand_label(hash, secret, b"quic ku", &[], hash.digest_bytes())
}

impl PacketKeys {
    /// The keys of `secret` for a connection using `aead` with `hash`.
    pub fn new(hash: HashKind, aead: AeadKind, secret: &[u8]) -> Result<Self, QuicCodecError> {
        let (key, iv_bytes, header_key) = derive_key_material(hash, aead, secret);
        let mut iv = [0u8; AEAD_NONCE_BYTES];
        iv.copy_from_slice(iv_bytes.get(..AEAD_NONCE_BYTES).ok_or(QuicCodecError::InvalidValue)?);
        let (sealer, header) = match aead {
            AeadKind::Aes128Gcm | AeadKind::Aes256Gcm => {
                let gcm = AesGcm::new(&key).map_err(|_| QuicCodecError::InvalidValue)?;
                let header = HeaderProtection::Aes(Aes::new(&header_key).map_err(|_| QuicCodecError::InvalidValue)?);
                (Sealer::AesGcm(gcm), header)
            }
            AeadKind::ChaCha20Poly1305 => {
                let key: [u8; 32] = key.as_slice().try_into().map_err(|_| QuicCodecError::InvalidValue)?;
                let header_key: [u8; 32] = header_key.as_slice().try_into().map_err(|_| QuicCodecError::InvalidValue)?;
                (Sealer::ChaCha(ChaCha20Poly1305::new(&key)), HeaderProtection::ChaCha(header_key))
            }
        };
        Ok(Self { sealer, iv, header })
    }

    /// The Initial keys of one side: `secret` from [`initial_secrets`].
    pub fn initial(secret: &[u8]) -> Result<Self, QuicCodecError> {
        Self::new(HashKind::Sha256, AeadKind::Aes128Gcm, secret)
    }

    fn nonce(&self, packet_number: u64) -> [u8; AEAD_NONCE_BYTES] {
        let mut nonce = self.iv;
        for (byte, number) in nonce[AEAD_NONCE_BYTES - 8..].iter_mut().zip(packet_number.to_be_bytes()) {
            *byte ^= number;
        }
        nonce
    }

    fn seal(&self, nonce: &[u8; AEAD_NONCE_BYTES], header: &[u8], payload: &mut [u8]) -> [u8; AEAD_TAG_BYTES] {
        match &self.sealer {
            Sealer::AesGcm(gcm) => gcm.seal_in_place(nonce, header, payload),
            Sealer::ChaCha(cipher) => cipher.seal_in_place(nonce, header, payload),
        }
    }

    fn open(&self, nonce: &[u8; AEAD_NONCE_BYTES], header: &[u8], payload: &mut [u8], tag: &[u8; AEAD_TAG_BYTES]) -> bool {
        match &self.sealer {
            Sealer::AesGcm(gcm) => gcm.open_in_place(nonce, header, payload, tag).is_ok(),
            Sealer::ChaCha(cipher) => cipher.open_in_place(nonce, header, payload, tag).is_ok(),
        }
    }

    /// Protects a packet in place: `packet` holds the header with its packet number
    /// (`packet_number_length` bytes at `packet_number_offset`) and then the plaintext payload.
    /// The AEAD tag is appended, then the header protection applied. The payload must be long
    /// enough that 16 bytes can be sampled 4 bytes after the packet number's start.
    pub fn protect(
        &self,
        packet: &mut Vec<u8>,
        packet_number_offset: usize,
        packet_number_length: usize,
        packet_number: u64,
    ) -> Result<(), QuicCodecError> {
        let payload_start = packet_number_offset + packet_number_length;
        if packet.len() < payload_start {
            return Err(QuicCodecError::InvalidValue);
        }
        let nonce = self.nonce(packet_number);
        let (header, payload) = packet.split_at_mut(payload_start);
        let tag = self.seal(&nonce, header, payload);
        packet.extend_from_slice(&tag);
        let sample_start = packet_number_offset + SAMPLE_OFFSET_FROM_PACKET_NUMBER;
        let sample: [u8; SAMPLE_BYTES] = packet
            .get(sample_start..sample_start + SAMPLE_BYTES)
            .ok_or(QuicCodecError::InvalidValue)?
            .try_into()
            .map_err(|_| QuicCodecError::InvalidValue)?;
        let mask = self.header.mask(&sample);
        let long = packet[0] & 0x80 != 0;
        packet[0] ^= mask[0] & if long { 0x0f } else { 0x1f };
        for (byte, mask) in packet[packet_number_offset..payload_start].iter_mut().zip(&mask[1..]) {
            *byte ^= mask;
        }
        Ok(())
    }

    /// Removes the protection of the packet that is `packet` (its datagram's bytes for this
    /// packet only), whose packet number starts at `packet_number_offset`. Returns the full
    /// packet number and the range of the decrypted payload in `packet`.
    pub fn unprotect(
        &self,
        packet: &mut [u8],
        packet_number_offset: usize,
        largest_received: Option<u64>,
    ) -> Result<(u64, std::ops::Range<usize>), QuicCodecError> {
        let sample_start = packet_number_offset + SAMPLE_OFFSET_FROM_PACKET_NUMBER;
        let sample: [u8; SAMPLE_BYTES] = packet
            .get(sample_start..sample_start + SAMPLE_BYTES)
            .ok_or(QuicCodecError::Truncated)?
            .try_into()
            .map_err(|_| QuicCodecError::Truncated)?;
        let mask = self.header.mask(&sample);
        let long = packet[0] & 0x80 != 0;
        packet[0] ^= mask[0] & if long { 0x0f } else { 0x1f };
        let length = usize::from(packet[0] & 0x03) + 1;
        let payload_start = packet_number_offset + length;
        let mut truncated = 0u64;
        for (byte, mask) in packet[packet_number_offset..payload_start].iter_mut().zip(&mask[1..]) {
            *byte ^= mask;
            truncated = (truncated << 8) | u64::from(*byte);
        }
        let packet_number = decode_packet_number(largest_received, truncated, length);
        let tag_start = packet
            .len()
            .checked_sub(AEAD_TAG_BYTES)
            .filter(|start| *start >= payload_start)
            .ok_or(QuicCodecError::Truncated)?;
        let tag: [u8; AEAD_TAG_BYTES] = packet[tag_start..].try_into().map_err(|_| QuicCodecError::Truncated)?;
        let nonce = self.nonce(packet_number);
        let (header, rest) = packet.split_at_mut(payload_start);
        let payload = &mut rest[..tag_start - payload_start];
        if !self.open(&nonce, header, payload, &tag) {
            return Err(QuicCodecError::Protection);
        }
        Ok((packet_number, payload_start..tag_start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{LongPacketKind, write_long_header};

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc_9001_appendix_a_1_initial_keys() {
        let (client, server) = initial_secrets(&hex("8394c8f03e515708"));
        assert_eq!(client, hex("c00cf151ca5be075ed0ebfb5c80323c42d6b7db67881289af4008f1f6c357aea"));
        assert_eq!(server, hex("3c199828fd139efd216c155ad844cc81fb82fa8d7446fa7d78be803acdda951b"));
        let (key, iv, header) = derive_key_material(HashKind::Sha256, AeadKind::Aes128Gcm, &client);
        assert_eq!(key, hex("1f369613dd76d5467730efcbe3b1a22d"));
        assert_eq!(iv, hex("fa044b2f42a3fd3b46fb255c"));
        assert_eq!(header, hex("9f50449e04a0e810283a1e9933adedd2"));
        let (key, iv, header) = derive_key_material(HashKind::Sha256, AeadKind::Aes128Gcm, &server);
        assert_eq!(key, hex("cf3a5331653c364c88f0f379b6067e37"));
        assert_eq!(iv, hex("0ac1493ca1905853b0bba03e"));
        assert_eq!(header, hex("c206b8d9b9f0f37644430b490eeaa314"));
    }

    fn protected_initial(keys: &PacketKeys, packet_number: u64) -> (Vec<u8>, usize) {
        let payload = vec![0x01u8; 40];
        let mut packet = Vec::new();
        write_long_header(&mut packet, LongPacketKind::Initial, &[8; 8], &[], &[], 2, 2 + payload.len() + AEAD_TAG_BYTES)
            .unwrap();
        let offset = packet.len();
        packet.extend_from_slice(&(packet_number as u16).to_be_bytes());
        packet.extend_from_slice(&payload);
        keys.protect(&mut packet, offset, 2, packet_number).unwrap();
        (packet, offset)
    }

    #[test]
    fn a_protected_packet_opens_with_the_same_keys_only() {
        let (client, server) = initial_secrets(&[8; 8]);
        let keys = PacketKeys::initial(&client).unwrap();
        let (mut packet, offset) = protected_initial(&keys, 7);
        let mut tampered = packet.clone();
        let (number, payload) = keys.unprotect(&mut packet, offset, Some(5)).unwrap();
        assert_eq!(number, 7);
        assert!(packet[payload].iter().all(|byte| *byte == 0x01));
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert_eq!(keys.unprotect(&mut tampered, offset, Some(5)), Err(QuicCodecError::Protection));
        let (mut again, _) = protected_initial(&keys, 7);
        let other = PacketKeys::initial(&server).unwrap();
        assert!(other.unprotect(&mut again, offset, Some(5)).is_err());
    }

    #[test]
    fn chacha20_keys_protect_and_open_too() {
        let secret = [3u8; 32];
        let keys = PacketKeys::new(HashKind::Sha256, AeadKind::ChaCha20Poly1305, &secret).unwrap();
        let mut packet = vec![0x40u8, 9, 9, 9, 9, 9, 9, 9, 9, 0x2a];
        packet.extend_from_slice(&[0x02; 30]);
        keys.protect(&mut packet, 9, 1, 0x2a).unwrap();
        let (number, payload) = keys.unprotect(&mut packet, 9, Some(0x29)).unwrap();
        assert_eq!(number, 0x2a);
        assert!(packet[payload].iter().all(|byte| *byte == 0x02));
        assert_eq!(next_secret(HashKind::Sha256, &secret).len(), 32);
    }

    #[test]
    fn a_packet_too_short_to_sample_is_truncated() {
        let (client, _) = initial_secrets(&[1; 8]);
        let keys = PacketKeys::initial(&client).unwrap();
        let mut packet = vec![0xc0u8, 0, 0, 0, 1, 0, 0, 0, 1, 0];
        assert_eq!(keys.unprotect(&mut packet, 9, None), Err(QuicCodecError::Truncated));
    }
}
