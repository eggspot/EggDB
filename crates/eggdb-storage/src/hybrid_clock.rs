use crate::error::StorageError;
use eggdb_platform::Clock;
use std::fmt;
use std::sync::Arc;

const LOGICAL_BITS: u32 = 12;
const LOGICAL_MASK: u64 = (1 << LOGICAL_BITS) - 1;
const LATEST_PHYSICAL_MICROS: u64 = (1 << (64 - LOGICAL_BITS)) - 1;
const EPOCH_UNIX_MICROS: u64 = 1_767_225_600_000_000;

/// When a commit happened, as a hybrid-logical-clock stamp (D20, D77, format.md "Hybrid-clock
/// stamps"): 52 bits of microseconds since 2026-01-01T00:00:00Z and a 12-bit logical counter.
///
/// Stamps order commits: each commit's stamp is above every stamp handed out before it, in this
/// process or, through the `clock_high_water` every commit record carries, before the file was
/// last opened, whatever the wall clock does. [`CommitStamp::NONE`] (0) stands for "no commit
/// stamped yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CommitStamp(u64);

impl CommitStamp {
    /// The stamp of a file no transaction has committed to yet.
    pub const NONE: Self = Self(0);

    /// The stamp whose 64-bit encoding is `bits`, as stored in the file.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The stamp's 64-bit encoding, as stored in the file.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The physical part: microseconds since the Unix epoch.
    pub const fn unix_micros(self) -> u64 {
        (self.0 >> LOGICAL_BITS) + EPOCH_UNIX_MICROS
    }

    /// The logical part: how many stamps were handed out before this one within the same
    /// microsecond of the physical part.
    pub const fn logical(self) -> u16 {
        (self.0 & LOGICAL_MASK) as u16
    }
}

impl fmt::Display for CommitStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:04}", self.unix_micros(), self.logical())
    }
}

pub(crate) struct HybridClock {
    clock: Arc<dyn Clock>,
    last: u64,
}

impl HybridClock {
    pub(crate) fn new(clock: Arc<dyn Clock>, high_water: u64) -> Self {
        Self {
            clock,
            last: high_water,
        }
    }

    pub(crate) fn next(&mut self) -> Result<CommitStamp, StorageError> {
        let after_last = self
            .last
            .checked_add(1)
            .ok_or(StorageError::ClockExhausted)?;
        let stamp = wall_stamp(self.clock.unix_time_nanos()).max(after_last);
        self.last = stamp;
        Ok(CommitStamp(stamp))
    }
}

impl fmt::Debug for HybridClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HybridClock")
            .field("last", &CommitStamp(self.last))
            .finish_non_exhaustive()
    }
}

fn wall_stamp(unix_nanos: u64) -> u64 {
    let micros = (unix_nanos / 1000).saturating_sub(EPOCH_UNIX_MICROS);
    micros.min(LATEST_PHYSICAL_MICROS) << LOGICAL_BITS
}

#[cfg(test)]
mod tests {
    use super::{CommitStamp, EPOCH_UNIX_MICROS, HybridClock};
    use crate::error::StorageError;
    use eggdb_platform::SimClock;
    use std::sync::Arc;
    use std::time::Duration;

    const OCTOBER_2026_NANOS: u64 = 1_792_000_000_000_000_000;

    fn clock_at(unix_nanos: u64) -> (Arc<SimClock>, HybridClock) {
        let sim = Arc::new(SimClock::new(unix_nanos));
        let clock = HybridClock::new(sim.clone(), 0);
        (sim, clock)
    }

    #[test]
    fn a_stamp_carries_the_wall_clock_in_microseconds_since_2026() {
        let (_, mut clock) = clock_at(OCTOBER_2026_NANOS + 999);
        let stamp = clock.next().unwrap();
        assert_eq!(stamp.unix_micros(), OCTOBER_2026_NANOS / 1000);
        assert_eq!(stamp.logical(), 0);
        assert_eq!(
            stamp.bits() >> 12,
            OCTOBER_2026_NANOS / 1000 - EPOCH_UNIX_MICROS
        );
    }

    #[test]
    fn stamps_within_one_microsecond_count_up_and_carry_into_the_physical_part() {
        let (_, mut clock) = clock_at(OCTOBER_2026_NANOS);
        let first = clock.next().unwrap();
        let stamps: Vec<CommitStamp> = (0..5000).map(|_| clock.next().unwrap()).collect();
        assert_eq!(stamps[0].logical(), 1);
        assert_eq!(stamps[4094].logical(), 4095);
        assert_eq!(stamps[4095].logical(), 0);
        assert_eq!(stamps[4095].unix_micros(), first.unix_micros() + 1);
        assert!(stamps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_wall_clock_set_back_never_moves_stamps_back() {
        let (sim, mut clock) = clock_at(OCTOBER_2026_NANOS);
        let before = clock.next().unwrap();
        sim.set_unix_time_nanos(OCTOBER_2026_NANOS - 3_600_000_000_000);
        let after = clock.next().unwrap();
        assert_eq!(after.bits(), before.bits() + 1);
        sim.set_unix_time_nanos(OCTOBER_2026_NANOS + 5_000);
        sim.advance(Duration::from_micros(1));
        let caught_up = clock.next().unwrap();
        assert_eq!(caught_up.unix_micros(), OCTOBER_2026_NANOS / 1000 + 6);
        assert_eq!(caught_up.logical(), 0);
    }

    #[test]
    fn a_wall_clock_before_2026_counts_up_from_the_high_water_mark() {
        let sim = Arc::new(SimClock::new(0));
        let mut clock = HybridClock::new(sim, 41);
        assert_eq!(clock.next().unwrap().bits(), 42);
        assert_eq!(clock.next().unwrap().bits(), 43);
    }

    #[test]
    fn the_high_water_mark_from_the_file_bounds_the_first_stamp() {
        let (_, mut fresh) = clock_at(OCTOBER_2026_NANOS);
        let wall = fresh.next().unwrap();
        let sim = Arc::new(SimClock::new(OCTOBER_2026_NANOS));
        let mut reopened = HybridClock::new(sim, wall.bits() + 100);
        assert_eq!(reopened.next().unwrap().bits(), wall.bits() + 101);
    }

    #[test]
    fn the_last_stamp_is_refused_instead_of_wrapping() {
        let sim = Arc::new(SimClock::new(u64::MAX));
        let mut clock = HybridClock::new(sim, u64::MAX - 1);
        assert_eq!(clock.next().unwrap().bits(), u64::MAX);
        assert!(matches!(clock.next(), Err(StorageError::ClockExhausted)));
        assert!(matches!(clock.next(), Err(StorageError::ClockExhausted)));
    }

    #[test]
    fn a_wall_clock_past_the_physical_range_saturates() {
        let (_, mut clock) = clock_at(u64::MAX);
        let stamp = clock.next().unwrap();
        assert_eq!(stamp.logical(), 0);
        assert_eq!(stamp.bits() >> 12, (1 << 52) - 1);
        assert_eq!(clock.next().unwrap().bits(), stamp.bits() + 1);
    }

    #[test]
    fn stamps_print_as_microseconds_and_counter() {
        assert_eq!(
            CommitStamp::from_bits((5 << 12) | 7).to_string(),
            format!("{}.0007", EPOCH_UNIX_MICROS + 5)
        );
        assert_eq!(CommitStamp::NONE.bits(), 0);
    }
}
