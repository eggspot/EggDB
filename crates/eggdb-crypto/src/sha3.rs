/// The bytes of a SHA3-256 digest.
pub const SHA3_256_DIGEST_BYTES: usize = 32;
/// The bytes of a SHA3-512 digest.
pub const SHA3_512_DIGEST_BYTES: usize = 64;

const ROUNDS: usize = 24;
const SHA3_SUFFIX: u8 = 0x06;
const SHAKE_SUFFIX: u8 = 0x1f;
const SHA3_256_RATE: usize = 136;
const SHA3_512_RATE: usize = 72;
const SHAKE128_RATE: usize = 168;
const SHAKE256_RATE: usize = 136;

const ROUND_CONSTANTS: [u64; ROUNDS] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

const ROTATIONS: [[u32; 5]; 5] = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
];

fn keccak_f(state: &mut [u64; 25]) {
    for constant in ROUND_CONSTANTS {
        let parity: [u64; 5] = std::array::from_fn(|x| {
            state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20]
        });
        for x in 0..5 {
            let mix = parity[(x + 4) % 5] ^ parity[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                state[x + 5 * y] ^= mix;
            }
        }
        let mut moved = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                let to_x = y;
                let to_y = (2 * x + 3 * y) % 5;
                moved[to_x + 5 * to_y] = state[x + 5 * y].rotate_left(ROTATIONS[x][y]);
            }
        }
        for y in 0..5 {
            for x in 0..5 {
                state[x + 5 * y] =
                    moved[x + 5 * y] ^ (!moved[(x + 1) % 5 + 5 * y] & moved[(x + 2) % 5 + 5 * y]);
            }
        }
        state[0] ^= constant;
    }
}

/// A Keccak sponge (FIPS 202) of a given rate and domain-separation suffix: absorb any number
/// of times, then squeeze any number of bytes.
#[derive(Clone)]
pub(crate) struct Keccak {
    state: [u64; 25],
    rate: usize,
    suffix: u8,
    buffered: usize,
    squeezing: bool,
}

impl Keccak {
    fn new(rate: usize, suffix: u8) -> Self {
        Self {
            state: [0; 25],
            rate,
            suffix,
            buffered: 0,
            squeezing: false,
        }
    }

    /// SHAKE128.
    pub(crate) fn shake128() -> Self {
        Self::new(SHAKE128_RATE, SHAKE_SUFFIX)
    }

    /// SHAKE256.
    pub(crate) fn shake256() -> Self {
        Self::new(SHAKE256_RATE, SHAKE_SUFFIX)
    }

    fn xor_byte(&mut self, at: usize, byte: u8) {
        self.state[at / 8] ^= u64::from(byte) << (8 * (at % 8));
    }

    fn byte(&self, at: usize) -> u8 {
        (self.state[at / 8] >> (8 * (at % 8))) as u8
    }

    /// Absorbs `data`; absorbing after squeezing started is ignored.
    pub(crate) fn absorb(&mut self, data: &[u8]) {
        if self.squeezing {
            return;
        }
        for &byte in data {
            self.xor_byte(self.buffered, byte);
            self.buffered += 1;
            if self.buffered == self.rate {
                keccak_f(&mut self.state);
                self.buffered = 0;
            }
        }
    }

    fn finish_absorbing(&mut self) {
        if self.squeezing {
            return;
        }
        self.xor_byte(self.buffered, self.suffix);
        self.xor_byte(self.rate - 1, 0x80);
        keccak_f(&mut self.state);
        self.buffered = 0;
        self.squeezing = true;
    }

    /// Fills `out` with the next bytes of output.
    pub(crate) fn squeeze(&mut self, out: &mut [u8]) {
        self.finish_absorbing();
        for slot in out {
            if self.buffered == self.rate {
                keccak_f(&mut self.state);
                self.buffered = 0;
            }
            *slot = self.byte(self.buffered);
            self.buffered += 1;
        }
    }
}

fn digest<const N: usize>(rate: usize, data: &[&[u8]]) -> [u8; N] {
    let mut sponge = Keccak::new(rate, SHA3_SUFFIX);
    for part in data {
        sponge.absorb(part);
    }
    let mut out = [0u8; N];
    sponge.squeeze(&mut out);
    out
}

/// SHA3-256 of the concatenation of `data`.
pub fn sha3_256(data: &[&[u8]]) -> [u8; SHA3_256_DIGEST_BYTES] {
    digest(SHA3_256_RATE, data)
}

/// SHA3-512 of the concatenation of `data`.
pub fn sha3_512(data: &[&[u8]]) -> [u8; SHA3_512_DIGEST_BYTES] {
    digest(SHA3_512_RATE, data)
}

/// `length` bytes of SHAKE256 over the concatenation of `data`.
pub fn shake256(data: &[&[u8]], length: usize) -> Vec<u8> {
    let mut sponge = Keccak::shake256();
    for part in data {
        sponge.absorb(part);
    }
    let mut out = vec![0u8; length];
    sponge.squeeze(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_empty_message_digests_match_fips_202() {
        assert_eq!(
            hex(&sha3_256(&[])),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
        assert_eq!(
            hex(&sha3_512(&[])),
            "a69f73cca23a9ac5c8b567dc185a756e97c982164fe25859e0d1dcc1475c80a615b2123af1f5f94c11e3e9402c3ac558f500199d95b6d3e301758586281dcd26"
        );
        assert_eq!(
            hex(&shake256(&[], 32)),
            "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f"
        );
        let mut shake = Keccak::shake128();
        let mut out = [0u8; 32];
        shake.squeeze(&mut out);
        assert_eq!(
            hex(&out),
            "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26"
        );
    }

    #[test]
    fn abc_and_split_input_agree() {
        assert_eq!(
            hex(&sha3_256(&[b"abc"])),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert_eq!(sha3_256(&[b"a", b"bc"]), sha3_256(&[b"abc"]));
        let long = vec![0x61u8; 1000];
        assert_eq!(sha3_512(&[&long[..500], &long[500..]]), sha3_512(&[&long]));
    }
}
