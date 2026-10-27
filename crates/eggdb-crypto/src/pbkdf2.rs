use crate::hmac::HmacSha256;
use crate::sha256::SHA256_DIGEST_BYTES;

/// Fills `output` with PBKDF2 (RFC 8018) of `password` and `salt` over `iterations` rounds of
/// HMAC-SHA-256. An iteration count of 0 is taken as 1; an output longer than PBKDF2 allows
/// (more than 2^32 - 1 blocks) is filled only up to that limit, which no caller reaches.
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, output: &mut [u8]) {
    let keyed = HmacSha256::new(password);
    for (index, chunk) in output.chunks_mut(SHA256_DIGEST_BYTES).enumerate() {
        let Ok(block_number) = u32::try_from(index + 1) else {
            return;
        };
        let mut mac = keyed.clone();
        mac.update(salt);
        mac.update(&block_number.to_be_bytes());
        let mut round = mac.finish();
        let mut block = round;
        for _ in 1..iterations.max(1) {
            let mut mac = keyed.clone();
            mac.update(&round);
            round = mac.finish();
            for (byte, next) in block.iter_mut().zip(round) {
                *byte ^= next;
            }
        }
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}
