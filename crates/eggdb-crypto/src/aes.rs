/// The length of an AES block in bytes.
pub const AES_BLOCK_BYTES: usize = 16;

const MAX_ROUNDS: usize = 14;
const LANES: usize = 64;
const ROUND_CONSTANTS: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
const AFFINE_CONSTANT: u8 = 0x63;
const INVERSE_AFFINE_CONSTANT: u8 = 0x05;

/// The key given to AES is not 16, 24 or 32 bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AesKeyLengthError {
    /// The length that was given, in bytes.
    pub length: usize,
}

impl std::fmt::Display for AesKeyLengthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "an AES key is 16, 24 or 32 bytes long, not {}",
            self.length
        )
    }
}

impl std::error::Error for AesKeyLengthError {}

type Planes = [u64; 8];

fn gf_multiply(a: &Planes, b: &Planes) -> Planes {
    let mut product = [0u64; 15];
    for (i, a_bit) in a.iter().enumerate() {
        for (j, b_bit) in b.iter().enumerate() {
            product[i + j] ^= a_bit & b_bit;
        }
    }
    for k in (8..15).rev() {
        let high = product[k];
        product[k - 4] ^= high;
        product[k - 5] ^= high;
        product[k - 7] ^= high;
        product[k - 8] ^= high;
    }
    let mut reduced = [0u64; 8];
    reduced.copy_from_slice(&product[..8]);
    reduced
}

fn gf_inverse(x: &Planes) -> Planes {
    let x2 = gf_multiply(x, x);
    let x3 = gf_multiply(&x2, x);
    let x6 = gf_multiply(&x3, &x3);
    let x12 = gf_multiply(&x6, &x6);
    let x15 = gf_multiply(&x12, &x3);
    let x30 = gf_multiply(&x15, &x15);
    let x60 = gf_multiply(&x30, &x30);
    let x120 = gf_multiply(&x60, &x60);
    let x240 = gf_multiply(&x120, &x120);
    let x252 = gf_multiply(&x240, &x12);
    gf_multiply(&x252, &x2)
}

fn substitute_planes(planes: &Planes) -> Planes {
    let inverse = gf_inverse(planes);
    let mut out = [0u64; 8];
    for (bit, plane) in out.iter_mut().enumerate() {
        let constant = 0u64.wrapping_sub(u64::from((AFFINE_CONSTANT >> bit) & 1));
        *plane = inverse[bit]
            ^ inverse[(bit + 7) % 8]
            ^ inverse[(bit + 6) % 8]
            ^ inverse[(bit + 5) % 8]
            ^ inverse[(bit + 4) % 8]
            ^ constant;
    }
    out
}

fn sub_bytes(bytes: &mut [u8]) {
    for group in bytes.chunks_mut(LANES) {
        let mut planes = [0u64; 8];
        for (lane, byte) in group.iter().enumerate() {
            for (bit, plane) in planes.iter_mut().enumerate() {
                *plane |= u64::from((byte >> bit) & 1) << lane;
            }
        }
        let substituted = substitute_planes(&planes);
        for (lane, byte) in group.iter_mut().enumerate() {
            let mut value = 0u8;
            for (bit, plane) in substituted.iter().enumerate() {
                value |= (((plane >> lane) & 1) as u8) << bit;
            }
            *byte = value;
        }
    }
}

fn inverse_substitute_planes(planes: &Planes) -> Planes {
    let mut affine = [0u64; 8];
    for (bit, plane) in affine.iter_mut().enumerate() {
        let constant = 0u64.wrapping_sub(u64::from((INVERSE_AFFINE_CONSTANT >> bit) & 1));
        *plane = planes[(bit + 2) % 8] ^ planes[(bit + 5) % 8] ^ planes[(bit + 7) % 8] ^ constant;
    }
    gf_inverse(&affine)
}

fn inverse_sub_bytes(bytes: &mut [u8]) {
    for group in bytes.chunks_mut(LANES) {
        let mut planes = [0u64; 8];
        for (lane, byte) in group.iter().enumerate() {
            for (bit, plane) in planes.iter_mut().enumerate() {
                *plane |= u64::from((byte >> bit) & 1) << lane;
            }
        }
        let substituted = inverse_substitute_planes(&planes);
        for (lane, byte) in group.iter_mut().enumerate() {
            let mut value = 0u8;
            for (bit, plane) in substituted.iter().enumerate() {
                value |= (((plane >> lane) & 1) as u8) << bit;
            }
            *byte = value;
        }
    }
}

