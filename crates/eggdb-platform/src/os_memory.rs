use crate::error::{Error, ErrorKind, Result};
use crate::memory::{Memory, MemoryRegion, Pages, round_up};
use std::ptr::NonNull;
use std::sync::OnceLock;

/// Memory from the operating system: anonymous mappings (`mmap` on Unix, `VirtualAlloc` on
/// Windows), and the heap with page alignment where neither exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsMemory;

impl OsMemory {
    /// Returns the operating system's memory.
    pub fn new() -> Self {
        Self
    }
}

impl Memory for OsMemory {
    fn page_size(&self) -> usize {
        static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
        *PAGE_SIZE.get_or_init(platform::page_size)
    }

    fn huge_page_size(&self) -> Option<usize> {
        static HUGE_PAGE_SIZE: OnceLock<Option<usize>> = OnceLock::new();
        *HUGE_PAGE_SIZE.get_or_init(platform::huge_page_size)
    }

    fn allocate(&self, len: usize, pages: Pages) -> Result<MemoryRegion> {
        let granularity = match pages {
            Pages::Normal => self.page_size(),
            Pages::Huge => self
                .huge_page_size()
                .ok_or_else(|| Error::new(ErrorKind::Unsupported))?,
        };
        let len = round_up(len, granularity).ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        platform::allocate(len, granularity, pages)
    }
}

pub(crate) fn release(ptr: NonNull<u8>, len: usize, pages: Pages) {
    platform::release(ptr, len, pages);
}

fn checked_power_of_two(size: usize) -> Option<usize> {
    size.is_power_of_two().then_some(size)
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn parse_huge_page_size(meminfo: &str) -> Option<usize> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("Hugepagesize:"))?;
    let kibibytes = line
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<usize>()
        .ok()?;
    kibibytes
        .checked_mul(1024)
        .and_then(checked_power_of_two)
        .filter(|&size| size > 0)
}

