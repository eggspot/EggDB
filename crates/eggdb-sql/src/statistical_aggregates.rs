use crate::ast::AggregateFunction;
use crate::error::SqlResult;
use crate::numeric::Numeric;
use crate::types::{Value, float_overflow};
use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Measure {
    Variance,
    Deviation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Divisor {
    Sample,
    SampleOrZero,
    Population,
}

fn measure_of(function: AggregateFunction) -> Option<(Measure, Divisor)> {
    Some(match function {
        AggregateFunction::Stddev | AggregateFunction::StddevSamp => {
            (Measure::Deviation, Divisor::Sample)
        }
        AggregateFunction::StddevPop => (Measure::Deviation, Divisor::Population),
        AggregateFunction::OracleStddev => (Measure::Deviation, Divisor::SampleOrZero),
        AggregateFunction::Variance | AggregateFunction::VarSamp => {
            (Measure::Variance, Divisor::Sample)
        }
        AggregateFunction::VarPop => (Measure::Variance, Divisor::Population),
        AggregateFunction::OracleVariance => (Measure::Variance, Divisor::SampleOrZero),
        _ => return None,
    })
}

#[derive(Clone)]
pub(crate) enum Moments {
    Empty,
    Integers {
        count: i64,
        sum: i128,
        squares: i128,
    },
    Exact {
        count: i64,
        sum: Numeric,
        squares: Numeric,
    },
    Float {
        count: i64,
        mean: f64,
        squared_deviations: f64,
    },
}

impl Moments {
    pub(crate) fn add(&mut self, value: &Value) -> SqlResult<()> {
        match value {
            Value::Null => Ok(()),
            Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => match value.as_integer() {
                Some(number) => self.add_integer(number),
                None => Ok(()),
            },
            Value::Numeric(number) => self.add_numeric(number),
            other => match other.as_float() {
                Some(number) => self.add_float(number),
                None => Ok(()),
            },
        }
    }

    fn add_integer(&mut self, number: i64) -> SqlResult<()> {
        let wide = i128::from(number);
        let square = wide * wide;
        match self {
            Self::Empty => {
                *self = Self::Integers {
                    count: 1,
                    sum: wide,
                    squares: square,
                };
                Ok(())
            }
            Self::Integers {
                count,
                sum,
                squares,
            } => match (sum.checked_add(wide), squares.checked_add(square)) {
                (Some(next_sum), Some(next_squares)) => {
                    *count = count.saturating_add(1);
                    *sum = next_sum;
                    *squares = next_squares;
                    Ok(())
                }
                _ => {
                    *self = Self::Exact {
                        count: *count,
                        sum: Numeric::from_i128(*sum),
                        squares: Numeric::from_i128(*squares),
                    };
                    self.add_numeric(&Numeric::from_i64(number))
                }
            },
            Self::Exact { .. } => self.add_numeric(&Numeric::from_i64(number)),
            Self::Float { .. } => self.add_float(number as f64),
        }
    }

    fn add_numeric(&mut self, number: &Numeric) -> SqlResult<()> {
        if let Self::Integers {
            count,
            sum,
            squares,
        } = self
        {
            *self = Self::Exact {
                count: *count,
                sum: Numeric::from_i128(*sum),
                squares: Numeric::from_i128(*squares),
            };
        }
        match self {
            Self::Empty => {
                *self = Self::Exact {
                    count: 1,
                    sum: number.clone(),
                    squares: number.multiply(number)?,
                };
                Ok(())
            }
            Self::Exact {
                count,
                sum,
                squares,
            } => {
                let next_sum = sum.add(number)?;
                let next_squares = squares.add(&number.multiply(number)?)?;
                *count = count.saturating_add(1);
                *sum = next_sum;
                *squares = next_squares;
                Ok(())
            }
            Self::Float { .. } => self.add_float(number.to_f64()),
            Self::Integers { .. } => Ok(()),
        }
    }

    fn add_float(&mut self, number: f64) -> SqlResult<()> {
        let (count, mean, squared_deviations) = match self {
            Self::Empty => (0, 0.0, 0.0),
            Self::Float {
                count,
                mean,
                squared_deviations,
            } => (*count, *mean, *squared_deviations),
            Self::Integers {
                count,
                sum,
                squares,
            } => float_moments(
                *count,
                &Numeric::from_i128(*sum),
                &Numeric::from_i128(*squares),
            )?,
            Self::Exact {
                count,
                sum,
                squares,
            } => float_moments(*count, sum, squares)?,
        };
        let next_count = count.saturating_add(1);
        let delta = number - mean;
        let next_mean = mean + delta / next_count as f64;
        let next_deviations = squared_deviations + delta * (number - next_mean);
        let finite_before =
            number.is_finite() && mean.is_finite() && squared_deviations.is_finite();
        if finite_before && (next_mean.is_infinite() || next_deviations.is_infinite()) {
            return Err(float_overflow());
        }
        *self = Self::Float {
            count: next_count,
            mean: next_mean,
            squared_deviations: next_deviations,
        };
        Ok(())
    }

    pub(crate) fn finish(&self, function: AggregateFunction) -> SqlResult<Value> {
        let Some((measure, divisor)) = measure_of(function) else {
            return Ok(Value::Null);
        };
        match self {
            Self::Empty => Ok(Value::Null),
            Self::Integers {
                count,
                sum,
                squares,
            } => exact_result(
                *count,
                &Numeric::from_i128(*sum),
                &Numeric::from_i128(*squares),
                measure,
                divisor,
            ),
            Self::Exact {
                count,
                sum,
                squares,
            } => exact_result(*count, sum, squares, measure, divisor),
            Self::Float {
                count,
                squared_deviations,
                ..
            } => Ok(float_result(*count, *squared_deviations, measure, divisor)),
        }
    }
}

fn float_moments(count: i64, sum: &Numeric, squares: &Numeric) -> SqlResult<(i64, f64, f64)> {
    if count == 0 {
        return Ok((0, 0.0, 0.0));
    }
    let count_value = Numeric::from_i64(count);
    let mean = sum.divide(&count_value)?.to_f64();
    let numerator = squares
        .multiply(&count_value)?
        .subtract(&sum.multiply(sum)?)?;
    let deviations = numerator.divide(&count_value)?.to_f64().max(0.0);
    Ok((count, mean, deviations))
}

fn exact_result(
    count: i64,
    sum: &Numeric,
    squares: &Numeric,
    measure: Measure,
    divisor: Divisor,
) -> SqlResult<Value> {
    if count == 1 && divisor == Divisor::Sample {
        return Ok(Value::Null);
    }
    if sum.is_special() || squares.is_special() {
        return Ok(Value::Numeric(Numeric::nan()));
    }
    let count_value = Numeric::from_i64(count);
    let numerator = squares
        .multiply(&count_value)?
        .subtract(&sum.multiply(sum)?)?;
    if count == 1 || numerator.compare(&Numeric::from_i64(0)) != Ordering::Greater {
        return Ok(Value::Numeric(Numeric::from_i64(0)));
    }
    let denominator = match divisor {
        Divisor::Population => count_value.multiply(&count_value)?,
        Divisor::Sample | Divisor::SampleOrZero => {
            count_value.multiply(&Numeric::from_i64(count - 1))?
        }
    };
    let variance = numerator.divide(&denominator)?;
    Ok(Value::Numeric(match measure {
        Measure::Variance => variance,
        Measure::Deviation => variance.square_root(usize::from(variance.scale()))?,
    }))
}

fn float_result(count: i64, squared_deviations: f64, measure: Measure, divisor: Divisor) -> Value {
    let variance = match (divisor, count) {
        (Divisor::Sample, 1) => return Value::Null,
        (Divisor::SampleOrZero, 1) => 0.0,
        (Divisor::Population, _) => squared_deviations / count as f64,
        (Divisor::Sample | Divisor::SampleOrZero, _) => squared_deviations / (count - 1) as f64,
    };
    let variance = if variance < 0.0 { 0.0 } else { variance };
    Value::Double(match measure {
        Measure::Variance => variance,
        Measure::Deviation => variance.sqrt(),
    })
}

#[cfg(test)]
mod tests {
    use super::Moments;
    use crate::ast::AggregateFunction;
    use crate::numeric::Numeric;
    use crate::types::Value;

    fn finished(values: &[Value], function: AggregateFunction) -> String {
        let mut moments = Moments::Empty;
        for value in values {
            if let Err(error) = moments.add(value) {
                return format!("error: {}", error.message());
            }
        }
        match moments.finish(function) {
            Ok(value) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Err(error) => format!("error: {}", error.message()),
        }
    }

    fn integers(values: &[i32]) -> Vec<Value> {
        values.iter().map(|value| Value::Integer(*value)).collect()
    }

    fn doubles(values: &[f64]) -> Vec<Value> {
        values.iter().map(|value| Value::Double(*value)).collect()
    }

    #[test]
    fn integer_input_gives_exact_numeric_results() {
        let values = integers(&[1, 2, 3, 4]);
        assert_eq!(
            finished(&values, AggregateFunction::Variance),
            "1.6666666666666667"
        );
        assert_eq!(
            finished(&values, AggregateFunction::Stddev),
            "1.2909944487358056"
        );
        assert_eq!(
            finished(&values, AggregateFunction::VarPop),
            "1.2500000000000000"
        );
        assert_eq!(
            finished(&values, AggregateFunction::StddevPop),
            "1.1180339887498948"
        );
    }

    #[test]
    fn one_row_is_null_for_samples_zero_for_population_and_for_oracle() {
        let values = integers(&[7]);
        assert_eq!(finished(&values, AggregateFunction::VarSamp), "NULL");
        assert_eq!(finished(&values, AggregateFunction::StddevSamp), "NULL");
        assert_eq!(finished(&values, AggregateFunction::VarPop), "0");
        assert_eq!(finished(&values, AggregateFunction::OracleVariance), "0");
        assert_eq!(finished(&values, AggregateFunction::OracleStddev), "0");
        assert_eq!(finished(&[], AggregateFunction::VarPop), "NULL");
    }

    #[test]
    fn identical_values_have_no_spread() {
        let values = integers(&[5, 5, 5]);
        assert_eq!(finished(&values, AggregateFunction::Variance), "0");
        assert_eq!(finished(&values, AggregateFunction::StddevPop), "0");
    }

    #[test]
    fn float_input_uses_a_stable_running_method() {
        let values = doubles(&[1.0, 3.0, 5.0, 7.0]);
        assert_eq!(finished(&values, AggregateFunction::VarPop), "5");
        assert_eq!(
            finished(&values, AggregateFunction::VarSamp),
            "6.666666666666667"
        );
        assert_eq!(
            finished(&doubles(&[3.0, 5.0]), AggregateFunction::StddevSamp),
            "1.4142135623730951"
        );
        let shifted = doubles(&[1.0e9 + 4.0, 1.0e9 + 7.0, 1.0e9 + 13.0, 1.0e9 + 16.0]);
        assert_eq!(finished(&shifted, AggregateFunction::VarSamp), "30");
        assert_eq!(
            finished(&doubles(&[f64::INFINITY]), AggregateFunction::VarPop),
            "NaN"
        );
        assert_eq!(
            finished(&doubles(&[1.0e308, -1.0e308]), AggregateFunction::VarPop),
            "error: value out of range: overflow"
        );
    }

    #[test]
    fn numeric_input_keeps_its_scale_and_wide_integers_switch_to_numeric() {
        let values = vec![
            Value::Numeric(Numeric::parse("1.10").unwrap()),
            Value::Numeric(Numeric::parse("2.20").unwrap()),
        ];
        assert_eq!(
            finished(&values, AggregateFunction::VarSamp),
            "0.60500000000000000000"
        );
        let wide = vec![
            Value::BigInt(i64::MAX),
            Value::BigInt(i64::MAX),
            Value::BigInt(i64::MAX - 2),
        ];
        assert_eq!(
            finished(&wide, AggregateFunction::VarSamp),
            "1.3333333333333333"
        );
    }
}
