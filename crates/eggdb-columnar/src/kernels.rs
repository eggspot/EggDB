//! Vectorized kernels over column values (D1341): comparisons that build a selection mask,
//! and sums, minimums, maximums and counts over the selected values. Each kernel is one plain
//! loop over fixed-width lanes; on x86-64 processors with AVX2 the same loops are compiled for
//! AVX2 as well and chosen at runtime, elsewhere the compiler's baseline vectors (SSE2, NEON)
//! carry them.

/// How a value is compared with a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `=`
    Equal,
    /// `<>`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessOrEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterOrEqual,
}

/// Which values a filter kept: one flag per value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    kept: Vec<bool>,
}

impl Selection {
    /// Every one of `count` values kept.
    pub fn all(count: usize) -> Self {
        Self { kept: vec![true; count] }
    }

    /// How many values it keeps.
    pub fn count(&self) -> usize {
        self.kept.iter().filter(|kept| **kept).count()
    }

    /// The flags, one per value.
    pub fn flags(&self) -> &[bool] {
        &self.kept
    }

    /// Keeps only the values both selections keep.
    pub fn and(&mut self, other: &Selection) {
        for (kept, also) in self.kept.iter_mut().zip(&other.kept) {
            *kept &= *also;
        }
    }
}

macro_rules! comparing {
    ($name:ident, $kind:ty) => {
        fn $name(values: &[$kind], comparison: Comparison, constant: $kind, kept: &mut [bool]) {
            match comparison {
                Comparison::Equal => kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value == constant),
                Comparison::NotEqual => kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value != constant),
                Comparison::Less => kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value < constant),
                Comparison::LessOrEqual => kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value <= constant),
                Comparison::Greater => kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value > constant),
                Comparison::GreaterOrEqual => {
                    kept.iter_mut().zip(values).for_each(|(kept, value)| *kept = *value >= constant)
                }
            }
        }
    };
}

comparing!(compare_integers_portable, i64);
comparing!(compare_floats_portable, f64);

fn sum_integers_portable(values: &[i64], kept: &[bool]) -> i128 {
    let mut lanes = [0i64; 8];
    let mut wide = 0i128;
    for (values, kept) in values.chunks(8).zip(kept.chunks(8)) {
        for ((lane, value), kept) in lanes.iter_mut().zip(values).zip(kept) {
            let (sum, overflowed) = lane.overflowing_add(if *kept { *value } else { 0 });
            if overflowed {
                wide += i128::from(*lane) + i128::from(if *kept { *value } else { 0 });
                *lane = 0;
            } else {
                *lane = sum;
            }
        }
    }
    wide + lanes.iter().map(|lane| i128::from(*lane)).sum::<i128>()
}

fn sum_floats_portable(values: &[f64], kept: &[bool]) -> f64 {
    let mut lanes = [0f64; 8];
    for (values, kept) in values.chunks(8).zip(kept.chunks(8)) {
        for ((lane, value), kept) in lanes.iter_mut().zip(values).zip(kept) {
            *lane += if *kept { *value } else { 0.0 };
        }
    }
    lanes.iter().sum()
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::Comparison;

    #[target_feature(enable = "avx2")]
    pub(super) fn compare_integers(values: &[i64], comparison: Comparison, constant: i64, kept: &mut [bool]) {
        super::compare_integers_portable(values, comparison, constant, kept);
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn compare_floats(values: &[f64], comparison: Comparison, constant: f64, kept: &mut [bool]) {
        super::compare_floats_portable(values, comparison, constant, kept);
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn sum_integers(values: &[i64], kept: &[bool]) -> i128 {
        super::sum_integers_portable(values, kept)
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn sum_floats(values: &[f64], kept: &[bool]) -> f64 {
        super::sum_floats_portable(values, kept)
    }
}

#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

/// The values of `values` that compare with `constant` as `comparison` says.
pub fn filter_integers(values: &[i64], comparison: Comparison, constant: i64) -> Selection {
    let mut kept = vec![false; values.len()];
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the processor supports AVX2, checked just above.
        unsafe { avx2::compare_integers(values, comparison, constant, &mut kept) };
        return Selection { kept };
    }
    compare_integers_portable(values, comparison, constant, &mut kept);
    Selection { kept }
}

/// The values of `values` that compare with `constant` as `comparison` says; NaN compares as
/// SQL's floats do here, unequal to everything.
pub fn filter_floats(values: &[f64], comparison: Comparison, constant: f64) -> Selection {
    let mut kept = vec![false; values.len()];
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the processor supports AVX2, checked just above.
        unsafe { avx2::compare_floats(values, comparison, constant, &mut kept) };
        return Selection { kept };
    }
    compare_floats_portable(values, comparison, constant, &mut kept);
    Selection { kept }
}

/// The sum of the selected values, without overflow.
pub fn sum_integers(values: &[i64], selection: &Selection) -> i128 {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the processor supports AVX2, checked just above.
        return unsafe { avx2::sum_integers(values, &selection.kept) };
    }
    sum_integers_portable(values, &selection.kept)
}

