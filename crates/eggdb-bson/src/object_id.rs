use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

static PROCESS: OnceLock<[u8; 5]> = OnceLock::new();
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A new object id as the specification lays it out: four bytes of seconds since the Unix
/// epoch, five bytes `process_unique` chooses once per process, and a three-byte counter that
/// starts at `counter_start` the first time.
pub fn new_object_id(seconds: u32, process_unique: impl FnOnce() -> [u8; 5], counter_start: u32) -> [u8; 12] {
    let process = PROCESS.get_or_init(|| {
        COUNTER.store(counter_start & 0x00ff_ffff, Ordering::Relaxed);
        process_unique()
    });
    let count = COUNTER.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
    let mut id = [0u8; 12];
    id[..4].copy_from_slice(&seconds.to_be_bytes());
    id[4..9].copy_from_slice(process);
    id[9..].copy_from_slice(&count.to_be_bytes()[1..]);
    id
}
