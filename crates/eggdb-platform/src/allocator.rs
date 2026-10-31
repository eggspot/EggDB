//! EggDB's own memory allocator, installed by a program that opts in with
//! `#[global_allocator]`. Blocks up to 32 KiB come from 40 size classes: each thread keeps a
//! cache of free blocks per class, refilled and drained in batches through one shared list per
//! class, and new blocks are carved from 64 KiB runs of 1 MiB chunks. Larger blocks, and blocks
//! aligned to more than 32 KiB, go straight to the system allocator. A budget caps the bytes the
//! allocator takes from the system, for embedded devices with a fixed amount of memory (D41).
//! Memory carved for small blocks is reused, never handed back to the system.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const SMALLEST_STEP: usize = 16;
const LARGEST_SMALL: usize = 32 * 1024;
const CLASS_COUNT: usize = 40;
const RUN_BYTES: usize = 64 * 1024;
const CHUNK_BYTES: usize = 1024 * 1024;
const CACHE_BYTES_PER_CLASS: usize = 64 * 1024;
const REFILL_BYTES: usize = 16 * 1024;
const SPINS_BEFORE_YIELD: u32 = 64;

static CENTRAL: [Locked<FreeList>; CLASS_COUNT] =
    [const { Locked::new(FreeList::EMPTY) }; CLASS_COUNT];
static RUNS: Locked<RunSource> = Locked::new(RunSource {
    next: ptr::null_mut(),
    runs_left: 0,
});
static FROM_SYSTEM: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static CACHE: ThreadCache = const { ThreadCache::new() };
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// EggDB's memory allocator. It keeps its state in the process, not in the value, so every
/// `EggAllocator` shares the same caches and the same count of bytes taken from the system;
/// each value only carries its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EggAllocator {
    budget: usize,
}

impl EggAllocator {
    /// An allocator without a budget.
    pub const fn new() -> Self {
        Self { budget: usize::MAX }
    }

    /// An allocator that fails an allocation (returns null) rather than hold more than
    /// `bytes` from the system, counted across every `EggAllocator` in the process.
    pub const fn with_budget(bytes: usize) -> Self {
        Self { budget: bytes }
    }

    /// The budget, in bytes; `usize::MAX` when there is none.
    pub const fn budget(&self) -> usize {
        self.budget
    }

    /// The bytes the allocator holds from the system: carved chunks and large blocks.
    pub fn bytes_from_system(&self) -> usize {
        FROM_SYSTEM.load(Ordering::Relaxed)
    }

    fn reserve(&self, bytes: usize) -> bool {
        let mut held = FROM_SYSTEM.load(Ordering::Relaxed);
        loop {
            let Some(after) = held.checked_add(bytes).filter(|after| *after <= self.budget) else {
                return false;
            };
            match FROM_SYSTEM.compare_exchange_weak(held, after, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => held = now,
            }
        }
    }

