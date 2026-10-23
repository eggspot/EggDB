use std::fmt;
use std::ops::{Deref, DerefMut};

/// The alignment and smallest size of [`CachePadded`]: the target architecture's cache line,
/// doubled where the hardware fetches lines in pairs (x86_64) or has 128-byte lines (Apple's
/// aarch64): 128 bytes on x86_64, aarch64, arm64ec and powerpc64, 256 on s390x, 32 on arm and
/// mips, and 64 elsewhere. A constant, because a struct's layout cannot depend on a value read
/// at run time.
pub const CACHE_LINE_PADDING: usize = std::mem::align_of::<CachePadded<u8>>();

/// A value aligned and padded to [`CACHE_LINE_PADDING`], so that values written by different
/// threads never share a cache line (false sharing).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64"
    ),
    repr(align(128))
)]
#[cfg_attr(target_arch = "s390x", repr(align(256)))]
#[cfg_attr(
    any(
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ),
    repr(align(32))
)]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
        target_arch = "s390x",
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    )),
    repr(align(64))
)]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    /// Pads `value`.
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    /// Returns the value without its padding.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> From<T> for CachePadded<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: fmt::Debug> fmt::Debug for CachePadded<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CachePadded").field(&self.value).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{CACHE_LINE_PADDING, CachePadded};
    use std::mem::{align_of, size_of};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn padded_values_fill_whole_cache_lines() {
        assert!(CACHE_LINE_PADDING.is_power_of_two());
        assert_eq!(align_of::<CachePadded<u8>>(), CACHE_LINE_PADDING);
        assert_eq!(size_of::<CachePadded<u8>>(), CACHE_LINE_PADDING);
        assert_eq!(
            size_of::<CachePadded<[u8; 129]>>(),
            129_usize.next_multiple_of(CACHE_LINE_PADDING)
        );
        let pair = [CachePadded::new(0_u8), CachePadded::new(1_u8)];
        let distance = (&raw const pair[1]).addr() - (&raw const pair[0]).addr();
        assert!(distance >= CACHE_LINE_PADDING);
    }

    #[test]
    fn padded_values_behave_like_the_value() {
        let counter = CachePadded::new(AtomicU64::new(1));
        counter.fetch_add(2, Ordering::Relaxed);
        assert_eq!(counter.into_inner().into_inner(), 3);
        let mut text = CachePadded::from(String::from("a"));
        text.push('b');
        assert_eq!(format!("{text:?}"), "CachePadded(\"ab\")");
    }
}
