use crate::cells;
use crate::error::StorageError;
#[cfg(feature = "std")]
use eggdb_platform::{Memory, MemoryRegion, PageMode};
use core::cell::Cell;
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use crate::prelude::*;

const UNLATCHED: u32 = 0;
const WRITER: u32 = u32::MAX;
const MAX_READERS: u32 = WRITER - 1;

enum Backing {
    #[cfg(feature = "std")]
    Region(MemoryRegion),
    Buffer,
}

impl fmt::Debug for Backing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "std")]
            Backing::Region(region) => fmt::Debug::fmt(region, f),
            Backing::Buffer => f.write_str("a caller's buffer"),
        }
    }
}

pub(crate) struct Frames {
    base: NonNull<u8>,
    frame_bytes: usize,
    latches: Box<[Cell<u32>]>,
    backing: Backing,
}

impl Frames {
    #[cfg(feature = "std")]
    pub(crate) fn lock_in_memory(&mut self) -> eggdb_platform::Result<()> {
        match &mut self.backing {
            Backing::Region(region) => region.lock(),
            Backing::Buffer => Ok(()),
        }
    }

    #[cfg(feature = "std")]
    pub(crate) fn place_on_numa_node(
        &mut self,
        node: eggdb_platform::NumaNodeId,
    ) -> eggdb_platform::Result<()> {
        match &mut self.backing {
            Backing::Region(region) => region.place_on_numa_node(node),
            Backing::Buffer => Err(eggdb_platform::Error::new(eggdb_platform::ErrorKind::Unsupported)),
        }
    }

    /// Frames laid over `buffer`, `count` of `frame_bytes` each, for a cache that takes no
    /// memory from the platform (D1082).
    pub(crate) fn over_buffer(buffer: &'static mut [u8], count: usize, frame_bytes: usize) -> Result<Self, StorageError> {
        if count.checked_mul(frame_bytes).is_none_or(|needed| needed > buffer.len()) {
            return Err(StorageError::InvalidCacheSize { pages: count });
        }
        Ok(Self {
            base: NonNull::from(buffer).cast::<u8>(),
            frame_bytes,
            latches: cells::filled(count, UNLATCHED)?,
            backing: Backing::Buffer,
        })
    }

    #[cfg(feature = "std")]
    pub(crate) fn new(
        memory: &impl Memory,
        count: usize,
        frame_bytes: usize,
    ) -> Result<Self, StorageError> {
        let bytes = count
            .checked_mul(frame_bytes)
            .and_then(|bytes| bytes.checked_next_multiple_of(memory.page_size()))
            .ok_or(StorageError::InvalidCacheSize { pages: count })?;
        let mut region = memory.allocate(bytes, PageMode::Normal)?;
        let base = NonNull::from(&mut region[..]).cast::<u8>();
        Ok(Self {
            base,
            frame_bytes,
            latches: cells::filled(count, UNLATCHED)?,
            backing: Backing::Region(region),
        })
    }

    pub(crate) fn count(&self) -> usize {
        self.latches.len()
    }

    pub(crate) fn is_latched(&self, frame: usize) -> bool {
        self.latches[frame].get() != UNLATCHED
    }

    pub(crate) fn read(&self, frame: usize) -> Option<FrameRef<'_>> {
        let latch = &self.latches[frame];
        let readers = latch.get();
        if readers >= MAX_READERS {
            return None;
        }
        latch.set(readers + 1);
        Some(FrameRef {
            frames: self,
            frame,
        })
    }

    pub(crate) fn write(&self, frame: usize) -> Option<FrameMut<'_>> {
        let latch = &self.latches[frame];
        if latch.get() != UNLATCHED {
            return None;
        }
        latch.set(WRITER);
        Some(FrameMut {
            frames: self,
            frame,
        })
    }

    fn start(&self, frame: usize) -> *mut u8 {
        self.base
            .as_ptr()
            .wrapping_add(frame.wrapping_mul(self.frame_bytes))
    }
}

