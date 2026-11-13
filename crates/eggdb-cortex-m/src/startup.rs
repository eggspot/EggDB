//! The start-up routine: the vector table, the reset handler that copies `.data` and clears
//! `.bss` as `link.x` lays them out, a fixed heap for `alloc`, and the panic handler.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_BYTES: usize = 2 << 20;

unsafe extern "C" {
    static mut __data_start: u32;
    static mut __data_end: u32;
    static __data_load: u32;
    static mut __bss_start: u32;
    static mut __bss_end: u32;
}

#[unsafe(link_section = ".vector_table.reset")]
#[unsafe(no_mangle)]
static RESET_VECTOR: unsafe extern "C" fn() -> ! = reset;

#[unsafe(link_section = ".vector_table.exceptions")]
#[unsafe(no_mangle)]
static EXCEPTIONS: [unsafe extern "C" fn() -> !; 14] = [fault; 14];

unsafe extern "C" fn fault() -> ! {
    crate::semihosting::write("eggdb-cortex-m: a fault or unexpected exception\n");
    crate::semihosting::exit(false)
}

/// The reset handler: lays out memory, then runs the program.
#[unsafe(no_mangle)]
unsafe extern "C" fn reset() -> ! {
    let data_start = core::ptr::addr_of_mut!(__data_start);
    let data_end = core::ptr::addr_of_mut!(__data_end);
    let mut load = core::ptr::addr_of!(__data_load);
    let mut word = data_start;
    while word < data_end {
        // SAFETY: `load` walks `.data`'s image in flash and `word` its place in RAM, both laid
        // out by `link.x` with the same length, word aligned, and nothing else runs yet.
        let value = unsafe { core::ptr::read_volatile(load) };
        // SAFETY: `word` is inside `.data` in RAM, as above.
        unsafe { core::ptr::write_volatile(word, value) };
        word = word.wrapping_add(1);
        load = load.wrapping_add(1);
    }
    let mut word = core::ptr::addr_of_mut!(__bss_start);
    let bss_end = core::ptr::addr_of_mut!(__bss_end);
    while word < bss_end {
        // SAFETY: `word` is inside `.bss`, word aligned, which `link.x` reserves and nothing
        // else uses before this loop clears it.
        unsafe { core::ptr::write_volatile(word, 0) };
        word = word.wrapping_add(1);
    }
    crate::main()
}

struct Heap {
    next: AtomicUsize,
}

static mut HEAP_MEMORY: [u8; HEAP_BYTES] = [0; HEAP_BYTES];

#[global_allocator]
static HEAP: Heap = Heap { next: AtomicUsize::new(0) };

// SAFETY: `alloc` hands out disjoint ranges of the heap array, each aligned as asked and never
// handed out twice, and `dealloc` returns nothing, so no two live allocations overlap.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = core::ptr::addr_of_mut!(HEAP_MEMORY) as *mut u8;
        let mut current = self.next.load(Ordering::Relaxed);
        loop {
            let address = base as usize + current;
            let aligned = address.next_multiple_of(layout.align());
            let end = aligned - base as usize + layout.size();
            if end > HEAP_BYTES {
                return core::ptr::null_mut();
            }
            match self.next.compare_exchange_weak(current, end, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return base.wrapping_add(aligned - base as usize),
                Err(seen) => current = seen,
            }
        }
    }

    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: Layout) {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    crate::semihosting::write("eggdb-cortex-m: the program panicked\n");
    crate::semihosting::exit(false)
}
