use crate::error::{Error, ErrorKind, Result};
use std::alloc::{self, Layout};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// A zero-initialised heap buffer whose address and length are multiples of
/// [`AlignedBuffer::ALIGNMENT`], as uncached file handles require.
pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuffer {
    /// The alignment of every buffer, and the unit its length is measured in.
    pub const ALIGNMENT: usize = 4096;

    /// Allocates a zeroed buffer of `len` bytes. Fails with [`ErrorKind::InvalidInput`] when
    /// `len` is zero or not a multiple of [`AlignedBuffer::ALIGNMENT`], and with
    /// [`ErrorKind::OutOfMemory`] when the allocation fails.
    pub fn zeroed(len: usize) -> Result<Self> {
        if len == 0 || !len.is_multiple_of(Self::ALIGNMENT) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let layout = Layout::from_size_align(len, Self::ALIGNMENT)
            .map_err(|_| Error::new(ErrorKind::InvalidInput))?;
        // SAFETY: `layout` has a non-zero size, checked above.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).ok_or_else(|| Error::new(ErrorKind::OutOfMemory))?;
        Ok(Self { ptr, layout })
    }
}

impl Deref for AlignedBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` points to `layout.size()` initialised bytes owned by `self`, and the
        // shared borrow of `self` keeps them from being mutated or freed.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `layout.size()` initialised bytes owned by `self`, and the
        // exclusive borrow of `self` makes this the only access to them.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` was returned by `alloc_zeroed` with this same `layout` and is freed only
        // here, once.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

// SAFETY: the buffer exclusively owns its allocation, like a `Box<[u8]>`, so moving it to another
// thread is sound.
unsafe impl Send for AlignedBuffer {}

// SAFETY: shared access only hands out `&[u8]`, which is safe to use from several threads.
unsafe impl Sync for AlignedBuffer {}

impl fmt::Debug for AlignedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlignedBuffer")
            .field("len", &self.layout.size())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::AlignedBuffer;
    use crate::ErrorKind;

    #[test]
    fn buffers_are_aligned_zeroed_and_writable() {
        let mut buffer = AlignedBuffer::zeroed(3 * AlignedBuffer::ALIGNMENT).unwrap();
        assert_eq!(buffer.len(), 3 * AlignedBuffer::ALIGNMENT);
        assert!(
            buffer
                .as_ptr()
                .addr()
                .is_multiple_of(AlignedBuffer::ALIGNMENT)
        );
        assert!(buffer.iter().all(|&byte| byte == 0));
        buffer[0] = 7;
        buffer[AlignedBuffer::ALIGNMENT * 3 - 1] = 9;
        assert_eq!((buffer[0], buffer[buffer.len() - 1]), (7, 9));
    }

    #[test]
    fn lengths_that_are_zero_or_unaligned_are_rejected() {
        for len in [0, 1, AlignedBuffer::ALIGNMENT + 512] {
            let error = AlignedBuffer::zeroed(len).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn impossible_sizes_are_errors_not_aborts() {
        let largest_aligned = usize::MAX - (AlignedBuffer::ALIGNMENT - 1);
        let error = AlignedBuffer::zeroed(largest_aligned).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn buffers_move_between_threads() {
        let mut buffer = AlignedBuffer::zeroed(AlignedBuffer::ALIGNMENT).unwrap();
        buffer[1] = 5;
        let read = std::thread::spawn(move || buffer[1]).join().unwrap();
        assert_eq!(read, 5);
    }
}
