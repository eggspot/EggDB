//! PostgreSQL's mathematical functions (D1289): logarithms and powers, the trigonometric
//! functions in radians and in degrees, the hyperbolic ones, `erf`, `gamma`, integer
//! arithmetic (`mod`, `div`, `gcd`, `lcm`), `width_bucket` and `random`, with PostgreSQL's
//! errors. A `numeric` argument gives a `numeric` result, computed in double precision; other
//! numbers give `double precision`. One-argument `log` is the natural logarithm in MySQL's and
//! SQL Server's dialects, and SQL Server's `LOG(x, base)` takes the base second.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::types::{SqlType, Value};

/// The functions this module answers.
pub(crate) const FUNCTIONS: [&str; 25] = [
    "setseed", "factorial", "exp", "ln", "log", "log10", "log2", "power", "pow", "cbrt", "degrees", "radians", "pi", "sign", "mod", "div", "gcd", "lcm",
    "width_bucket", "atan2", "atan2d", "random", "erf", "erfc", "gamma",
];

const TRIGONOMETRIC: [&str; 21] = [
    "sin", "cos", "tan", "cot", "asin", "acos", "atan", "sind", "cosd", "tand", "cotd", "asind", "acosd", "atand", "sinh",
    "cosh", "tanh", "asinh", "acosh", "atanh", "lgamma",
];

fn known(name: &str) -> bool {
    FUNCTIONS.contains(&name) || TRIGONOMETRIC.contains(&name)
}

const NUMERIC_DIGITS: usize = 17;

fn out_of_range(what: &str) -> SqlError {
    SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, what.to_owned())
}

fn logarithm_error(message: &str) -> SqlError {
    SqlError::new(SqlState::INVALID_ARGUMENT_FOR_LOGARITHM, message.to_owned())
}

fn shaped(input: &Value, result: f64) -> SqlResult<Value> {
    if result.is_nan() && !input.as_float().is_some_and(f64::is_nan) {
        return Err(out_of_range("input is out of range"));
    }
    match input {
        Value::Numeric(_) => Numeric::from_f64(result, NUMERIC_DIGITS).map(Value::Numeric),
        _ => Ok(Value::Double(result)),
    }
}

fn logarithm(value: f64, base: Option<f64>) -> SqlResult<f64> {
    if value == 0.0 {
        return Err(logarithm_error("cannot take logarithm of zero"));
    }
    if value < 0.0 {
        return Err(logarithm_error("cannot take logarithm of a negative number"));
    }
    match base {
        None => Ok(value.ln()),
        Some(base) if base <= 0.0 => Err(logarithm_error(if base == 0.0 {
            "cannot take logarithm of zero"
        } else {
            "cannot take logarithm of a negative number"
        })),
        Some(base) if base == 1.0 => Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero")),
        Some(base) if base == 10.0 => Ok(value.log10()),
        Some(base) if base == 2.0 => Ok(value.log2()),
        Some(base) => Ok(value.ln() / base.ln()),
    }
}

