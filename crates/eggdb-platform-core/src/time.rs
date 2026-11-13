//! The clocks every crate of EggDB reads (D1090): the standard library's [`Instant`],
//! [`SystemTime`] and [`UNIX_EPOCH`] wherever it has them, and on `wasm32-unknown-unknown`,
//! where the standard library's clocks panic, the same types over two functions the host
//! provides in the import module `eggdb_host`: `monotonic_nanos` and `wall_clock_nanos`, both
//! returning a `u64` of nanoseconds. [`sleep`] waits on the same clock.

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

/// Waits `duration`: the standard library's sleep, or on `wasm32-unknown-unknown` a loop on
/// the host's monotonic clock, since a module there cannot block.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub fn sleep(duration: core::time::Duration) {
    std::thread::sleep(duration);
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use host::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH, sleep};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod host {
    use core::fmt;
    use core::ops::{Add, AddAssign, Sub, SubAssign};
    use core::time::Duration;

    #[link(wasm_import_module = "eggdb_host")]
    unsafe extern "C" {
        fn monotonic_nanos() -> u64;
        fn wall_clock_nanos() -> u64;
    }

    fn nanos(duration: Duration) -> u64 {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }

    /// A moment on the host's monotonic clock.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        /// Now.
        pub fn now() -> Self {
            // SAFETY: the host import takes no arguments and returns a plain integer.
            Self(unsafe { monotonic_nanos() })
        }

        /// The time since `earlier`, or zero when `earlier` is later.
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }

        /// The time since `earlier`, or `None` when `earlier` is later.
        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0).map(Duration::from_nanos)
        }

        /// The time since `earlier`, or zero when `earlier` is later.
        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(earlier.0))
        }

        /// The time since this moment.
        pub fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }

        /// This moment plus `duration`, or `None` past the clock's range.
        pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
            self.0.checked_add(nanos(duration)).map(Instant)
        }

        /// This moment minus `duration`, or `None` before the clock's start.
        pub fn checked_sub(&self, duration: Duration) -> Option<Instant> {
            self.0.checked_sub(nanos(duration)).map(Instant)
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, duration: Duration) -> Instant {
            Instant(self.0.saturating_add(nanos(duration)))
        }
    }

    impl AddAssign<Duration> for Instant {
        fn add_assign(&mut self, duration: Duration) {
            *self = *self + duration;
        }
    }

    impl Sub<Duration> for Instant {
        type Output = Instant;

        fn sub(self, duration: Duration) -> Instant {
            Instant(self.0.saturating_sub(nanos(duration)))
        }
    }

    impl SubAssign<Duration> for Instant {
        fn sub_assign(&mut self, duration: Duration) {
            *self = *self - duration;
        }
    }

    impl Sub<Instant> for Instant {
        type Output = Duration;

        fn sub(self, earlier: Instant) -> Duration {
            self.saturating_duration_since(earlier)
        }
    }

    /// A moment on the host's wall clock, in nanoseconds since 1970.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct SystemTime(u64);

    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: SystemTime = SystemTime(0);

    impl SystemTime {
        /// The start of 1970, as [`UNIX_EPOCH`].
        pub const UNIX_EPOCH: SystemTime = UNIX_EPOCH;

        /// Now.
        pub fn now() -> Self {
            // SAFETY: the host import takes no arguments and returns a plain integer.
            Self(unsafe { wall_clock_nanos() })
        }

        /// The time since `earlier`, or an error holding how much later `earlier` is.
        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            match self.0.checked_sub(earlier.0) {
                Some(difference) => Ok(Duration::from_nanos(difference)),
                None => Err(SystemTimeError(Duration::from_nanos(earlier.0 - self.0))),
            }
        }

        /// The time since this moment, or an error when the clock is behind it.
        pub fn elapsed(&self) -> Result<Duration, SystemTimeError> {
            SystemTime::now().duration_since(*self)
        }

        /// This moment plus `duration`, or `None` past the clock's range.
        pub fn checked_add(&self, duration: Duration) -> Option<SystemTime> {
            self.0.checked_add(nanos(duration)).map(SystemTime)
        }

        /// This moment minus `duration`, or `None` before 1970.
        pub fn checked_sub(&self, duration: Duration) -> Option<SystemTime> {
            self.0.checked_sub(nanos(duration)).map(SystemTime)
        }
    }

    impl Add<Duration> for SystemTime {
        type Output = SystemTime;

        fn add(self, duration: Duration) -> SystemTime {
            SystemTime(self.0.saturating_add(nanos(duration)))
        }
    }

    impl Sub<Duration> for SystemTime {
        type Output = SystemTime;

        fn sub(self, duration: Duration) -> SystemTime {
            SystemTime(self.0.saturating_sub(nanos(duration)))
        }
    }

    /// How much later than a [`SystemTime`] the moment asked about is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SystemTimeError(Duration);

    impl SystemTimeError {
        /// How much later the other moment is.
        pub fn duration(&self) -> Duration {
            self.0
        }
    }

    impl fmt::Display for SystemTimeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "the second time is {:?} later than the first", self.0)
        }
    }

    impl core::error::Error for SystemTimeError {}

    /// Waits `duration` on the host's monotonic clock.
    pub fn sleep(duration: Duration) {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            core::hint::spin_loop();
        }
    }
}
