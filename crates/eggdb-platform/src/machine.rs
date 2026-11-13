//! What the machine has, for a fleet's inventory (D1112): its installed memory where the system
//! says it without privileges (Linux and Android, from `/proc/meminfo`), and `None` elsewhere.

/// The machine's installed memory in bytes, or `None` where this layer cannot read it.
pub fn total_memory_bytes() -> Option<u64> {
    os::total_memory_bytes()
}

/// Reads `MemTotal` from the text of `/proc/meminfo`.
pub fn mem_total_of(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod os {
    pub(super) fn total_memory_bytes() -> Option<u64> {
        super::mem_total_of(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod os {
    pub(super) fn total_memory_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::mem_total_of;

    #[test]
    fn mem_total_is_read_in_kibibytes() {
        assert_eq!(mem_total_of("MemTotal:       16384000 kB\nMemFree: 1 kB\n"), Some(16_384_000 * 1024));
        assert_eq!(mem_total_of("nothing"), None);
    }
}
