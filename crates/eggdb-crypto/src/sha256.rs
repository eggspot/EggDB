/// The length of a SHA-256 digest in bytes.
pub const SHA256_DIGEST_BYTES: usize = 32;
/// The length of a SHA-256 message block in bytes.
pub const SHA256_BLOCK_BYTES: usize = 64;

const ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const INITIAL_STATE: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A SHA-256 hash in progress (FIPS 180-4): feed it with [`Sha256::update`] and read the digest
/// with [`Sha256::finish`].
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; SHA256_BLOCK_BYTES],
    filled: usize,
    total_bytes: u64,
}

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sha256")
            .field("total_bytes", &self.total_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A hash of nothing yet.
    pub fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            block: [0; SHA256_BLOCK_BYTES],
            filled: 0,
            total_bytes: 0,
        }
    }

    /// Adds `bytes` to the message.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(bytes.len() as u64);
        if self.filled > 0 {
            let taken = bytes.len().min(SHA256_BLOCK_BYTES - self.filled);
            let (head, rest) = bytes.split_at(taken);
            self.block[self.filled..self.filled + taken].copy_from_slice(head);
            self.filled += taken;
            bytes = rest;
            if self.filled < SHA256_BLOCK_BYTES {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<SHA256_BLOCK_BYTES>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// The digest of everything added.
    pub fn finish(mut self) -> [u8; SHA256_DIGEST_BYTES] {
        let bit_length = self.total_bytes.wrapping_mul(8);
        let mut padding = [0u8; SHA256_BLOCK_BYTES + 8];
        padding[0] = 0x80;
        let zeros = (SHA256_BLOCK_BYTES + 56 - self.filled - 1) % SHA256_BLOCK_BYTES;
        let padded = 1 + zeros;
        padding[padded..padded + 8].copy_from_slice(&bit_length.to_be_bytes());
        let total = self.total_bytes;
        self.update(&padding[..padded + 8]);
        self.total_bytes = total;
        let mut digest = [0u8; SHA256_DIGEST_BYTES];
        for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *bytes = word.to_be_bytes();
        }
        digest
    }
}

fn compress(state: &mut [u32; 8], block: &[u8; SHA256_BLOCK_BYTES]) {
    let mut schedule = [0u32; 64];
    for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for t in 16..64 {
        let s0 = schedule[t - 15].rotate_right(7)
            ^ schedule[t - 15].rotate_right(18)
            ^ (schedule[t - 15] >> 3);
        let s1 = schedule[t - 2].rotate_right(17)
            ^ schedule[t - 2].rotate_right(19)
            ^ (schedule[t - 2] >> 10);
        schedule[t] = schedule[t - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[t - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (constant, word) in ROUND_CONSTANTS.iter().zip(schedule) {
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ (!e & g);
        let first = h
            .wrapping_add(sum1)
            .wrapping_add(choice)
            .wrapping_add(*constant)
            .wrapping_add(word);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let second = sum0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(first);
        d = c;
        c = b;
        b = a;
        a = first.wrapping_add(second);
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

/// The SHA-256 digest of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; SHA256_DIGEST_BYTES] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finish()
}
