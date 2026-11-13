//! Page encryption in builds without `std`: none yet, as the AES and HKDF code lives in
//! `eggdb-crypto`, which needs the standard library (D1082). The type cannot be made, so a
//! file's cipher is always absent and an encrypted file is refused when its key is given.

use crate::error::StorageError;
use core::convert::Infallible;
use core::fmt;
use eggdb_platform_core::{Error, ErrorKind};

/// The length of the key a database is encrypted with.
pub const PAGE_KEY_BYTES: usize = 32;
/// The length of the value that tells a file's key from others, kept in the superblock.
pub const PAGE_KEY_CHECK_BYTES: usize = 16;

/// Encrypts and decrypts page bodies; without `std` it cannot be made.
#[derive(Clone)]
pub struct PageCipher {
    never: Infallible,
}

impl fmt::Debug for PageCipher {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.never {}
    }
}

impl PageCipher {
    /// Refuses: pages are not encrypted without `std`.
    pub fn new(_key: &[u8]) -> Result<Self, StorageError> {
        Err(StorageError::Io(Error::new(ErrorKind::Unsupported)))
    }

    /// The value the superblock keeps to tell this key from others.
    pub fn key_check(&self) -> [u8; PAGE_KEY_CHECK_BYTES] {
        match self.never {}
    }

    /// Whether this is the key whose check value is `stored`.
    pub fn matches(&self, _stored: &[u8; PAGE_KEY_CHECK_BYTES]) -> bool {
        match self.never {}
    }

    /// XORs `body` with the keystream of page `page_id` at `lsn`.
    pub fn apply(&self, _body: &mut [u8], _page_id: u64, _lsn: u64) {
        match self.never {}
    }
}
