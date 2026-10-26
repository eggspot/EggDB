pub(crate) fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub(crate) fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let mut le = [0u8; 4];
    le.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(le)
}

pub(crate) fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut le = [0u8; 8];
    le.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(le)
}

pub(crate) fn write_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn write_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
