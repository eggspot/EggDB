use crate::cpus::{CpuSet, Cpus};
use crate::error::{Error, ErrorKind, Result};
use crate::topology::CpuTopology;
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock};

/// The machine's processors. The available CPUs and the parallelism are read when the value is
/// created, before any thread is pinned; the topology is read once, on first use, and shared by
/// clones, so a program that never asks for it never pays for reading it.
#[derive(Debug, Clone)]
pub struct OsCpus {
    available: CpuSet,
    parallelism: NonZeroUsize,
    topology: Arc<OnceLock<CpuTopology>>,
}

impl OsCpus {
    /// Reads the CPUs this process may run on and how many threads can run at once.
    pub fn new() -> Self {
        let reported = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let available = platform::available()
            .filter(|set| !set.is_empty())
            .unwrap_or_else(|| first(reported));
        Self {
            parallelism: at_most_one_per_cpu(reported, &available),
            available,
            topology: Arc::default(),
        }
    }
}

impl Default for OsCpus {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpus for OsCpus {
    fn available(&self) -> CpuSet {
        self.available.clone()
    }

    fn parallelism(&self) -> NonZeroUsize {
        self.parallelism
    }

    fn topology(&self) -> CpuTopology {
        self.topology.get_or_init(platform::topology).clone()
    }

