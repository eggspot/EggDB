use crate::chacha20::{CHACHA20_KEY_BYTES, ChaCha20};
use crate::constant_time::constant_time_eq;
use crate::poly1305::{POLY1305_KEY_BYTES, Poly1305};

/// The length of every AEAD tag in this crate, in bytes.
pub const AEAD_TAG_BYTES: usize = 16;
/// The length of every AEAD nonce in this crate, in bytes.
pub const AEAD_NONCE_BYTES: usize = 12;

/// A sealed message did not open: its tag does not match its key, nonce, associated data and
/// ciphertext, so it was altered or never sealed under this key. Nothing of it is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AeadError;

impl std::fmt::Display for AeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the message failed authentication")
    }
}

impl std::error::Error for AeadError {}

/// AEAD_CHACHA20_POLY1305 (RFC 8439 section 2.8): ChaCha20 encryption with a Poly1305 tag over
/// the associated data and the ciphertext.
#[derive(Clone)]
pub struct ChaCha20Poly1305 {
    key: [u8; CHACHA20_KEY_BYTES],
}

impl std::fmt::Debug for ChaCha20Poly1305 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChaCha20Poly1305").finish_non_exhaustive()
    }
}

impl ChaCha20Poly1305 {
    /// The AEAD under `key`.
    pub fn new(key: &[u8; CHACHA20_KEY_BYTES]) -> Self {
        Self { key: *key }
    }

    fn tag(
        cipher: &ChaCha20,
        associated_data: &[u8],
        ciphertext: &[u8],
    ) -> [u8; AEAD_TAG_BYTES] {
        let mut one_time_key = [0u8; POLY1305_KEY_BYTES];
        one_time_key.copy_from_slice(&cipher.block(0)[..POLY1305_KEY_BYTES]);
        let mut mac = Poly1305::new(&one_time_key);
        mac.update(associated_data);
        mac.pad_to_block();
        mac.update(ciphertext);
        mac.pad_to_block();
        mac.update(&(associated_data.len() as u64).to_le_bytes());
        mac.update(&(ciphertext.len() as u64).to_le_bytes());
        mac.finish()
    }

    /// Encrypts `buffer` in place under `nonce` and returns the tag over `associated_data` and
    /// the ciphertext.
    pub fn seal_in_place(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        buffer: &mut [u8],
    ) -> [u8; AEAD_TAG_BYTES] {
        let cipher = ChaCha20::new(&self.key, nonce);
        cipher.apply_keystream(1, buffer);
        Self::tag(&cipher, associated_data, buffer)
    }

    /// Checks `tag` and, only when it matches, decrypts `buffer` in place under `nonce`. On a
    /// mismatch `buffer` is left as the ciphertext it was.
    pub fn open_in_place(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        buffer: &mut [u8],
        tag: &[u8; AEAD_TAG_BYTES],
    ) -> Result<(), AeadError> {
        let cipher = ChaCha20::new(&self.key, nonce);
        let expected = Self::tag(&cipher, associated_data, buffer);
        if !constant_time_eq(&expected, tag) {
            return Err(AeadError);
        }
        cipher.apply_keystream(1, buffer);
        Ok(())
    }
}
