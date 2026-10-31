use crate::aead::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AeadError};
use crate::aes::{AES_BLOCK_BYTES, Aes, AesKeyLengthError};
use crate::constant_time::constant_time_eq;

const REDUCTION: u128 = 0xe1 << 120;
const COUNTER_BATCH: usize = 4;

fn gf128_multiply(x: u128, y: u128) -> u128 {
    let mut product = 0u128;
    let mut shifted = y;
    for bit in (0..128).rev() {
        let take = 0u128.wrapping_sub((x >> bit) & 1);
        product ^= shifted & take;
        let carry = 0u128.wrapping_sub(shifted & 1);
        shifted = (shifted >> 1) ^ (REDUCTION & carry);
    }
    product
}

struct Ghash {
    key: u128,
    accumulator: u128,
}

impl Ghash {
    fn absorb_padded(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(AES_BLOCK_BYTES) {
            let mut block = [0u8; AES_BLOCK_BYTES];
            block[..chunk.len()].copy_from_slice(chunk);
            self.accumulator = gf128_multiply(self.accumulator ^ u128::from_be_bytes(block), self.key);
        }
    }
}

/// AES in Galois/Counter Mode (NIST SP 800-38D) with 96-bit nonces and 128-bit tags, as TLS uses
/// it. GHASH multiplies bit by bit with masks, so its time depends only on the lengths.
#[derive(Clone)]
pub struct AesGcm {
    cipher: Aes,
    hash_key: u128,
}

impl std::fmt::Debug for AesGcm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AesGcm").finish_non_exhaustive()
    }
}

impl AesGcm {
    /// The AEAD under `key`, which is 16, 24 or 32 bytes long.
    pub fn new(key: &[u8]) -> Result<Self, AesKeyLengthError> {
        let cipher = Aes::new(key)?;
        let mut zero = [0u8; AES_BLOCK_BYTES];
        cipher.encrypt_block(&mut zero);
        Ok(Self {
            cipher,
            hash_key: u128::from_be_bytes(zero),
        })
    }

    fn counter_block(nonce: &[u8; AEAD_NONCE_BYTES], counter: u32) -> [u8; AES_BLOCK_BYTES] {
        let mut block = [0u8; AES_BLOCK_BYTES];
        block[..AEAD_NONCE_BYTES].copy_from_slice(nonce);
        block[AEAD_NONCE_BYTES..].copy_from_slice(&counter.to_be_bytes());
        block
    }

    fn apply_keystream(&self, nonce: &[u8; AEAD_NONCE_BYTES], data: &mut [u8]) {
        let mut counter = 2u32;
        for batch in data.chunks_mut(AES_BLOCK_BYTES * COUNTER_BATCH) {
            let mut keystream = [[0u8; AES_BLOCK_BYTES]; COUNTER_BATCH];
            let blocks = batch.len().div_ceil(AES_BLOCK_BYTES);
            for block in keystream.iter_mut().take(blocks) {
                *block = Self::counter_block(nonce, counter);
                counter = counter.wrapping_add(1);
            }
            self.cipher.encrypt_blocks(&mut keystream[..blocks]);
            for (byte, key) in batch.iter_mut().zip(keystream.as_flattened()) {
                *byte ^= key;
            }
        }
    }

    fn tag(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        ciphertext: &[u8],
    ) -> [u8; AEAD_TAG_BYTES] {
        let mut ghash = Ghash {
            key: self.hash_key,
            accumulator: 0,
        };
        ghash.absorb_padded(associated_data);
        ghash.absorb_padded(ciphertext);
        let lengths = (((associated_data.len() as u128) * 8) << 64) | ((ciphertext.len() as u128) * 8);
        ghash.absorb_padded(&lengths.to_be_bytes());
        let mut mask = Self::counter_block(nonce, 1);
        self.cipher.encrypt_block(&mut mask);
        (ghash.accumulator ^ u128::from_be_bytes(mask)).to_be_bytes()
    }

    /// Encrypts `buffer` in place under `nonce` and returns the tag over `associated_data` and
    /// the ciphertext.
    pub fn seal_in_place(
        &self,
        nonce: &[u8; AEAD_NONCE_BYTES],
        associated_data: &[u8],
        buffer: &mut [u8],
    ) -> [u8; AEAD_TAG_BYTES] {
        self.apply_keystream(nonce, buffer);
        self.tag(nonce, associated_data, buffer)
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
        let expected = self.tag(nonce, associated_data, buffer);
        if !constant_time_eq(&expected, tag) {
            return Err(AeadError);
        }
        self.apply_keystream(nonce, buffer);
        Ok(())
    }
}
