use crate::ast::{AggregateFunction, Expr, ExprKind};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{Value, float_overflow};

pub(crate) const PAIRED_ARGUMENTS: &str = "paired_aggregate_arguments";

pub(crate) fn arguments(dependent: Expr, independent: Expr, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: PAIRED_ARGUMENTS.to_owned(),
            arguments: vec![dependent, independent],
        },
        position,
    }
}

pub(crate) fn no_such_function(
    function: AggregateFunction,
    dependent: &str,
    independent: &str,
) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "function {}({dependent}, {independent}) does not exist",
            function.name()
        ),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
}

#[derive(Clone, Default)]
pub(crate) struct PairSums {
    count: i64,
    sum_x: f64,
    sum_xx: f64,
    sum_y: f64,
    sum_yy: f64,
    sum_xy: f64,
    pairs: Vec<(f64, f64)>,
}

impl PairSums {
    pub(crate) fn held_bytes(&self) -> usize {
        self.pairs.len().saturating_mul(std::mem::size_of::<(f64, f64)>())
    }

    pub(crate) fn add(
        &mut self,
        function: AggregateFunction,
        dependent: &Value,
        independent: &Value,
    ) -> SqlResult<()> {
        if dependent.is_null() || independent.is_null() {
            return Ok(());
        }
        let (Some(y), Some(x)) = (dependent.as_float(), independent.as_float()) else {
            return Err(no_such_function(
                function,
                &dependent.type_name(),
                &independent.type_name(),
            ));
        };
        if matches!(function, AggregateFunction::CorrS | AggregateFunction::CorrK) {
            if x.is_nan() || y.is_nan() {
                return Ok(());
            }
            self.pairs
                .try_reserve(1)
                .map_err(|_| SqlError::new(SqlState::OUT_OF_MEMORY, "out of memory"))?;
            self.pairs.push((x, y));
            return Ok(());
        }
        self.add_pair(y, x)
    }

    fn add_pair(&mut self, y: f64, x: f64) -> SqlResult<()> {
        let previous_count = self.count as f64;
        let previous_sum_x = self.sum_x;
        let previous_sum_y = self.sum_y;
        let count = previous_count + 1.0;
        let sum_x = previous_sum_x + x;
        let sum_y = previous_sum_y + y;
        let (mut sum_xx, mut sum_yy, mut sum_xy) = (self.sum_xx, self.sum_yy, self.sum_xy);
        if self.count > 0 {
            let spread_x = x * count - sum_x;
            let spread_y = y * count - sum_y;
            let scale = 1.0 / (count * previous_count);
            sum_xx += spread_x * spread_x * scale;
            sum_yy += spread_y * spread_y * scale;
            sum_xy += spread_x * spread_y * scale;
            let x_overflowed = (sum_x.is_infinite() || sum_xx.is_infinite())
                && previous_sum_x.is_finite()
                && x.is_finite();
            let y_overflowed = (sum_y.is_infinite() || sum_yy.is_infinite())
                && previous_sum_y.is_finite()
                && y.is_finite();
            let product_overflowed = sum_xy.is_infinite()
                && previous_sum_x.is_finite()
                && x.is_finite()
                && previous_sum_y.is_finite()
                && y.is_finite();
            if x_overflowed || y_overflowed || product_overflowed {
                return Err(float_overflow());
            }
            if sum_xx.is_infinite() {
                sum_xx = f64::NAN;
            }
            if sum_yy.is_infinite() {
                sum_yy = f64::NAN;
            }
            if sum_xy.is_infinite() {
                sum_xy = f64::NAN;
            }
        } else {
            if !x.is_finite() {
                sum_xx = f64::NAN;
                sum_xy = f64::NAN;
            }
            if !y.is_finite() {
                sum_yy = f64::NAN;
                sum_xy = f64::NAN;
            }
        }
        self.count = self.count.saturating_add(1);
        self.sum_x = sum_x;
        self.sum_xx = sum_xx;
        self.sum_y = sum_y;
        self.sum_yy = sum_yy;
        self.sum_xy = sum_xy;
        Ok(())
    }

