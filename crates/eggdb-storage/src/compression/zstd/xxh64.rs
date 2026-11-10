const PRIME_1: u64 = 11_400_714_785_074_694_791;
const PRIME_2: u64 = 14_029_467_366_897_019_727;
const PRIME_3: u64 = 1_609_587_929_392_839_161;
const PRIME_4: u64 = 9_650_029_242_287_828_579;
const PRIME_5: u64 = 2_870_177_450_012_600_261;

fn read_u64(bytes: &[u8]) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

fn read_u32(bytes: &[u8]) -> u64 {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[..4]);
    u64::from(u32::from_le_bytes(word))
}

fn round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(PRIME_2))
        .rotate_left(31)
        .wrapping_mul(PRIME_1)
}

fn merge(accumulator: u64, value: u64) -> u64 {
    (accumulator ^ round(0, value))
        .wrapping_mul(PRIME_1)
        .wrapping_add(PRIME_4)
}

pub(super) fn hash(input: &[u8]) -> u64 {
    let mut rest = input;
    let mut hash = if input.len() >= 32 {
        let mut lanes = [
            PRIME_1.wrapping_add(PRIME_2),
            PRIME_2,
            0,
            0_u64.wrapping_sub(PRIME_1),
        ];
        while rest.len() >= 32 {
            for (index, lane) in lanes.iter_mut().enumerate() {
                *lane = round(*lane, read_u64(&rest[index * 8..]));
            }
            rest = &rest[32..];
        }
        let mut hash = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            hash = merge(hash, lane);
        }
        hash
    } else {
        PRIME_5
    };
    hash = hash.wrapping_add(input.len() as u64);
    while rest.len() >= 8 {
        hash ^= round(0, read_u64(rest));
        hash = hash.rotate_left(27).wrapping_mul(PRIME_1).wrapping_add(PRIME_4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        hash ^= read_u32(rest).wrapping_mul(PRIME_1);
        hash = hash.rotate_left(23).wrapping_mul(PRIME_2).wrapping_add(PRIME_3);
        rest = &rest[4..];
    }
    for &byte in rest {
        hash ^= u64::from(byte).wrapping_mul(PRIME_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME_1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME_3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn published_vectors_hash_to_their_values() {
        assert_eq!(hash(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(hash(b"a"), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(hash(b"abc"), 0x44BC_2CF5_AD77_0999);
    }
}
