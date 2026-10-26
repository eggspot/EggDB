use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct CountingAllocator {
    allocations: AtomicU64,
}

impl CountingAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
        }
    }

    pub(crate) fn count(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
    }

    fn record(&self) {
        self.allocations.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every call goes unchanged to the system allocator, which meets `GlobalAlloc`'s
// contract; counting the calls changes nothing about the memory handed out.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.record();
        // SAFETY: the caller meets `alloc`'s contract, which `System.alloc` shares.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.record();
        // SAFETY: the caller meets `alloc_zeroed`'s contract, which `System.alloc_zeroed` shares.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.record();
        // SAFETY: the caller meets `realloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller meets `dealloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        unsafe { System.dealloc(ptr, layout) }
    }
}
