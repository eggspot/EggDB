pub(crate) fn key(index: u64) -> [u8; 16] {
    let mut state = index.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = [0u8; 16];
    for half in mixed.as_chunks_mut::<8>().0 {
        state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        state ^= state >> 31;
        *half = state.to_le_bytes();
    }
    mixed
}

pub(crate) fn value(index: u64) -> [u8; 16] {
    let mut value = [0u8; 16];
    value[..8].copy_from_slice(&index.to_le_bytes());
    value
}
