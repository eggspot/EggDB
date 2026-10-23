use crate::error::Result;
use std::alloc::{self, Layout};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Which pages back a [`MemoryRegion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pages {
    /// The operating system's normal pages, [`Memory::page_size`] bytes each.
    Normal,
    /// Explicit huge pages, [`Memory::huge_page_size`] bytes each, which cut the cost of
    /// translating addresses for large buffer pools. Asking for them fails with
    /// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) when the platform has none to
    /// give, rather than silently handing out normal pages.
    Huge,
}

/// A source of page-aligned memory.
pub trait Memory: Send + Sync {
    /// The size of a normal page in bytes; a power of two.
    fn page_size(&self) -> usize;

    /// The size of a huge page in bytes, or `None` when the platform has no huge pages.
    fn huge_page_size(&self) -> Option<usize>;

    /// Allocates a zeroed region of at least `len` bytes, starting on a page boundary of the
    /// chosen `pages`. The length is rounded up to a whole number of those pages, and
    /// the region's length is the rounded one.
    ///
    /// Fails with [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput) when `len` is zero
    /// or too large to round, with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported)
    /// when `pages` is [`Pages::Huge`] and no huge pages can be had, and with
    /// [`ErrorKind::OutOfMemory`](crate::ErrorKind::OutOfMemory) when the memory is not there.
    fn allocate(&self, len: usize, pages: Pages) -> Result<MemoryRegion>;
}

/// Zeroed, page-aligned memory owned by one value and returned when it is dropped.
pub struct MemoryRegion {
    ptr: NonNull<u8>,
    len: usize,
    pages: Pages,
    release: Release,
}

pub(crate) enum Release {
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    Os,
    Heap {
        layout: Layout,
        live_bytes: Option<Arc<AtomicUsize>>,
    },
}

impl MemoryRegion {
    /// # Safety
    ///
    /// `ptr` must point to `len` zeroed, writable bytes that nothing else uses, and `release`
    /// must say how they were obtained, so that dropping the region returns them correctly.
    pub(crate) unsafe fn from_raw(
        ptr: NonNull<u8>,
        len: usize,
        pages: Pages,
        release: Release,
    ) -> Self {
        Self {
            ptr,
            len,
            pages,
            release,
        }
    }

    pub(crate) fn heap(
        len: usize,
        alignment: usize,
        pages: Pages,
        live_bytes: Option<Arc<AtomicUsize>>,
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
        if let Some(live) = &live_bytes {
            live.fetch_add(len, Ordering::Relaxed);
        }
        let release = Release::Heap { layout, live_bytes };
        // SAFETY: `alloc_zeroed` returned `len` zeroed bytes owned by nobody else, and `release`
        // records the layout they must be freed with.
        Ok(unsafe { Self::from_raw(ptr, len, pages, release) })
    }

    /// Which pages back the region.
    pub fn pages(&self) -> Pages {
        self.pages
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
            Release::Os => crate::os_memory::release(self.ptr, self.len, self.pages),
            Release::Heap { layout, live_bytes } => {
                // SAFETY: `ptr` was returned by `alloc_zeroed` with this same `layout` and is
                // freed only here, once.
                unsafe { alloc::dealloc(self.ptr.as_ptr(), *layout) }
                if let Some(live) = live_bytes {
                    live.fetch_sub(self.len, Ordering::Relaxed);
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
            .field("pages", &self.pages)
            .finish_non_exhaustive()
    }
}

pub(crate) fn round_up(len: usize, granularity: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    len.checked_next_multiple_of(granularity)
}