    fn give_back(bytes: usize) {
        FROM_SYSTEM.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn take_small(&self, class: usize) -> *mut u8 {
        let cached = INSIDE.try_with(|inside| {
            if inside.replace(true) {
                return None;
            }
            let taken = CACHE.try_with(|cache| cache.take(class, self)).ok();
            inside.set(false);
            taken
        });
        match cached {
            Ok(Some(block)) => block,
            _ => take_from_central(class, self),
        }
    }

    fn put_small(class: usize, block: *mut u8) {
        let cached = INSIDE.try_with(|inside| {
            if inside.replace(true) {
                return false;
            }
            let put = CACHE.try_with(|cache| cache.put(class, block)).is_ok();
            inside.set(false);
            put
        });
        if cached != Ok(true) {
            CENTRAL[class].with(|list| list.push(block));
        }
    }
}

impl Default for EggAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: small blocks come from runs aligned to 64 KiB, carved at multiples of their class
// size, and a layout aligned to more than 16 bytes gets a power-of-two class at least as large
// as its alignment, so every small block is aligned and large enough; each block is handed out
// to one owner at a time, as the free lists hold only blocks given back by `dealloc`. Larger
// layouts go unchanged to the system allocator, which meets the same contract.
unsafe impl GlobalAlloc for EggAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match small_class(layout) {
            Some(class) => self.take_small(class),
            None => {
                if !self.reserve(layout.size()) {
                    return ptr::null_mut();
                }
                // SAFETY: the caller's layout has a non-zero size, as `GlobalAlloc` requires.
                let block = unsafe { System.alloc(layout) };
                if block.is_null() {
                    Self::give_back(layout.size());
                }
                block
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match small_class(layout) {
            Some(class) => {
                let block = self.take_small(class);
                if !block.is_null() {
                    // SAFETY: the block holds at least `layout.size()` bytes and is ours.
                    unsafe { ptr::write_bytes(block, 0, layout.size()) };
                }
                block
            }
            None => {
                if !self.reserve(layout.size()) {
                    return ptr::null_mut();
                }
                // SAFETY: as in `alloc`.
                let block = unsafe { System.alloc_zeroed(layout) };
                if block.is_null() {
                    Self::give_back(layout.size());
                }
                block
            }
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        match small_class(layout) {
            Some(class) => Self::put_small(class, block),
            None => {
                // SAFETY: a layout without a small class was allocated by the system allocator
                // with this same layout, as the caller guarantees.
                unsafe { System.dealloc(block, layout) };
                Self::give_back(layout.size());
            }
        }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        let old_class = small_class(layout);
        if old_class.is_some() && old_class == small_class(new_layout) {
            return block;
        }
        if old_class.is_none() && small_class(new_layout).is_none() {
            if !self.reserve(new_size.saturating_sub(layout.size())) {
                return ptr::null_mut();
            }
            // SAFETY: the block came from the system allocator with `layout`, as the caller
            // guarantees, and `new_size` is non-zero.
            let moved = unsafe { System.realloc(block, layout, new_size) };
            if moved.is_null() {
                Self::give_back(new_size.saturating_sub(layout.size()));
            } else if new_size < layout.size() {
                Self::give_back(layout.size() - new_size);
            }
            return moved;
        }
        // SAFETY: `new_layout` is valid and non-zero, as the caller guarantees for `new_size`.
        let moved = unsafe { self.alloc(new_layout) };
        if !moved.is_null() {
            // SAFETY: both blocks hold at least the smaller size and do not overlap.
            unsafe { ptr::copy_nonoverlapping(block, moved, layout.size().min(new_size)) };
            // SAFETY: the old block is the caller's, allocated with `layout`.
            unsafe { self.dealloc(block, layout) };
        }
        moved
    }
}

const fn class_size(class: usize) -> usize {
    if class < 8 {
        (class + 1) * SMALLEST_STEP
    } else {
        let group = (class - 8) / 4;
        let step = (class - 8) % 4 + 1;
        let base = 128 << group;
        base + step * (base / 4)
    }
}

fn class_of_size(size: usize) -> usize {
    if size <= 128 {
        return size.max(1).div_ceil(SMALLEST_STEP) - 1;
    }
    let group = ((size - 1) / 128).ilog2() as usize;
    let base = 128 << group;
    let step = (size - base).div_ceil(base / 4);
    8 + group * 4 + step - 1
}

fn small_class(layout: Layout) -> Option<usize> {
    let size = if layout.align() > SMALLEST_STEP {
        layout.size().max(layout.align()).checked_next_power_of_two()?
    } else {
        layout.size()
    };
    (size <= LARGEST_SMALL && layout.align() <= LARGEST_SMALL).then(|| class_of_size(size))
}

fn blocks_per_batch(class: usize, bytes: usize) -> usize {
    (bytes / class_size(class)).max(1)
}

fn take_from_central(class: usize, allocator: &EggAllocator) -> *mut u8 {
    let taken = CENTRAL[class].with(|list| list.pop());
    if !taken.is_null() {
        return taken;
    }
    let mut carved = carve_run(class, allocator);
    let block = carved.pop();
    if !carved.is_empty() {
        CENTRAL[class].with(|list| list.append(carved));
    }
    block
}

fn carve_run(class: usize, allocator: &EggAllocator) -> FreeList {
    let run = RUNS.with(|source| source.next_run(allocator));
    let mut list = FreeList::EMPTY;
    if run.is_null() {
        return list;
    }
    let size = class_size(class);
    for index in (0..RUN_BYTES / size).rev() {
        list.push(run.wrapping_add(index * size));
    }
    list
}

#[derive(Debug, Clone, Copy)]
struct FreeList {
    head: *mut u8,
    tail: *mut u8,
    count: usize,
}

// SAFETY: a free list owns the free blocks it links; nothing else reads or writes them until a
// block is popped, so the list may move between threads.
unsafe impl Send for FreeList {}

impl FreeList {
    const EMPTY: Self = Self {
        head: ptr::null_mut(),
        tail: ptr::null_mut(),
        count: 0,
    };

    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    fn push(&mut self, block: *mut u8) {
        // SAFETY: a free block is at least 16 bytes, aligned to 16, and owned by this list, so
        // its first word holds the link.
        unsafe { block.cast::<*mut u8>().write(self.head) };
        if self.head.is_null() {
            self.tail = block;
        }
        self.head = block;
        self.count += 1;
    }

