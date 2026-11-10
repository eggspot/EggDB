use super::{compress, decompress};

fn round_trip(input: &[u8]) {
    let frame = compress(input);
    assert_eq!(decompress(&frame, input.len()).unwrap(), input);
}

fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 56) as u8
        })
        .collect()
}

#[test]
fn every_kind_of_input_round_trips() {
    round_trip(b"");
    round_trip(b"a");
    round_trip(&[7; 300_000]);
    round_trip(&pseudo_random(200_000, 3));
    let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog. "
        .iter()
        .copied()
        .cycle()
        .take(500_000)
        .collect();
    round_trip(&text);
    let mixed: Vec<u8> = (0..300_000_u32).map(|n| (n % 251 ^ (n / 1000)) as u8).collect();
    round_trip(&mixed);
}

#[test]
fn repetition_shrinks() {
    let text: Vec<u8> = b"row 1, row 2, row 3; "
        .iter()
        .copied()
        .cycle()
        .take(64 * 1024)
        .collect();
    assert!(compress(&text).len() < text.len() / 8);
}

#[test]
fn a_frame_from_the_reference_format_decodes() {
    let frame = [
        0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x05, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
    ];
    assert_eq!(decompress(&frame, 5).unwrap(), b"hello");
}

#[test]
fn a_huffman_literals_block_decodes() {
    let weights_direct = [0x80 + 2, 0x11];
    let mut block = Vec::new();
    let literals_bits = {
        let mut bits = super::bits::BitWriter::default();
        for _ in 0..4 {
            bits.add(0, 2);
        }
        bits.close()
    };
    let compressed = weights_direct.len() + literals_bits.len();
    let header = 2_u32 | (4 << 4) | ((compressed as u32) << 14);
    block.extend_from_slice(&header.to_le_bytes()[..3]);
    block.extend_from_slice(&weights_direct);
    block.extend_from_slice(&literals_bits);
    block.push(0);
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x04];
    let block_header = 1_u32 | (2 << 1) | ((block.len() as u32) << 3);
    frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
    frame.extend_from_slice(&block);
    let decoded = decompress(&frame, 4).unwrap();
    assert_eq!(decoded.len(), 4);
    assert!(decoded.iter().all(|&byte| byte == decoded[0]));
}

#[test]
fn hostile_frames_are_errors_never_panics() {
    let good = compress(&pseudo_random(5000, 9).repeat(3));
    for cut in 0..good.len() {
        let _ = decompress(&good[..cut], 15_000);
    }
    let mut state = 1_u64;
    for round in 0..2000 {
        let mut damaged = good.clone();
        for _ in 0..1 + round % 4 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let at = (state >> 33) as usize % damaged.len();
            damaged[at] ^= (state >> 7) as u8 | 1;
        }
        let _ = decompress(&damaged, 15_000);
    }
    for length in 0..64 {
        let _ = decompress(&pseudo_random(length, length as u64), 1 << 20);
    }
}

#[test]
fn a_frame_larger_than_expected_is_refused() {
    let frame = compress(&[1; 10_000]);
    assert!(decompress(&frame, 9_999).is_err());
    assert!(decompress(&frame, 10_001).is_err());
}

#[test]
fn a_damaged_checksum_is_refused() {
    let mut frame = compress(b"checksummed content, long enough to matter");
    let last = frame.len() - 1;
    frame[last] ^= 1;
    assert!(decompress(&frame, 42).is_err());
}
