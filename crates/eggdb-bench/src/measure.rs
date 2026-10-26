use std::time::{Duration, Instant};

const SAMPLES: usize = 15;
const SAMPLE_TIME: Duration = Duration::from_millis(20);
const MAX_ITERATIONS: u64 = 1 << 34;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Paired {
    pub(crate) subject_ns: f64,
    pub(crate) reference_ns: f64,
    pub(crate) ratio: f64,
}

pub(crate) fn paired(mut subject: impl FnMut(u64), mut reference: impl FnMut(u64)) -> Paired {
    let subject_iterations = calibrate(&mut subject);
    let reference_iterations = calibrate(&mut reference);
    let mut subject_ns = Vec::with_capacity(SAMPLES);
    let mut reference_ns = Vec::with_capacity(SAMPLES);
    let mut ratios = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let reference_sample = ns_per_op(&mut reference, reference_iterations);
        let subject_sample = ns_per_op(&mut subject, subject_iterations);
        reference_ns.push(reference_sample);
        subject_ns.push(subject_sample);
        ratios.push(subject_sample / reference_sample);
    }
    Paired {
        subject_ns: median(&mut subject_ns),
        reference_ns: median(&mut reference_ns),
        ratio: median(&mut ratios),
    }
}

pub(crate) fn single(mut subject: impl FnMut(u64)) -> f64 {
    let iterations = calibrate(&mut subject);
    let mut samples: Vec<f64> = (0..SAMPLES)
        .map(|_| ns_per_op(&mut subject, iterations))
        .collect();
    median(&mut samples)
}

fn calibrate(run: &mut impl FnMut(u64)) -> u64 {
    let mut iterations = 64;
    loop {
        let start = Instant::now();
        run(iterations);
        if start.elapsed() >= SAMPLE_TIME || iterations >= MAX_ITERATIONS {
            return iterations;
        }
        iterations *= 2;
    }
}

fn ns_per_op(run: &mut impl FnMut(u64), iterations: u64) -> f64 {
    let start = Instant::now();
    run(iterations);
    start.elapsed().as_nanos() as f64 / iterations as f64
}

pub(crate) fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => f64::NAN,
        len if len % 2 == 1 => values[len / 2],
        len => (values[len / 2 - 1] + values[len / 2]) / 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn the_median_is_the_middle_value_or_the_mean_of_the_two_middle_values() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        assert!(median(&mut []).is_nan());
    }
}
