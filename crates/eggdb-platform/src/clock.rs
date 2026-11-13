use std::sync::atomic::{AtomicU64, Ordering};
use eggdb_platform_core::time::{Instant, SystemTime, UNIX_EPOCH};
use std::time::Duration;

pub use eggdb_platform_core::Clock;


/// The operating system's clocks.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Creates a clock whose monotonic origin is now.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn unix_time_nanos(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            })
    }
}

/// A clock that only moves when a test moves it.
#[derive(Debug, Default)]
pub struct SimClock {
    monotonic_nanos: AtomicU64,
    unix_nanos: AtomicU64,
}

impl SimClock {
    /// Creates a clock at monotonic time zero with the wall clock at `unix_time_nanos`.
    pub fn new(unix_time_nanos: u64) -> Self {
        Self {
            monotonic_nanos: AtomicU64::new(0),
            unix_nanos: AtomicU64::new(unix_time_nanos),
        }
    }

    /// Moves both clocks forward by `by`, saturating at their largest value.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let add = |current: u64| Some(current.saturating_add(nanos));
        let _ = self
            .monotonic_nanos
            .try_update(Ordering::Relaxed, Ordering::Relaxed, add);
        let _ = self
            .unix_nanos
            .try_update(Ordering::Relaxed, Ordering::Relaxed, add);
    }

    /// Sets the wall clock, forwards or backwards, leaving the monotonic clock alone.
    pub fn set_unix_time_nanos(&self, unix_time_nanos: u64) {
        self.unix_nanos.store(unix_time_nanos, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn monotonic(&self) -> Duration {
        Duration::from_nanos(self.monotonic_nanos.load(Ordering::Relaxed))
    }

    fn unix_time_nanos(&self) -> u64 {
        self.unix_nanos.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, SimClock, SystemClock};
    use std::time::Duration;

    #[test]
    fn the_system_monotonic_clock_never_goes_backwards() {
        let clock = SystemClock::new();
        let mut previous = clock.monotonic();
        for _ in 0..1000 {
            let now = clock.monotonic();
            assert!(now >= previous);
            previous = now;
        }
    }

    #[test]
    fn the_system_wall_clock_is_after_october_2026() {
        let first_of_october_2026 = 1_790_812_800_000_000_000;
        assert!(SystemClock::new().unix_time_nanos() > first_of_october_2026);
    }

    #[test]
    fn the_simulated_clock_moves_only_when_advanced() {
        let clock = SimClock::new(1_000);
        assert_eq!(clock.monotonic(), Duration::ZERO);
        clock.advance(Duration::from_nanos(500));
        assert_eq!(clock.monotonic(), Duration::from_nanos(500));
        assert_eq!(clock.unix_time_nanos(), 1_500);
    }

    #[test]
    fn the_simulated_wall_clock_can_jump_back_without_moving_monotonic_time() {
        let clock = SimClock::new(10_000);
        clock.advance(Duration::from_nanos(5));
        clock.set_unix_time_nanos(1);
        assert_eq!(clock.unix_time_nanos(), 1);
        assert_eq!(clock.monotonic(), Duration::from_nanos(5));
    }

    #[test]
    fn the_simulated_clock_saturates_instead_of_wrapping() {
        let clock = SimClock::new(u64::MAX - 1);
        clock.advance(Duration::MAX);
        assert_eq!(clock.unix_time_nanos(), u64::MAX);
        assert_eq!(clock.monotonic(), Duration::from_nanos(u64::MAX));
    }
}
