use crate::ast::{AggregateFunction, Expr, ExprKind, Literal, UnaryOperator};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Scope;
use crate::numeric::Numeric;
use crate::sqlserver_types::{SqlServerType, value_type_name};
use crate::types::{NumericModifier, SqlType, Value};

const MAX_PRECISION: u16 = 38;
const MIN_QUOTIENT_SCALE: u16 = 6;
const MIN_AVERAGE_SCALE: u16 = 6;
const KEPT_INTEGRAL_DIGITS: u16 = 32;
const DOUBLE_DIGITS: usize = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberFunction {
    Round,
    Ceiling,
    Floor,
}

impl NumberFunction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Round => "round",
            Self::Ceiling => "ceiling",
            Self::Floor => "floor",
        }
    }

    pub(crate) fn result_type(self, argument: Option<SqlType>) -> Option<SqlType> {
        match (self, argument?) {
            (Self::Round, SqlType::SmallInt | SqlType::Integer) => Some(SqlType::Integer),
            (Self::Round, SqlType::Real | SqlType::Double) => Some(SqlType::Double),
            (Self::Ceiling | Self::Floor, SqlType::Numeric(Some(modifier))) => {
                Some(SqlType::Numeric(Some(NumericModifier {
                    precision: modifier.precision,
                    scale: 0,
                })))
            }
            (_, sql_type) if sql_type.is_numeric() => Some(sql_type),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecimalType {
    pub(crate) precision: u16,
    pub(crate) scale: u16,
}

impl DecimalType {
    fn sql_type(self) -> SqlType {
        SqlType::Numeric(Some(NumericModifier {
            precision: self.precision,
            scale: self.scale as i16,
        }))
    }
}

fn argument_type_invalid(value: &Value, argument: usize, function: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "Argument data type {} is invalid for argument {argument} of {function} function.",
            value_type_name(value)
        ),
    )
    .with_sqlserver_code(8116)
}

fn overflow(target: &str) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("Arithmetic overflow error converting expression to data type {target}."),
    )
    .with_sqlserver_code(8115)
}

pub(crate) fn decimal_type_of(sql_type: SqlType) -> Option<DecimalType> {
    let (precision, scale) = match sql_type {
        SqlType::Boolean => (1, 0),
        SqlType::SmallInt => (5, 0),
        SqlType::Integer => (10, 0),
        SqlType::BigInt => (19, 0),
        SqlType::Numeric(Some(modifier)) if modifier.scale >= 0 => (
            modifier.precision.min(MAX_PRECISION),
            (modifier.scale as u16).min(MAX_PRECISION),
        ),
        _ => return None,
    };
    Some(DecimalType { precision, scale })
}

fn digits_type(text: &str) -> DecimalType {
    let unsigned = text.trim().trim_start_matches(['-', '+']);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let integral = whole.trim_start_matches('0').len();
    let scale = fraction.len().min(usize::from(MAX_PRECISION));
    let precision = (integral + scale).clamp(1, usize::from(MAX_PRECISION));
    DecimalType {
        precision: precision as u16,
        scale: scale.min(precision) as u16,
    }
}

pub(crate) fn literal_decimal_type(text: &str) -> DecimalType {
    digits_type(text)
}

pub(crate) fn value_decimal_type(value: &Value) -> Option<DecimalType> {
    match value {
        Value::Numeric(number) if !number.is_nan() => Some(digits_type(&number.to_string())),
        other => decimal_type_of(other.sql_type()?),
    }
}

pub(crate) fn declared_decimal(expr: &Expr, scope: Scope<'_>) -> Option<DecimalType> {
    match &expr.kind {
        ExprKind::Literal(Literal::Decimal(text)) if !text.contains(['e', 'E']) => {
            Some(literal_decimal_type(text))
        }
        ExprKind::Unary(UnaryOperator::Minus | UnaryOperator::Plus, operand) => {
            declared_decimal(operand, scope)
        }
        _ => decimal_type_of(crate::typing::static_type(expr, scope)?),
    }
}

pub(crate) fn quotient_type(dividend: DecimalType, divisor: DecimalType) -> DecimalType {
    let (p1, s1) = (i64::from(dividend.precision), i64::from(dividend.scale));
    let (p2, s2) = (i64::from(divisor.precision), i64::from(divisor.scale));
    let mut scale = (s1 + p2 + 1).max(i64::from(MIN_QUOTIENT_SCALE));
    let mut precision = p1 - s1 + s2 + scale;
    let most = i64::from(MAX_PRECISION);
    if precision > most {
        let integral = precision - scale;
        scale = if integral < i64::from(KEPT_INTEGRAL_DIGITS) {
            scale.min(most - integral)
        } else if scale < i64::from(MIN_QUOTIENT_SCALE) {
            scale
        } else {
            i64::from(MIN_QUOTIENT_SCALE)
        };
        precision = most;
    }
    DecimalType {
        precision: precision.clamp(1, most) as u16,
        scale: scale.clamp(0, most) as u16,
    }
}

