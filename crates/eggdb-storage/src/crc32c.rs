const POLYNOMIAL_REFLECTED: u32 = 0x82F6_3B78;
const SLICES: usize = 8;

static TABLES: [[u32; 256]; SLICES] = build_tables();

const fn build_tables() -> [[u32; 256]; SLICES] {
    let mut tables = [[0u32; 256]; SLICES];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL_REFLECTED
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut slice = 1;
    while slice < SLICES {
        let mut byte = 0;
        while byte < 256 {
            let previous = tables[slice - 1][byte];
            tables[slice][byte] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            byte += 1;
        }
        slice += 1;
    }
    tables
}

/// The CRC-32C (Castagnoli) checksum of `bytes`, as defined by RFC 3720, used for every page
/// and log record of the file format.
///
/// Uses the processor's CRC instructions where it has them (SSE 4.2 on x86-64, the CRC
/// extension on AArch64) and an eight-way table otherwise; both give the same value.
pub fn crc32c(bytes: &[u8]) -> u32 {
    crc32c_append(0, bytes)
}

/// Continues the checksum `crc` of earlier bytes over `bytes`, so that
/// `crc32c_append(crc32c(a), b)` equals the checksum of `a` followed by `b`.
pub fn crc32c_append(crc: u32, bytes: &[u8]) -> u32 {
    !hardware::update(!crc, bytes).unwrap_or_else(|| update_with_tables(!crc, bytes))
}

fn update_with_tables(mut crc: u32, bytes: &[u8]) -> u32 {
    let (chunks, remainder) = bytes.as_chunks::<SLICES>();
    for chunk in chunks {
        let low = crc ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        crc = TABLES[7][(low & 0xFF) as usize]
            ^ TABLES[6][((low >> 8) & 0xFF) as usize]
            ^ TABLES[5][((low >> 16) & 0xFF) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][usize::from(chunk[4])]
            ^ TABLES[2][usize::from(chunk[5])]
            ^ TABLES[1][usize::from(chunk[6])]
            ^ TABLES[0][usize::from(chunk[7])];
    }
    for &byte in remainder {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ u32::from(byte)) & 0xFF) as usize];
    }
    crc
}

#[cfg(target_arch = "x86_64")]
mod hardware {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    pub(super) fn update(crc: u32, bytes: &[u8]) -> Option<u32> {
        if !std::arch::is_x86_feature_detected!("sse4.2") {
            return None;
        }
        // SAFETY: the processor was just detected to support SSE 4.2, the only feature
        // `update_sse42` enables.
        Some(unsafe { update_sse42(crc, bytes) })
    }

    #[target_feature(enable = "sse4.2")]
    fn update_sse42(crc: u32, bytes: &[u8]) -> u32 {
        let mut crc = u64::from(crc);
        let (words, remainder) = bytes.as_chunks::<8>();
        for &word in words {
            crc = _mm_crc32_u64(crc, u64::from_le_bytes(word));
        }
        let mut crc = crc as u32;
        for &byte in remainder {
            crc = _mm_crc32_u8(crc, byte);
        }
        crc
    }
}

#[cfg(target_arch = "aarch64")]
mod hardware {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    pub(super) fn update(crc: u32, bytes: &[u8]) -> Option<u32> {
        if !std::arch::is_aarch64_feature_detected!("crc") {
            return None;
        }
        // SAFETY: the processor was just detected to support the CRC extension, the only
        // feature `update_crc` enables.
        Some(unsafe { update_crc(crc, bytes) })
    }

    #[target_feature(enable = "crc")]
    fn update_crc(mut crc: u32, bytes: &[u8]) -> u32 {
        let (words, remainder) = bytes.as_chunks::<8>();
        for &word in words {
            crc = __crc32cd(crc, u64::from_le_bytes(word));
        }
        for &byte in remainder {
            crc = __crc32cb(crc, byte);
        }
        crc
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod hardware {
    pub(super) fn update(_crc: u32, _bytes: &[u8]) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{crc32c, crc32c_append, hardware, update_with_tables};
    use crate::test_rng::TestRng;

    #[test]
    fn matches_the_rfc_3720_vectors() {
        let ascending: Vec<u8> = (0..32).collect();
        let descending: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&[0; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFF; 32]), 0x62A8_AB43);
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
    }

    #[test]
    fn matches_the_catalogued_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn appending_equals_checksumming_the_concatenation() {
        let mut rng = TestRng::new(3720);
        for _ in 0..200 {
            let len = rng.below(300);
            let bytes = rng.bytes(len);
            let split = rng.below(bytes.len() + 1);
            let (head, tail) = bytes.split_at(split);
            assert_eq!(crc32c_append(crc32c(head), tail), crc32c(&bytes));
        }
    }

    #[test]
    fn hardware_and_tables_agree_on_random_inputs() {
        let mut rng = TestRng::new(32);
        for _ in 0..500 {
            let len = rng.below(1100);
            let bytes = rng.bytes(len);
            let start = rng.next_u64() as u32;
            let tables = update_with_tables(start, &bytes);
            if let Some(hardware) = hardware::update(start, &bytes) {
                assert_eq!(hardware, tables);
            }
        }
    }
}