fn inverse_shift_rows(state: &mut [u8; AES_BLOCK_BYTES]) {
    let old = *state;
    for row in 1..4 {
        for column in 0..4 {
            state[row + 4 * column] = old[row + 4 * ((column + 4 - row) % 4)];
        }
    }
}

fn inverse_mix_columns(state: &mut [u8; AES_BLOCK_BYTES]) {
    for column in state.as_chunks_mut::<4>().0 {
        let even = double(double(column[0] ^ column[2]));
        let odd = double(double(column[1] ^ column[3]));
        column[0] ^= even;
        column[1] ^= odd;
        column[2] ^= even;
        column[3] ^= odd;
    }
    mix_columns(state);
}

fn double(byte: u8) -> u8 {
    (byte << 1) ^ (0x1b & 0u8.wrapping_sub(byte >> 7))
}

fn shift_rows(state: &mut [u8; AES_BLOCK_BYTES]) {
    let old = *state;
    for row in 1..4 {
        for column in 0..4 {
            state[row + 4 * column] = old[row + 4 * ((column + row) % 4)];
        }
    }
}

fn mix_columns(state: &mut [u8; AES_BLOCK_BYTES]) {
    for column in state.as_chunks_mut::<4>().0 {
        let [a0, a1, a2, a3] = *column;
        let all = a0 ^ a1 ^ a2 ^ a3;
        column[0] = a0 ^ all ^ double(a0 ^ a1);
        column[1] = a1 ^ all ^ double(a1 ^ a2);
        column[2] = a2 ^ all ^ double(a2 ^ a3);
        column[3] = a3 ^ all ^ double(a3 ^ a0);
    }
}

fn add_round_key(state: &mut [u8; AES_BLOCK_BYTES], round_key: &[u8; AES_BLOCK_BYTES]) {
    for (byte, key) in state.iter_mut().zip(round_key) {
        *byte ^= key;
    }
}

/// The AES block cipher (FIPS 197) under one key of 128, 192 or 256 bits. The S-box and its
/// inverse are computed, not looked up: each byte is inverted in GF(2^8) on bit planes shared by
/// up to 64 bytes, so no memory access and no branch depends on the key or the data.
///
/// Where the processor has AES instructions (AES-NI on x86-64, the Armv8 Cryptography Extension
/// on AArch64), detected once when the cipher is made, blocks go through them instead; they
/// take the same time for every key and block too.
#[derive(Clone)]
pub struct Aes {
    round_keys: [[u8; AES_BLOCK_BYTES]; MAX_ROUNDS + 1],
    rounds: usize,
    instructions: bool,
}

impl std::fmt::Debug for Aes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aes")
            .field("rounds", &self.rounds)
            .field("instructions", &self.instructions)
            .finish_non_exhaustive()
    }
}

impl Aes {
    /// The cipher under `key`, which is 16, 24 or 32 bytes long.
    pub fn new(key: &[u8]) -> Result<Self, AesKeyLengthError> {
        let key_words = match key.len() {
            16 => 4,
            24 => 6,
            32 => 8,
            length => return Err(AesKeyLengthError { length }),
        };
        let rounds = key_words + 6;
        let total_words = 4 * (rounds + 1);
        let mut words = [[0u8; 4]; 4 * (MAX_ROUNDS + 1)];
        for (word, chunk) in words.iter_mut().zip(key.as_chunks::<4>().0) {
            *word = *chunk;
        }
        for index in key_words..total_words {
            let mut temp = words[index - 1];
            if index % key_words == 0 {
                temp.rotate_left(1);
                sub_bytes(&mut temp);
                temp[0] ^= ROUND_CONSTANTS[index / key_words - 1];
            } else if key_words > 6 && index % key_words == 4 {
                sub_bytes(&mut temp);
            }
            let earlier = words[index - key_words];
            for (byte, (before, mixed)) in words[index].iter_mut().zip(earlier.iter().zip(temp)) {
                *byte = before ^ mixed;
            }
        }
        let mut round_keys = [[0u8; AES_BLOCK_BYTES]; MAX_ROUNDS + 1];
        for (round_key, four) in round_keys
            .iter_mut()
            .zip(words[..total_words].as_chunks::<4>().0)
        {
            round_key.copy_from_slice(four.as_flattened());
        }
        Ok(Self {
            round_keys,
            rounds,
            instructions: instructions::available(),
        })
    }