pub(crate) fn quotient_sql_type(left: &Expr, right: &Expr, scope: Scope<'_>) -> Option<SqlType> {
    Some(quotient_type(declared_decimal(left, scope)?, declared_decimal(right, scope)?).sql_type())
}

fn fits(number: &Numeric, decimal: DecimalType) -> bool {
    number
        .with_modifier(i64::from(decimal.precision), i64::from(decimal.scale))
        .is_ok()
}

pub(crate) fn divide(
    left: &Value,
    right: &Value,
    dividend: DecimalType,
    divisor: DecimalType,
) -> Option<SqlResult<Value>> {
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    let (a, b) = (left.as_numeric()?, right.as_numeric()?);
    let result = quotient_type(dividend, divisor);
    Some(
        a.divide_truncated(&b, usize::from(result.scale))
            .and_then(|quotient| {
                if fits(&quotient, result) {
                    Ok(Value::Numeric(quotient))
                } else {
                    Err(overflow("numeric"))
                }
            }),
    )
}

pub(crate) fn aggregate_type(function: AggregateFunction, input: Option<SqlType>) -> Option<SqlType> {
    let input = input?;
    match (function, input) {
        (AggregateFunction::Sum | AggregateFunction::Avg, SqlType::SmallInt | SqlType::Integer) => {
            Some(SqlType::Integer)
        }
        (AggregateFunction::Sum | AggregateFunction::Avg, SqlType::BigInt) => Some(SqlType::BigInt),
        (AggregateFunction::Sum, SqlType::Numeric(Some(modifier))) => {
            Some(SqlType::Numeric(Some(NumericModifier {
                precision: MAX_PRECISION,
                scale: modifier.scale,
            })))
        }
        (AggregateFunction::Avg, SqlType::Numeric(Some(modifier))) => {
            Some(SqlType::Numeric(Some(NumericModifier {
                precision: MAX_PRECISION,
                scale: modifier.scale.max(MIN_AVERAGE_SCALE as i16),
            })))
        }
        (AggregateFunction::Sum | AggregateFunction::Avg, SqlType::Numeric(None)) => {
            Some(SqlType::Numeric(None))
        }
        (AggregateFunction::Sum | AggregateFunction::Avg, SqlType::Real | SqlType::Double) => {
            Some(SqlType::Double)
        }
        _ => None,
    }
}

fn whole_number(value: &Value) -> Option<i128> {
    match value {
        Value::Numeric(number) => number
            .truncated(0)
            .ok()
            .and_then(|whole| whole.integral_value()),
        other => other.as_integer().map(i128::from),
    }
}

pub(crate) fn aggregate_value(
    function: AggregateFunction,
    input: Option<SqlType>,
    value: Value,
) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(value);
    }
    match aggregate_type(function, input) {
        Some(SqlType::Integer) => whole_number(&value)
            .and_then(|whole| i32::try_from(whole).ok())
            .map(Value::Integer)
            .ok_or_else(|| overflow("int")),
        Some(SqlType::BigInt) => whole_number(&value)
            .and_then(|whole| i64::try_from(whole).ok())
            .map(Value::BigInt)
            .ok_or_else(|| overflow("bigint")),
        Some(SqlType::Numeric(Some(modifier))) => {
            let Value::Numeric(number) = &value else {
                return Ok(value);
            };
            let scale = i64::from(modifier.scale);
            let kept = if function == AggregateFunction::Avg {
                number.truncated(scale)?
            } else {
                number.clone()
            };
            kept.with_modifier(i64::from(modifier.precision), scale)
                .map(Value::Numeric)
                .map_err(|_| overflow("numeric"))
        }
        Some(SqlType::Double) => Ok(match value {
            Value::Real(number) => Value::Double(f64::from(number)),
            other => other,
        }),
        _ => Ok(value),
    }
}

fn integer_argument(value: &Value, argument: usize, function: &str) -> SqlResult<Option<i64>> {
    Ok(match value {
        Value::Null => None,
        Value::Boolean(_) => return Err(argument_type_invalid(value, argument, function)),
        Value::Numeric(number) => Some(
            number
                .truncated(0)
                .ok()
                .and_then(|whole| whole.integral_value())
                .and_then(|whole| i64::try_from(whole).ok())
                .ok_or_else(|| overflow("int"))?,
        ),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN).trunc();
            if !number.is_finite() || number.abs() > 9e18 {
                return Err(overflow("int"));
            }
            Some(number as i64)
        }
        Value::Text(_) | Value::Bpchar(_) => crate::sqlserver_conversion::convert(
            value.clone(),
            SqlServerType::Int,
            None,
            None,
        )?
        .as_integer(),
        other => match other.as_integer() {
            Some(number) => Some(number),
            None => return Err(argument_type_invalid(other, argument, function)),
        },
    })
}

