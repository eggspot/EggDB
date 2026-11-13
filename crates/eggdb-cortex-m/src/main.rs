//! EggDB's storage engine on a Cortex-M3 (D1084), run by QEMU's `mps2-an385` board with
//! semihosting: a start-up routine and vector table of our own, a fixed heap, flash emulated in
//! RAM under the wear-levelled region, and the engine writing, committing and reading back
//! keys through its page cache, log and transactions, then twenty one-key commits, reporting
//! the bytes programmed into flash per byte of keys and values committed. The program exits
//! with status 0 when every check holds, 1 otherwise.

#![no_std]
#![no_main]

extern crate alloc;

mod semihosting;
mod startup;

use alloc::format;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use eggdb_format::{Flash, WearLevelled};
use eggdb_platform_core::Clock;
use eggdb_storage::{CreateOptions, FlashFile, PageCache, PageFile, Store};

const FLASH_BYTES: usize = 1 << 20;
const SECTOR_BYTES: u32 = 4096;
const FRAME_BYTES: usize = 64 * 1024;
const KEYS: u32 = 300;
const SMALL_COMMITS: u32 = 20;
const SMALL_VALUE_BYTES: usize = 100;

static PROGRAMMED_BYTES: AtomicUsize = AtomicUsize::new(0);
static ERASED_SECTORS: AtomicUsize = AtomicUsize::new(0);
static USER_BYTES: AtomicUsize = AtomicUsize::new(0);

static mut FLASH_MEMORY: [u8; FLASH_BYTES] = [0; FLASH_BYTES];
static mut FRAME_MEMORY: [u8; FRAME_BYTES] = [0; FRAME_BYTES];

/// Flash emulated in a RAM array: erased bytes are `0xFF` and programming only clears bits.
#[derive(Debug)]
struct RamFlash {
    bytes: &'static mut [u8],
}

impl Flash for RamFlash {
    type Error = ();

    fn capacity(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn sector_size(&self) -> u32 {
        SECTOR_BYTES
    }

    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        buffer.copy_from_slice(self.bytes.get(start..start + buffer.len()).ok_or(())?);
        Ok(())
    }

    fn erase(&mut self, offset: u64) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        self.bytes.get_mut(start..start + SECTOR_BYTES as usize).ok_or(())?.fill(0xFF);
        ERASED_SECTORS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn program(&mut self, offset: u64, data: &[u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let target = self.bytes.get_mut(start..start + data.len()).ok_or(())?;
        if target.iter().zip(data).any(|(old, new)| old & new != *new) {
            return Err(());
        }
        target.copy_from_slice(data);
        PROGRAMMED_BYTES.fetch_add(data.len(), Ordering::Relaxed);
        Ok(())
    }
}

/// A clock that only counts: the board has no wall clock, and the engine needs only order.
#[derive(Debug, Default)]
struct CountingClock {
    ticks: core::sync::atomic::AtomicU32,
}

impl Clock for CountingClock {
    fn monotonic(&self) -> Duration {
        Duration::from_micros(u64::from(self.ticks.fetch_add(1, core::sync::atomic::Ordering::Relaxed)))
    }