    fn pin_current_thread(&self, cpus: &CpuSet) -> Result<()> {
        if cpus.is_empty() || !cpus.is_subset(&self.available) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        platform::pin_current_thread(cpus)
    }
}

fn first(count: NonZeroUsize) -> CpuSet {
    CpuSet::first(count.get().min(CpuSet::MAX_CPUS)).unwrap_or_default()
}

fn at_most_one_per_cpu(parallelism: NonZeroUsize, available: &CpuSet) -> NonZeroUsize {
    NonZeroUsize::new(available.len()).map_or(parallelism, |cpus| parallelism.min(cpus))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::{CpuSet, CpuTopology, Error, Result};
    use crate::cpus::CpuId;
    use std::ffi::{c_int, c_ulong};
    use std::io;

    const EINVAL: i32 = 22;
    const WORD_BITS: usize = c_ulong::BITS as usize;

    unsafe extern "C" {
        fn sched_getaffinity(pid: c_int, size: usize, mask: *mut c_ulong) -> c_int;
        fn sched_setaffinity(pid: c_int, size: usize, mask: *const c_ulong) -> c_int;
    }

    pub(super) fn available() -> Option<CpuSet> {
        let mut words = 1024 / WORD_BITS;
        loop {
            let mut mask: Vec<c_ulong> = vec![0; words];
            let size = words * size_of::<c_ulong>();
            // SAFETY: `mask` holds `size` writable bytes for the kernel to fill, and pid 0
            // means the calling thread.
            let result = unsafe { sched_getaffinity(0, size, mask.as_mut_ptr()) };
            if result != -1 {
                return Some(from_words(&mask));
            }
            let too_small = io::Error::last_os_error().raw_os_error() == Some(EINVAL);
            if !too_small || words * WORD_BITS >= CpuSet::MAX_CPUS {
                return None;
            }
            words *= 2;
        }
    }

    pub(super) fn topology() -> CpuTopology {
        crate::topology_sysfs::read(&|path| {
            std::fs::read_to_string(format!("/sys/devices/system/{path}")).ok()
        })
    }

    pub(super) fn pin_current_thread(cpus: &CpuSet) -> Result<()> {
        let mask = to_words(cpus);
        // SAFETY: `mask` holds the given number of readable bytes, and pid 0 means the calling
        // thread.
        let result =
            unsafe { sched_setaffinity(0, mask.len() * size_of::<c_ulong>(), mask.as_ptr()) };
        if result == -1 {
            return Err(Error::from(io::Error::last_os_error()));
        }
        Ok(())
    }

    fn from_words(mask: &[c_ulong]) -> CpuSet {
        mask.iter()
            .enumerate()
            .flat_map(|(word, &bits)| {
                (0..WORD_BITS)
                    .filter(move |bit| bits & (1 << bit) != 0)
                    .map(move |bit| word * WORD_BITS + bit)
            })
            .filter_map(|index| u32::try_from(index).ok())
            .filter_map(|index| CpuId::new(index).ok())
            .collect()
    }

    fn to_words(cpus: &CpuSet) -> Vec<c_ulong> {
        let highest = cpus.iter().last().map_or(0, |cpu| cpu.index() as usize);
        let mut mask: Vec<c_ulong> = vec![0; highest / WORD_BITS + 1];
        for cpu in cpus.iter() {
            let index = cpu.index() as usize;
            mask[index / WORD_BITS] |= 1 << (index % WORD_BITS);
        }
        mask
    }

    #[cfg(test)]
    mod tests {
        use super::{CpuSet, from_words, to_words};

        #[test]
        fn masks_and_sets_convert_both_ways() {
            let set = CpuSet::parse_list("0,2,63-65,200").unwrap();
            assert_eq!(from_words(&to_words(&set)), set);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{CpuSet, CpuTopology, Error, ErrorKind, Result};
    use crate::cpus::CpuId;
    use crate::topology_windows::{self, AffinityLayout};
    use std::ffi::c_void;
    use std::io;

    const GROUP_WIDTH: u32 = 64;
    const NARROW_GROUP_WIDTH: u32 = 32;
    const RELATION_ALL: u32 = 0xFFFF;
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
    const ATTEMPTS: usize = 4;

    #[repr(C)]
    struct GroupAffinity {
        mask: usize,
        group: u16,
        reserved: [u16; 3],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetActiveProcessorGroupCount() -> u16;
        fn GetActiveProcessorCount(group: u16) -> u32;
        fn GetCurrentThread() -> *mut c_void;
        fn GetLogicalProcessorInformationEx(
            relationship: u32,
            buffer: *mut u64,
            returned_length: *mut u32,
        ) -> i32;
        fn SetThreadGroupAffinity(
            thread: *mut c_void,
            affinity: *const GroupAffinity,
            previous: *mut GroupAffinity,
        ) -> i32;
    }

    pub(super) fn available() -> Option<CpuSet> {
        // SAFETY: `GetActiveProcessorGroupCount` takes no arguments and only reads system state.
        let groups = unsafe { GetActiveProcessorGroupCount() };
        let mut set = CpuSet::new();
        for group in 0..groups {
            // SAFETY: `GetActiveProcessorCount` takes a group number by value and returns 0
            // for a group that does not exist.
            let count = unsafe { GetActiveProcessorCount(group) }.min(GROUP_WIDTH);
            for bit in 0..count {
                set.insert(CpuId::new(u32::from(group) * GROUP_WIDTH + bit).ok()?);
            }
        }
        Some(set)
    }

    pub(super) fn topology() -> CpuTopology {
        if AffinityLayout::NATIVE == AffinityLayout::Narrow && groups_wider_than_narrow_masks() {
            return CpuTopology::unknown();
        }
        let mut words: Vec<u64> = Vec::new();
        let mut length = 0_u32;
        for _ in 0..ATTEMPTS {
            let buffer = if words.is_empty() {
                std::ptr::null_mut()
            } else {
                words.as_mut_ptr()
            };
            // SAFETY: `buffer` is null while `length` is zero, or points at `words`, whose
            // `length` bytes are writable and aligned for the records; `length` is a live
            // in-out pointer.
            let done =
                unsafe { GetLogicalProcessorInformationEx(RELATION_ALL, buffer, &mut length) };
            let Ok(bytes) = usize::try_from(length) else {
                return CpuTopology::unknown();
            };
            if done != 0 {
                let mut records: Vec<u8> =
                    words.iter().flat_map(|word| word.to_ne_bytes()).collect();
                records.truncate(bytes);
                return topology_windows::parse(&records, AffinityLayout::NATIVE);
            }
            if io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) {
                return CpuTopology::unknown();
            }
            let needed_words = bytes.div_ceil(size_of::<u64>());
            words.resize(needed_words, 0);
            let Ok(capacity) = u32::try_from(needed_words * size_of::<u64>()) else {
                return CpuTopology::unknown();
            };
            length = capacity;
        }
        CpuTopology::unknown()
    }

    fn groups_wider_than_narrow_masks() -> bool {
        // SAFETY: `GetActiveProcessorGroupCount` takes no arguments and only reads system state.
        let groups = unsafe { GetActiveProcessorGroupCount() };
        (0..groups).any(|group| {
            // SAFETY: `GetActiveProcessorCount` takes a group number by value and returns 0
            // for a group that does not exist.
            let count = unsafe { GetActiveProcessorCount(group) };
            count > NARROW_GROUP_WIDTH
        })
    }

    pub(super) fn pin_current_thread(cpus: &CpuSet) -> Result<()> {
        let invalid = || Error::new(ErrorKind::InvalidInput);
        let group = cpus.iter().next().ok_or_else(invalid)?.index() / GROUP_WIDTH;
        let mut mask = 0_usize;
        for cpu in cpus.iter() {
            if cpu.index() / GROUP_WIDTH != group {
                return Err(invalid());
            }
            mask |= 1_usize
                .checked_shl(cpu.index() % GROUP_WIDTH)
                .ok_or_else(invalid)?;
        }
        let affinity = GroupAffinity {
            mask,
            group: u16::try_from(group).map_err(|_| invalid())?,
            reserved: [0; 3],
        };
        // SAFETY: `GetCurrentThread` takes no arguments and returns a pseudo-handle that needs
        // no closing.
        let thread = unsafe { GetCurrentThread() };
        // SAFETY: `thread` is the calling thread's pseudo-handle, `affinity` outlives the call,
        // and a null pointer asks for no previous affinity.
        let set = unsafe { SetThreadGroupAffinity(thread, &affinity, std::ptr::null_mut()) };
        if set == 0 {
            return Err(Error::from(io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd"))]
mod platform {
    use super::{CpuSet, CpuTopology, Error, Result};
    use crate::cpus::CpuId;
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::io;
    use std::num::NonZeroUsize;

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            old: *mut c_void,
            old_length: *mut usize,
            new: *const c_void,
            new_length: usize,
        ) -> c_int;
    }

    fn number(name: &CStr) -> Option<NonZeroUsize> {
        let mut value = [0_u8; 8];
        let mut length = value.len();
        // SAFETY: `name` is NUL-terminated, `value` has `length` writable bytes, and a null new
        // value with length 0 sets nothing.
        let result = unsafe {
            sysctlbyname(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null(),
                0,
            )
        };
        if result != 0 {
            return None;
        }
        let number = match length {
            4 => u64::from(u32::from_ne_bytes([value[0], value[1], value[2], value[3]])),
            8 => u64::from_ne_bytes(value),
            _ => return None,
        };
        usize::try_from(number).ok().and_then(NonZeroUsize::new)
    }

    pub(super) fn topology() -> CpuTopology {
        let logical = number(c"hw.ncpu");
        let cores = number(c"kern.smp.cores").filter(|cores| logical.is_none_or(|l| *cores <= l));
        CpuTopology::from_counts(cores, logical, None, None).unwrap_or_default()
    }

    #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
    mod mask {
        use super::{CpuId, CpuSet};
        use std::ffi::c_ulong;

        pub(super) const WORD_BITS: usize = c_ulong::BITS as usize;

        pub(super) fn from_words(words: &[c_ulong]) -> CpuSet {
            let mut set = CpuSet::new();
            for (at, word) in words.iter().enumerate() {
                for bit in 0..WORD_BITS {
                    if word & (1 << bit) == 0 {
                        continue;
                    }
                    let index = at * WORD_BITS + bit;
                    if let Some(cpu) = u32::try_from(index).ok().and_then(|i| CpuId::new(i).ok()) {
                        set.insert(cpu);
                    }
                }
            }
            set
        }

        pub(super) fn to_words(cpus: &CpuSet, words: usize) -> Option<Vec<c_ulong>> {
            let mut mask = vec![0; words];
            for cpu in cpus.iter() {
                let index = cpu.index() as usize;
                let word = mask.get_mut(index / WORD_BITS)?;
                *word |= 1 << (index % WORD_BITS);
            }
            Some(mask)
        }
    }

    #[cfg(target_os = "freebsd")]
    mod affinity {
        use super::mask::{WORD_BITS, from_words, to_words};
        use super::{CpuSet, Error, Result, io};
        use crate::error::ErrorKind;
        use std::ffi::{c_int, c_ulong};

        const CPU_LEVEL_WHICH: c_int = 3;
        const CPU_WHICH_TID: c_int = 1;
        const CURRENT_THREAD: i64 = -1;
        const ERANGE: i32 = 34;
        const MASK_BITS: [usize; 2] = [1024, 256];

        unsafe extern "C" {
            fn cpuset_getaffinity(
                level: c_int,
                which: c_int,
                id: i64,
                size: usize,
                mask: *mut c_ulong,
            ) -> c_int;
            fn cpuset_setaffinity(
                level: c_int,
                which: c_int,
                id: i64,
                size: usize,
                mask: *const c_ulong,
            ) -> c_int;
        }

        pub(in super::super) fn available() -> Option<CpuSet> {
            for bits in MASK_BITS {
                let mut mask: Vec<c_ulong> = vec![0; bits / WORD_BITS];
                let size = mask.len() * size_of::<c_ulong>();
                // SAFETY: `mask` holds `size` writable bytes for the kernel to fill, and id -1
                // with CPU_WHICH_TID means the calling thread.
                let result = unsafe {
                    cpuset_getaffinity(
                        CPU_LEVEL_WHICH,
                        CPU_WHICH_TID,
                        CURRENT_THREAD,
                        size,
                        mask.as_mut_ptr(),
                    )
                };
                if result == 0 {
                    return Some(from_words(&mask));
                }
                if io::Error::last_os_error().raw_os_error() != Some(ERANGE) {
                    return None;
                }
            }
            None
        }

        pub(in super::super) fn pin_current_thread(cpus: &CpuSet) -> Result<()> {
            let mut last = Error::new(ErrorKind::InvalidInput);
            for bits in MASK_BITS {
                let Some(mask) = to_words(cpus, bits / WORD_BITS) else {
                    continue;
                };
                // SAFETY: `mask` holds the given number of readable bytes, and id -1 with
                // CPU_WHICH_TID means the calling thread.
                let result = unsafe {
                    cpuset_setaffinity(
                        CPU_LEVEL_WHICH,
                        CPU_WHICH_TID,
                        CURRENT_THREAD,
                        mask.len() * size_of::<c_ulong>(),
                        mask.as_ptr(),
                    )
                };
                if result == 0 {
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                let too_wide = error.raw_os_error() == Some(ERANGE);
                last = Error::from(error);
                if !too_wide {
                    break;
                }
            }
            Err(last)
        }
    }

    #[cfg(target_os = "dragonfly")]
    mod affinity {
        use super::mask::{WORD_BITS, from_words, to_words};
        use super::{CpuSet, Error, Result, io};
        use crate::error::ErrorKind;
        use std::ffi::{c_int, c_ulong, c_void};

        const MASK_BITS: usize = 256;

        unsafe extern "C" {
            fn pthread_self() -> *mut c_void;
            fn pthread_getaffinity_np(thread: *mut c_void, size: usize, mask: *mut c_ulong)
            -> c_int;
            fn pthread_setaffinity_np(
                thread: *mut c_void,
                size: usize,
                mask: *const c_ulong,
            ) -> c_int;
        }

        pub(in super::super) fn available() -> Option<CpuSet> {
            let mut mask: Vec<c_ulong> = vec![0; MASK_BITS / WORD_BITS];
            // SAFETY: pthread_self takes no arguments and names the calling thread.
            let thread = unsafe { pthread_self() };
            // SAFETY: `mask` holds the given number of writable bytes for the call to fill.
            let result = unsafe {
                pthread_getaffinity_np(
                    thread,
                    mask.len() * size_of::<c_ulong>(),
                    mask.as_mut_ptr(),
                )
            };
            (result == 0).then(|| from_words(&mask))
        }

        pub(in super::super) fn pin_current_thread(cpus: &CpuSet) -> Result<()> {
            let mask = to_words(cpus, MASK_BITS / WORD_BITS)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
            // SAFETY: pthread_self takes no arguments and names the calling thread.
            let thread = unsafe { pthread_self() };
            // SAFETY: `mask` holds the given number of readable bytes.
            let result = unsafe {
                pthread_setaffinity_np(thread, mask.len() * size_of::<c_ulong>(), mask.as_ptr())
            };
            if result != 0 {
                return Err(Error::from(io::Error::from_raw_os_error(result)));
            }
            Ok(())
        }
    }

    #[cfg(target_os = "netbsd")]
    mod affinity {
        use super::{CpuId, CpuSet, Error, Result, io, number};
        use crate::error::ErrorKind;
        use std::ffi::{c_int, c_ulong, c_void};

        unsafe extern "C" {
            fn pthread_self() -> *mut c_void;
            fn pthread_getaffinity_np(thread: *mut c_void, size: usize, set: *mut c_void) -> c_int;
            fn pthread_setaffinity_np(
                thread: *mut c_void,
                size: usize,
                set: *const c_void,
            ) -> c_int;
            fn _cpuset_create() -> *mut c_void;
            fn _cpuset_destroy(set: *mut c_void);
            fn _cpuset_size(set: *const c_void) -> usize;
            fn _cpuset_set(cpu: c_ulong, set: *mut c_void) -> c_int;
            fn _cpuset_isset(cpu: c_ulong, set: *const c_void) -> c_int;
        }

        struct OwnedCpuSet(*mut c_void);

        impl OwnedCpuSet {
            fn new() -> Option<Self> {
                // SAFETY: _cpuset_create takes no arguments; a null result means no memory.
                let set = unsafe { _cpuset_create() };
                (!set.is_null()).then_some(Self(set))
            }

            fn size(&self) -> usize {
                // SAFETY: self.0 is a live set from _cpuset_create.
                unsafe { _cpuset_size(self.0) }
            }
        }

        impl Drop for OwnedCpuSet {
            fn drop(&mut self) {
                // SAFETY: self.0 came from _cpuset_create and is destroyed only here.
                unsafe { _cpuset_destroy(self.0) };
            }
        }

        fn known_cpus() -> usize {
            number(c"hw.ncpu").map_or(1, std::num::NonZeroUsize::get)
        }

        pub(in super::super) fn available() -> Option<CpuSet> {
            let set = OwnedCpuSet::new()?;
            // SAFETY: pthread_self takes no arguments and names the calling thread.
            let thread = unsafe { pthread_self() };
            // SAFETY: set is a live set of set.size() bytes for the call to fill.
            let result = unsafe { pthread_getaffinity_np(thread, set.size(), set.0) };
            if result != 0 {
                return None;
            }
            let mut cpus = CpuSet::new();
            for index in 0..known_cpus().min(CpuSet::MAX_CPUS) {
                // SAFETY: set is a live set; an index past its size reads as unset.
                let member = unsafe { _cpuset_isset(index as c_ulong, set.0) } > 0;
                if member && let Ok(cpu) = CpuId::new(index as u32) {
                    cpus.insert(cpu);
                }
            }
            Some(cpus).filter(|cpus| !cpus.is_empty())
        }

        pub(in super::super) fn pin_current_thread(cpus: &CpuSet) -> Result<()> {
            let set = OwnedCpuSet::new().ok_or_else(|| Error::new(ErrorKind::OutOfMemory))?;
            for cpu in cpus.iter() {
                // SAFETY: set is a live set; _cpuset_set refuses an index past its size.
                if unsafe { _cpuset_set(c_ulong::from(cpu.index()), set.0) } != 0 {
                    return Err(Error::new(ErrorKind::InvalidInput));
                }
            }
            // SAFETY: pthread_self takes no arguments and names the calling thread.
            let thread = unsafe { pthread_self() };
            // SAFETY: set is a live set of set.size() readable bytes.
            let result = unsafe { pthread_setaffinity_np(thread, set.size(), set.0) };
            if result != 0 {
                return Err(Error::from(io::Error::from_raw_os_error(result)));
            }
            Ok(())
        }
    }

    pub(super) use affinity::{available, pin_current_thread};
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd"
)))]
mod platform {
    use super::{CpuSet, CpuTopology, Error, ErrorKind, Result};

    pub(super) fn available() -> Option<CpuSet> {
        None
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn topology() -> CpuTopology {
        use std::ffi::{CStr, c_char, c_int, c_void};
        use std::num::NonZeroUsize;

        unsafe extern "C" {
            fn sysctlbyname(
                name: *const c_char,
                old: *mut c_void,
                old_length: *mut usize,
                new: *mut c_void,
                new_length: usize,
            ) -> c_int;
        }

        fn number(name: &CStr) -> Option<NonZeroUsize> {
            let mut value = [0_u8; 8];
            let mut length = value.len();
            // SAFETY: `name` is NUL-terminated, `value` has `length` writable bytes, and a null
            // new value with length 0 sets nothing.
            let result = unsafe {
                sysctlbyname(
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if result != 0 {
                return None;
            }
            let number = match length {
                4 => u64::from(u32::from_ne_bytes([value[0], value[1], value[2], value[3]])),
                8 => u64::from_ne_bytes(value),
                _ => return None,
            };
            usize::try_from(number).ok().and_then(NonZeroUsize::new)
        }

        CpuTopology::from_counts(
            number(c"hw.physicalcpu"),
            number(c"hw.logicalcpu"),
            number(c"hw.packages"),
            number(c"hw.cachelinesize").filter(|size| size.is_power_of_two()),
        )
        .map(|topology| {
            topology.with_performance_core_count(
                number(c"hw.nperflevels")
                    .filter(|levels| levels.get() > 1)
                    .and_then(|_| number(c"hw.perflevel0.logicalcpu")),
            )
        })
        .unwrap_or_default()
    }

    #[cfg(not(target_vendor = "apple"))]
    pub(super) fn topology() -> CpuTopology {
        CpuTopology::unknown()
    }

    pub(super) fn pin_current_thread(_cpus: &CpuSet) -> Result<()> {
        Err(Error::new(ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuSet, at_most_one_per_cpu};
    use std::num::NonZeroUsize;

    #[test]
    fn parallelism_never_exceeds_the_available_cpus() {
        let two = CpuSet::parse_list("4-5").unwrap();
        let count = |n| NonZeroUsize::new(n).unwrap();
        assert_eq!(at_most_one_per_cpu(count(8), &two), count(2));
        assert_eq!(at_most_one_per_cpu(count(1), &two), count(1));
        assert_eq!(at_most_one_per_cpu(count(3), &CpuSet::new()), count(3));
    }
}