fn as_float_argument(value: &Value, function: &str) -> SqlResult<f64> {
    match value {
        Value::Text(_) | Value::Bpchar(_) => {
            crate::sqlserver_conversion::convert(value.clone(), SqlServerType::Float, None, None)
                .map(|converted| converted.as_float().unwrap_or(f64::NAN))
        }
        other => other
            .as_float()
            .ok_or_else(|| argument_type_invalid(other, 1, function)),
    }
}

fn rescaled(number: &Numeric, places: i64, truncate: bool) -> SqlResult<Numeric> {
    if truncate {
        number.truncated(places)
    } else {
        number.rounded(places)
    }
}

fn round_integer(value: &Value, places: i64, truncate: bool) -> SqlResult<Value> {
    let number = value.as_integer().unwrap_or(0);
    let shifted = if places >= 0 {
        i128::from(number)
    } else {
        rescaled(&Numeric::from_i64(number), places, truncate)?
            .integral_value()
            .unwrap_or(0)
    };
    match value {
        Value::BigInt(_) => i64::try_from(shifted)
            .map(Value::BigInt)
            .map_err(|_| overflow("bigint")),
        _ => i32::try_from(shifted)
            .map(Value::Integer)
            .map_err(|_| overflow("int")),
    }
}

fn round_decimal(
    number: &Numeric,
    places: i64,
    truncate: bool,
    declared: Option<SqlType>,
) -> SqlResult<Value> {
    let decimal = declared
        .and_then(decimal_type_of)
        .filter(|_| matches!(declared, Some(SqlType::Numeric(Some(_)))))
        .unwrap_or_else(|| digits_type(&number.to_string()));
    let scale = i64::from(number.scale());
    let rounded = if places >= scale {
        number.clone()
    } else {
        rescaled(number, places, truncate)?
    };
    rounded
        .with_modifier(i64::from(decimal.precision), scale)
        .map(Value::Numeric)
        .map_err(|_| overflow("numeric"))
}

fn round_float(number: f64, places: i64, truncate: bool) -> SqlResult<Value> {
    if !number.is_finite() {
        return Ok(Value::Double(number));
    }
    let exact = Numeric::from_f64(number, DOUBLE_DIGITS)?;
    let rounded = rescaled(&exact, places, truncate)?;
    Ok(Value::Double(rounded.to_f64()))
}

fn round(arguments: &[Value], declared: Option<SqlType>) -> SqlResult<Value> {
    let value = arguments.first().unwrap_or(&Value::Null);
    let Some(places) = integer_argument(arguments.get(1).unwrap_or(&Value::Null), 2, "round")?
    else {
        return Ok(Value::Null);
    };
    let truncate = match arguments.get(2) {
        None | Some(Value::Null) => false,
        Some(function) => integer_argument(function, 3, "round")?.is_some_and(|code| code != 0),
    };
    match value {
        Value::Null => Ok(Value::Null),
        Value::Boolean(_) => Err(argument_type_invalid(value, 1, "round")),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            round_integer(value, places, truncate)
        }
        Value::Numeric(number) if number.is_nan() => Ok(value.clone()),
        Value::Numeric(number) => round_decimal(number, places, truncate, declared),
        other => round_float(as_float_argument(other, "round")?, places, truncate),
    }
}

fn to_whole(function: NumberFunction, value: &Value, declared: Option<SqlType>) -> SqlResult<Value> {
    let up = function == NumberFunction::Ceiling;
    match value {
        Value::Null => Ok(Value::Null),
        Value::Boolean(_) => Err(argument_type_invalid(value, 1, function.name())),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => Ok(value.clone()),
        Value::Numeric(number) => {
            let whole = if up { number.ceiling()? } else { number.floor()? };
            match declared {
                Some(SqlType::Numeric(Some(modifier))) => whole
                    .with_modifier(i64::from(modifier.precision), 0)
                    .map(Value::Numeric)
                    .map_err(|_| overflow("numeric")),
                _ => Ok(Value::Numeric(whole)),
            }
        }
        Value::Real(number) => Ok(Value::Real(if up { number.ceil() } else { number.floor() })),
        other => {
            let number = as_float_argument(other, function.name())?;
            Ok(Value::Double(if up { number.ceil() } else { number.floor() }))
        }
    }
}

