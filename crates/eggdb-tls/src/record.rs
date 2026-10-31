//! The TLS record layer: record headers, their size limits, and AEAD record protection for TLS 1.3
//! (RFC 8446 section 5) and TLS 1.2 (RFC 5288, RFC 7905).

use crate::cipher_suite::{AeadKind, RecordAead, version};
use crate::key_schedule::TrafficKeys;
use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES};

/// The content types of records.
pub mod content_type {
    /// `change_cipher_spec`, sent once for middlebox compatibility in TLS 1.3.
    pub const CHANGE_CIPHER_SPEC: u8 = 20;
    /// `alert`.
    pub const ALERT: u8 = 21;
    /// `handshake`.
    pub const HANDSHAKE: u8 = 22;
    /// `application_data`.
    pub const APPLICATION_DATA: u8 = 23;
}

/// The length of a record header.
pub const RECORD_HEADER_BYTES: usize = 5;
/// The largest plaintext fragment a record may carry (2^14 bytes).
pub const MAX_PLAINTEXT_BYTES: usize = 1 << 14;
/// The largest protected TLS 1.3 record body (2^14 + 256 bytes).
pub const MAX_TLS13_CIPHERTEXT_BYTES: usize = MAX_PLAINTEXT_BYTES + 256;
/// The largest protected TLS 1.2 record body (2^14 + 2048 bytes).
pub const MAX_TLS12_CIPHERTEXT_BYTES: usize = MAX_PLAINTEXT_BYTES + 2048;
const TLS12_EXPLICIT_NONCE_BYTES: usize = 8;
const TLS12_GCM_FIXED_IV_BYTES: usize = 4;

/// Why a record was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// A record longer than its limit (alert `record_overflow`).
    Overflow,
    /// A record whose authentication failed (alert `bad_record_mac`).
    BadRecordMac,
    /// A record with an unknown content type or no content type at all (alert
    /// `unexpected_message`).
    UnexpectedMessage,
    /// A header that is not a TLS record header (alert `decode_error`).
    MalformedHeader,
    /// 2^64 records were protected under one key; the connection must end.
    SequenceExhausted,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Overflow => "a TLS record is longer than the protocol allows",
            Self::BadRecordMac => "a TLS record failed its integrity check",
            Self::UnexpectedMessage => "a TLS record has an unknown content type",
            Self::MalformedHeader => "the peer did not send a TLS record header",
            Self::SequenceExhausted => "the TLS connection protected too many records under one key",
        })
    }
}

impl std::error::Error for RecordError {}

/// A parsed record header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// The content type.
    pub content_type: u8,
    /// The legacy record version (`0x0301`..`0x0303`).
    pub legacy_version: u16,
    /// The length of the record body.
    pub length: usize,
}

impl RecordHeader {
    /// Parses a header and checks the body length against `max_body_bytes`.
    pub fn parse(
        bytes: &[u8; RECORD_HEADER_BYTES],
        max_body_bytes: usize,
    ) -> Result<Self, RecordError> {
        let [content_type, major, minor, high, low] = *bytes;
        if !matches!(
            content_type,
            content_type::CHANGE_CIPHER_SPEC
                | content_type::ALERT
                | content_type::HANDSHAKE
                | content_type::APPLICATION_DATA
        ) {
            return Err(RecordError::MalformedHeader);
        }
        let legacy_version = u16::from_be_bytes([major, minor]);
        if major != 3 || minor > 4 {
            return Err(RecordError::MalformedHeader);
        }
        let length = usize::from(u16::from_be_bytes([high, low]));
        if length > max_body_bytes {
            return Err(RecordError::Overflow);
        }
        Ok(Self { content_type, legacy_version, length })
    }

    /// The five header bytes.
    pub fn encode(&self) -> [u8; RECORD_HEADER_BYTES] {
        let [major, minor] = self.legacy_version.to_be_bytes();
        let [high, low] = (self.length as u16).to_be_bytes();
        [self.content_type, major, minor, high, low]
    }
}

/// An unprotected record of `content_type` carrying `fragment`, which must fit in one record.
pub fn plaintext_record(content_type: u8, legacy_version: u16, fragment: &[u8]) -> Vec<u8> {
    let header = RecordHeader { content_type, legacy_version, length: fragment.len() };
    let mut record = header.encode().to_vec();
    record.extend_from_slice(fragment);
    record
}