impl fmt::Debug for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frames")
            .field("count", &self.count())
            .field("frame_bytes", &self.frame_bytes)
            .field("backing", &self.backing)
            .finish_non_exhaustive()
    }
}

// SAFETY: `Frames` owns its region, or holds the only reference to the `'static` buffer it was
// given, and `base` points into it, so moving the frames to another thread moves the memory's
// only owner with them. The `Cell` latches keep `Frames` `!Sync`, so
// two threads never use the same frames at once.
unsafe impl Send for Frames {}

#[derive(Debug)]
pub(crate) struct FrameRef<'a> {
    frames: &'a Frames,
    frame: usize,
}

impl Deref for FrameRef<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.frames.start(self.frame);
        // SAFETY: `start` is the first of the `frame_bytes` bytes of a frame inside the region,
        // which lives as long as `self.frames`. This handle counts as a reader in the frame's
        // latch, and a `FrameMut` is only made for an unlatched frame, so nothing writes the
        // bytes while this shared borrow lives.
        unsafe { core::slice::from_raw_parts(start, self.frames.frame_bytes) }
    }
}

impl Drop for FrameRef<'_> {
    fn drop(&mut self) {
        let latch = &self.frames.latches[self.frame];
        latch.set(latch.get() - 1);
    }
}

#[derive(Debug)]
pub(crate) struct FrameMut<'a> {
    frames: &'a Frames,
    frame: usize,
}

impl Deref for FrameMut<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.frames.start(self.frame);
        // SAFETY: `start` is the first of the `frame_bytes` bytes of a frame inside the region,
        // which lives as long as `self.frames`. The frame's latch marks this handle as its only
        // user, and the shared borrow of the handle rules out a mutable one meanwhile.
        unsafe { core::slice::from_raw_parts(start, self.frames.frame_bytes) }
    }
}

impl DerefMut for FrameMut<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        let start = self.frames.start(self.frame);
        // SAFETY: `start` is the first of the `frame_bytes` bytes of a frame inside the region,
        // which lives as long as `self.frames`, and frames never overlap. The frame's latch
        // marks this handle as its only user, and the exclusive borrow of the handle makes
        // this the only access to the bytes while it lives.
        unsafe { core::slice::from_raw_parts_mut(start, self.frames.frame_bytes) }
    }
}

impl Drop for FrameMut<'_> {
    fn drop(&mut self) {
        self.frames.latches[self.frame].set(UNLATCHED);
    }
}

#[cfg(test)]
mod tests {
    use super::Frames;
    use eggdb_platform::SimMemory;

    fn frames(count: usize) -> Frames {
        Frames::new(&SimMemory::new(), count, 4096).unwrap()
    }

    #[test]
    fn readers_share_a_frame_and_exclude_a_writer() {
        let frames = frames(2);
        let first = frames.read(0).unwrap();
        let second = frames.read(0).unwrap();
        assert!(frames.write(0).is_none());
        assert!(frames.is_latched(0));
        drop((first, second));
        assert!(!frames.is_latched(0));
        assert!(frames.write(0).is_some());
    }

    #[test]
    fn a_writer_excludes_readers_and_other_writers() {
        let frames = frames(2);
        let writer = frames.write(1).unwrap();
        assert!(frames.read(1).is_none());
        assert!(frames.write(1).is_none());
        assert!(frames.write(0).is_some());
        drop(writer);
        assert!(frames.read(1).is_some());
    }

    #[test]
    fn frames_are_whole_pages_that_never_overlap() {
        let frames = frames(3);
        for frame in 0..3 {
            frames.write(frame).unwrap().fill(frame as u8 + 1);
        }
        for frame in 0..3 {
            let bytes = frames.read(frame).unwrap();
            assert_eq!(bytes.len(), 4096);
            assert!(bytes.iter().all(|&byte| byte == frame as u8 + 1));
            assert_eq!(bytes.as_ptr() as usize % 4096, 0);
        }
    }

    #[test]
    fn a_size_that_overflows_is_refused() {
        assert!(Frames::new(&SimMemory::new(), usize::MAX, 4096).is_err());
    }
}
