use crate::sha256::{SHA256_BLOCK_BYTES, SHA256_DIGEST_BYTES, Sha256, sha256};

const INNER_PAD: u8 = 0x36;
const OUTER_PAD: u8 = 0x5c;

/// An HMAC-SHA-256 computation in progress (RFC 2104): the key is fixed when it starts, the
/// message is fed with [`HmacSha256::update`].
#[derive(Clone)]
pub struct HmacSha256 {
    inner: Sha256,
    outer: Sha256,
}

impl std::fmt::Debug for HmacSha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacSha256").finish_non_exhaustive()
    }
}

impl HmacSha256 {
    /// Starts an HMAC with `key`, of any length.
    pub fn new(key: &[u8]) -> Self {
        let mut block_key = [0u8; SHA256_BLOCK_BYTES];
        if key.len() > SHA256_BLOCK_BYTES {
            block_key[..SHA256_DIGEST_BYTES].copy_from_slice(&sha256(key));
        } else {
            block_key[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha256::new();
        let mut outer = Sha256::new();
        inner.update(&block_key.map(|byte| byte ^ INNER_PAD));
        outer.update(&block_key.map(|byte| byte ^ OUTER_PAD));
        Self { inner, outer }
    }

    /// Adds `bytes` to the message.
    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    /// The authentication code of the message.
    pub fn finish(self) -> [u8; SHA256_DIGEST_BYTES] {
        let inner = self.inner.finish();
        let mut outer = self.outer;
        outer.update(&inner);
        outer.finish()
    }
}

/// The HMAC-SHA-256 of `message` under `key`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; SHA256_DIGEST_BYTES] {
    let mut mac = HmacSha256::new(key);
    mac.update(message);
    mac.finish()
}