    fn unix_time_nanos(&self) -> u64 {
        u64::from(self.ticks.fetch_add(1, core::sync::atomic::Ordering::Relaxed)) * 1_000
    }
}

fn run() -> Result<(), alloc::string::String> {
    // SAFETY: `run` is called once, from the reset routine, before anything else touches the
    // flash array, so this is its only reference for the life of the program.
    let flash_memory: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FLASH_MEMORY) };
    // SAFETY: as above, the frame array is taken once and handed to the one page cache.
    let frames: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_MEMORY) };
    flash_memory.fill(0xFF);
    let region = WearLevelled::format(RamFlash { bytes: flash_memory }, 8).map_err(|error| format!("format: {error:?}"))?;
    PROGRAMMED_BYTES.store(0, Ordering::Relaxed);
    ERASED_SECTORS.store(0, Ordering::Relaxed);
    let file = PageFile::create_in(FlashFile::new(region), CreateOptions::default()).map_err(|error| format!("create: {error}"))?;
    let cache = PageCache::with_frame_buffer(file, frames).map_err(|error| format!("cache: {error}"))?;
    let store = Store::new(cache, Arc::new(CountingClock::default()));
    let mut write = store.begin_write().map_err(|error| format!("begin: {error}"))?;
    let tree = write.create_tree().map_err(|error| format!("tree: {error}"))?;
    for key in 0..KEYS {
        let value = format!("value {key}");
        write
            .put(&tree, &key.to_be_bytes(), value.as_bytes())
            .map_err(|error| format!("put {key}: {error}"))?;
        USER_BYTES.fetch_add(key.to_be_bytes().len() + value.len(), Ordering::Relaxed);
    }
    write.set_catalog_root(tree.root());
    write.commit().map_err(|error| format!("commit: {error}"))?;
    let read = store.begin_read().map_err(|error| format!("read: {error}"))?;
    let tree = read.open_tree(read.catalog_root()).map_err(|error| format!("open: {error}"))?;
    for key in 0..KEYS {
        let found = read.get(&tree, &key.to_be_bytes()).map_err(|error| format!("get {key}: {error}"))?;
        if found.as_deref() != Some(format!("value {key}").as_bytes()) {
            return Err(format!("key {key} read back {found:?}"));
        }
    }
    drop(read);
    let small_value = [b's'; SMALL_VALUE_BYTES];
    for commit in 0..SMALL_COMMITS {
        let mut write = store.begin_write().map_err(|error| format!("begin small {commit}: {error}"))?;
        let tree = write.open_tree(write.catalog_root()).map_err(|error| format!("open small {commit}: {error}"))?;
        let key = (commit * 7 % KEYS).to_be_bytes();
        write.put(&tree, &key, &small_value).map_err(|error| format!("put small {commit}: {error}"))?;
        write.set_catalog_root(tree.root());
        write.commit().map_err(|error| format!("commit small {commit}: {error}"))?;
        USER_BYTES.fetch_add(key.len() + SMALL_VALUE_BYTES, Ordering::Relaxed);
    }
    let read = store.begin_read().map_err(|error| format!("read small: {error}"))?;
    let tree = read.open_tree(read.catalog_root()).map_err(|error| format!("open small: {error}"))?;
    let found = read.get(&tree, &0u32.to_be_bytes()).map_err(|error| format!("get small: {error}"))?;
    if found.as_deref() != Some(&small_value[..]) {
        return Err(format!("key 0 read back {found:?} after the small commits"));
    }
    drop(read);
    store.check().map_err(|error| format!("check: {error}"))?;
    Ok(())
}

fn write_amplification_milli() -> usize {
    PROGRAMMED_BYTES.load(Ordering::Relaxed).saturating_mul(1000) / USER_BYTES.load(Ordering::Relaxed).max(1)
}

/// The program, called by the reset routine once memory is set up.
fn main() -> ! {
    match run() {
        Ok(()) => {
            semihosting::write("eggdb-cortex-m: the engine wrote, committed and read back every key\n");
            let statics = startup::static_bytes().saturating_sub(FLASH_BYTES + FRAME_BYTES + startup::HEAP_BYTES);
            semihosting::write(&format!(
                "cortex-m3 engine heap_peak_bytes {}\ncortex-m3 engine frame_bytes {FRAME_BYTES}\ncortex-m3 engine static_bytes {statics}\ncortex-m3 engine ram_floor_bytes {}\n",
                startup::heap_peak(),
                startup::heap_peak() + FRAME_BYTES + statics
            ));
            semihosting::write(&format!(
                "cortex-m3 engine flash_write_amplification_milli {}\ncortex-m3 engine flash_erased_sectors {}\n",
                write_amplification_milli(),
                ERASED_SECTORS.load(Ordering::Relaxed)
            ));
            semihosting::exit(true)
        }
        Err(message) => {
            semihosting::write("eggdb-cortex-m: ");
            semihosting::write(&message);
            semihosting::write("\n");
            semihosting::exit(false)
        }
    }
}
