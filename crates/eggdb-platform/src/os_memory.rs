use crate::error::{Error, ErrorKind, Result};
use crate::memory::{Memory, MemoryRegion, PageMode, check_len};
use std::ptr::NonNull;
use std::sync::OnceLock;

/// Memory from the operating system: anonymous mappings (`mmap` on Linux, Android, Apple's
/// systems and the BSDs, `VirtualAlloc` on Windows), and the heap with page alignment elsewhere.
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

    fn allocate(&self, len: usize, mode: PageMode) -> Result<MemoryRegion> {
        let granularity = match mode {
            PageMode::Normal => self.page_size(),
            PageMode::Huge | PageMode::TransparentHuge => self
                .huge_page_size()
                .ok_or_else(|| Error::new(ErrorKind::Unsupported))?,
        };
        check_len(len, granularity)?;
        platform::allocate(len, granularity, mode)
    }
}

pub(crate) fn release(ptr: NonNull<u8>, len: usize) {
    platform::release(ptr, len);
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
    kibibytes.checked_mul(1024).and_then(checked_power_of_two)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, PageMode, Result, checked_power_of_two};
    use crate::memory::Release;
    use std::ffi::{c_int, c_void};
    use std::io;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 2;

    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        ))
    ))]
    const MAP_ANONYMOUS: c_int = 0x20;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        )
    ))]
    const MAP_ANONYMOUS: c_int = 0x800;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAP_ANONYMOUS: c_int = 0x1000;

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
            target_arch = "sparc64",
            target_arch = "loongarch64"
        )
    ))]
    const MAP_HUGETLB: Option<c_int> = Some(0x40000);
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        )
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
            target_arch = "sparc64",
            target_arch = "loongarch64",
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        )
    )))]
    const MAP_HUGETLB: Option<c_int> = None;

    unsafe extern "C" {
        fn getpagesize() -> c_int;
        #[cfg_attr(
            all(
                any(target_os = "linux", target_os = "android"),
                not(any(target_env = "musl", target_env = "ohos"))
            ),
            link_name = "mmap64"
        )]
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
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

    pub(super) fn allocate(len: usize, granularity: usize, mode: PageMode) -> Result<MemoryRegion> {
        let ptr = match mode {
            PageMode::Normal => map(len, 0).map_err(Error::from)?,
            PageMode::Huge => {
                let flag = MAP_HUGETLB.ok_or_else(|| Error::new(ErrorKind::Unsupported))?;
                map(len, flag).map_err(huge_page_error)?
            }
            PageMode::TransparentHuge => transparent::allocate(len, granularity)?,
        };
        // SAFETY: `ptr` is the start of a fresh, zero-filled, writable mapping of exactly `len`
        // bytes that no other value refers to, and `Release::Os` unmaps it with that length.
        Ok(unsafe { MemoryRegion::from_raw(ptr, len, mode, Release::Os) })
    }

    pub(super) fn release(ptr: NonNull<u8>, len: usize) {
        unmap(ptr.as_ptr(), len);
    }

    fn map(len: usize, extra_flags: c_int) -> io::Result<NonNull<u8>> {
        // SAFETY: an anonymous private mapping at an address the kernel picks touches no
        // existing memory; the descriptor of an anonymous mapping must be -1.
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | extra_flags,
                -1,
                0,
            )
        };
        if raw.addr() == usize::MAX {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(raw.cast::<u8>()).ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn unmap(start: *mut u8, len: usize) {
        // SAFETY: callers pass a page-aligned range inside one mapping made by `map` that no
        // value refers to any more.
        let _ = unsafe { munmap(start.cast::<c_void>(), len) };
    }

    fn huge_page_error(error: io::Error) -> Error {
        let error = Error::from(error);
        if error.kind() == ErrorKind::OutOfMemory {
            error
        } else {
            error.with_kind(ErrorKind::Unsupported)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod transparent {
        use super::{Error, ErrorKind, NonNull, Result, c_int, c_void, io, map, unmap};

        const MADV_HUGEPAGE: c_int = 14;

        unsafe extern "C" {
            fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        }

        pub(super) fn allocate(len: usize, huge_page_size: usize) -> Result<NonNull<u8>> {
            let span = len
                .checked_add(huge_page_size)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
            let raw = map(span, 0).map_err(Error::from)?.as_ptr();
            let head = raw.addr().next_multiple_of(huge_page_size) - raw.addr();
            let start = raw.wrapping_add(head);
            if head > 0 {
                unmap(raw, head);
            }
            unmap(start.wrapping_add(len), span - head - len);
            // SAFETY: `start..start + len` is the part of the new mapping kept above, which
            // nothing else refers to; the advice only changes how the kernel backs it.
            let advised = unsafe { madvise(start.cast::<c_void>(), len, MADV_HUGEPAGE) };
            if advised == -1 {
                let error = io::Error::last_os_error();
                unmap(start, len);
                return Err(Error::from(error).with_kind(ErrorKind::Unsupported));
            }
            NonNull::new(start).ok_or_else(|| Error::new(ErrorKind::OutOfMemory))
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    mod transparent {
        use super::{Error, ErrorKind, NonNull, Result};

        pub(super) fn allocate(_len: usize, _huge_page_size: usize) -> Result<NonNull<u8>> {
            Err(Error::new(ErrorKind::Unsupported))
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, PageMode, Result, checked_power_of_two};
    use crate::memory::Release;
    use std::ffi::c_void;
    use std::io;
    use std::sync::OnceLock;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const MEM_LARGE_PAGES: u32 = 0x2000_0000;
    const PAGE_READWRITE: u32 = 0x04;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
    const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
    const ERROR_NOT_ALL_ASSIGNED: i32 = 1300;
    const ERROR_NO_SYSTEM_RESOURCES: i32 = 1450;
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

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Luid {
        low_part: u32,
        high_part: i32,
    }

    #[repr(C)]
    struct TokenPrivileges {
        privilege_count: u32,
        luid: Luid,
        attributes: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemInfo(info: *mut SystemInfo);
        fn GetLargePageMinimum() -> usize;
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn VirtualAlloc(
            address: *mut c_void,
            size: usize,
            allocation_type: u32,
            protect: u32,
        ) -> *mut c_void;
        fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut Luid) -> i32;
        fn AdjustTokenPrivileges(
            token: *mut c_void,
            disable_all: i32,
            new_state: *const TokenPrivileges,
            previous_state_len: u32,
            previous_state: *mut TokenPrivileges,
            returned_len: *mut u32,
        ) -> i32;
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

    pub(super) fn allocate(
        len: usize,
        _granularity: usize,
        mode: PageMode,
    ) -> Result<MemoryRegion> {
        let large = match mode {
            PageMode::Normal => 0,
            PageMode::Huge => {
                lock_memory_privilege()?;
                MEM_LARGE_PAGES
            }
            PageMode::TransparentHuge => return Err(Error::new(ErrorKind::Unsupported)),
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
            return Err(allocation_error(io::Error::last_os_error(), mode));
        };
        // SAFETY: the system committed `len` fresh zero-filled, writable bytes at `ptr` that no
        // other value refers to, and `Release::Os` frees the whole allocation.
        Ok(unsafe { MemoryRegion::from_raw(ptr, len, mode, Release::Os) })
    }

    pub(super) fn release(ptr: NonNull<u8>, _len: usize) {
        // SAFETY: `ptr` is the base of one allocation made by `allocate`, which the dropped
        // region owned; MEM_RELEASE with size 0 frees exactly that allocation.
        let _ = unsafe { VirtualFree(ptr.as_ptr().cast::<c_void>(), 0, MEM_RELEASE) };
    }

    fn allocation_error(error: io::Error, mode: PageMode) -> Error {
        let exhausted = matches!(
            error.raw_os_error(),
            Some(ERROR_NOT_ENOUGH_MEMORY | ERROR_NO_SYSTEM_RESOURCES | ERROR_COMMITMENT_LIMIT)
        );
        let error = Error::from(error);
        match mode {
            _ if exhausted => error.with_kind(ErrorKind::OutOfMemory),
            PageMode::Normal => error,
            PageMode::Huge | PageMode::TransparentHuge => error.with_kind(ErrorKind::Unsupported),
        }
    }

    fn lock_memory_privilege() -> Result<()> {
        static ENABLED: OnceLock<core::result::Result<(), i32>> = OnceLock::new();
        ENABLED
            .get_or_init(|| {
                enable_lock_memory_privilege().map_err(|error| error.raw_os_error().unwrap_or(0))
            })
            .map_err(|code| {
                Error::from(io::Error::from_raw_os_error(code)).with_kind(ErrorKind::Unsupported)
            })
    }

    fn enable_lock_memory_privilege() -> io::Result<()> {
        let mut token = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` takes no arguments and returns a pseudo-handle that needs
        // no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: `process` is the always-valid current-process pseudo-handle, and `token` is a
        // live out-pointer for the token handle.
        let opened =
            unsafe { OpenProcessToken(process, TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = enable_privilege(token, "SeLockMemoryPrivilege");
        // SAFETY: `token` was opened above and is closed only here, once.
        let _ = unsafe { CloseHandle(token) };
        result
    }

    fn enable_privilege(token: *mut c_void, name: &str) -> io::Result<()> {
        let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut luid = Luid {
            low_part: 0,
            high_part: 0,
        };
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call, a null
        // system name means the local system, and `luid` is a live out-pointer.
        let found = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) };
        if found == 0 {
            return Err(io::Error::last_os_error());
        }
        let privileges = TokenPrivileges {
            privilege_count: 1,
            luid,
            attributes: SE_PRIVILEGE_ENABLED,
        };
        // SAFETY: `token` is an open token handle with TOKEN_ADJUST_PRIVILEGES, `privileges`
        // outlives the call, and no previous state is requested.
        let adjusted = unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &privileges,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let error = io::Error::last_os_error();
        if adjusted == 0 || error.raw_os_error() == Some(ERROR_NOT_ALL_ASSIGNED) {
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
mod platform {
    use super::{Error, ErrorKind, MemoryRegion, NonNull, PageMode, Result};

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

    pub(super) fn allocate(len: usize, granularity: usize, mode: PageMode) -> Result<MemoryRegion> {
        match mode {
            PageMode::Normal => MemoryRegion::heap(len, granularity, mode, None),
            PageMode::Huge | PageMode::TransparentHuge => Err(Error::new(ErrorKind::Unsupported)),
        }
    }

    pub(super) fn release(_ptr: NonNull<u8>, _len: usize) {}
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