fn power(base: f64, exponent: f64) -> SqlResult<f64> {
    if base == 0.0 && exponent < 0.0 {
        return Err(SqlError::new(SqlState::INVALID_ARGUMENT_FOR_POWER, "zero raised to a negative power is undefined"));
    }
    if base < 0.0 && exponent.fract() != 0.0 {
        return Err(SqlError::new(
            SqlState::INVALID_ARGUMENT_FOR_POWER,
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    let result = base.powf(exponent);
    if result.is_infinite() && base.is_finite() && exponent.is_finite() {
        return Err(out_of_range("value out of range: overflow"));
    }
    Ok(result)
}

fn within_one(value: f64) -> SqlResult<f64> {
    if (-1.0..=1.0).contains(&value) || value.is_nan() {
        Ok(value)
    } else {
        Err(out_of_range("input is out of range"))
    }
}

fn exact_degrees(radians: f64) -> f64 {
    let degrees = radians.to_degrees();
    let rounded = degrees.round();
    if (degrees - rounded).abs() < 1e-9 { rounded } else { degrees }
}

fn exact_unit(value: f64) -> f64 {
    [-1.0, -0.5, 0.0, 0.5, 1.0]
        .into_iter()
        .find(|exact: &f64| (value - exact).abs() < 1e-15)
        .unwrap_or(value)
}

fn trigonometric(name: &str, x: f64) -> SqlResult<f64> {
    let in_degrees = |degrees: f64| degrees.to_radians();
    Ok(match name {
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "cot" => 1.0 / x.tan(),
        "asin" => within_one(x)?.asin(),
        "acos" => within_one(x)?.acos(),
        "atan" => x.atan(),
        "sind" => exact_unit(in_degrees(x % 360.0).sin()),
        "cosd" => exact_unit(in_degrees(x % 360.0).cos()),
        "tand" => {
            let reduced = x % 180.0;
            if reduced.abs() == 90.0 {
                f64::INFINITY.copysign(reduced)
            } else {
                exact_unit(in_degrees(reduced).tan())
            }
        }
        "cotd" => {
            let reduced = x % 180.0;
            if reduced == 0.0 { f64::INFINITY } else { exact_unit(1.0 / in_degrees(reduced).tan()) }
        }
        "asind" => exact_degrees(within_one(x)?.asin()),
        "acosd" => exact_degrees(within_one(x)?.acos()),
        "atand" => exact_degrees(x.atan()),
        "sinh" => x.sinh(),
        "cosh" => x.cosh(),
        "tanh" => x.tanh(),
        "asinh" => x.asinh(),
        "acosh" if x < 1.0 => return Err(out_of_range("input is out of range")),
        "acosh" => x.acosh(),
        "atanh" if !(-1.0..=1.0).contains(&x) => return Err(out_of_range("input is out of range")),
        "atanh" => x.atanh(),
        _ => f64::NAN,
    })
}

fn erf_series(x: f64) -> f64 {
    let mut term = x;
    let mut sum = x;
    let square = x * x;
    for n in 1..200 {
        let n = f64::from(n);
        term *= -square / n;
        let added = term / (2.0 * n + 1.0);
        sum += added;
        if added.abs() < 1e-17 * sum.abs() {
            break;
        }
    }
    sum * 2.0 / std::f64::consts::PI.sqrt()
}

fn erfc_continued(x: f64) -> f64 {
    let tiny = 1e-300;
    let mut f = x;
    let mut c = x;
    let mut d = 0.0;
    for n in 1..300 {
        let a = f64::from(n) / 2.0;
        d = x + a * d;
        d = if d.abs() < tiny { tiny } else { d };
        c = x + a / c;
        c = if c.abs() < tiny { tiny } else { c };
        d = 1.0 / d;
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() < 1e-16 {
            break;
        }
    }
    (-x * x).exp() / (f * std::f64::consts::PI.sqrt())
}

/// The error function, by its Taylor series near zero and the continued fraction of `erfc`
/// beyond 2.5.
pub(crate) fn erf(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x.abs() < 2.5 {
        erf_series(x)
    } else {
        (1.0 - erfc_continued(x.abs())).copysign(x)
    }
}

/// The complementary error function, `1 - erf(x)`, without losing precision for large `x`.
pub(crate) fn erfc(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x < 2.5 { 1.0 - erf(x) } else { erfc_continued(x) }
}

const LANCZOS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    9.984_369_578_019_572e-6,
    1.505_632_735_149_311_6e-7,
];

/// The gamma function by Lanczos's approximation, reflected below one half.
pub(crate) fn gamma(x: f64) -> f64 {
    if x < 0.5 {
        return std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x));
    }
    let x = x - 1.0;
    let mut sum = LANCZOS[0];
    for (at, coefficient) in LANCZOS.iter().enumerate().skip(1) {
        sum += coefficient / (x + at as f64);
    }
    let t = x + 7.5;
    (2.0 * std::f64::consts::PI).sqrt() * t.powf(x + 0.5) * (-t).exp() * sum
}

