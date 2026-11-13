//! Hybrid logical clocks for commit timestamps across shards (D1042): a timestamp is a wall
//! time in milliseconds with a 16-bit counter below it, so it stays close to real time while
//! every event a node sees or sends gets a larger one than the last.

/// A hybrid logical timestamp: milliseconds since the Unix epoch in the high 48 bits and a
/// counter in the low 16, ordered as one number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Timestamp(pub u64);

const LOGICAL_BITS: u32 = 16;

impl Timestamp {
    /// The timestamp at `physical_ms` with counter `logical`.
    pub fn new(physical_ms: u64, logical: u16) -> Self {
        Self((physical_ms << LOGICAL_BITS) | u64::from(logical))
    }

    /// Its wall time in milliseconds.
    pub fn physical_ms(self) -> u64 {
        self.0 >> LOGICAL_BITS
    }

    /// Its counter.
    pub fn logical(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// The next timestamp after it.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// A timestamp from another node too far ahead of this node's wall clock to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSkew {
    /// How far ahead it was, in milliseconds.
    pub ahead_ms: u64,
}

impl std::fmt::Display for ClockSkew {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "another node's clock is {} ms ahead of this one's, more than the cluster allows",
            self.ahead_ms
        )
    }
}

impl std::error::Error for ClockSkew {}

/// One node's hybrid logical clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HybridClock {
    last: Timestamp,
    max_offset_ms: u64,
}

impl HybridClock {
    /// A clock that refuses other nodes' timestamps more than `max_offset_ms` ahead of its
    /// own wall time.
    pub fn new(max_offset_ms: u64) -> Self {
        Self {
            last: Timestamp::default(),
            max_offset_ms,
        }
    }

    /// A timestamp for an event now, after every one this clock gave or saw.
    pub fn now(&mut self, wall_ms: u64) -> Timestamp {
        self.last = self.last.next().max(Timestamp::new(wall_ms, 0));
        self.last
    }

    /// Takes in a timestamp from another node, so this clock's next one is after it; refuses
    /// one too far ahead of `wall_ms`.
    pub fn observe(&mut self, remote: Timestamp, wall_ms: u64) -> Result<Timestamp, ClockSkew> {
        let ahead_ms = remote.physical_ms().saturating_sub(wall_ms);
        if ahead_ms > self.max_offset_ms {
            return Err(ClockSkew { ahead_ms });
        }
        self.last = self
            .last
            .next()
            .max(remote.next())
            .max(Timestamp::new(wall_ms, 0));
        Ok(self.last)
    }

    /// The last timestamp it gave or saw.
    pub fn last(&self) -> Timestamp {
        self.last
    }
}
