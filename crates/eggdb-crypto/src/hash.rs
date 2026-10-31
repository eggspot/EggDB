use crate::sha1::{SHA1_BLOCK_BYTES, SHA1_DIGEST_BYTES, Sha1};
use crate::sha256::{SHA256_BLOCK_BYTES, SHA256_DIGEST_BYTES, Sha256};
use crate::sha512::{
    SHA384_DIGEST_BYTES, SHA512_BLOCK_BYTES, SHA512_DIGEST_BYTES, Sha384, Sha512,
};

/// The largest message block of any [`HashFunction`] in this crate, in bytes.
pub const MAX_HASH_BLOCK_BYTES: usize = SHA512_BLOCK_BYTES;
/// The largest digest of any [`HashFunction`] in this crate, in bytes.
pub const MAX_HASH_DIGEST_BYTES: usize = SHA512_DIGEST_BYTES;

/// A hash function of the SHA family, so HMAC, HKDF and TLS can be written once for all of them.
pub trait HashFunction: Clone {
    /// The digest: a byte array of [`HashFunction::DIGEST_BYTES`] bytes.
    type Digest: AsRef<[u8]> + AsMut<[u8]> + Copy;
    /// The length of a message block in bytes.
    const BLOCK_BYTES: usize;
    /// The length of the digest in bytes.
    const DIGEST_BYTES: usize;

    /// A hash of nothing yet.
    fn new() -> Self;
    /// Adds `bytes` to the message.
    fn update(&mut self, bytes: &[u8]);
    /// The digest of everything added.
    fn finish(self) -> Self::Digest;

    /// The digest of `bytes`.
    fn digest(bytes: &[u8]) -> Self::Digest {
        let mut hash = Self::new();
        hash.update(bytes);
        hash.finish()
    }
}

impl HashFunction for Sha1 {
    type Digest = [u8; SHA1_DIGEST_BYTES];
    const BLOCK_BYTES: usize = SHA1_BLOCK_BYTES;
    const DIGEST_BYTES: usize = SHA1_DIGEST_BYTES;

    fn new() -> Self {
        Sha1::new()
    }

    fn update(&mut self, bytes: &[u8]) {
        Sha1::update(self, bytes);
    }

    fn finish(self) -> Self::Digest {
        Sha1::finish(self)
    }
}

impl HashFunction for Sha256 {
    type Digest = [u8; SHA256_DIGEST_BYTES];
    const BLOCK_BYTES: usize = SHA256_BLOCK_BYTES;
    const DIGEST_BYTES: usize = SHA256_DIGEST_BYTES;

    fn new() -> Self {
        Sha256::new()
    }

    fn update(&mut self, bytes: &[u8]) {
        Sha256::update(self, bytes);
    }

    fn finish(self) -> Self::Digest {
        Sha256::finish(self)
    }
}

impl HashFunction for Sha384 {
    type Digest = [u8; SHA384_DIGEST_BYTES];
    const BLOCK_BYTES: usize = SHA512_BLOCK_BYTES;
    const DIGEST_BYTES: usize = SHA384_DIGEST_BYTES;

    fn new() -> Self {
        Sha384::new()
    }

    fn update(&mut self, bytes: &[u8]) {
        Sha384::update(self, bytes);
    }

    fn finish(self) -> Self::Digest {
        Sha384::finish(self)
    }
}

impl HashFunction for Sha512 {
    type Digest = [u8; SHA512_DIGEST_BYTES];
    const BLOCK_BYTES: usize = SHA512_BLOCK_BYTES;
    const DIGEST_BYTES: usize = SHA512_DIGEST_BYTES;

    fn new() -> Self {
        Sha512::new()
    }

    fn update(&mut self, bytes: &[u8]) {
        Sha512::update(self, bytes);
    }

    fn finish(self) -> Self::Digest {
        Sha512::finish(self)
    }
}
