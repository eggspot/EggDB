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
}

impl PairSums {
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
        *self = Self {
            count: self.count.saturating_add(1),
            sum_x,
            sum_xx,
            sum_y,
            sum_yy,
            sum_xy,
        };
        Ok(())
    }

    pub(crate) fn finish(&self, function: AggregateFunction) -> Value {
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
}