    /// Whether this cipher runs on the processor's AES instructions.
    pub fn uses_processor_instructions(&self) -> bool {
        self.instructions
    }

    /// Encrypts one block in place.
    pub fn encrypt_block(&self, block: &mut [u8; AES_BLOCK_BYTES]) {
        self.encrypt_blocks(std::slice::from_mut(block));
    }

    /// Encrypts every block of `blocks` in place, four at a time through the shared S-box.
    pub fn encrypt_blocks(&self, blocks: &mut [[u8; AES_BLOCK_BYTES]]) {
        if self.instructions && instructions::encrypt_blocks(&self.round_keys, self.rounds, blocks) {
            return;
        }
        self.encrypt_blocks_computed(blocks);
    }

    fn encrypt_blocks_computed(&self, blocks: &mut [[u8; AES_BLOCK_BYTES]]) {
        for group in blocks.chunks_mut(LANES / AES_BLOCK_BYTES) {
            for block in group.iter_mut() {
                add_round_key(block, &self.round_keys[0]);
            }
            for round in 1..=self.rounds {
                sub_bytes(group.as_flattened_mut());
                for block in group.iter_mut() {
                    shift_rows(block);
                    if round < self.rounds {
                        mix_columns(block);
                    }
                    add_round_key(block, &self.round_keys[round]);
                }
            }
        }
    }

    /// Decrypts one block in place with FIPS 197's inverse cipher.
    pub fn decrypt_block(&self, block: &mut [u8; AES_BLOCK_BYTES]) {
        if self.instructions && instructions::decrypt_block(&self.round_keys, self.rounds, block) {
            return;
        }
        self.decrypt_block_computed(block);
    }

    fn decrypt_block_computed(&self, block: &mut [u8; AES_BLOCK_BYTES]) {
        add_round_key(block, &self.round_keys[self.rounds]);
        for round in (1..self.rounds).rev() {
            inverse_shift_rows(block);
            inverse_sub_bytes(block);
            add_round_key(block, &self.round_keys[round]);
            inverse_mix_columns(block);
        }
        inverse_shift_rows(block);
        inverse_sub_bytes(block);
        add_round_key(block, &self.round_keys[0]);
    }
}

type RoundKeys = [[u8; AES_BLOCK_BYTES]; MAX_ROUNDS + 1];

#[cfg(target_arch = "x86_64")]
mod instructions {
    use super::{AES_BLOCK_BYTES, RoundKeys};
    use std::arch::x86_64::{
        __m128i, _mm_aesdec_si128, _mm_aesdeclast_si128, _mm_aesenc_si128, _mm_aesenclast_si128,
        _mm_aesimc_si128, _mm_cvtsi128_si64, _mm_extract_epi64, _mm_set_epi64x, _mm_xor_si128,
    };

    pub(super) fn available() -> bool {
        std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("sse4.1")
    }

    pub(super) fn encrypt_blocks(keys: &RoundKeys, rounds: usize, blocks: &mut [[u8; AES_BLOCK_BYTES]]) -> bool {
        if !available() {
            return false;
        }
        // SAFETY: the processor was just detected to support AES-NI and SSE 4.1, the only
        // features `encrypt_with_aes_ni` enables.
        unsafe { encrypt_with_aes_ni(keys, rounds, blocks) };
        true
    }

    pub(super) fn decrypt_block(keys: &RoundKeys, rounds: usize, block: &mut [u8; AES_BLOCK_BYTES]) -> bool {
        if !available() {
            return false;
        }
        // SAFETY: the processor was just detected to support AES-NI and SSE 4.1, the only
        // features `decrypt_with_aes_ni` enables.
        unsafe { decrypt_with_aes_ni(keys, rounds, block) };
        true
    }