fn sequence_nonce(iv: &[u8; AEAD_NONCE_BYTES], sequence: u64) -> [u8; AEAD_NONCE_BYTES] {
    let mut nonce = *iv;
    for (byte, sequence_byte) in nonce[AEAD_NONCE_BYTES - 8..].iter_mut().zip(sequence.to_be_bytes()) {
        *byte ^= sequence_byte;
    }
    nonce
}

fn split_tag(payload: &mut Vec<u8>) -> Result<[u8; AEAD_TAG_BYTES], RecordError> {
    let body_length = payload.len().checked_sub(AEAD_TAG_BYTES).ok_or(RecordError::BadRecordMac)?;
    let mut tag = [0u8; AEAD_TAG_BYTES];
    tag.copy_from_slice(&payload[body_length..]);
    payload.truncate(body_length);
    Ok(tag)
}

/// The protection of one direction of a TLS 1.3 connection under one traffic secret.
#[derive(Debug)]
pub struct Tls13RecordProtection {
    aead: RecordAead,
    iv: [u8; AEAD_NONCE_BYTES],
    sequence: u64,
}

impl Tls13RecordProtection {
    /// Protection with `keys` for `aead`, or `None` when the key length is wrong.
    pub fn new(aead: AeadKind, keys: &TrafficKeys) -> Option<Self> {
        Some(Self { aead: RecordAead::new(aead, &keys.key)?, iv: keys.iv, sequence: 0 })
    }

    fn next_nonce(&mut self) -> Result<[u8; AEAD_NONCE_BYTES], RecordError> {
        let nonce = sequence_nonce(&self.iv, self.sequence);
        self.sequence = self.sequence.checked_add(1).ok_or(RecordError::SequenceExhausted)?;
        Ok(nonce)
    }

    /// One protected record holding `fragment` as `content_type`, header included.
    pub fn seal(&mut self, content_type: u8, fragment: &[u8]) -> Result<Vec<u8>, RecordError> {
        if fragment.len() > MAX_PLAINTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        let nonce = self.next_nonce()?;
        let header = RecordHeader {
            content_type: content_type::APPLICATION_DATA,
            legacy_version: version::TLS12,
            length: fragment.len() + 1 + AEAD_TAG_BYTES,
        }
        .encode();
        let mut record =
            Vec::with_capacity(RECORD_HEADER_BYTES + fragment.len() + 1 + AEAD_TAG_BYTES);
        record.extend_from_slice(&header);
        record.extend_from_slice(fragment);
        record.push(content_type);
        let tag = self.aead.seal_in_place(&nonce, &header, &mut record[RECORD_HEADER_BYTES..]);
        record.extend_from_slice(&tag);
        Ok(record)
    }

    /// Opens the protected body `payload` of a record with `header` in place and returns the
    /// inner content type; `payload` is left holding the plaintext without its padding.
    pub fn open(
        &mut self,
        header: &RecordHeader,
        payload: &mut Vec<u8>,
    ) -> Result<u8, RecordError> {
        if header.content_type != content_type::APPLICATION_DATA {
            return Err(RecordError::UnexpectedMessage);
        }
        if payload.len() > MAX_TLS13_CIPHERTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        let nonce = self.next_nonce()?;
        let tag = split_tag(payload)?;
        self.aead
            .open_in_place(&nonce, &header.encode(), payload, &tag)
            .map_err(|_| RecordError::BadRecordMac)?;
        let content_end = payload.iter().rposition(|&byte| byte != 0).ok_or(RecordError::UnexpectedMessage)?;
        let inner_type = payload[content_end];
        payload.truncate(content_end);
        if payload.len() > MAX_PLAINTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        if !matches!(
            inner_type,
            content_type::ALERT | content_type::HANDSHAKE | content_type::APPLICATION_DATA
        ) {
            return Err(RecordError::UnexpectedMessage);
        }
        Ok(inner_type)
    }
}

/// The protection of one direction of a TLS 1.2 connection: AES-GCM with an explicit nonce
/// (RFC 5288) or ChaCha20-Poly1305 with an XORed sequence number (RFC 7905).
#[derive(Debug)]
pub struct Tls12RecordProtection {
    aead: RecordAead,
    kind: AeadKind,
    iv: [u8; AEAD_NONCE_BYTES],
    sequence: u64,
}

