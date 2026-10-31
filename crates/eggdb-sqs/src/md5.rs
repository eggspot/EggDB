const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23,
    4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// MD5 (RFC 1321), only because SQS reports message digests with it; never for security.
pub(crate) fn md5_hex(bytes: &[u8]) -> String {
    let constants: Vec<u32> = (0..64).map(|at| ((at as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32).collect();
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = bytes.to_vec();
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_le_bytes());
    for block in message.chunks(64) {
        let words: Vec<u32> = block.chunks(4).map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]])).collect();
        let [mut a, mut b, mut c, mut d] = state;
        for at in 0..64 {
            let (mixed, index) = match at / 16 {
                0 => ((b & c) | (!b & d), at),
                1 => ((d & b) | (!d & c), (5 * at + 1) % 16),
                2 => (b ^ c ^ d, (3 * at + 5) % 16),
                _ => (c ^ (b | !d), (7 * at) % 16),
            };
            let rotated = a.wrapping_add(mixed).wrapping_add(constants[at]).wrapping_add(words[index]).rotate_left(SHIFTS[at]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    state.iter().flat_map(|word| word.to_le_bytes()).map(|byte| format!("{byte:02x}")).collect()
}
