//! MD5 (RFC 1321), kept only for checksums that protocols define with it, such as S3's ETag and
//! SQS's message digests. It is broken for security: never use it to authenticate anything.

const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

const INITIAL_STATE: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const BLOCK_BYTES: usize = 64;

fn compress(state: &mut [u32; 4], block: &[u8; BLOCK_BYTES]) {
    let mut words = [0u32; 16];
    for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for at in 0..64 {
        let (mixed, index) = match at / 16 {
            0 => ((b & c) | (!b & d), at),
            1 => ((d & b) | (!d & c), (5 * at + 1) % 16),
            2 => (b ^ c ^ d, (3 * at + 5) % 16),
            _ => (c ^ (b | !d), (7 * at) % 16),
        };
        let rotated = a
            .wrapping_add(mixed)
            .wrapping_add(SINES[at])
            .wrapping_add(words[index])
            .rotate_left(SHIFTS[at]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(rotated);
    }
    for (word, added) in state.iter_mut().zip([a, b, c, d]) {
        *word = word.wrapping_add(added);
    }
}

/// MD5 over data given in pieces, for protocol checksums of bodies too large to hold whole,
/// such as an S3 object's ETag computed as the object arrives.
#[derive(Debug, Clone)]
pub struct Md5 {
    state: [u32; 4],
    pending: Vec<u8>,
    length: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self {
            state: INITIAL_STATE,
            pending: Vec::with_capacity(BLOCK_BYTES),
            length: 0,
        }
    }
}

impl Md5 {
    /// A digest of nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        let mut rest = bytes;
        if !self.pending.is_empty() {
            let wanted = (BLOCK_BYTES - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..wanted]);
            rest = &rest[wanted..];
            if self.pending.len() < BLOCK_BYTES {
                return;
            }
            let mut block = [0u8; BLOCK_BYTES];
            block.copy_from_slice(&self.pending);
            compress(&mut self.state, &block);
            self.pending.clear();
        }
        let (blocks, tail) = rest.as_chunks::<BLOCK_BYTES>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.pending.extend_from_slice(tail);
    }

    /// The digest of everything added.
    pub fn finish(mut self) -> [u8; 16] {
        let bit_length = self.length.wrapping_mul(8);
        let mut tail = std::mem::take(&mut self.pending);
        tail.push(0x80);
        while tail.len() % BLOCK_BYTES != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bit_length.to_le_bytes());
        for block in tail.as_chunks::<BLOCK_BYTES>().0 {
            compress(&mut self.state, block);
        }
        let mut digest = [0u8; 16];
        for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        digest
    }
}

/// The MD5 digest of `bytes`, for protocol checksums only.
pub fn md5_checksum(bytes: &[u8]) -> [u8; 16] {
    let mut digest = Md5::new();
    digest.update(bytes);
    digest.finish()
}

#[cfg(test)]
mod tests {
    use super::{Md5, md5_checksum};

    #[test]
    fn digesting_in_pieces_matches_digesting_whole() {
        let data: Vec<u8> = (0..1000u32).map(|value| value as u8).collect();
        for cut in [0, 1, 63, 64, 65, 500, 999] {
            let mut digest = Md5::new();
            digest.update(&data[..cut]);
            digest.update(&data[cut..]);
            assert_eq!(digest.finish(), md5_checksum(&data), "cut at {cut}");
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_rfc_1321_test_suite_matches() {
        for (input, digest) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(hex(&md5_checksum(input.as_bytes())), digest, "{input}");
        }
    }
}