impl Tls12RecordProtection {
    /// Protection with `key` and the key block's `iv`: 4 bytes for AES-GCM, 12 for
    /// ChaCha20-Poly1305. `None` when either length is wrong.
    pub fn new(kind: AeadKind, key: &[u8], iv: &[u8]) -> Option<Self> {
        let aead = RecordAead::new(kind, key)?;
        let mut full_iv = [0u8; AEAD_NONCE_BYTES];
        match kind {
            AeadKind::Aes128Gcm | AeadKind::Aes256Gcm if iv.len() == TLS12_GCM_FIXED_IV_BYTES => {
                full_iv[..TLS12_GCM_FIXED_IV_BYTES].copy_from_slice(iv);
            }
            AeadKind::ChaCha20Poly1305 if iv.len() == AEAD_NONCE_BYTES => full_iv.copy_from_slice(iv),
            _ => return None,
        }
        Some(Self { aead, kind, iv: full_iv, sequence: 0 })
    }

    /// The fixed IV length a suite with `kind` takes from the key block.
    pub fn iv_bytes(kind: AeadKind) -> usize {
        match kind {
            AeadKind::Aes128Gcm | AeadKind::Aes256Gcm => TLS12_GCM_FIXED_IV_BYTES,
            AeadKind::ChaCha20Poly1305 => AEAD_NONCE_BYTES,
        }
    }

    fn uses_explicit_nonce(&self) -> bool {
        self.kind != AeadKind::ChaCha20Poly1305
    }

    fn nonce_for(&self, explicit: &[u8; TLS12_EXPLICIT_NONCE_BYTES]) -> [u8; AEAD_NONCE_BYTES] {
        if self.uses_explicit_nonce() {
            let mut nonce = self.iv;
            nonce[TLS12_GCM_FIXED_IV_BYTES..].copy_from_slice(explicit);
            nonce
        } else {
            sequence_nonce(&self.iv, self.sequence)
        }
    }

    fn additional_data(&self, content_type: u8, plaintext_length: usize) -> [u8; 13] {
        let mut data = [0u8; 13];
        data[..8].copy_from_slice(&self.sequence.to_be_bytes());
        data[8] = content_type;
        data[9..11].copy_from_slice(&version::TLS12.to_be_bytes());
        data[11..].copy_from_slice(&(plaintext_length as u16).to_be_bytes());
        data
    }

    fn advance(&mut self) -> Result<(), RecordError> {
        self.sequence = self.sequence.checked_add(1).ok_or(RecordError::SequenceExhausted)?;
        Ok(())
    }

    /// One protected record holding `fragment` as `content_type`, header included.
    pub fn seal(&mut self, content_type: u8, fragment: &[u8]) -> Result<Vec<u8>, RecordError> {
        if fragment.len() > MAX_PLAINTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        let explicit = self.sequence.to_be_bytes();
        let nonce = self.nonce_for(&explicit);
        let additional_data = self.additional_data(content_type, fragment.len());
        let explicit_bytes = if self.uses_explicit_nonce() { TLS12_EXPLICIT_NONCE_BYTES } else { 0 };
        let header = RecordHeader {
            content_type,
            legacy_version: version::TLS12,
            length: explicit_bytes + fragment.len() + AEAD_TAG_BYTES,
        }
        .encode();
        let mut record = Vec::with_capacity(
            RECORD_HEADER_BYTES + TLS12_EXPLICIT_NONCE_BYTES + fragment.len() + AEAD_TAG_BYTES,
        );
        record.extend_from_slice(&header);
        if self.uses_explicit_nonce() {
            record.extend_from_slice(&explicit);
        }
        let body_start = record.len();
        record.extend_from_slice(fragment);
        let tag = self.aead.seal_in_place(&nonce, &additional_data, &mut record[body_start..]);
        record.extend_from_slice(&tag);
        self.advance()?;
        Ok(record)
    }