/// The sum of the selected values, added in eight lanes.
pub fn sum_floats(values: &[f64], selection: &Selection) -> f64 {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the processor supports AVX2, checked just above.
        return unsafe { avx2::sum_floats(values, &selection.kept) };
    }
    sum_floats_portable(values, &selection.kept)
}

/// The least and greatest selected values, or `None` when none is selected.
pub fn min_max_integers(values: &[i64], selection: &Selection) -> Option<(i64, i64)> {
    values
        .iter()
        .zip(&selection.kept)
        .filter(|(_, kept)| **kept)
        .fold(None, |found, (&value, _)| match found {
            None => Some((value, value)),
            Some((low, high)) => Some((low.min(value), high.max(value))),
        })
}

impl crate::IntegerChunk {
    /// How many of the chunk's values compare with `constant` as `comparison` says, answered
    /// from the statistics when the whole chunk is in or out, with a vectorized filter otherwise.
    pub fn count_where(&self, comparison: Comparison, constant: i64) -> Result<u64, crate::DecodeError> {
        let Some(stats) = self.stats() else {
            return Ok(0);
        };
        let all = match comparison {
            Comparison::Less => (stats.max < constant, stats.min >= constant),
            Comparison::LessOrEqual => (stats.max <= constant, stats.min > constant),
            Comparison::Greater => (stats.min > constant, stats.max <= constant),
            Comparison::GreaterOrEqual => (stats.min >= constant, stats.max < constant),
            Comparison::Equal => (stats.min == constant && stats.max == constant, constant < stats.min || constant > stats.max),
            Comparison::NotEqual => (constant < stats.min || constant > stats.max, stats.min == constant && stats.max == constant),
        };
        match all {
            (true, _) => Ok(stats.count),
            (_, true) => Ok(0),
            _ => Ok(filter_integers(&self.decode()?, comparison, constant).count() as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_sums_agree_with_plain_loops() {
        let values: Vec<i64> = (0..1_000).map(|index| (index * 7919) % 1_013 - 500).collect();
        for comparison in [
            Comparison::Equal,
            Comparison::NotEqual,
            Comparison::Less,
            Comparison::LessOrEqual,
            Comparison::Greater,
            Comparison::GreaterOrEqual,
        ] {
            let selection = filter_integers(&values, comparison, 17);
            let expected: Vec<i64> = values
                .iter()
                .copied()
                .filter(|value| match comparison {
                    Comparison::Equal => *value == 17,
                    Comparison::NotEqual => *value != 17,
                    Comparison::Less => *value < 17,
                    Comparison::LessOrEqual => *value <= 17,
                    Comparison::Greater => *value > 17,
                    Comparison::GreaterOrEqual => *value >= 17,
                })
                .collect();
            assert_eq!(selection.count(), expected.len());
            assert_eq!(sum_integers(&values, &selection), expected.iter().map(|value| i128::from(*value)).sum::<i128>());
            let chunk = crate::IntegerChunk::encode(&values);
            assert_eq!(chunk.count_where(comparison, 17).unwrap(), expected.len() as u64);
        }
        let huge = vec![i64::MAX; 20];
        assert_eq!(sum_integers(&huge, &Selection::all(20)), i128::from(i64::MAX) * 20);
        let floats: Vec<f64> = (0..100).map(|index| f64::from(index) / 4.0).collect();
        let selection = filter_floats(&floats, Comparison::GreaterOrEqual, 20.0);
        assert_eq!(selection.count(), 20);
        assert_eq!(sum_floats(&floats, &selection), (80..100).map(|index| f64::from(index) / 4.0).sum::<f64>());
        assert_eq!(min_max_integers(&values, &filter_integers(&values, Comparison::Less, -499)), Some((-500, -500)));
    }
}
