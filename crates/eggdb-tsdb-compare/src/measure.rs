//! Timing and reporting shared by every comparison of this crate.

use std::time::{Duration, Instant};

/// One measured side: units of work done, the time it took and each step's latency.
#[derive(Debug, Clone, Default)]
pub(crate) struct Measured {
    pub(crate) units: u64,
    pub(crate) elapsed: Duration,
    pub(crate) latencies_us: Vec<u64>,
}

impl Measured {
    pub(crate) fn per_second(&self) -> f64 {
        self.units as f64 / self.elapsed.as_secs_f64().max(1e-9)
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

/// Runs `step` `count` times, timing each, and counts `units_per_step` for each.
pub(crate) fn timed(count: usize, units_per_step: u64, step: &mut dyn FnMut(usize) -> Result<(), String>) -> Result<Measured, String> {
    let mut latencies_us = Vec::with_capacity(count);
    let started = Instant::now();
    for index in 0..count {
        let step_started = Instant::now();
        step(index)?;
        latencies_us.push(step_started.elapsed().as_micros() as u64);
    }
    Ok(Measured { units: count as u64 * units_per_step, elapsed: started.elapsed(), latencies_us })
}

/// Prints one comparison and keeps its line.
pub(crate) fn report(name: &str, unit: &str, eggdb: &Measured, other: &Measured, other_name: &str, lines: &mut Vec<String>) {
    let ratio = eggdb.per_second() / other.per_second().max(1e-9);
    let line = format!(
        "{name}: EggDB {:.0} {unit}/s (p50 {} us, p99 {} us), {other_name} {:.0} {unit}/s (p50 {} us, p99 {} us), EggDB/{other_name} {ratio:.2}",
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
