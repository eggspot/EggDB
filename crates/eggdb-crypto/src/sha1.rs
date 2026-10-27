/// The length of a SHA-1 digest in bytes.
pub const SHA1_DIGEST_BYTES: usize = 20;
/// The length of a SHA-1 message block in bytes.
pub const SHA1_BLOCK_BYTES: usize = 64;

const INITIAL_STATE: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

/// A SHA-1 hash in progress (FIPS 180-4, section 6.1): feed it with [`Sha1::update`] and read
/// the digest with [`Sha1::finish`].
///
/// SHA-1 is broken for collision resistance; EggDB uses it only where a protocol requires it,
/// such as MySQL's `mysql_native_password` authentication.
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; SHA1_BLOCK_BYTES],
    filled: usize,
    total_bytes: u64,
}

impl std::fmt::Debug for Sha1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sha1")
            .field("total_bytes", &self.total_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    /// A hash of nothing yet.
    pub fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            block: [0; SHA1_BLOCK_BYTES],
            filled: 0,
            total_bytes: 0,
        }
    }

    /// Adds `bytes` to the message.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(bytes.len() as u64);
        if self.filled > 0 {
            let taken = bytes.len().min(SHA1_BLOCK_BYTES - self.filled);
            let (head, rest) = bytes.split_at(taken);
            self.block[self.filled..self.filled + taken].copy_from_slice(head);
            self.filled += taken;
            bytes = rest;
            if self.filled < SHA1_BLOCK_BYTES {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<SHA1_BLOCK_BYTES>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// The digest of everything added.
    pub fn finish(mut self) -> [u8; SHA1_DIGEST_BYTES] {
        let bit_length = self.total_bytes.wrapping_mul(8);
        let mut padding = [0u8; SHA1_BLOCK_BYTES + 8];
        padding[0] = 0x80;
        let zeros = (SHA1_BLOCK_BYTES + 56 - self.filled - 1) % SHA1_BLOCK_BYTES;
        let padded = 1 + zeros;
        padding[padded..padded + 8].copy_from_slice(&bit_length.to_be_bytes());
        let total = self.total_bytes;
        self.update(&padding[..padded + 8]);
        self.total_bytes = total;
        let mut digest = [0u8; SHA1_DIGEST_BYTES];
        for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *bytes = word.to_be_bytes();
        }
        digest
    }
}

fn compress(state: &mut [u32; 5], block: &[u8; SHA1_BLOCK_BYTES]) {
    let mut schedule = [0u32; 80];
    for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for t in 16..80 {
        schedule[t] = (schedule[t - 3] ^ schedule[t - 8] ^ schedule[t - 14] ^ schedule[t - 16])
            .rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (t, word) in schedule.iter().enumerate() {
        let (mixed, constant) = match t {
            0..=19 => ((b & c) | (!b & d), 0x5A827999),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
            _ => (b ^ c ^ d, 0xCA62C1D6),
        };
        let next = a
            .rotate_left(5)
            .wrapping_add(mixed)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = next;
    }
    for (word, add) in state.iter_mut().zip([a, b, c, d, e]) {
        *word = word.wrapping_add(add);
    }
}

/// The SHA-1 digest of `bytes`.
pub fn sha1(bytes: &[u8]) -> [u8; SHA1_DIGEST_BYTES] {
    let mut hash = Sha1::new();
    hash.update(bytes);
    hash.finish()
}
