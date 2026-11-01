/// The length of a ChaCha20 key in bytes.
pub const CHACHA20_KEY_BYTES: usize = 32;
/// The length of a ChaCha20 nonce in bytes (RFC 8439's 96-bit nonce).
pub const CHACHA20_NONCE_BYTES: usize = 12;
/// The length of one ChaCha20 keystream block in bytes.
pub const CHACHA20_BLOCK_BYTES: usize = 64;

const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

fn words<const N: usize>(bytes: &[u8]) -> [u32; N] {
    let mut words = [0u32; N];
    for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    words
}

/// The ChaCha20 cipher of RFC 8439 under one key and nonce: a keystream of 64-byte blocks
/// numbered by a 32-bit counter.
#[derive(Clone)]
pub struct ChaCha20 {
    key: [u32; 8],
    nonce: [u32; 3],
}

impl std::fmt::Debug for ChaCha20 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChaCha20").finish_non_exhaustive()
    }
}

impl ChaCha20 {
    /// The cipher under `key` and `nonce`.
    pub fn new(key: &[u8; CHACHA20_KEY_BYTES], nonce: &[u8; CHACHA20_NONCE_BYTES]) -> Self {
        Self {
            key: words(key),
            nonce: words(nonce),
        }
    }

    /// The keystream block numbered `counter` (RFC 8439 section 2.3).
    pub fn block(&self, counter: u32) -> [u8; CHACHA20_BLOCK_BYTES] {
        let mut initial = [0u32; 16];
        initial[..4].copy_from_slice(&CONSTANTS);
        initial[4..12].copy_from_slice(&self.key);
        initial[12] = counter;
        initial[13..].copy_from_slice(&self.nonce);
        let mut state = initial;
        for _ in 0..10 {
            quarter_round(&mut state, 0, 4, 8, 12);
            quarter_round(&mut state, 1, 5, 9, 13);
            quarter_round(&mut state, 2, 6, 10, 14);
            quarter_round(&mut state, 3, 7, 11, 15);
            quarter_round(&mut state, 0, 5, 10, 15);
            quarter_round(&mut state, 1, 6, 11, 12);
            quarter_round(&mut state, 2, 7, 8, 13);
            quarter_round(&mut state, 3, 4, 9, 14);
        }
        let mut block = [0u8; CHACHA20_BLOCK_BYTES];
        for ((bytes, word), start) in block
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(state)
            .zip(initial)
        {
            *bytes = word.wrapping_add(start).to_le_bytes();
        }
        block
    }

    /// Encrypts or decrypts `data` in place, starting at keystream block `counter` (RFC 8439
    /// section 2.4). The counter wraps after 2^32 blocks; callers keep messages far shorter.
    pub fn apply_keystream(&self, counter: u32, data: &mut [u8]) {
        for (index, chunk) in data.chunks_mut(CHACHA20_BLOCK_BYTES).enumerate() {
            let keystream = self.block(counter.wrapping_add(index as u32));
            for (byte, key) in chunk.iter_mut().zip(keystream) {
                *byte ^= key;
            }
        }
    }
}