pub(crate) fn call(
    function: NumberFunction,
    arguments: &[Value],
    declared: Option<SqlType>,
) -> SqlResult<Value> {
    match function {
        NumberFunction::Round => round(arguments, declared),
        NumberFunction::Ceiling | NumberFunction::Floor => {
            to_whole(function, arguments.first().unwrap_or(&Value::Null), declared)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DecimalType, NumberFunction, aggregate_value, call, divide, literal_decimal_type,
        quotient_type,
    };
    use crate::ast::AggregateFunction;
    use crate::numeric::Numeric;
    use crate::types::{NumericModifier, SqlType, Value};

    fn number(text: &str) -> Value {
        Value::Numeric(Numeric::parse(text).unwrap_or_else(|_| Numeric::nan()))
    }

    fn shown(result: crate::error::SqlResult<Value>) -> String {
        match result {
            Ok(value) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Err(error) => format!("error {}", error.vendor_code().unwrap_or(0)),
        }
    }

    fn decimal(precision: u16, scale: u16) -> DecimalType {
        DecimalType { precision, scale }
    }

    #[test]
    fn literals_are_decimals_of_their_own_digits() {
        assert_eq!(literal_decimal_type("123.45"), decimal(5, 2));
        assert_eq!(literal_decimal_type("0.5"), decimal(1, 1));
        assert_eq!(literal_decimal_type("10.0"), decimal(3, 1));
        assert_eq!(literal_decimal_type("7"), decimal(1, 0));
    }

    #[test]
    fn division_follows_the_precision_and_scale_table() {
        assert_eq!(quotient_type(decimal(3, 1), decimal(10, 0)), decimal(14, 12));
        assert_eq!(quotient_type(decimal(10, 0), decimal(2, 1)), decimal(17, 6));
        assert_eq!(quotient_type(decimal(38, 10), decimal(38, 10)), decimal(38, 6));
        assert_eq!(quotient_type(decimal(18, 2), decimal(18, 2)), decimal(38, 20));
        let quotient = divide(
            &Value::Integer(2),
            &number("3.0"),
            decimal(10, 0),
            decimal(2, 1),
        );
        assert_eq!(quotient.map(shown).as_deref(), Some("0.666666"));
        let by_zero = divide(&number("1.0"), &Value::Integer(0), decimal(2, 1), decimal(10, 0));
        assert!(matches!(by_zero, Some(Err(_))));
    }

    #[test]
    fn round_keeps_the_type_and_its_scale() {
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[number("123.9994"), Value::Integer(3)],
                None
            )),
            "123.9990"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[number("123.9995"), Value::Integer(3)],
                None
            )),
            "124.0000"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[number("150.75"), Value::Integer(0), Value::Integer(1)],
                None
            )),
            "150.00"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[number("748.58"), Value::Integer(-3)],
                None
            )),
            "error 8115"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[Value::Integer(748), Value::Integer(-2)],
                None
            )),
            "700"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[Value::Integer(i32::MAX), Value::Integer(-1)],
                None
            )),
            "error 8115"
        );
        assert_eq!(
            shown(call(
                NumberFunction::Round,
                &[number("1.5"), Value::Integer(i32::MIN)],
                None
            )),
            "0.0"
        );
    }

    #[test]
    fn ceiling_and_floor_keep_integers_and_drop_decimal_places() {
        assert_eq!(
            shown(call(NumberFunction::Ceiling, &[number("123.45")], None)),
            "124"
        );
        assert_eq!(
            shown(call(NumberFunction::Floor, &[number("-123.45")], None)),
            "-124"
        );
        assert_eq!(
            shown(call(NumberFunction::Floor, &[Value::Integer(7)], None)),
            "7"
        );
        assert_eq!(
            shown(call(NumberFunction::Ceiling, &[Value::Boolean(true)], None)),
            "error 8116"
        );
    }

    #[test]
    fn sum_and_average_take_sql_servers_result_types() {
        let int_sum = aggregate_value(
            AggregateFunction::Sum,
            Some(SqlType::Integer),
            Value::BigInt(3_000_000_000),
        );
        assert_eq!(shown(int_sum), "error 8115");
        let int_average =
            aggregate_value(AggregateFunction::Avg, Some(SqlType::Integer), number("1.5"));
        assert_eq!(int_average.ok(), Some(Value::Integer(1)));
        let decimal_input = Some(SqlType::Numeric(Some(NumericModifier {
            precision: 10,
            scale: 2,
        })));
        let average = aggregate_value(
            AggregateFunction::Avg,
            decimal_input,
            number("1.66666666666666666667"),
        );
        assert_eq!(shown(average), "1.666666");
        let sum = aggregate_value(AggregateFunction::Sum, decimal_input, number("12.50"));
        assert_eq!(shown(sum), "12.50");
    }
}
