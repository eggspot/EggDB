//! Unpredictable bytes from the operating system's cryptographically secure generator, for
//! salts, nonces and keys (D23).

/// Fills `bytes` with unpredictable bytes from the operating system: `/dev/urandom` on Unix
/// and `BCryptGenRandom` on Windows. Fails with [`std::io::ErrorKind::Unsupported`] on systems
/// without such a generator.
pub fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
    os::fill(bytes)
}

#[cfg(unix)]
mod os {
    use std::io::Read;

    pub(super) fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
        std::fs::File::open("/dev/urandom")?.read_exact(bytes)
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32)
        -> i32;
    }

    pub(super) fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
        for chunk in bytes.chunks_mut(u32::MAX as usize) {
            let length = chunk.len() as u32;
            // SAFETY: the pointer and length describe `chunk`, which is writable; no algorithm
            // handle is needed with the system-preferred flag.
            let status = unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    length,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            if status != 0 {
                return Err(std::io::Error::other(format!(
                    "BCryptGenRandom failed with status {status:#x}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    pub(super) fn fill(_bytes: &mut [u8]) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::fill;

    #[test]
    fn fills_with_bytes_that_differ_between_calls() {
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        fill(&mut first).unwrap();
        fill(&mut second).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, [0u8; 32]);
    }
}
