use crate::error::StorageError;
use eggdb_crypto::{Aes, Sha256, constant_time_eq, hkdf_expand, hkdf_extract, hmac_sha256};
use std::fmt;

/// The length of the key a database is encrypted with.
pub const PAGE_KEY_BYTES: usize = 32;
/// The length of the value that tells a file's key from others, kept in the superblock.
pub const PAGE_KEY_CHECK_BYTES: usize = 16;
const BLOCK: usize = 16;

/// Encrypts and decrypts page bodies with AES-256 in counter mode (NIST SP 800-38A), keyed by
/// HKDF-SHA256 (RFC 5869) from the database key. Each counter block is the page's LSN and its
/// page number with the block index, so no two committed page images share a keystream.
#[derive(Clone)]
pub struct PageCipher {
    aes: Aes,
    key_check: [u8; PAGE_KEY_CHECK_BYTES],
}

impl fmt::Debug for PageCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageCipher").finish_non_exhaustive()
    }
}

impl PageCipher {
    /// The cipher of the database key `key`, [`PAGE_KEY_BYTES`] long.
    pub fn new(key: &[u8]) -> Result<Self, StorageError> {
        if key.len() != PAGE_KEY_BYTES {
            return Err(StorageError::WrongKey);
        }
        let pseudorandom = hkdf_extract::<Sha256>(b"EggDB page encryption", key);
        let mut page_key = [0u8; 32];
        hkdf_expand::<Sha256>(&pseudorandom, b"AES-256-CTR page key", &mut page_key)
            .map_err(|_| StorageError::WrongKey)?;
        let aes = Aes::new(&page_key).map_err(|_| StorageError::WrongKey)?;
        let mut key_check = [0u8; PAGE_KEY_CHECK_BYTES];
        key_check.copy_from_slice(
            &hmac_sha256(&pseudorandom, b"EggDB page key check")[..PAGE_KEY_CHECK_BYTES],
        );
        Ok(Self { aes, key_check })
    }

    /// The value the superblock keeps to tell this key from others.
    pub fn key_check(&self) -> [u8; PAGE_KEY_CHECK_BYTES] {
        self.key_check
    }

    /// Whether this is the key whose check value is `stored`.
    pub fn matches(&self, stored: &[u8; PAGE_KEY_CHECK_BYTES]) -> bool {
        constant_time_eq(&self.key_check, stored)
    }

    /// XORs `body` with the keystream of page `page_id` at `lsn`; the same call decrypts.
    pub fn apply(&self, body: &mut [u8], page_id: u64, lsn: u64) {
        let mut counter = [[0u8; BLOCK]; 64];
        let mut index = 0u64;
        for chunk in body.chunks_mut(BLOCK * counter.len()) {
            let blocks = chunk.len().div_ceil(BLOCK);
            for block in counter.iter_mut().take(blocks) {
                block[..8].copy_from_slice(&lsn.to_be_bytes());
                block[8..].copy_from_slice(&((page_id << 16) | (index & 0xffff)).to_be_bytes());
                index += 1;
            }
            self.aes.encrypt_blocks(&mut counter[..blocks]);
            for (byte, key) in chunk.iter_mut().zip(counter.as_flattened()) {
                *byte ^= key;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_round_trips_and_every_input_changes_the_keystream() {
        let cipher = PageCipher::new(&[7; 32]).unwrap();
        let plain: Vec<u8> = (0..4080u32).map(|value| value as u8).collect();
        let mut body = plain.clone();
        cipher.apply(&mut body, 5, 4096);
        assert_ne!(body, plain);
        let mut other_page = plain.clone();
        cipher.apply(&mut other_page, 6, 4096);
        let mut other_lsn = plain.clone();
        cipher.apply(&mut other_lsn, 5, 8192);
        assert_ne!(body, other_page);
        assert_ne!(body, other_lsn);
        let blocks: Vec<&[u8]> = body.chunks(16).collect();
        let distinct: std::collections::HashSet<Vec<u8>> = body
            .chunks(16)
            .zip(plain.chunks(16))
            .map(|(cipher, plain)| cipher.iter().zip(plain).map(|(a, b)| a ^ b).collect())
            .collect();
        assert_eq!(distinct.len(), blocks.len());
        cipher.apply(&mut body, 5, 4096);
        assert_eq!(body, plain);
    }

    #[test]
    fn keys_are_told_apart_by_their_check_values() {
        let first = PageCipher::new(&[1; 32]).unwrap();
        let second = PageCipher::new(&[2; 32]).unwrap();
        assert!(first.matches(&first.key_check()));
        assert!(!first.matches(&second.key_check()));
        assert!(PageCipher::new(&[1; 16]).is_err());
    }

    #[test]
    fn the_keystream_is_aes_256_ctr_under_the_derived_key() {
        let key = [9u8; 32];
        let cipher = PageCipher::new(&key).unwrap();
        let pseudorandom = hkdf_extract::<Sha256>(b"EggDB page encryption", &key);
        let mut page_key = [0u8; 32];
        hkdf_expand::<Sha256>(&pseudorandom, b"AES-256-CTR page key", &mut page_key).unwrap();
        let mut expected = [0u8; 16];
        expected[..8].copy_from_slice(&77u64.to_be_bytes());
        expected[8..].copy_from_slice(&((3u64 << 16) | 1).to_be_bytes());
        Aes::new(&page_key).unwrap().encrypt_block(&mut expected);
        let mut body = [0u8; 32];
        cipher.apply(&mut body, 3, 77);
        assert_eq!(&body[16..], &expected);
    }
}
