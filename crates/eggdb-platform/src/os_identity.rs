//! Which operating system the process runs on, when the build target does not say it alone:
//! EggOS runs Linux programs through Linux's own system call ABI (its native one), and is told
//! apart by the release `uname` reports, which ends in `-eggos` (D1080).

use std::sync::OnceLock;

/// The operating system the process runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatingSystem {
    /// EggOS, running this Linux build.
    EggOs,
    /// The operating system the build targets, by `std::env::consts::OS`.
    Target(&'static str),
}

impl OperatingSystem {
    /// Its name, `EggOS` or the target's (`linux`, `macos`, `windows`, ...).
    pub fn name(self) -> &'static str {
        match self {
            OperatingSystem::EggOs => "EggOS",
            OperatingSystem::Target(name) => name,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use std::ffi::{c_char, c_int};

    const FIELD: usize = 65;

    #[repr(C)]
    struct Utsname {
        sysname: [c_char; FIELD],
        nodename: [c_char; FIELD],
        release: [c_char; FIELD],
        version: [c_char; FIELD],
        machine: [c_char; FIELD],
        domainname: [c_char; FIELD],
    }

    unsafe extern "C" {
        fn uname(name: *mut Utsname) -> c_int;
    }

    pub(super) fn release() -> Option<String> {
        let mut name = Utsname {
            sysname: [0; FIELD],
            nodename: [0; FIELD],
            release: [0; FIELD],
            version: [0; FIELD],
            machine: [0; FIELD],
            domainname: [0; FIELD],
        };
        // SAFETY: `name` is a writable `struct utsname` with Linux's layout (six 65-byte
        // fields), which `uname` fills and nothing else.
        let result = unsafe { uname(&mut name) };
        if result != 0 {
            return None;
        }
        let bytes: Vec<u8> = name.release.iter().take_while(|byte| **byte != 0).map(|byte| *byte as u8).collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod platform {
    pub(super) fn release() -> Option<String> {
        None
    }
}

/// Whether a kernel release names EggOS.
pub fn release_is_eggos(release: &str) -> bool {
    release.trim_end().to_ascii_lowercase().ends_with("-eggos")
}

/// The operating system this process runs on, found once.
pub fn operating_system() -> OperatingSystem {
    static FOUND: OnceLock<OperatingSystem> = OnceLock::new();
    *FOUND.get_or_init(|| match platform::release() {
        Some(release) if release_is_eggos(&release) => OperatingSystem::EggOs,
        _ => OperatingSystem::Target(std::env::consts::OS),
    })
}

/// Whether this process runs on EggOS.
pub fn on_eggos() -> bool {
    operating_system() == OperatingSystem::EggOs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eggos_is_known_by_its_release() {
        assert!(release_is_eggos("6.1.0-eggos"));
        assert!(!release_is_eggos("6.8.0-45-generic"));
        assert_eq!(OperatingSystem::EggOs.name(), "EggOS");
        assert!(!operating_system().name().is_empty());
    }
}