    pub(crate) fn finish(&self, function: AggregateFunction) -> Value {
        match function {
            AggregateFunction::CorrS => {
                return spearman(&self.pairs).map_or(Value::Null, Value::Double);
            }
            AggregateFunction::CorrK => {
                return kendall_tau_b(&self.pairs).map_or(Value::Null, Value::Double);
            }
            _ => {}
        }
        if function == AggregateFunction::RegrCount {
            return Value::BigInt(self.count);
        }
        if self.count == 0 {
            return Value::Null;
        }
        let count = self.count as f64;
        let result = match function {
            AggregateFunction::RegrSxx => Some(self.sum_xx),
            AggregateFunction::RegrSyy => Some(self.sum_yy),
            AggregateFunction::RegrSxy => Some(self.sum_xy),
            AggregateFunction::RegrAvgx => Some(self.sum_x / count),
            AggregateFunction::RegrAvgy => Some(self.sum_y / count),
            AggregateFunction::CovarPop => Some(self.sum_xy / count),
            AggregateFunction::CovarSamp => (self.count > 1).then(|| self.sum_xy / (count - 1.0)),
            AggregateFunction::Corr => self.correlation(),
            AggregateFunction::RegrR2 => self.determination(),
            AggregateFunction::RegrSlope => (self.sum_xx != 0.0).then(|| self.sum_xy / self.sum_xx),
            AggregateFunction::RegrIntercept => (self.sum_xx != 0.0)
                .then(|| (self.sum_y - self.sum_x * self.sum_xy / self.sum_xx) / count),
            _ => None,
        };
        result.map_or(Value::Null, Value::Double)
    }

    fn correlation(&self) -> Option<f64> {
        if self.sum_xx == 0.0 || self.sum_yy == 0.0 {
            return None;
        }
        let product = self.sum_xx * self.sum_yy;
        let spread = if product.is_finite() && product != 0.0 {
            product.sqrt()
        } else {
            self.sum_xx.sqrt() * self.sum_yy.sqrt()
        };
        Some(self.sum_xy / spread)
    }

    fn determination(&self) -> Option<f64> {
        if self.sum_xx == 0.0 {
            return None;
        }
        if self.sum_yy == 0.0 {
            return Some(1.0);
        }
        Some((self.sum_xy * self.sum_xy) / (self.sum_xx * self.sum_yy))
    }
}

fn average_ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&left, &right| values[left].total_cmp(&values[right]));
    let mut ranks = vec![0.0; values.len()];
    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && values[order[end]] == values[order[start]] {
            end += 1;
        }
        let rank = (start + end + 1) as f64 / 2.0;
        for &at in &order[start..end] {
            ranks[at] = rank;
        }
        start = end;
    }
    ranks
}

fn spearman(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.len() < 2 {
        return None;
    }
    let xs: Vec<f64> = pairs.iter().map(|&(x, _)| x).collect();
    let ys: Vec<f64> = pairs.iter().map(|&(_, y)| y).collect();
    let mut sums = PairSums::default();
    for (y, x) in average_ranks(&ys).into_iter().zip(average_ranks(&xs)) {
        sums.add_pair(y, x).ok()?;
    }
    sums.correlation()
}

fn tied_pairs<T: PartialEq>(sorted: &[T]) -> u64 {
    let mut total = 0u64;
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start + 1;
        while end < sorted.len() && sorted[end] == sorted[start] {
            end += 1;
        }
        let run = (end - start) as u64;
        total = total.saturating_add(run * (run - 1) / 2);
        start = end;
    }
    total
}

fn count_inversions(values: &mut [f64], scratch: &mut Vec<f64>) -> u64 {
    if values.len() < 2 {
        return 0;
    }
    let middle = values.len() / 2;
    let (left, right) = values.split_at_mut(middle);
    let mut inversions = count_inversions(left, scratch) + count_inversions(right, scratch);
    scratch.clear();
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if right[j] < left[i] {
            scratch.push(right[j]);
            inversions += (left.len() - i) as u64;
            j += 1;
        } else {
            scratch.push(left[i]);
            i += 1;
        }
    }
    scratch.extend_from_slice(&left[i..]);
    scratch.extend_from_slice(&right[j..]);
    values.copy_from_slice(scratch);
    inversions
}

