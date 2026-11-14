//! Loss detection and congestion control (RFC 9002): RTT estimation, packet and time
//! thresholds for declaring packets lost, the probe timeout, and NewReno's congestion window.

use std::time::{Duration, Instant};

/// RFC 9002's packet reordering threshold.
pub const PACKET_THRESHOLD: u64 = 3;
const TIME_THRESHOLD_NUMERATOR: u32 = 9;
const TIME_THRESHOLD_DENOMINATOR: u32 = 8;
const GRANULARITY: Duration = Duration::from_millis(1);
const INITIAL_RTT: Duration = Duration::from_millis(333);
/// The datagram size the congestion window counts in.
pub const MAX_DATAGRAM_BYTES: usize = 1200;
const INITIAL_WINDOW: usize = 10 * MAX_DATAGRAM_BYTES;
const MINIMUM_WINDOW: usize = 2 * MAX_DATAGRAM_BYTES;

/// The round-trip time estimate of RFC 9002 section 5.
#[derive(Debug, Clone, Copy)]
pub struct RttEstimate {
    /// The latest sample.
    pub latest: Duration,
    /// The smallest sample seen.
    pub minimum: Duration,
    /// The smoothed round-trip time.
    pub smoothed: Duration,
    /// The round-trip time's variation.
    pub variation: Duration,
    has_sample: bool,
}

impl Default for RttEstimate {
    fn default() -> Self {
        Self {
            latest: INITIAL_RTT,
            minimum: INITIAL_RTT,
            smoothed: INITIAL_RTT,
            variation: INITIAL_RTT / 2,
            has_sample: false,
        }
    }
}

impl RttEstimate {
    /// Takes a sample: the time from sending the largest newly acknowledged packet to its
    /// acknowledgement, less the peer's reported ACK delay (capped by `max_ack_delay`) when that
    /// does not take it below the minimum.
    pub fn update(&mut self, sample: Duration, ack_delay: Duration, max_ack_delay: Duration) {
        self.latest = sample;
        if !self.has_sample {
            self.has_sample = true;
            self.minimum = sample;
            self.smoothed = sample;
            self.variation = sample / 2;
            return;
        }
        self.minimum = self.minimum.min(sample);
        let ack_delay = ack_delay.min(max_ack_delay);
        let adjusted = if sample >= self.minimum + ack_delay { sample - ack_delay } else { sample };
        let deviation = if self.smoothed > adjusted { self.smoothed - adjusted } else { adjusted - self.smoothed };
        self.variation = (self.variation * 3 + deviation) / 4;
        self.smoothed = (self.smoothed * 7 + adjusted) / 8;
    }

    /// How long after a packet was sent it is declared lost when later packets were acknowledged.
    pub fn loss_delay(&self) -> Duration {
        (self.smoothed.max(self.latest) * TIME_THRESHOLD_NUMERATOR / TIME_THRESHOLD_DENOMINATOR).max(GRANULARITY)
    }

    /// The probe timeout before backing off: smoothed RTT, four variations and `max_ack_delay`.
    pub fn probe_timeout(&self, max_ack_delay: Duration) -> Duration {
        self.smoothed + (self.variation * 4).max(GRANULARITY) + max_ack_delay
    }
}

/// NewReno congestion control (RFC 9002 section 7).
#[derive(Debug, Clone)]
pub struct CongestionControl {
    window: usize,
    slow_start_threshold: usize,
    in_flight: usize,
    recovery_start: Option<Instant>,
}

impl Default for CongestionControl {
    fn default() -> Self {
        Self {
            window: INITIAL_WINDOW,
            slow_start_threshold: usize::MAX,
            in_flight: 0,
            recovery_start: None,
        }
    }
}

impl CongestionControl {
    /// The congestion window in bytes.
    pub fn window(&self) -> usize {
        self.window
    }

    /// The bytes of ack-eliciting packets sent and not yet acknowledged or lost.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Whether an ack-eliciting packet of `bytes` may be sent now.
    pub fn may_send(&self, bytes: usize) -> bool {
        self.in_flight.saturating_add(bytes) <= self.window
    }

    /// Counts a sent packet that is in flight.
    pub fn on_sent(&mut self, bytes: usize) {
        self.in_flight = self.in_flight.saturating_add(bytes);
    }

    /// Counts an acknowledged packet sent at `sent_at`.
    pub fn on_acknowledged(&mut self, bytes: usize, sent_at: Instant) {
        self.in_flight = self.in_flight.saturating_sub(bytes);
        if self.recovery_start.is_some_and(|start| sent_at <= start) {
            return;
        }
        if self.window < self.slow_start_threshold {
            self.window = self.window.saturating_add(bytes);
        } else {
            self.window = self.window.saturating_add(MAX_DATAGRAM_BYTES * bytes / self.window.max(1));
        }
    }

    /// Counts lost packets, the latest sent at `latest_sent_at`, at `now`: the window halves once
    /// per round trip.
    pub fn on_lost(&mut self, bytes: usize, latest_sent_at: Instant, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(bytes);
        if self.recovery_start.is_some_and(|start| latest_sent_at <= start) {
            return;
        }
        self.recovery_start = Some(now);
        self.window = (self.window / 2).max(MINIMUM_WINDOW);
        self.slow_start_threshold = self.window;
    }

    /// Forgets packets whose keys were discarded, which are neither acknowledged nor lost.
    pub fn on_discarded(&mut self, bytes: usize) {
        self.in_flight = self.in_flight.saturating_sub(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sample_sets_the_estimate_and_later_ones_smooth_it() {
        let mut rtt = RttEstimate::default();
        rtt.update(Duration::from_millis(100), Duration::ZERO, Duration::from_millis(25));
        assert_eq!(rtt.smoothed, Duration::from_millis(100));
        assert_eq!(rtt.variation, Duration::from_millis(50));
        rtt.update(Duration::from_millis(140), Duration::from_millis(20), Duration::from_millis(25));
        assert_eq!(rtt.minimum, Duration::from_millis(100));
        assert_eq!(rtt.smoothed, Duration::from_millis(102) + Duration::from_micros(500));
    }

    #[test]
    fn the_window_grows_in_slow_start_and_halves_once_per_loss_episode() {
        let mut control = CongestionControl::default();
        let start = Instant::now();
        control.on_sent(1200);
        control.on_acknowledged(1200, start);
        assert_eq!(control.window(), INITIAL_WINDOW + 1200);
        control.on_sent(2400);
        let later = start + Duration::from_millis(10);
        control.on_lost(1200, start, later);
        let halved = control.window();
        assert_eq!(halved, (INITIAL_WINDOW + 1200) / 2);
        control.on_lost(1200, start, later + Duration::from_millis(1));
        assert_eq!(control.window(), halved);
        assert_eq!(control.in_flight(), 0);
    }
}