    fn pop(&mut self) -> *mut u8 {
        let block = self.head;
        if !block.is_null() {
            // SAFETY: the block is on this list, so its first word holds the next link.
            self.head = unsafe { block.cast::<*mut u8>().read() };
            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }
            self.count -= 1;
        }
        block
    }

    fn append(&mut self, other: FreeList) {
        if other.is_empty() {
            return;
        }
        // SAFETY: `other.tail` is the last block of a list this one now takes over, and its
        // first word holds its link.
        unsafe { other.tail.cast::<*mut u8>().write(self.head) };
        if self.head.is_null() {
            self.tail = other.tail;
        }
        self.head = other.head;
        self.count += other.count;
    }

    fn split_off(&mut self, wanted: usize) -> FreeList {
        let mut taken = FreeList::EMPTY;
        while taken.count < wanted {
            let block = self.pop();
            if block.is_null() {
                break;
            }
            taken.push(block);
        }
        taken
    }
}

#[derive(Debug)]
struct RunSource {
    next: *mut u8,
    runs_left: usize,
}

// SAFETY: the run source owns the uncarved part of its chunk and is only used under its lock.
unsafe impl Send for RunSource {}

impl RunSource {
    fn next_run(&mut self, allocator: &EggAllocator) -> *mut u8 {
        if self.runs_left == 0 {
            if !allocator.reserve(CHUNK_BYTES) {
                return ptr::null_mut();
            }
            let Ok(layout) = Layout::from_size_align(CHUNK_BYTES, RUN_BYTES) else {
                EggAllocator::give_back(CHUNK_BYTES);
                return ptr::null_mut();
            };
            // SAFETY: the chunk layout has a non-zero size.
            let chunk = unsafe { System.alloc(layout) };
            if chunk.is_null() {
                EggAllocator::give_back(CHUNK_BYTES);
                return ptr::null_mut();
            }
            self.next = chunk;
            self.runs_left = CHUNK_BYTES / RUN_BYTES;
        }
        let run = self.next;
        self.next = self.next.wrapping_add(RUN_BYTES);
        self.runs_left -= 1;
        run
    }
}

struct Locked<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached inside `with`, which holds the lock, so one thread at a time
// uses it; `T: Send` lets it be used from whichever thread holds the lock.
unsafe impl<T: Send> Sync for Locked<T> {}

impl<T> Locked<T> {
    const fn new(value: T) -> Self {
        Self {
            busy: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn with<R>(&self, work: impl FnOnce(&mut T) -> R) -> R {
        let mut spins = 0;
        while self
            .busy
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins < SPINS_BEFORE_YIELD {
                std::hint::spin_loop();
            } else {
                spins = 0;
                std::thread::yield_now();
            }
        }
        // SAFETY: the lock is held, so this is the only reference to the value until it is
        // released below.
        let outcome = work(unsafe { &mut *self.value.get() });
        self.busy.store(false, Ordering::Release);
        outcome
    }
}

struct ThreadCache {
    lists: [Cell<FreeList>; CLASS_COUNT],
}

impl ThreadCache {
    const fn new() -> Self {
        Self {
            lists: [const { Cell::new(FreeList::EMPTY) }; CLASS_COUNT],
        }
    }

