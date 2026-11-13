//! Timing and reporting shared by every broker comparison.

use std::time::{Duration, Instant};

/// One measured side: messages moved, the time it took and each round trip's latency.
#[derive(Debug, Clone, Default)]
pub(crate) struct Measured {
    pub(crate) messages: u64,
    pub(crate) elapsed: Duration,
    pub(crate) latencies_us: Vec<u64>,
}

impl Measured {
    pub(crate) fn per_second(&self) -> f64 {
        self.messages as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }

    pub(crate) fn percentile(&self, percent: f64) -> u64 {
        if self.latencies_us.is_empty() {
            return 0;
        }
        let mut sorted = self.latencies_us.clone();
        sorted.sort_unstable();
        sorted[(((sorted.len() - 1) as f64) * percent / 100.0).round() as usize]
    }
}

/// Times `body`, which returns how many messages it moved.
pub(crate) fn timed(body: impl FnOnce(&mut Vec<u64>) -> Result<u64, String>) -> Result<Measured, String> {
    let mut latencies_us = Vec::new();
    let started = Instant::now();
    let messages = body(&mut latencies_us)?;
    Ok(Measured { messages, elapsed: started.elapsed(), latencies_us })
}

/// Prints one comparison: both sides' throughput and latencies, and EggDB's throughput as a
/// ratio of the other's.
pub(crate) fn report(name: &str, eggdb: &Measured, other: &Measured, other_name: &str, lines: &mut Vec<String>) {
    let ratio = eggdb.per_second() / other.per_second().max(1e-9);
    let line = format!(
        "{name}: EggDB {:.0} msg/s (p50 {} us, p99 {} us), {other_name} {:.0} msg/s (p50 {} us, p99 {} us), EggDB/{other_name} {ratio:.2}",
        eggdb.per_second(),
        eggdb.percentile(50.0),
        eggdb.percentile(99.0),
        other.per_second(),
        other.percentile(50.0),
        other.percentile(99.0),
    );
    println!("{line}");
    lines.push(line);
}