fn kendall_tau_b(pairs: &[(f64, f64)]) -> Option<f64> {
    let count = pairs.len() as u64;
    if count < 2 {
        return None;
    }
    let mut sorted = pairs.to_vec();
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.total_cmp(&right.1)));
    let xs: Vec<f64> = sorted.iter().map(|&(x, _)| x).collect();
    let tied_x = tied_pairs(&xs);
    let tied_both = tied_pairs(&sorted);
    let mut ys: Vec<f64> = sorted.iter().map(|&(_, y)| y).collect();
    let mut scratch = Vec::with_capacity(ys.len());
    let discordant = count_inversions(&mut ys, &mut scratch);
    let tied_y = tied_pairs(&ys);
    let total = count * (count - 1) / 2;
    let difference = total as f64 - tied_x as f64 - tied_y as f64 + tied_both as f64
        - 2.0 * discordant as f64;
    let spread = ((total - tied_x) as f64).sqrt() * ((total - tied_y) as f64).sqrt();
    (spread != 0.0).then(|| difference / spread)
}

#[cfg(test)]
mod tests {
    use super::PairSums;
    use crate::ast::AggregateFunction;
    use crate::numeric::Numeric;
    use crate::types::Value;

    fn finished(pairs: &[(Value, Value)], function: AggregateFunction) -> String {
        let mut sums = PairSums::default();
        for (y, x) in pairs {
            if let Err(error) = sums.add(function, y, x) {
                return format!("error: {}", error.message());
            }
        }
        sums.finish(function)
            .to_text()
            .unwrap_or_else(|| "NULL".to_owned())
    }

    fn doubles(pairs: &[(f64, f64)]) -> Vec<(Value, Value)> {
        pairs
            .iter()
            .map(|(y, x)| (Value::Double(*y), Value::Double(*x)))
            .collect()
    }

    #[test]
    fn a_line_has_its_slope_intercept_and_full_correlation() {
        let line = doubles(&[(3.0, 1.0), (5.0, 2.0), (7.0, 3.0), (9.0, 4.0)]);
        assert_eq!(finished(&line, AggregateFunction::RegrSlope), "2");
        assert_eq!(finished(&line, AggregateFunction::RegrIntercept), "1");
        assert_eq!(finished(&line, AggregateFunction::Corr), "1");
        assert_eq!(finished(&line, AggregateFunction::RegrR2), "1");
        assert_eq!(finished(&line, AggregateFunction::RegrCount), "4");
        assert_eq!(finished(&line, AggregateFunction::RegrAvgx), "2.5");
        assert_eq!(finished(&line, AggregateFunction::RegrAvgy), "6");
        assert_eq!(finished(&line, AggregateFunction::RegrSxx), "5");
        assert_eq!(finished(&line, AggregateFunction::RegrSyy), "20");
        assert_eq!(finished(&line, AggregateFunction::RegrSxy), "10");
        assert_eq!(finished(&line, AggregateFunction::CovarPop), "2.5");
        assert_eq!(
            finished(&line, AggregateFunction::CovarSamp),
            "3.3333333333333335"
        );
    }

    #[test]
    fn pairs_with_a_null_are_skipped_and_numbers_of_every_type_count() {
        let pairs = vec![
            (Value::Integer(3), Value::BigInt(1)),
            (Value::Null, Value::Integer(10)),
            (Value::Integer(10), Value::Null),
            (
                Value::Numeric(Numeric::parse("5").unwrap()),
                Value::Real(2.0),
            ),
        ];
        assert_eq!(finished(&pairs, AggregateFunction::RegrCount), "2");
        assert_eq!(finished(&pairs, AggregateFunction::RegrSlope), "2");
    }