#[cfg(unix)]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, Pages, Result, checked_power_of_two};
    use crate::memory::Release;
    use std::ffi::{c_int, c_void};
    use std::io;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    type Offset = std::ffi::c_long;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type Offset = i64;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 2;

    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(target_arch = "mips", target_arch = "mips64"))
    ))]
    const MAP_ANONYMOUS: c_int = 0x20;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(target_arch = "mips", target_arch = "mips64")
    ))]
    const MAP_ANONYMOUS: c_int = 0x800;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_ANONYMOUS: c_int = 0x1000;

    unsafe extern "C" {
        fn getpagesize() -> c_int;
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: Offset,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    pub(super) fn page_size() -> usize {
        // SAFETY: `getpagesize` takes no arguments and only reads a constant of the process.
        let size = unsafe { getpagesize() };
        usize::try_from(size)
            .ok()
            .and_then(checked_power_of_two)
            .unwrap_or(4096)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn huge_page_size() -> Option<usize> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        super::parse_huge_page_size(&meminfo)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub(super) fn huge_page_size() -> Option<usize> {
        None
    }

    pub(super) fn allocate(len: usize, _granularity: usize, pages: Pages) -> Result<MemoryRegion> {
        let flags = MAP_PRIVATE | MAP_ANONYMOUS | huge_flag(pages)?;
        // SAFETY: an anonymous private mapping at an address the kernel picks touches no
        // existing memory; the descriptor is ignored for anonymous mappings and -1 as POSIX asks.
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if raw.addr() == usize::MAX {
            let error = io::Error::last_os_error();
            return Err(allocation_error(error, pages));
        }
        let ptr = NonNull::new(raw.cast::<u8>()).ok_or_else(|| Error::new(ErrorKind::Other))?;
        // SAFETY: the kernel mapped `len` fresh zero-filled, writable bytes at `ptr` that no
        // other value refers to, and `Release::Os` unmaps them with the same length.
        Ok(unsafe { MemoryRegion::from_raw(ptr, len, pages, Release::Os) })
    }

    fn allocation_error(error: io::Error, pages: Pages) -> Error {
        let out_of_memory = error.kind() == io::ErrorKind::OutOfMemory;
        let error = Error::from(error);
        match pages {
            Pages::Huge => error.with_kind(ErrorKind::Unsupported),
            Pages::Normal if out_of_memory => error.with_kind(ErrorKind::OutOfMemory),
            Pages::Normal => error,
        }
    }

    pub(super) fn release(ptr: NonNull<u8>, len: usize, _pages: Pages) {
        // SAFETY: `ptr` and `len` describe exactly one mapping made by `allocate`, which the
        // dropped region owned and nothing else refers to any more.
        let _ = unsafe { munmap(ptr.as_ptr().cast::<c_void>(), len) };
    }

    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64"
        )
    ))]
    const MAP_HUGETLB: Option<c_int> = Some(0x40000);
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(target_arch = "mips", target_arch = "mips64")
    ))]
    const MAP_HUGETLB: Option<c_int> = Some(0x80000);
    #[cfg(not(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "s390x",
            target_arch = "loongarch64",
            target_arch = "mips",
            target_arch = "mips64"
        )
    )))]
    const MAP_HUGETLB: Option<c_int> = None;

    fn huge_flag(pages: Pages) -> Result<c_int> {
        match pages {
            Pages::Normal => Ok(0),
            Pages::Huge => MAP_HUGETLB.ok_or_else(|| Error::new(ErrorKind::Unsupported)),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, Pages, Result, checked_power_of_two};
    use crate::memory::Release;
    use std::ffi::c_void;
    use std::io;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const MEM_LARGE_PAGES: u32 = 0x2000_0000;
    const PAGE_READWRITE: u32 = 0x04;
    const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
    const ERROR_COMMITMENT_LIMIT: i32 = 1455;

    #[repr(C)]
    struct SystemInfo {
        processor_architecture: u16,
        reserved: u16,
        page_size: u32,
        minimum_application_address: *mut c_void,
        maximum_application_address: *mut c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    unsafe extern "system" {
        fn GetSystemInfo(info: *mut SystemInfo);
        fn GetLargePageMinimum() -> usize;
        fn VirtualAlloc(
            address: *mut c_void,
            size: usize,
            allocation_type: u32,
            protect: u32,
        ) -> *mut c_void;
        fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    }

    pub(super) fn page_size() -> usize {
        let mut info = std::mem::MaybeUninit::<SystemInfo>::uninit();
        // SAFETY: `GetSystemInfo` fills the whole structure it is given and cannot fail.
        unsafe { GetSystemInfo(info.as_mut_ptr()) };
        // SAFETY: `GetSystemInfo` above initialised every field.
        let info = unsafe { info.assume_init() };
        usize::try_from(info.page_size)
            .ok()
            .and_then(checked_power_of_two)
            .unwrap_or(4096)
    }

    pub(super) fn huge_page_size() -> Option<usize> {
        // SAFETY: `GetLargePageMinimum` takes no arguments and only reads a system constant.
        let size = unsafe { GetLargePageMinimum() };
        checked_power_of_two(size)
    }

    pub(super) fn allocate(len: usize, _granularity: usize, pages: Pages) -> Result<MemoryRegion> {
        let large = match pages {
            Pages::Normal => 0,
            Pages::Huge => MEM_LARGE_PAGES,
        };
        // SAFETY: a new allocation at an address the system picks touches no existing memory.
        let raw = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                len,
                MEM_COMMIT | MEM_RESERVE | large,
                PAGE_READWRITE,
            )
        };
        let Some(ptr) = NonNull::new(raw.cast::<u8>()) else {
            return Err(allocation_error(io::Error::last_os_error(), pages));
        };
        // SAFETY: the system committed `len` fresh zero-filled, writable bytes at `ptr` that no
        // other value refers to, and `Release::Os` frees the whole allocation.
        Ok(unsafe { MemoryRegion::from_raw(ptr, len, pages, Release::Os) })
    }

    fn allocation_error(error: io::Error, pages: Pages) -> Error {
        let out_of_memory = matches!(
            error.raw_os_error(),
            Some(ERROR_NOT_ENOUGH_MEMORY | ERROR_COMMITMENT_LIMIT)
        );
        let error = Error::from(error);
        match pages {
            Pages::Huge => error.with_kind(ErrorKind::Unsupported),
            Pages::Normal if out_of_memory => error.with_kind(ErrorKind::OutOfMemory),
            Pages::Normal => error,
        }
    }

    pub(super) fn release(ptr: NonNull<u8>, _len: usize, _pages: Pages) {
        // SAFETY: `ptr` is the base of one allocation made by `allocate`, which the dropped
        // region owned; MEM_RELEASE with size 0 frees exactly that allocation.
        let _ = unsafe { VirtualFree(ptr.as_ptr().cast::<c_void>(), 0, MEM_RELEASE) };
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, Pages, Result};

    pub(super) fn page_size() -> usize {
        if cfg!(target_family = "wasm") {
            65536
        } else {
            4096
        }
    }

    pub(super) fn huge_page_size() -> Option<usize> {
        None
    }

    pub(super) fn allocate(len: usize, granularity: usize, pages: Pages) -> Result<MemoryRegion> {
        match pages {
            Pages::Normal => MemoryRegion::heap(len, granularity, pages, None),
            Pages::Huge => Err(Error::new(ErrorKind::Unsupported)),
        }
    }

    pub(super) fn release(_ptr: NonNull<u8>, _len: usize, _pages: Pages) {}
}

#[cfg(test)]
mod tests {
    use super::parse_huge_page_size;

    #[test]
    fn the_huge_page_size_is_read_from_meminfo() {
        let meminfo = "MemTotal:       16303708 kB\nHugePages_Total:       0\nHugepagesize:       2048 kB\nHugetlb:               0 kB\n";
        assert_eq!(parse_huge_page_size(meminfo), Some(2 << 20));
    }

    #[test]
    fn missing_or_malformed_huge_page_sizes_are_none() {
        for meminfo in [
            "",
            "MemTotal: 1 kB\n",
            "Hugepagesize: lots kB\n",
            "Hugepagesize: 2048 MB\n",
            "Hugepagesize: 3000 kB\n",
            "Hugepagesize: 0 kB\n",
            "Hugepagesize: 99999999999999999999 kB\n",
        ] {
            assert_eq!(parse_huge_page_size(meminfo), None, "{meminfo:?}");
        }
    }
}
