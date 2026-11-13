//! End-to-end encrypted sync (D1108): a device seals each change before it leaves and opens it
//! when it comes back, with a secret only the user's devices hold, so the server stores and
//! relays changes it cannot read. A sealed change's key is an HMAC-SHA-256 of the key, so the
//! server still keeps one row per key and the newest change still wins; its value is the
//! original key and value under AES-256-GCM, with a nonce derived from the change's unique
//! stamp and key, and the stamp is left as it is for the server to order changes by.

use crate::{Change, SyncError};
use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AesGcm, hmac_sha256};

/// Seals and opens changes with keys derived from one 32-byte secret.
pub struct Sealer {
    cipher: AesGcm,
    key_mac: [u8; 32],
    nonce_mac: [u8; 32],
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer")
    }
}

fn stamp_bytes(change: &Change) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20);
    bytes.extend_from_slice(&change.stamp.millis.to_be_bytes());
    bytes.extend_from_slice(&change.stamp.counter.to_be_bytes());
    bytes.extend_from_slice(&change.stamp.replica.to_be_bytes());
    bytes
}

impl Sealer {
    /// A sealer for `secret`, which every device of the user holds and the server never does.
    pub fn new(secret: &[u8; 32]) -> Result<Self, SyncError> {
        let encryption = hmac_sha256(secret, b"eggdb sync encryption");
        Ok(Self {
            cipher: AesGcm::new(&encryption).map_err(|_| SyncError::Malformed("the sync secret makes no AES-256 key"))?,
            key_mac: hmac_sha256(secret, b"eggdb sync keys"),
            nonce_mac: hmac_sha256(secret, b"eggdb sync nonces"),
        })
    }

    fn nonce(&self, change: &Change) -> [u8; AEAD_NONCE_BYTES] {
        let mut input = stamp_bytes(change);
        input.extend_from_slice(&change.key);
        let digest = hmac_sha256(&self.nonce_mac, &input);
        let mut nonce = [0u8; AEAD_NONCE_BYTES];
        nonce.copy_from_slice(&digest[..AEAD_NONCE_BYTES]);
        nonce
    }

    /// The key the server sees for `key`.
    pub fn sealed_key(&self, key: &[u8]) -> Vec<u8> {
        hmac_sha256(&self.key_mac, key).to_vec()
    }

    /// `change` as the server stores it: the key hidden, the key and value encrypted.
    pub fn seal(&self, change: &Change) -> Change {
        let nonce = self.nonce(change);
        let mut plain = Vec::with_capacity(change.key.len() + 5 + change.value.as_ref().map_or(0, Vec::len));
        plain.extend_from_slice(&u32::try_from(change.key.len()).unwrap_or(u32::MAX).to_be_bytes());
        plain.extend_from_slice(&change.key);
        match &change.value {
            Some(value) => {
                plain.push(1);
                plain.extend_from_slice(value);
            }
            None => plain.push(0),
        }
        let tag = self.cipher.seal_in_place(&nonce, &stamp_bytes(change), &mut plain);
        let mut sealed = nonce.to_vec();
        sealed.extend_from_slice(&tag);
        sealed.extend_from_slice(&plain);
        Change { key: self.sealed_key(&change.key), value: Some(sealed), stamp: change.stamp }
    }

    /// The change `sealed` holds, checked: a change sealed with another secret, or altered on
    /// the way, fails.
    pub fn open(&self, sealed: &Change) -> Result<Change, SyncError> {
        let bytes = sealed.value.as_deref().ok_or(SyncError::Malformed("a sealed change has no body"))?;
        if bytes.len() < AEAD_NONCE_BYTES + AEAD_TAG_BYTES + 5 {
            return Err(SyncError::Malformed("a sealed change is too short"));
        }
        let nonce: [u8; AEAD_NONCE_BYTES] = bytes[..AEAD_NONCE_BYTES].try_into().map_err(|_| SyncError::Malformed("a sealed change is damaged"))?;
        let tag: [u8; AEAD_TAG_BYTES] = bytes[AEAD_NONCE_BYTES..AEAD_NONCE_BYTES + AEAD_TAG_BYTES]
            .try_into()
            .map_err(|_| SyncError::Malformed("a sealed change is damaged"))?;
        let mut plain = bytes[AEAD_NONCE_BYTES + AEAD_TAG_BYTES..].to_vec();
        let shell = Change { key: Vec::new(), value: None, stamp: sealed.stamp };
        self.cipher
            .open_in_place(&nonce, &stamp_bytes(&shell), &mut plain, &tag)
            .map_err(|_| SyncError::Malformed("a sealed change does not open with this secret"))?;
        let length = plain
            .get(..4)
            .and_then(|raw| raw.try_into().ok())
            .map(u32::from_be_bytes)
            .and_then(|length| usize::try_from(length).ok())
            .ok_or(SyncError::Malformed("a sealed change is damaged"))?;
        let key = plain.get(4..4 + length).ok_or(SyncError::Malformed("a sealed change is damaged"))?.to_vec();
        let value = match plain.get(4 + length) {
            Some(1) => Some(plain[5 + length..].to_vec()),
            Some(0) => None,
            _ => return Err(SyncError::Malformed("a sealed change is damaged")),
        };
        if self.sealed_key(&key) != sealed.key {
            return Err(SyncError::Malformed("a sealed change was moved to another key"));
        }
        Ok(Change { key, value, stamp: sealed.stamp })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stamp;

    fn change(value: Option<&[u8]>) -> Change {
        Change { key: b"diary:1".to_vec(), value: value.map(<[u8]>::to_vec), stamp: Stamp { millis: 5, counter: 1, replica: 9 } }
    }

    #[test]
    fn a_sealed_change_opens_only_with_its_secret_and_unaltered() {
        let sealer = Sealer::new(&[7u8; 32]).unwrap();
        for original in [change(Some(b"dear diary")), change(None)] {
            let sealed = sealer.seal(&original);
            assert_ne!(sealed.key, original.key);
            assert!(!sealed.value.as_ref().unwrap().windows(10).any(|window| window == b"dear diary"));
            assert_eq!(sealer.open(&sealed).unwrap(), original);
            assert!(Sealer::new(&[8u8; 32]).unwrap().open(&sealed).is_err());
            let mut altered = sealed.clone();
            altered.stamp.millis += 1;
            assert!(sealer.open(&altered).is_err());
            let mut moved = sealed;
            moved.key = sealer.sealed_key(b"other");
            assert!(sealer.open(&moved).is_err());
        }
        assert_eq!(sealer.sealed_key(b"k"), sealer.sealed_key(b"k"));
    }
}