    #[test]
    fn too_few_rows_and_no_spread_give_null() {
        assert_eq!(finished(&[], AggregateFunction::RegrCount), "0");
        assert_eq!(finished(&[], AggregateFunction::CovarPop), "NULL");
        let one = doubles(&[(4.0, 2.0)]);
        assert_eq!(finished(&one, AggregateFunction::CovarSamp), "NULL");
        assert_eq!(finished(&one, AggregateFunction::CovarPop), "0");
        assert_eq!(finished(&one, AggregateFunction::RegrSxx), "0");
        assert_eq!(finished(&one, AggregateFunction::RegrSlope), "NULL");
        let vertical = doubles(&[(1.0, 2.0), (5.0, 2.0)]);
        assert_eq!(finished(&vertical, AggregateFunction::RegrSlope), "NULL");
        assert_eq!(finished(&vertical, AggregateFunction::RegrR2), "NULL");
        assert_eq!(finished(&vertical, AggregateFunction::Corr), "NULL");
        let horizontal = doubles(&[(2.0, 1.0), (2.0, 5.0)]);
        assert_eq!(finished(&horizontal, AggregateFunction::RegrSlope), "0");
        assert_eq!(finished(&horizontal, AggregateFunction::RegrR2), "1");
        assert_eq!(finished(&horizontal, AggregateFunction::Corr), "NULL");
    }

    #[test]
    fn values_far_from_zero_lose_no_precision() {
        let offset = 1.0e9;
        let shifted = doubles(&[
            (offset + 3.0, offset + 1.0),
            (offset + 5.0, offset + 2.0),
            (offset + 7.0, offset + 3.0),
            (offset + 9.0, offset + 4.0),
        ]);
        assert_eq!(finished(&shifted, AggregateFunction::RegrSxx), "5");
        assert_eq!(finished(&shifted, AggregateFunction::RegrSxy), "10");
        assert_eq!(finished(&shifted, AggregateFunction::RegrSlope), "2");
    }

    #[test]
    fn infinity_gives_nan_and_overflow_from_finite_values_is_an_error() {
        let infinite = doubles(&[(f64::INFINITY, 1.0)]);
        assert_eq!(finished(&infinite, AggregateFunction::RegrSyy), "NaN");
        assert_eq!(finished(&infinite, AggregateFunction::RegrSxx), "0");
        let huge = doubles(&[(1.0e308, 1.0), (-1.0e308, 2.0)]);
        assert_eq!(
            finished(&huge, AggregateFunction::RegrSyy),
            "error: value out of range: overflow"
        );
    }

    #[test]
    fn a_non_number_names_both_argument_types() {
        let pairs = vec![(Value::Text("a".to_owned()), Value::Integer(1))];
        assert_eq!(
            finished(&pairs, AggregateFunction::Corr),
            "error: function corr(text, integer) does not exist"
        );
    }

    #[test]
    fn rank_correlations_follow_spearman_and_kendall_tau_b() {
        let pairs: Vec<(Value, Value)> = [(1, 10), (2, 20), (3, 25), (4, 40)]
            .iter()
            .map(|&(y, x)| (Value::Integer(y), Value::Integer(x)))
            .collect();
        assert_eq!(finished(&pairs, AggregateFunction::CorrS), "1");
        assert_eq!(finished(&pairs, AggregateFunction::CorrK), "1");
        let reversed: Vec<(Value, Value)> = [(4, 10), (3, 20), (2, 30), (1, 40)]
            .iter()
            .map(|&(y, x)| (Value::Integer(y), Value::Integer(x)))
            .collect();
        assert_eq!(finished(&reversed, AggregateFunction::CorrS), "-1");
        assert_eq!(finished(&reversed, AggregateFunction::CorrK), "-1");
        let tied: Vec<(Value, Value)> = [(1, 1), (2, 1), (3, 2), (3, 3)]
            .iter()
            .map(|&(y, x)| (Value::Integer(y), Value::Integer(x)))
            .collect();
        let tau = finished(&tied, AggregateFunction::CorrK)
            .parse::<f64>()
            .unwrap_or(f64::NAN);
        assert!((tau - 0.8).abs() < 1e-12, "{tau}");
        assert_eq!(finished(&pairs[..1], AggregateFunction::CorrS), "NULL");
    }
}