    #[target_feature(enable = "aes,sse4.1")]
    fn load(bytes: &[u8; AES_BLOCK_BYTES]) -> __m128i {
        let (halves, _) = bytes.as_chunks::<8>();
        _mm_set_epi64x(i64::from_le_bytes(halves[1]), i64::from_le_bytes(halves[0]))
    }

    #[target_feature(enable = "aes,sse4.1")]
    fn store(state: __m128i, bytes: &mut [u8; AES_BLOCK_BYTES]) {
        bytes[..8].copy_from_slice(&_mm_cvtsi128_si64(state).to_le_bytes());
        bytes[8..].copy_from_slice(&_mm_extract_epi64::<1>(state).to_le_bytes());
    }

    #[target_feature(enable = "aes,sse4.1")]
    fn encrypt_with_aes_ni(keys: &RoundKeys, rounds: usize, blocks: &mut [[u8; AES_BLOCK_BYTES]]) {
        let mut loaded = [load(&keys[0]); 15];
        for (slot, key) in loaded.iter_mut().zip(keys.iter()).take(rounds + 1) {
            *slot = load(key);
        }
        for block in blocks {
            let mut state = _mm_xor_si128(load(block), loaded[0]);
            for key in &loaded[1..rounds] {
                state = _mm_aesenc_si128(state, *key);
            }
            state = _mm_aesenclast_si128(state, loaded[rounds]);
            store(state, block);
        }
    }

    #[target_feature(enable = "aes,sse4.1")]
    fn decrypt_with_aes_ni(keys: &RoundKeys, rounds: usize, block: &mut [u8; AES_BLOCK_BYTES]) {
        let mut state = _mm_xor_si128(load(block), load(&keys[rounds]));
        for round in (1..rounds).rev() {
            state = _mm_aesdec_si128(state, _mm_aesimc_si128(load(&keys[round])));
        }
        state = _mm_aesdeclast_si128(state, load(&keys[0]));
        store(state, block);
    }
}

#[cfg(target_arch = "aarch64")]
mod instructions {
    use super::{AES_BLOCK_BYTES, RoundKeys};
    use std::arch::aarch64::{
        uint8x16_t, vaesdq_u8, vaeseq_u8, vaesimcq_u8, vaesmcq_u8, vcombine_u64, vcreate_u64,
        vdupq_n_u8, veorq_u8, vgetq_lane_u64, vreinterpretq_u8_u64, vreinterpretq_u64_u8,
    };

    pub(super) fn available() -> bool {
        std::arch::is_aarch64_feature_detected!("aes")
    }

    pub(super) fn encrypt_blocks(keys: &RoundKeys, rounds: usize, blocks: &mut [[u8; AES_BLOCK_BYTES]]) -> bool {
        if !available() {
            return false;
        }
        // SAFETY: the processor was just detected to support the AES instructions, the only
        // feature `encrypt_with_armv8_aes` enables beyond the baseline NEON.
        unsafe { encrypt_with_armv8_aes(keys, rounds, blocks) };
        true
    }

    pub(super) fn decrypt_block(keys: &RoundKeys, rounds: usize, block: &mut [u8; AES_BLOCK_BYTES]) -> bool {
        if !available() {
            return false;
        }
        // SAFETY: the processor was just detected to support the AES instructions, the only
        // feature `decrypt_with_armv8_aes` enables beyond the baseline NEON.
        unsafe { decrypt_with_armv8_aes(keys, rounds, block) };
        true
    }

    #[target_feature(enable = "aes")]
    fn load(bytes: &[u8; AES_BLOCK_BYTES]) -> uint8x16_t {
        let (halves, _) = bytes.as_chunks::<8>();
        vreinterpretq_u8_u64(vcombine_u64(
            vcreate_u64(u64::from_le_bytes(halves[0])),
            vcreate_u64(u64::from_le_bytes(halves[1])),
        ))
    }

    #[target_feature(enable = "aes")]
    fn store(state: uint8x16_t, bytes: &mut [u8; AES_BLOCK_BYTES]) {
        let words = vreinterpretq_u64_u8(state);
        bytes[..8].copy_from_slice(&vgetq_lane_u64::<0>(words).to_le_bytes());
        bytes[8..].copy_from_slice(&vgetq_lane_u64::<1>(words).to_le_bytes());
    }

