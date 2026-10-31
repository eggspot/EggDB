/// The length of an AES block in bytes.
pub const AES_BLOCK_BYTES: usize = 16;

const MAX_ROUNDS: usize = 14;
const LANES: usize = 64;
const ROUND_CONSTANTS: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
const AFFINE_CONSTANT: u8 = 0x63;

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

/// The AES block cipher (FIPS 197) under one key of 128, 192 or 256 bits, encryption only (the
/// modes TLS uses never decrypt a block). The S-box is computed, not looked up: each byte is
/// inverted in GF(2^8) on bit planes shared by up to 64 bytes, so no memory access and no branch
/// depends on the key or the data.
#[derive(Clone)]
pub struct Aes {
    round_keys: [[u8; AES_BLOCK_BYTES]; MAX_ROUNDS + 1],
    rounds: usize,
}

impl std::fmt::Debug for Aes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aes")
            .field("rounds", &self.rounds)
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
        Ok(Self { round_keys, rounds })
    }

    /// Encrypts one block in place.
    pub fn encrypt_block(&self, block: &mut [u8; AES_BLOCK_BYTES]) {
        self.encrypt_blocks(std::slice::from_mut(block));
    }

    /// Encrypts every block of `blocks` in place, four at a time through the shared S-box.
    pub fn encrypt_blocks(&self, blocks: &mut [[u8; AES_BLOCK_BYTES]]) {
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
}

#[cfg(test)]
mod tests {
    use super::sub_bytes;

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
