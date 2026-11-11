//! A merging t-digest (Dunning and Ertl): a sketch of a distribution in bounded memory whose
//! quantiles are most accurate near the tails, which `APPROX_PERCENTILE_CONT` reads.

const COMPRESSION: f64 = 100.0;
const BUFFERED_VALUES: usize = 500;

/// Values summarised as weighted centroids, with the newest values buffered until there are
/// enough to merge; while nothing has been merged, the buffer answers exactly.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct TDigest {
    centroids: Vec<(f64, f64)>,
    buffer: Vec<f64>,
    count: f64,
    min: f64,
    max: f64,
    merged: bool,
    pub(crate) fraction: Option<f64>,
}

fn scale(quantile: f64) -> f64 {
    COMPRESSION / (2.0 * std::f64::consts::PI) * (2.0 * quantile - 1.0).clamp(-1.0, 1.0).asin()
}

fn scale_inverse(k: f64) -> f64 {
    let angle = (k * 2.0 * std::f64::consts::PI / COMPRESSION).clamp(
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    (angle.sin() + 1.0) / 2.0
}

impl TDigest {
    /// The bytes the sketch holds at most, for memory accounting.
    pub(crate) const BYTES: usize = (BUFFERED_VALUES + 2 * COMPRESSION as usize) * 16 + 64;

    pub(crate) fn add(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.count == 0.0 {
            self.min = value;
            self.max = value;
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.count += 1.0;
        self.buffer.push(value);
        if self.buffer.len() >= BUFFERED_VALUES {
            self.compress();
        }
    }

    fn compress(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut all: Vec<(f64, f64)> = std::mem::take(&mut self.centroids);
        all.extend(self.buffer.drain(..).map(|value| (value, 1.0)));
        all.sort_by(|left, right| left.0.total_cmp(&right.0));
        let total: f64 = all.iter().map(|(_, weight)| weight).sum();
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(COMPRESSION as usize * 2);
        let mut before = 0.0;
        let mut limit = total * scale_inverse(scale(0.0) + 1.0);
        let mut current = all[0];
        for &(mean, weight) in &all[1..] {
            if before + current.1 + weight <= limit {
                let combined = current.1 + weight;
                current.0 += (mean - current.0) * weight / combined;
                current.1 = combined;
            } else {
                before += current.1;
                merged.push(current);
                limit = total * scale_inverse(scale(before / total) + 1.0);
                current = (mean, weight);
            }
        }
        merged.push(current);
        self.centroids = merged;
        self.merged = true;
    }

    /// The value below which `quantile` of the values fall, interpolated as `PERCENTILE_CONT`
    /// does while the values are all held, between centroids after that; `None` with no values.
    pub(crate) fn quantile(&mut self, quantile: f64) -> Option<f64> {
        if self.count == 0.0 {
            return None;
        }
        if !self.merged {
            let mut values = self.buffer.clone();
            values.sort_by(f64::total_cmp);
            let place = quantile * (values.len() - 1) as f64;
            let (lower, upper) = (place.floor() as usize, place.ceil() as usize);
            let (low, high) = (values[lower], values[upper]);
            return Some(low + (high - low) * (place - lower as f64));
        }
        self.compress();
        let target = quantile * self.count;
        let mut cumulative = 0.0;
        let mut previous: Option<(f64, f64)> = None;
        for &(mean, weight) in &self.centroids {
            let center = cumulative + weight / 2.0;
            if target <= center {
                return Some(match previous {
                    None => {
                        let fraction = if center > 0.0 { target / center } else { 1.0 };
                        self.min + (mean - self.min) * fraction
                    }
                    Some((previous_mean, previous_center)) => {
                        let span = center - previous_center;
                        let fraction = if span > 0.0 { (target - previous_center) / span } else { 0.0 };
                        previous_mean + (mean - previous_mean) * fraction
                    }
                });
            }
            previous = Some((mean, center));
            cumulative += weight;
        }
        let (last_mean, last_center) = previous?;
        let span = self.count - last_center;
        let fraction = if span > 0.0 { (target - last_center) / span } else { 1.0 };
        Some(last_mean + (self.max - last_mean) * fraction.clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::TDigest;

    #[test]
    fn small_inputs_answer_exactly_and_large_ones_close_to_the_truth() {
        let mut few = TDigest::default();
        for value in [1.0, 2.0, 3.0, 4.0] {
            few.add(value);
        }
        assert_eq!(few.quantile(0.5), Some(2.5));
        let mut many = TDigest::default();
        for value in 0..100_000 {
            many.add(f64::from((value * 7_919) % 100_000));
        }
        for (quantile, truth) in [(0.01, 1_000.0), (0.5, 50_000.0), (0.99, 99_000.0)] {
            let estimate = many.quantile(quantile).unwrap();
            assert!((estimate - truth).abs() < 1_000.0, "{quantile}: {estimate}");
        }
        assert!(many.centroids.len() < 400);
        assert_eq!(TDigest::default().quantile(0.5), None);
    }
}