    #[target_feature(enable = "aes")]
    fn encrypt_with_armv8_aes(keys: &RoundKeys, rounds: usize, blocks: &mut [[u8; AES_BLOCK_BYTES]]) {
        let mut loaded = [load(&keys[0]); 15];
        for (slot, key) in loaded.iter_mut().zip(keys.iter()).take(rounds + 1) {
            *slot = load(key);
        }
        for block in blocks {
            let mut state = load(block);
            for key in &loaded[..rounds - 1] {
                state = vaesmcq_u8(vaeseq_u8(state, *key));
            }
            state = veorq_u8(vaeseq_u8(state, loaded[rounds - 1]), loaded[rounds]);
            store(state, block);
        }
    }

    #[target_feature(enable = "aes")]
    fn decrypt_with_armv8_aes(keys: &RoundKeys, rounds: usize, block: &mut [u8; AES_BLOCK_BYTES]) {
        let zero = vdupq_n_u8(0);
        let mut state = vaesdq_u8(load(block), load(&keys[rounds]));
        for round in (1..rounds).rev() {
            state = vaesimcq_u8(veorq_u8(state, load(&keys[round])));
            state = vaesdq_u8(state, zero);
        }
        state = veorq_u8(state, load(&keys[0]));
        store(state, block);
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod instructions {
    use super::{AES_BLOCK_BYTES, RoundKeys};

    pub(super) fn available() -> bool {
        false
    }

    pub(super) fn encrypt_blocks(_keys: &RoundKeys, _rounds: usize, _blocks: &mut [[u8; AES_BLOCK_BYTES]]) -> bool {
        false
    }

    pub(super) fn decrypt_block(_keys: &RoundKeys, _rounds: usize, _block: &mut [u8; AES_BLOCK_BYTES]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{Aes, inverse_sub_bytes, sub_bytes};

    #[test]
    fn the_processor_s_instructions_agree_with_the_computed_cipher() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for key_length in [16usize, 24, 32] {
            let key: Vec<u8> = (0..key_length).map(|_| next() as u8).collect();
            let cipher = Aes::new(&key).expect("a key of a valid length");
            let mut blocks: Vec<[u8; 16]> = (0..9)
                .map(|_| {
                    let mut block = [0u8; 16];
                    block.iter_mut().for_each(|byte| *byte = next() as u8);
                    block
                })
                .collect();
            let mut computed = blocks.clone();
            cipher.encrypt_blocks(&mut blocks);
            cipher.encrypt_blocks_computed(&mut computed);
            assert_eq!(blocks, computed, "encryption with a {key_length}-byte key");
            for (block, expected) in blocks.iter_mut().zip(computed.iter_mut()) {
                cipher.decrypt_block(block);
                cipher.decrypt_block_computed(expected);
                assert_eq!(block, expected, "decryption with a {key_length}-byte key");
            }
        }
    }

    #[test]
    fn decrypting_undoes_fips_197s_example() {
        let key: Vec<u8> = (0..16).collect();
        let cipher = Aes::new(&key).expect("a 128-bit key");
        let plain: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let expected: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let mut block = plain;
        cipher.encrypt_block(&mut block);
        assert_eq!(block, expected);
        cipher.decrypt_block(&mut block);
        assert_eq!(block, plain);
        let mut all: Vec<u8> = (0..=255).collect();
        sub_bytes(&mut all);
        inverse_sub_bytes(&mut all);
        assert!(all.iter().enumerate().all(|(index, value)| usize::from(*value) == index));
    }

    #[test]
    fn the_computed_s_box_is_fips_197s_table() {
        let mut all: Vec<u8> = (0..=255).collect();
        sub_bytes(&mut all);
        assert_eq!(
            all[..16],
            [
                0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7,
                0xab, 0x76
            ]
        );
        assert_eq!(all[0x53], 0xed);
        assert_eq!(all[0xff], 0x16);
        let mut seen = [false; 256];
        for value in &all {
            seen[usize::from(*value)] = true;
        }
        assert!(seen.iter().all(|seen| *seen));
    }
}
