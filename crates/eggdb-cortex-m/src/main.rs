//! EggDB's storage engine on a Cortex-M3 (D1084), run by QEMU's `mps2-an385` board with
//! semihosting: a start-up routine and vector table of our own, a fixed heap, flash emulated in
//! RAM under the wear-levelled region, and the engine writing, committing and reading back
//! keys through its page cache, log and transactions. The program exits with status 0 when
//! every check holds, 1 otherwise.

#![no_std]
#![no_main]

extern crate alloc;

mod semihosting;
mod startup;

use alloc::format;
use alloc::sync::Arc;
use core::time::Duration;
use eggdb_format::{Flash, WearLevelled};
use eggdb_platform_core::Clock;
use eggdb_storage::{CreateOptions, FlashFile, PageCache, PageFile, Store};

const FLASH_BYTES: usize = 1 << 20;
const SECTOR_BYTES: u32 = 4096;
const FRAME_BYTES: usize = 64 * 1024;
const KEYS: u32 = 300;

static mut FLASH_MEMORY: [u8; FLASH_BYTES] = [0xFF; FLASH_BYTES];
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
        Ok(())
    }

    fn program(&mut self, offset: u64, data: &[u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let target = self.bytes.get_mut(start..start + data.len()).ok_or(())?;
        if target.iter().zip(data).any(|(old, new)| old & new != *new) {
            return Err(());
        }
        target.copy_from_slice(data);
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
    let region = WearLevelled::format(RamFlash { bytes: flash_memory }, 8).map_err(|error| format!("format: {error:?}"))?;
    let file = PageFile::create_in(FlashFile::new(region), CreateOptions::default()).map_err(|error| format!("create: {error}"))?;
    let cache = PageCache::with_frame_buffer(file, frames).map_err(|error| format!("cache: {error}"))?;
    let store = Store::new(cache, Arc::new(CountingClock::default()));
    let mut write = store.begin_write().map_err(|error| format!("begin: {error}"))?;
    let tree = write.create_tree().map_err(|error| format!("tree: {error}"))?;
    for key in 0..KEYS {
        write
            .put(&tree, &key.to_be_bytes(), format!("value {key}").as_bytes())
            .map_err(|error| format!("put {key}: {error}"))?;
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
    store.check().map_err(|error| format!("check: {error}"))?;
    Ok(())
}

/// The program, called by the reset routine once memory is set up.
fn main() -> ! {
    match run() {
        Ok(()) => {
            semihosting::write("eggdb-cortex-m: the engine wrote, committed and read back every key\n");
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
