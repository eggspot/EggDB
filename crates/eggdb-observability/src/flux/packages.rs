//! Flux's standard packages for row expressions (D1064): `strings`, `math` and `date`, called
//! with named arguments as Flux writes them (`strings.toUpper(v: r.host)`,
//! `math.pow(x: r._value, y: 2.0)`, `date.truncate(t: r._time, unit: 1h)`).

use super::{Flux, UNIX_EPOCH_FROM_2000_MICROS, as_float};

const MICROS_PER_DAY: i64 = 86_400_000_000;
const MOST_REPEAT_BYTES: usize = 1 << 20;

/// Whether `name` is a package these row expressions can call.
pub(super) fn is_package(name: &str) -> bool {
    matches!(name, "strings" | "math" | "date")
}

/// A package's named constant, such as `math.pi`.
pub(super) fn constant(package: &str, member: &str) -> Option<Flux> {
    Some(Flux::Float(match (package, member) {
        ("math", "pi") => std::f64::consts::PI,
        ("math", "e") => std::f64::consts::E,
        ("math", "phi") => 1.618_033_988_749_895,
        ("math", "sqrt2") => std::f64::consts::SQRT_2,
        ("math", "ln2") => std::f64::consts::LN_2,
        ("math", "maxfloat") => f64::MAX,
        ("math", "smallestNonzeroFloat") => f64::MIN_POSITIVE,
        ("math", "mInf") => f64::NEG_INFINITY,
        _ => return None,
    }))
}

/// A time written in a query, as RFC 3339.
pub(super) fn time_constant(text: &str) -> Result<Flux, String> {
    crate::syslog::rfc3339(text.as_bytes())
        .map(|unix_micros| Flux::Time(unix_micros - UNIX_EPOCH_FROM_2000_MICROS))
        .ok_or_else(|| format!("{text} is not an RFC 3339 time"))
}

fn named<'a>(arguments: &'a [(&str, Flux)], name: &str) -> &'a Flux {
    arguments.iter().find(|(key, _)| *key == name).map_or(&Flux::Null, |(_, value)| value)
}

