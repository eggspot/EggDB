use crate::hash::{HashFunction, MAX_HASH_BLOCK_BYTES};
use crate::sha256::{SHA256_DIGEST_BYTES, Sha256};

const INNER_PAD: u8 = 0x36;
const OUTER_PAD: u8 = 0x5c;

/// An HMAC computation in progress (RFC 2104) over the hash function `H`: the key is fixed when
/// it starts, the message is fed with [`Hmac::update`].
#[derive(Clone)]
pub struct Hmac<H: HashFunction> {
    inner: H,
    outer: H,
}

/// HMAC-SHA-256 (RFC 2104, RFC 4231).
pub type HmacSha256 = Hmac<Sha256>;

impl<H: HashFunction> std::fmt::Debug for Hmac<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hmac").finish_non_exhaustive()
    }
}

impl<H: HashFunction> Hmac<H> {
    /// Starts an HMAC with `key`, of any length.
    pub fn new(key: &[u8]) -> Self {
        let mut block_key = [0u8; MAX_HASH_BLOCK_BYTES];
        if key.len() > H::BLOCK_BYTES {
            let digest = H::digest(key);
            block_key[..H::DIGEST_BYTES].copy_from_slice(digest.as_ref());
        } else {
            block_key[..key.len()].copy_from_slice(key);
        }
        let block_key = &block_key[..H::BLOCK_BYTES];
        let mut pad = [0u8; MAX_HASH_BLOCK_BYTES];
        let mut inner = H::new();
        for (pad, key) in pad.iter_mut().zip(block_key) {
            *pad = key ^ INNER_PAD;
        }
        inner.update(&pad[..H::BLOCK_BYTES]);
        let mut outer = H::new();
        for (pad, key) in pad.iter_mut().zip(block_key) {
            *pad = key ^ OUTER_PAD;
        }
        outer.update(&pad[..H::BLOCK_BYTES]);
        Self { inner, outer }
    }

    /// Adds `bytes` to the message.
    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    /// The authentication code of the message.
    pub fn finish(self) -> H::Digest {
        let inner = self.inner.finish();
        let mut outer = self.outer;
        outer.update(inner.as_ref());
        outer.finish()
    }
}

/// The HMAC of `message` under `key` with the hash function `H`.
pub fn hmac<H: HashFunction>(key: &[u8], message: &[u8]) -> H::Digest {
    let mut mac = Hmac::<H>::new(key);
    mac.update(message);
    mac.finish()
}

/// The HMAC-SHA-256 of `message` under `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; SHA256_DIGEST_BYTES] {
    hmac::<Sha256>(key, message)
}
