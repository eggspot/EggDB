/// The length of a Poly1305 one-time key in bytes.
pub const POLY1305_KEY_BYTES: usize = 32;
/// The length of a Poly1305 tag in bytes.
pub const POLY1305_TAG_BYTES: usize = 16;

const BLOCK_BYTES: usize = 16;
const LIMB_MASK: u32 = 0x03ff_ffff;
const FULL_BLOCK_BIT: u32 = 1 << 24;

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

/// A Poly1305 authenticator in progress (RFC 8439 section 2.5), in time that depends only on the
/// message length: the accumulator is kept in five 26-bit limbs and every reduction runs the
/// same carries whatever the values.
#[derive(Clone)]
pub struct Poly1305 {
    r: [u32; 5],
    pad: [u32; 4],
    accumulator: [u32; 5],
    partial: [u8; BLOCK_BYTES],
    filled: usize,
}

impl std::fmt::Debug for Poly1305 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Poly1305").finish_non_exhaustive()
    }
}

impl Poly1305 {
    /// Starts an authenticator under the one-time `key` (`r` then `s`, 16 bytes each); `r` is
    /// clamped as the RFC says.
    pub fn new(key: &[u8; POLY1305_KEY_BYTES]) -> Self {
        let r = [
            le32(key, 0) & 0x03ff_ffff,
            (le32(key, 3) >> 2) & 0x03ff_ff03,
            (le32(key, 6) >> 4) & 0x03ff_c0ff,
            (le32(key, 9) >> 6) & 0x03f0_3fff,
            (le32(key, 12) >> 8) & 0x000f_ffff,
        ];
        let pad = [le32(key, 16), le32(key, 20), le32(key, 24), le32(key, 28)];
        Self {
            r,
            pad,
            accumulator: [0; 5],
            partial: [0; BLOCK_BYTES],
            filled: 0,
        }
    }

    fn absorb(&mut self, block: &[u8; BLOCK_BYTES], high_bit: u32) {
        let [r0, r1, r2, r3, r4] = self.r.map(u64::from);
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let h = &mut self.accumulator;
        h[0] = h[0].wrapping_add(le32(block, 0) & LIMB_MASK);
        h[1] = h[1].wrapping_add((le32(block, 3) >> 2) & LIMB_MASK);
        h[2] = h[2].wrapping_add((le32(block, 6) >> 4) & LIMB_MASK);
        h[3] = h[3].wrapping_add((le32(block, 9) >> 6) & LIMB_MASK);
        h[4] = h[4].wrapping_add((le32(block, 12) >> 8) | high_bit);
        let [h0, h1, h2, h3, h4] = h.map(u64::from);
        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let mut d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let mut d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;
        let mask = u64::from(LIMB_MASK);
        d1 += d0 >> 26;
        let mut n0 = d0 & mask;
        d2 += d1 >> 26;
        let n1 = d1 & mask;
        d3 += d2 >> 26;
        let n2 = d2 & mask;
        d4 += d3 >> 26;
        let n3 = d3 & mask;
        let carry = d4 >> 26;
        let n4 = d4 & mask;
        n0 += carry * 5;
        let n1 = n1 + (n0 >> 26);
        n0 &= mask;
        *h = [n0 as u32, n1 as u32, n2 as u32, n3 as u32, n4 as u32];
    }

    /// Adds `bytes` to the message.
    pub fn update(&mut self, mut bytes: &[u8]) {
        if self.filled > 0 {
            let taken = bytes.len().min(BLOCK_BYTES - self.filled);
            let (head, rest) = bytes.split_at(taken);
            self.partial[self.filled..self.filled + taken].copy_from_slice(head);
            self.filled += taken;
            bytes = rest;
            if self.filled < BLOCK_BYTES {
                return;
            }
            let block = self.partial;
            self.absorb(&block, FULL_BLOCK_BIT);
            self.filled = 0;
        }
        let (blocks, rest) = bytes.as_chunks::<BLOCK_BYTES>();
        for block in blocks {
            self.absorb(block, FULL_BLOCK_BIT);
        }
        self.partial[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// Adds zero bytes up to the next 16-byte boundary, as the AEAD construction pads each part.
    pub fn pad_to_block(&mut self) {
        if self.filled > 0 {
            let zeros = [0u8; BLOCK_BYTES];
            let missing = BLOCK_BYTES - self.filled;
            self.update(&zeros[..missing]);
        }
    }

    /// The tag of the message.
    pub fn finish(mut self) -> [u8; POLY1305_TAG_BYTES] {
        if self.filled > 0 {
            let mut block = [0u8; BLOCK_BYTES];
            block[..self.filled].copy_from_slice(&self.partial[..self.filled]);
            block[self.filled] = 1;
            self.absorb(&block, 0);
        }
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.accumulator;
        let mut carry = h1 >> 26;
        h1 &= LIMB_MASK;
        h2 += carry;
        carry = h2 >> 26;
        h2 &= LIMB_MASK;
        h3 += carry;
        carry = h3 >> 26;
        h3 &= LIMB_MASK;
        h4 += carry;
        carry = h4 >> 26;
        h4 &= LIMB_MASK;
        h0 += carry * 5;
        carry = h0 >> 26;
        h0 &= LIMB_MASK;
        h1 += carry;

        let mut g0 = h0.wrapping_add(5);
        carry = g0 >> 26;
        g0 &= LIMB_MASK;
        let mut g1 = h1.wrapping_add(carry);
        carry = g1 >> 26;
        g1 &= LIMB_MASK;
        let mut g2 = h2.wrapping_add(carry);
        carry = g2 >> 26;
        g2 &= LIMB_MASK;
        let mut g3 = h3.wrapping_add(carry);
        carry = g3 >> 26;
        g3 &= LIMB_MASK;
        let mut g4 = h4.wrapping_add(carry).wrapping_sub(1 << 26);

        let use_reduced = (g4 >> 31).wrapping_sub(1);
        g0 &= use_reduced;
        g1 &= use_reduced;
        g2 &= use_reduced;
        g3 &= use_reduced;
        g4 &= use_reduced;
        let keep = !use_reduced;
        h0 = (h0 & keep) | g0;
        h1 = (h1 & keep) | g1;
        h2 = (h2 & keep) | g2;
        h3 = (h3 & keep) | g3;
        h4 = (h4 & keep) | g4;

        let words = [
            h0 | (h1 << 26),
            (h1 >> 6) | (h2 << 20),
            (h2 >> 12) | (h3 << 14),
            (h3 >> 18) | (h4 << 8),
        ];
        let mut tag = [0u8; POLY1305_TAG_BYTES];
        let mut sum = 0u64;
        for ((bytes, word), pad) in tag.as_chunks_mut::<4>().0.iter_mut().zip(words).zip(self.pad)
        {
            sum = u64::from(word) + u64::from(pad) + (sum >> 32);
            *bytes = (sum as u32).to_le_bytes();
        }
        tag
    }
}

/// The Poly1305 tag of `message` under the one-time `key`.
pub fn poly1305(key: &[u8; POLY1305_KEY_BYTES], message: &[u8]) -> [u8; POLY1305_TAG_BYTES] {
    let mut authenticator = Poly1305::new(key);
    authenticator.update(message);
    authenticator.finish()
}