fn text(arguments: &[(&str, Flux)], name: &str) -> Option<String> {
    match named(arguments, name) {
        Flux::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn float(arguments: &[(&str, Flux)], name: &str) -> Option<f64> {
    as_float(named(arguments, name))
}

fn integer(arguments: &[(&str, Flux)], name: &str) -> Option<i64> {
    match named(arguments, name) {
        Flux::Integer(whole) => Some(*whole),
        Flux::Float(number) if number.fract() == 0.0 => Some(*number as i64),
        _ => None,
    }
}

fn time(arguments: &[(&str, Flux)], name: &str) -> Option<i64> {
    match named(arguments, name) {
        Flux::Time(micros) => Some(*micros),
        Flux::Integer(micros) => Some(*micros),
        _ => None,
    }
}

fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut starts_word = true;
    for character in text.chars() {
        if starts_word {
            out.extend(character.to_uppercase());
        } else {
            out.push(character);
        }
        starts_word = character.is_whitespace();
    }
    out
}

fn characters_between(text: &str, start: i64, end: i64) -> String {
    let start = usize::try_from(start.max(0)).unwrap_or(0);
    let end = usize::try_from(end.max(0)).unwrap_or(0);
    text.chars().skip(start).take(end.saturating_sub(start)).collect()
}

fn strings(member: &str, arguments: &[(&str, Flux)]) -> Result<Flux, String> {
    let Some(value) = text(arguments, "v") else {
        return Ok(Flux::Null);
    };
    let other = |name: &str| text(arguments, name).ok_or_else(|| format!("strings.{member}() needs {name}: a string"));
    Ok(match member {
        "toUpper" => Flux::Text(value.to_uppercase()),
        "toLower" => Flux::Text(value.to_lowercase()),
        "trimSpace" => Flux::Text(value.trim().to_owned()),
        "title" => Flux::Text(title(&value)),
        "strlen" => Flux::Integer(value.chars().count() as i64),
        "hasPrefix" => Flux::Boolean(value.starts_with(&other("prefix")?)),
        "hasSuffix" => Flux::Boolean(value.ends_with(&other("suffix")?)),
        "containsStr" => Flux::Boolean(value.contains(&other("substr")?)),
        "trimPrefix" => {
            let prefix = other("prefix")?;
            Flux::Text(value.strip_prefix(prefix.as_str()).unwrap_or(&value).to_owned())
        }
        "trimSuffix" => {
            let suffix = other("suffix")?;
            Flux::Text(value.strip_suffix(suffix.as_str()).unwrap_or(&value).to_owned())
        }
        "replaceAll" => Flux::Text(value.replace(&other("t")?, &other("u")?)),
        "replace" => {
            let count = integer(arguments, "i").unwrap_or(-1);
            let (from, to) = (other("t")?, other("u")?);
            if count < 0 {
                Flux::Text(value.replace(&from, &to))
            } else {
                Flux::Text(value.replacen(&from, &to, usize::try_from(count).unwrap_or(0)))
            }
        }
        "substring" => {
            let start = integer(arguments, "start").unwrap_or(0);
            let end = integer(arguments, "end").unwrap_or(i64::MAX);
            Flux::Text(characters_between(&value, start, end))
        }
        "repeat" => {
            let count = usize::try_from(integer(arguments, "i").unwrap_or(0).max(0)).unwrap_or(0);
            if value.len().saturating_mul(count) > MOST_REPEAT_BYTES {
                return Err("strings.repeat() would make a string over 1 MiB".to_owned());
            }
            Flux::Text(value.repeat(count))
        }
        "index" => Flux::Integer(value.find(&other("substr")?).map_or(-1, |at| value[..at].chars().count() as i64)),
        "countStr" => {
            let needle = other("substr")?;
            Flux::Integer(if needle.is_empty() { 0 } else { value.matches(&needle).count() as i64 })
        }
        other => return Err(format!("strings.{other}() is not supported")),
    })
}

fn math(member: &str, arguments: &[(&str, Flux)]) -> Result<Flux, String> {
    let Some(x) = float(arguments, "x") else {
        return Ok(Flux::Null);
    };
    let y = || float(arguments, "y").ok_or_else(|| format!("math.{member}() needs y: a number"));
    Ok(Flux::Float(match member {
        "abs" => x.abs(),
        "ceil" => x.ceil(),
        "floor" => x.floor(),
        "round" => x.round(),
        "trunc" => x.trunc(),
        "sqrt" => x.sqrt(),
        "cbrt" => x.cbrt(),
        "exp" => x.exp(),
        "exp2" => x.exp2(),
        "log" => x.ln(),
        "log10" => x.log10(),
        "log2" => x.log2(),
        "log1p" => x.ln_1p(),
        "sin" => x.sin(),
        "cos" => x.cos(),
        "tan" => x.tan(),
        "asin" => x.asin(),
        "acos" => x.acos(),
        "atan" => x.atan(),
        "sinh" => x.sinh(),
        "cosh" => x.cosh(),
        "tanh" => x.tanh(),
        "pow" => x.powf(y()?),
        "atan2" => float(arguments, "y").map_or(f64::NAN, |y| y.atan2(x)),
        "mod" => x % y()?,
        "hypot" => x.hypot(y()?),
        "max" => x.max(y()?),
        "min" => x.min(y()?),
        "signbit" => return Ok(Flux::Boolean(x.is_sign_negative())),
        "isInf" => return Ok(Flux::Boolean(match integer(arguments, "sign").unwrap_or(0) {
            sign if sign > 0 => x == f64::INFINITY,
            sign if sign < 0 => x == f64::NEG_INFINITY,
            _ => x.is_infinite(),
        })),
        "isNaN" => return Ok(Flux::Boolean(x.is_nan())),
        other => return Err(format!("math.{other}() is not supported")),
    }))
}

fn date(member: &str, arguments: &[(&str, Flux)]) -> Result<Flux, String> {
    let Some(micros) = time(arguments, "t") else {
        return Ok(Flux::Null);
    };
    let unix = micros + UNIX_EPOCH_FROM_2000_MICROS;
    let days = unix.div_euclid(MICROS_PER_DAY);
    let of_day = unix.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = crate::influxql::civil(days);
    Ok(match member {
        "year" => Flux::Integer(year),
        "month" => Flux::Integer(month),
        "monthDay" => Flux::Integer(day),
        "weekDay" => Flux::Integer((days + 4).rem_euclid(7)),
        "yearDay" => {
            let first = (1..month).map(|earlier| days_in(year, earlier)).sum::<i64>();
            Flux::Integer(first + day)
        }
        "hour" => Flux::Integer(of_day / 3_600_000_000),
        "minute" => Flux::Integer(of_day / 60_000_000 % 60),
        "second" => Flux::Integer(of_day / 1_000_000 % 60),
        "millisecond" => Flux::Integer(of_day / 1_000 % 1_000),
        "microsecond" => Flux::Integer(of_day % 1_000_000),
        "truncate" => {
            let unit = time(arguments, "unit").filter(|unit| *unit > 0).ok_or("date.truncate() needs unit: a positive duration")?;
            Flux::Time(unix.div_euclid(unit) * unit - UNIX_EPOCH_FROM_2000_MICROS)
        }
        other => return Err(format!("date.{other}() is not supported")),
    })
}

fn days_in(year: i64, month: i64) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Calls the package function `name` (`package.member`) with its named `arguments`; a missing
/// main argument (`v`, `x` or `t`) makes the result null, as Flux does for null inputs.
pub(super) fn call(name: &str, arguments: &[(&str, Flux)]) -> Result<Flux, String> {
    match name.split_once('.') {
        Some(("strings", member)) => strings(member, arguments),
        Some(("math", member)) => math(member, arguments),
        Some(("date", member)) => date(member, arguments),
        _ => Err(format!("function {name}() is not supported yet")),
    }
}
