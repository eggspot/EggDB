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

type Run<'a> = dyn FnMut(u64) -> Result<(), String> + 'a;
type TimedRun<'a> = dyn FnMut(u64) -> Result<Duration, String> + 'a;

fn timed<'a>(run: &'a mut Run<'_>) -> impl FnMut(u64) -> Result<Duration, String> + 'a {
    move |iterations| {
        let start = Instant::now();
        run(iterations)?;
        Ok(start.elapsed())
    }
}

pub(crate) fn paired(subject: &mut Run<'_>, reference: &mut Run<'_>) -> Result<Paired, String> {
    paired_timed(&mut timed(subject), &mut timed(reference))
}

pub(crate) fn paired_timed(
    subject: &mut TimedRun<'_>,
    reference: &mut TimedRun<'_>,
) -> Result<Paired, String> {
    let subject_iterations = calibrate(subject)?;
    let reference_iterations = calibrate(reference)?;
    let mut subject_ns = Vec::with_capacity(SAMPLES);
    let mut reference_ns = Vec::with_capacity(SAMPLES);
    let mut ratios = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let reference_sample = ns_per_op(reference, reference_iterations)?;
        let subject_sample = ns_per_op(subject, subject_iterations)?;
        reference_ns.push(reference_sample);
        subject_ns.push(subject_sample);
        ratios.push(subject_sample / reference_sample);
    }
    Ok(Paired {
        subject_ns: median(&mut subject_ns),
        reference_ns: median(&mut reference_ns),
        ratio: median(&mut ratios),
    })
}

pub(crate) fn single(subject: &mut Run<'_>) -> Result<f64, String> {
    let subject = &mut timed(subject);
    let iterations = calibrate(subject)?;
    let mut samples = (0..SAMPLES)
        .map(|_| ns_per_op(subject, iterations))
        .collect::<Result<Vec<f64>, String>>()?;
    Ok(median(&mut samples))
}

fn calibrate(run: &mut TimedRun<'_>) -> Result<u64, String> {
    let mut iterations = 64;
    loop {
        if run(iterations)? >= SAMPLE_TIME || iterations >= MAX_ITERATIONS {
            return Ok(iterations);
        }
        iterations *= 2;
    }
}

fn ns_per_op(run: &mut TimedRun<'_>, iterations: u64) -> Result<f64, String> {
    Ok(run(iterations)?.as_nanos() as f64 / iterations as f64)
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
