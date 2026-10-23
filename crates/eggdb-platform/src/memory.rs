use crate::error::Result;
use std::alloc::{self, Layout};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Which pages back a [`MemoryRegion`]. A page here is always the operating system's page,
/// never a database page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageMode {
    /// The operating system's normal pages, [`Memory::page_size`] bytes each. Where the system
    /// turns transparent huge pages on for everything, these may still end up on huge pages.
    Normal,
    /// Explicit huge pages, [`Memory::huge_page_size`] bytes each: Linux `MAP_HUGETLB` from the
    /// pool the administrator reserved, Windows `MEM_LARGE_PAGES`, which needs the lock-memory
    /// privilege. They cut the cost of translating addresses for large buffer pools and are
    /// never swapped out.
    Huge,
    /// Normal pages that the kernel is asked to back with transparent huge pages when it can
    /// (Linux `madvise(MADV_HUGEPAGE)`). The region starts on a huge page boundary; whether the
    /// kernel actually uses huge pages is its choice.
    TransparentHuge,
}

/// A source of page-aligned memory.
pub trait Memory: Send + Sync {
    /// The size of a normal page in bytes; a power of two.
    fn page_size(&self) -> usize;

    /// The size of a huge page in bytes, or `None` when the platform has no huge pages.
    fn huge_page_size(&self) -> Option<usize>;

    /// Allocates a zeroed region of `len` bytes that starts on a page boundary of `mode`. `len`
    /// must be a non-zero multiple of [`Memory::page_size`] for [`PageMode::Normal`], and of
    /// [`Memory::huge_page_size`] for the huge modes.
    ///
    /// Asking for huge pages never silently hands out normal ones. It fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the platform, the kernel
    /// or the process's privileges rule them out, and with
    /// [`ErrorKind::OutOfMemory`](crate::ErrorKind::OutOfMemory) when they are configured but
    /// exhausted, which on Linux includes a pool of zero reserved huge pages, since the kernel
    /// reports both the same way. Any mode fails with
    /// [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) for a bad `len`, and with
    /// [`ErrorKind::OutOfMemory`](crate::ErrorKind::OutOfMemory) when the memory is not there.
    fn allocate(&self, len: usize, mode: PageMode) -> Result<MemoryRegion>;
}

/// Zeroed, page-aligned memory owned by one value and returned when it is dropped.
pub struct MemoryRegion {
    ptr: NonNull<u8>,
    len: usize,
    mode: PageMode,
    release: Release,
}

pub(crate) enum Release {
    #[cfg_attr(
        not(any(
            windows,
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )),
        allow(dead_code)
    )]
    Os,
    Heap {
        layout: Layout,
        accounting: Option<Arc<Accounting>>,
    },
}

#[derive(Debug, Default)]
pub(crate) struct Accounting {
    pub(crate) live_bytes: AtomicUsize,
    pub(crate) live_huge_bytes: AtomicUsize,
    pub(crate) peak_bytes: AtomicUsize,
}

impl Accounting {
    fn add(&self, len: usize, mode: PageMode) {
        let live = self.live_bytes.fetch_add(len, Ordering::Relaxed) + len;
        self.peak_bytes.fetch_max(live, Ordering::Relaxed);
        if mode == PageMode::Huge {
            self.live_huge_bytes.fetch_add(len, Ordering::Relaxed);
        }
    }

    fn remove(&self, len: usize, mode: PageMode) {
        self.live_bytes.fetch_sub(len, Ordering::Relaxed);
        if mode == PageMode::Huge {
            self.live_huge_bytes.fetch_sub(len, Ordering::Relaxed);
        }
    }
}

impl MemoryRegion {
    /// # Safety
    ///
    /// `ptr` must point to `len` zeroed, writable bytes that nothing else uses, and `release`
    /// must say how they were obtained, so that dropping the region returns them correctly.
    pub(crate) unsafe fn from_raw(
        ptr: NonNull<u8>,
        len: usize,
        mode: PageMode,
        release: Release,
    ) -> Self {
        Self {
            ptr,
            len,
            mode,
            release,
        }
    }

    pub(crate) fn heap(
        len: usize,
        alignment: usize,
        mode: PageMode,
        accounting: Option<Arc<Accounting>>,
    ) -> Result<Self> {
        use crate::error::{Error, ErrorKind};
        let layout = Layout::from_size_align(len, alignment)
            .map_err(|_| Error::new(ErrorKind::InvalidInput))?;
        if layout.size() == 0 {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        // SAFETY: `layout` has a non-zero size, checked above.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).ok_or_else(|| Error::new(ErrorKind::OutOfMemory))?;
        if let Some(accounting) = &accounting {
            accounting.add(len, mode);
        }
        let release = Release::Heap { layout, accounting };
        // SAFETY: `alloc_zeroed` returned `len` zeroed bytes owned by nobody else, and `release`
        // records the layout they must be freed with.
        Ok(unsafe { Self::from_raw(ptr, len, mode, release) })
    }

    /// Which pages back the region.
    pub fn mode(&self) -> PageMode {
        self.mode
    }
}

impl Deref for MemoryRegion {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialised bytes owned by `self`, and the shared borrow
        // of `self` keeps them from being mutated or freed.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for MemoryRegion {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `len` initialised bytes owned by `self`, and the exclusive
        // borrow of `self` makes this the only access to them.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MemoryRegion {
    fn drop(&mut self) {
        match &self.release {
            Release::Os => crate::os_memory::release(self.ptr, self.len),
            Release::Heap { layout, accounting } => {
                // SAFETY: `ptr` was returned by `alloc_zeroed` with this same `layout` and is
                // freed only here, once.
                unsafe { alloc::dealloc(self.ptr.as_ptr(), *layout) }
                if let Some(accounting) = accounting {
                    accounting.remove(self.len, self.mode);
                }
            }
        }
    }
}

// SAFETY: the region exclusively owns its memory, like a `Box<[u8]>`, so moving it to another
// thread is sound.
unsafe impl Send for MemoryRegion {}

// SAFETY: shared access only hands out `&[u8]`, which is safe to use from several threads.
unsafe impl Sync for MemoryRegion {}

impl fmt::Debug for MemoryRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryRegion")
            .field("len", &self.len)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

pub(crate) fn check_len(len: usize, granularity: usize) -> Result<()> {
    use crate::error::{Error, ErrorKind};
    if len == 0 || !len.is_multiple_of(granularity) {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    Ok(())
}