fn log_gamma(x: f64) -> f64 {
    if x < 0.5 {
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin().abs()).ln() - log_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut sum = LANCZOS[0];
    for (at, coefficient) in LANCZOS.iter().enumerate().skip(1) {
        sum += coefficient / (x + at as f64);
    }
    let t = x + 7.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

fn integer_pair(left: &Value, right: &Value) -> Option<(i64, i64)> {
    Some((left.as_integer()?, right.as_integer()?))
}

fn same_integer_type(left: &Value, right: &Value, result: i64) -> SqlResult<Value> {
    match (left, right) {
        (Value::SmallInt(_), Value::SmallInt(_)) => i16::try_from(result).map(Value::SmallInt).map_err(|_| out_of_range("smallint out of range")),
        (Value::BigInt(_), _) | (_, Value::BigInt(_)) => Ok(Value::BigInt(result)),
        _ => i32::try_from(result).map(Value::Integer).map_err(|_| out_of_range("integer out of range")),
    }
}

fn greatest_common(mut a: i64, mut b: i64) -> Option<i64> {
    while b != 0 {
        (a, b) = (b, a.checked_rem(b)?);
    }
    a.checked_abs()
}

fn natural_log_dialect() -> bool {
    matches!(crate::constraints::current_dialect(), crate::Dialect::MySql | crate::Dialect::SqlServer)
}

std::thread_local! {
    static SEEDED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn split_mix(state: u64) -> (u64, u64) {
    let next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = next;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (next, mixed ^ (mixed >> 31))
}

fn random_double() -> SqlResult<f64> {
    let word = match SEEDED.with(std::cell::Cell::get) {
        Some(state) => {
            let (next, word) = split_mix(state);
            SEEDED.with(|seeded| seeded.set(Some(next)));
            word
        }
        None => {
            let mut bytes = [0u8; 8];
            crate::random::fill(&mut bytes)?;
            u64::from_be_bytes(bytes)
        }
    };
    Ok((word >> 11) as f64 / (1u64 << 53) as f64)
}

fn factorial(n: i64) -> SqlResult<Value> {
    if n < 0 {
        return Err(out_of_range("factorial of a negative number is undefined"));
    }
    if n > 1_000 {
        return Err(out_of_range("value overflows numeric format"));
    }
    let mut product = Numeric::from_i64(1);
    for factor in 2..=n {
        product = product.multiply(&Numeric::from_i64(factor))?;
    }
    Ok(Value::Numeric(product))
}

/// Runs one of [`FUNCTIONS`]; `None` for other names or arguments these functions do not take.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !known(name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let number = |at: usize| values.get(at).and_then(Value::as_float);
    Some(match (name, values) {
        ("pi", []) => Ok(Value::Double(std::f64::consts::PI)),
        ("random", []) => random_double().map(Value::Double),
        ("setseed", [seed]) => {
            let seed = number(0)?;
            if !(-1.0..=1.0).contains(&seed) {
                return Some(Err(out_of_range(&format!("setseed parameter {seed} is out of allowed range [-1,1]"))));
            }
            SEEDED.with(|seeded| seeded.set(Some(seed.to_bits())));
            Ok(Value::Null)
        }
        ("factorial", [value]) => factorial(value.as_integer()?),
        ("exp", [value]) => shaped(value, number(0)?.exp()),
        ("ln", [value]) => logarithm(number(0)?, None).and_then(|result| shaped(value, result)),
        ("log", [value]) if natural_log_dialect() => logarithm(number(0)?, None).and_then(|result| shaped(value, result)),
        ("log" | "log10", [value]) => logarithm(number(0)?, Some(10.0)).and_then(|result| shaped(value, result)),
        ("log2", [value]) => logarithm(number(0)?, Some(2.0)).and_then(|result| shaped(value, result)),
        ("log", [first_value, second_value]) => {
            let (base, value, base_at, value_at) = match crate::constraints::current_dialect() {
                crate::Dialect::SqlServer => (second_value, first_value, 1, 0),
                _ => (first_value, second_value, 0, 1),
            };
            let shape = if matches!(base, Value::Numeric(_)) { base } else { value };
            logarithm(number(value_at)?, Some(number(base_at)?)).and_then(|result| shaped(shape, result))
        }
        ("power" | "pow", [base, exponent]) => {
            let shape = if matches!(exponent, Value::Numeric(_)) { exponent } else { base };
            power(number(0)?, number(1)?).and_then(|result| shaped(shape, result))
        }
        ("cbrt", [value]) => shaped(value, number(0)?.cbrt()),
        ("degrees", [value]) => shaped(value, number(0)?.to_degrees()),
        ("radians", [value]) => shaped(value, number(0)?.to_radians()),
        ("sign", [value]) => {
            let x = number(0)?;
            shaped(value, if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { 0.0 })
        }
        ("mod", [left, right]) if integer_pair(left, right).is_some() && !matches!(left, Value::Numeric(_)) && !matches!(right, Value::Numeric(_)) => {
            let (a, b) = integer_pair(left, right)?;
            match b {
                0 => Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero")),
                _ => same_integer_type(left, right, a.checked_rem(b).unwrap_or(0)),
            }
        }
        ("mod", [left, right]) => {
            let (a, b) = (number(0)?, number(1)?);
            if b == 0.0 {
                Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero"))
            } else {
                shaped(if matches!(left, Value::Numeric(_)) { left } else { right }, a % b)
            }
        }
        ("div", [left, right]) => {
            let (a, b) = (number(0)?, number(1)?);
            if b == 0.0 {
                Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero"))
            } else {
                Numeric::from_f64((a / b).trunc(), NUMERIC_DIGITS).map(Value::Numeric)
            }
        }
        ("gcd" | "lcm", [left, right]) => {
            let (a, b) = integer_pair(left, right)?;
            let common = greatest_common(a, b);
            let result = match (name, common) {
                (_, None) => return Some(Err(out_of_range("bigint out of range"))),
                ("gcd", Some(common)) => Some(common),
                (_, Some(0)) => Some(0),
                (_, Some(common)) => (a / common).checked_mul(b).and_then(i64::checked_abs),
            };
            match result {
                Some(result) => same_integer_type(left, right, result),
                None => Err(out_of_range("integer out of range")),
            }
        }
        ("width_bucket", [operand, low, high, count]) => {
            let (operand, low, high) = (number(0)?, number(1)?, number(2)?);
            let Some(count) = count.as_integer().filter(|count| *count > 0) else {
                return Some(Err(SqlError::new(SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "count must be greater than zero")));
            };
            if low == high {
                return Some(Err(SqlError::new(SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET, "lower bound cannot equal upper bound")));
            }
            let bucket = if low < high {
                if operand < low { 0 } else if operand >= high { count + 1 } else { ((operand - low) / (high - low) * count as f64) as i64 + 1 }
            } else if operand > low {
                0
            } else if operand <= high {
                count + 1
            } else {
                ((low - operand) / (low - high) * count as f64) as i64 + 1
            };
            i32::try_from(bucket).map(Value::Integer).map_err(|_| out_of_range("integer out of range"))
        }
        ("atan2" | "atan2d", [_, _]) => {
            let radians = number(0)?.atan2(number(1)?);
            Ok(Value::Double(if name == "atan2d" { exact_degrees(radians) } else { radians }))
        }
        ("erf", [value]) => shaped(value, erf(number(0)?)),
        ("erfc", [value]) => shaped(value, erfc(number(0)?)),
        ("gamma", [_]) => {
            let x = number(0)?;
            if x <= 0.0 && x.fract() == 0.0 {
                Err(out_of_range("value out of range: overflow"))
            } else {
                let result = gamma(x);
                if result.is_infinite() { Err(out_of_range("value out of range: overflow")) } else { Ok(Value::Double(result)) }
            }
        }
        ("lgamma", [_]) => {
            let x = number(0)?;
            if x <= 0.0 && x.fract() == 0.0 { Ok(Value::Double(f64::INFINITY)) } else { Ok(Value::Double(log_gamma(x))) }
        }
        (trigonometric_name, [value]) if TRIGONOMETRIC.contains(&trigonometric_name) => {
            trigonometric(trigonometric_name, number(0)?).and_then(|result| {
                if result.is_nan() && !value.as_float().is_some_and(f64::is_nan) {
                    Err(out_of_range("input is out of range"))
                } else {
                    Ok(Value::Double(result))
                }
            })
        }
        _ => return None,
    })
}

/// The result type of one of [`FUNCTIONS`] for its first argument's type.
pub(crate) fn result_type(name: &str, first: Option<SqlType>) -> Option<SqlType> {
    if !known(name) {
        return None;
    }
    let numeric = matches!(first, Some(SqlType::Numeric(_)));
    Some(match name {
        "width_bucket" => SqlType::Integer,
        "mod" | "gcd" | "lcm" => match first {
            Some(SqlType::SmallInt) => SqlType::SmallInt,
            Some(SqlType::BigInt) => SqlType::BigInt,
            Some(SqlType::Integer) => SqlType::Integer,
            Some(SqlType::Numeric(_)) => SqlType::Numeric(None),
            _ => SqlType::Double,
        },
        "div" | "factorial" => SqlType::Numeric(None),
        "setseed" => SqlType::Text,
        "exp" | "ln" | "log" | "log10" | "log2" | "power" | "pow" | "cbrt" | "degrees" | "radians" | "sign" | "erf" | "erfc" if numeric => {
            SqlType::Numeric(None)
        }
        _ => SqlType::Double,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, values: &[Value]) -> Value {
        function(name, values).expect("known").expect("runs")
    }

    #[test]
    fn logarithms_powers_and_their_errors() {
        assert_eq!(call("ln", &[Value::Double(1.0)]), Value::Double(0.0));
        assert_eq!(call("log", &[Value::Integer(100)]), Value::Double(2.0));
        assert_eq!(call("log", &[Value::Double(2.0), Value::Double(8.0)]), Value::Double(3.0));
        let Value::Double(cube) = call("log", &[Value::Double(3.0), Value::Double(27.0)]) else { panic!("double") };
        assert!((cube - 3.0).abs() < 1e-14);
        assert_eq!(call("power", &[Value::Integer(2), Value::Integer(10)]), Value::Double(1024.0));
        assert!(function("ln", &[Value::Integer(0)]).unwrap().is_err());
        assert!(function("power", &[Value::Integer(0), Value::Integer(-1)]).unwrap().is_err());
        assert!(function("power", &[Value::Double(-8.0), Value::Double(0.5)]).unwrap().is_err());
        assert_eq!(call("exp", &[Value::Null]), Value::Null);
    }

    #[test]
    fn integers_trigonometry_and_special_functions() {
        assert_eq!(call("mod", &[Value::Integer(7), Value::Integer(3)]), Value::Integer(1));
        assert!(function("mod", &[Value::Integer(7), Value::Integer(0)]).unwrap().is_err());
        assert_eq!(call("gcd", &[Value::BigInt(12), Value::BigInt(18)]), Value::BigInt(6));
        assert_eq!(call("lcm", &[Value::Integer(4), Value::Integer(6)]), Value::Integer(12));
        assert_eq!(call("width_bucket", &[Value::Double(5.35), Value::Double(0.024), Value::Double(10.06), Value::Integer(5)]), Value::Integer(3));
        assert_eq!(call("sind", &[Value::Double(30.0)]), Value::Double(0.5));
        assert_eq!(call("cosd", &[Value::Double(90.0)]), Value::Double(0.0));
        assert_eq!(call("atan2d", &[Value::Double(1.0), Value::Double(1.0)]), Value::Double(45.0));
        assert!(function("asin", &[Value::Double(2.0)]).unwrap().is_err());
        let Value::Double(error_function) = call("erf", &[Value::Double(1.0)]) else { panic!("double") };
        assert!((error_function - 0.842_700_792_949_714_9).abs() < 1e-14);
        let Value::Double(tail) = call("erfc", &[Value::Double(3.0)]) else { panic!("double") };
        assert!((tail - 2.209_049_699_858_544e-5).abs() < 1e-18);
        let Value::Double(factorial) = call("gamma", &[Value::Double(5.0)]) else { panic!("double") };
        assert!((factorial - 24.0).abs() < 1e-10);
        let Value::Double(random) = call("random", &[]) else { panic!("double") };
        assert!((0.0..1.0).contains(&random));
        call("setseed", &[Value::Double(0.5)]);
        let first = call("random", &[]);
        call("setseed", &[Value::Double(0.5)]);
        assert_eq!(call("random", &[]), first);
        assert!(function("setseed", &[Value::Double(2.0)]).unwrap().is_err());
        assert_eq!(call("factorial", &[Value::Integer(5)]), Value::Numeric(Numeric::from_i64(120)));
    }
}
