use crate::cpus::{CpuSet, Cpus};
use crate::error::{Error, ErrorKind, Result};
use std::num::NonZeroUsize;

/// The machine's processors, read when the value is created, before any thread is pinned.
#[derive(Debug, Clone)]
pub struct OsCpus {
    available: CpuSet,
    parallelism: NonZeroUsize,
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
    use super::{CpuSet, Error, Result};
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
    use super::{CpuSet, Error, ErrorKind, Result};
    use crate::cpus::CpuId;
    use std::ffi::c_void;
    use std::io;

    const GROUP_WIDTH: u32 = 64;

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

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
mod platform {
    use super::{CpuSet, Error, ErrorKind, Result};

    pub(super) fn available() -> Option<CpuSet> {
        None
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