    /// Opens the protected body `payload` of a record with `header` in place, leaving the
    /// plaintext in `payload`.
    pub fn open(&mut self, header: &RecordHeader, payload: &mut Vec<u8>) -> Result<(), RecordError> {
        if payload.len() > MAX_TLS12_CIPHERTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        let mut explicit = [0u8; TLS12_EXPLICIT_NONCE_BYTES];
        if self.uses_explicit_nonce() {
            if payload.len() < TLS12_EXPLICIT_NONCE_BYTES + AEAD_TAG_BYTES {
                return Err(RecordError::BadRecordMac);
            }
            explicit.copy_from_slice(&payload[..TLS12_EXPLICIT_NONCE_BYTES]);
            payload.drain(..TLS12_EXPLICIT_NONCE_BYTES);
        }
        let tag = split_tag(payload)?;
        if payload.len() > MAX_PLAINTEXT_BYTES {
            return Err(RecordError::Overflow);
        }
        let nonce = self.nonce_for(&explicit);
        let additional_data = self.additional_data(header.content_type, payload.len());
        self.aead
            .open_in_place(&nonce, &additional_data, payload, &tag)
            .map_err(|_| RecordError::BadRecordMac)?;
        self.advance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> TrafficKeys {
        TrafficKeys { key: vec![9u8; 16], iv: [3u8; AEAD_NONCE_BYTES] }
    }

    fn header_of(record: &[u8]) -> RecordHeader {
        let mut bytes = [0u8; RECORD_HEADER_BYTES];
        bytes.copy_from_slice(&record[..RECORD_HEADER_BYTES]);
        RecordHeader::parse(&bytes, MAX_TLS12_CIPHERTEXT_BYTES).unwrap()
    }

    #[test]
    fn tls13_records_round_trip_and_hide_their_type() {
        let mut writer = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys()).unwrap();
        let mut reader = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys()).unwrap();
        for message in [&b"first"[..], b"", b"third record"] {
            let record = writer.seal(content_type::HANDSHAKE, message).unwrap();
            assert_eq!(record[0], content_type::APPLICATION_DATA);
            let header = header_of(&record);
            let mut payload = record[RECORD_HEADER_BYTES..].to_vec();
            assert_eq!(reader.open(&header, &mut payload), Ok(content_type::HANDSHAKE));
            assert_eq!(payload, message);
        }
    }

    #[test]
    fn a_changed_byte_or_a_replayed_record_fails_authentication() {
        let mut writer = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys()).unwrap();
        let record = writer.seal(content_type::APPLICATION_DATA, b"data").unwrap();
        let header = header_of(&record);
        let mut reader = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys()).unwrap();
        let mut tampered = record[RECORD_HEADER_BYTES..].to_vec();
        tampered[0] ^= 1;
        assert_eq!(reader.open(&header, &mut tampered), Err(RecordError::BadRecordMac));
        let mut replayed = record[RECORD_HEADER_BYTES..].to_vec();
        assert_eq!(reader.open(&header, &mut replayed), Err(RecordError::BadRecordMac));
    }

    #[test]
    fn short_and_oversized_records_are_refused() {
        let mut reader = Tls13RecordProtection::new(AeadKind::Aes128Gcm, &keys()).unwrap();
        let header = RecordHeader { content_type: 23, legacy_version: 0x0303, length: 3 };
        assert_eq!(reader.open(&header, &mut vec![1, 2, 3]), Err(RecordError::BadRecordMac));
        let mut oversized = vec![0u8; MAX_TLS13_CIPHERTEXT_BYTES + 1];
        assert_eq!(reader.open(&header, &mut oversized), Err(RecordError::Overflow));
        assert_eq!(
            RecordHeader::parse(&[23, 3, 3, 0x48, 0x01], MAX_TLS13_CIPHERTEXT_BYTES),
            Err(RecordError::Overflow)
        );
        assert_eq!(RecordHeader::parse(&[0x47, 0x45, 0x54, 0x20, 0x2f], 100), Err(RecordError::MalformedHeader));
    }

    #[test]
    fn tls12_records_round_trip_for_both_nonce_styles() {
        for (kind, key, iv) in [
            (AeadKind::Aes128Gcm, vec![5u8; 16], vec![1u8; 4]),
            (AeadKind::Aes256Gcm, vec![5u8; 32], vec![1u8; 4]),
            (AeadKind::ChaCha20Poly1305, vec![5u8; 32], vec![1u8; 12]),
        ] {
            let mut writer = Tls12RecordProtection::new(kind, &key, &iv).unwrap();
            let mut reader = Tls12RecordProtection::new(kind, &key, &iv).unwrap();
            for message in [&b"one"[..], b"two"] {
                let record = writer.seal(content_type::APPLICATION_DATA, message).unwrap();
                let header = header_of(&record);
                let mut payload = record[RECORD_HEADER_BYTES..].to_vec();
                assert_eq!(reader.open(&header, &mut payload), Ok(()));
                assert_eq!(payload, message);
            }
        }
        assert!(Tls12RecordProtection::new(AeadKind::Aes128Gcm, &[0; 16], &[0; 12]).is_none());
    }
}