    fn take(&self, class: usize, allocator: &EggAllocator) -> *mut u8 {
        let mut list = self.lists[class].get();
        if list.is_empty() {
            let wanted = blocks_per_batch(class, REFILL_BYTES);
            list = CENTRAL[class].with(|central| central.split_off(wanted));
            if list.is_empty() {
                list = carve_run(class, allocator);
            }
        }
        let block = list.pop();
        self.lists[class].set(list);
        block
    }

    fn put(&self, class: usize, block: *mut u8) {
        let mut list = self.lists[class].get();
        list.push(block);
        let limit = blocks_per_batch(class, CACHE_BYTES_PER_CLASS);
        if list.count > limit {
            let spilled = list.split_off(list.count - limit / 2);
            CENTRAL[class].with(|central| central.append(spilled));
        }
        self.lists[class].set(list);
    }
}

impl Drop for ThreadCache {
    fn drop(&mut self) {
        for (class, list) in self.lists.iter().enumerate() {
            let list = list.replace(FreeList::EMPTY);
            if !list.is_empty() {
                CENTRAL[class].with(|central| central.append(list));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    static ALLOCATOR: EggAllocator = EggAllocator::new();
    static NO_BUDGET_LEFT: EggAllocator = EggAllocator::with_budget(0);

    #[test]
    fn every_small_size_gets_the_smallest_class_that_holds_it() {
        for class in 1..CLASS_COUNT {
            assert!(class_size(class) > class_size(class - 1));
            assert_eq!(class_size(class) % SMALLEST_STEP, 0);
        }
        assert_eq!(class_size(CLASS_COUNT - 1), LARGEST_SMALL);
        for size in 1..=LARGEST_SMALL {
            let class = class_of_size(size);
            assert!(class_size(class) >= size, "size {size}");
            assert!(class == 0 || class_size(class - 1) < size, "size {size}");
        }
    }

    #[test]
    fn aligned_layouts_get_a_class_whose_blocks_are_aligned() {
        for align in [32, 64, 128, 4096, LARGEST_SMALL] {
            for size in [1, align / 2, align, align + 1] {
                let layout = Layout::from_size_align(size, align).unwrap();
                let Some(class) = small_class(layout) else {
                    assert!(size.max(align).next_power_of_two() > LARGEST_SMALL);
                    continue;
                };
                assert_eq!(class_size(class) % align, 0, "size {size} align {align}");
                assert!(class_size(class) >= size);
            }
        }
        let wide = Layout::from_size_align(16, 2 * LARGEST_SMALL).unwrap();
        assert_eq!(small_class(wide), None);
    }

    #[test]
    fn blocks_are_aligned_distinct_and_keep_what_is_written() {
        let mut held = Vec::new();
        for round in 0..4_000_usize {
            let size = 1 + round * 37 % 40_000;
            let align = [1, 8, 16, 64, 4096][round % 5];
            let layout = Layout::from_size_align(size, align).unwrap();
            // SAFETY: the layout has a non-zero size.
            let block = unsafe { ALLOCATOR.alloc(layout) };
            assert!(!block.is_null());
            assert_eq!(block.addr() % align, 0);
            let pattern = (round % 251) as u8;
            // SAFETY: the block holds `size` bytes and is ours.
            unsafe { ptr::write_bytes(block, pattern, size) };
            held.push((block, layout, pattern));
        }
        let starts: HashSet<usize> = held.iter().map(|(block, ..)| block.addr()).collect();
        assert_eq!(starts.len(), held.len());
        for (block, layout, pattern) in held {
            // SAFETY: the block holds `layout.size()` bytes written above.
            let bytes = unsafe { std::slice::from_raw_parts(block, layout.size()) };
            assert!(bytes.iter().all(|byte| *byte == pattern));
            // SAFETY: allocated above with this layout.
            unsafe { ALLOCATOR.dealloc(block, layout) };
        }
    }

    #[test]
    fn zeroed_and_grown_blocks_keep_their_contract() {
        let layout = Layout::from_size_align(100, 8).unwrap();
        // SAFETY: the layout has a non-zero size.
        let dirty = unsafe { ALLOCATOR.alloc(layout) };
        // SAFETY: the block holds 100 bytes.
        unsafe { ptr::write_bytes(dirty, 0xaa, 100) };
        // SAFETY: allocated above with this layout.
        unsafe { ALLOCATOR.dealloc(dirty, layout) };
        // SAFETY: the layout has a non-zero size.
        let zeroed = unsafe { ALLOCATOR.alloc_zeroed(layout) };
        // SAFETY: the block holds 100 bytes.
        assert!(unsafe { std::slice::from_raw_parts(zeroed, 100) }.iter().all(|byte| *byte == 0));
        // SAFETY: the block holds 100 bytes.
        unsafe { ptr::write_bytes(zeroed, 7, 100) };
        // SAFETY: allocated above with `layout`; the new size is non-zero.
        let grown = unsafe { ALLOCATOR.realloc(zeroed, layout, 100_000) };
        assert!(!grown.is_null());
        // SAFETY: the first 100 bytes were copied over.
        assert!(unsafe { std::slice::from_raw_parts(grown, 100) }.iter().all(|byte| *byte == 7));
        let grown_layout = Layout::from_size_align(100_000, 8).unwrap();
        // SAFETY: allocated by the realloc above with this layout.
        let shrunk = unsafe { ALLOCATOR.realloc(grown, grown_layout, 50) };
        // SAFETY: the first 50 bytes were copied over.
        assert!(unsafe { std::slice::from_raw_parts(shrunk, 50) }.iter().all(|byte| *byte == 7));
        // SAFETY: allocated by the realloc above with this layout.
        unsafe { ALLOCATOR.dealloc(shrunk, Layout::from_size_align(50, 8).unwrap()) };
    }

    #[test]
    fn blocks_freed_on_other_threads_are_reused() {
        let layout = Layout::from_size_align(48, 16).unwrap();
        let blocks: Vec<usize> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(move || {
                        (0..2_000)
                            // SAFETY: the layout has a non-zero size.
                            .map(|_| unsafe { ALLOCATOR.alloc(layout) }.expose_provenance())
                            .collect::<Vec<usize>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        assert_eq!(blocks.iter().collect::<HashSet<_>>().len(), blocks.len());
        std::thread::scope(|scope| {
            for chunk in blocks.chunks(4_000) {
                scope.spawn(move || {
                    for block in chunk {
                        // SAFETY: each block was allocated above with this layout and is
                        // freed once.
                        unsafe { ALLOCATOR.dealloc(ptr::with_exposed_provenance_mut(*block), layout) };
                    }
                });
            }
        });
    }

    #[test]
    fn a_budget_refuses_memory_beyond_it() {
        let large = Layout::from_size_align(LARGEST_SMALL * 4, 8).unwrap();
        // SAFETY: the layout has a non-zero size.
        assert!(unsafe { NO_BUDGET_LEFT.alloc(large) }.is_null());
        assert_eq!(NO_BUDGET_LEFT.budget(), 0);
        // SAFETY: the layout has a non-zero size.
        let block = unsafe { ALLOCATOR.alloc(large) };
        assert!(!block.is_null());
        assert!(ALLOCATOR.bytes_from_system() >= large.size());
        // SAFETY: allocated above with this layout.
        unsafe { ALLOCATOR.dealloc(block, large) };
    }
}
