//! SPARQL's operations on RDF terms (SPARQL 1.1 Query Language, section 17): effective
//! boolean values, numeric arithmetic with type promotion, comparisons, the ordering of
//! `ORDER BY`, and the built-in functions.

use crate::term::{
    Literal, RDF_LANG_STRING, Term, XSD_BOOLEAN, XSD_DATE_TIME, XSD_DECIMAL, XSD_DOUBLE, XSD_INTEGER,
    XSD_STRING,
};
use std::cmp::Ordering;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const MAX_REGEX_STEPS: usize = 1_000_000;

/// An expression's failure: SPARQL's type error, which makes a filter false and leaves a
/// projected variable unbound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypeError;

pub(crate) type Evaluated = Result<Term, TypeError>;

const INTEGER_TYPES: [&str; 13] = [
    "integer",
    "nonPositiveInteger",
    "negativeInteger",
    "long",
    "int",
    "short",
    "byte",
    "nonNegativeInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
    "positiveInteger",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Number {
    Integer(i128),
    Decimal(f64),
    Double(f64),
}

impl Number {
    fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Decimal(value) | Self::Double(value) => value,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Integer(_) => 0,
            Self::Decimal(_) => 1,
            Self::Double(_) => 2,
        }
    }

    pub(crate) fn term(self) -> Term {
        match self {
            Self::Integer(value) => Term::Literal(Literal::typed(&value.to_string(), XSD_INTEGER)),
            Self::Decimal(value) => {
                let mut text = format!("{value}");
                if !text.contains('.') && value.is_finite() {
                    text.push_str(".0");
                }
                Term::Literal(Literal::typed(&text, XSD_DECIMAL))
            }
            Self::Double(value) => Term::Literal(Literal::typed(&double_text(value), XSD_DOUBLE)),
        }
    }
}

fn double_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    let text = format!("{value:E}");
    match text.split_once('E') {
        Some((mantissa, exponent)) if !mantissa.contains('.') => format!("{mantissa}.0E{exponent}"),
        _ => text,
    }
}

/// The literal's value as a number, when its datatype is numeric and its lexical form valid.
pub(crate) fn number(term: &Term) -> Option<Number> {
    let Term::Literal(literal) = term else {
        return None;
    };
    let local = literal.datatype.strip_prefix(XSD)?;
    let text = literal.lexical.trim();
    if INTEGER_TYPES.contains(&local) {
        return text.strip_prefix('+').unwrap_or(text).parse::<i128>().ok().map(Number::Integer);
    }
    match local {
        "decimal" => {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'+')) {
                return None;
            }
            text.parse::<f64>().ok().map(Number::Decimal)
        }
        "double" | "float" => match text {
            "INF" | "+INF" => Some(Number::Double(f64::INFINITY)),
            "-INF" => Some(Number::Double(f64::NEG_INFINITY)),
            "NaN" => Some(Number::Double(f64::NAN)),
            _ => text.parse::<f64>().ok().map(Number::Double),
        },
        _ => None,
    }
}

pub(crate) fn boolean(value: bool) -> Term {
    Term::Literal(Literal::typed(if value { "true" } else { "false" }, XSD_BOOLEAN))
}

pub(crate) fn plain(text: &str) -> Term {
    Term::Literal(Literal::string(text))
}

fn is_string_literal(literal: &Literal) -> bool {
    literal.datatype == XSD_STRING || literal.datatype == RDF_LANG_STRING
}

/// The effective boolean value of a term (section 17.2.2).
pub(crate) fn effective_boolean(term: &Term) -> Result<bool, TypeError> {
    let Term::Literal(literal) = term else {
        return Err(TypeError);
    };
    if literal.datatype == XSD_BOOLEAN {
        return match literal.lexical.as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Ok(false),
        };
    }
    if let Some(number) = number(term) {
        let value = number.as_f64();
        return Ok(value != 0.0 && !value.is_nan());
    }
    if is_string_literal(literal) {
        return Ok(!literal.lexical.is_empty());
    }
    Err(TypeError)
}

/// `left op right` with SPARQL's numeric type promotion: integers stay integers except under
/// division, which gives a decimal.
pub(crate) fn arithmetic(operator: crate::sparql_ast::Arithmetic, left: &Term, right: &Term) -> Evaluated {
    use crate::sparql_ast::Arithmetic;
    let (Some(left), Some(right)) = (number(left), number(right)) else {
        return Err(TypeError);
    };
    let rank = left.rank().max(right.rank());
    if rank == 0 && operator != Arithmetic::Divide {
        let (Number::Integer(a), Number::Integer(b)) = (left, right) else {
            return Err(TypeError);
        };
        let value = match operator {
            Arithmetic::Add => a.checked_add(b),
            Arithmetic::Subtract => a.checked_sub(b),
            Arithmetic::Multiply => a.checked_mul(b),
            Arithmetic::Divide => None,
        }
        .ok_or(TypeError)?;
        return Ok(Number::Integer(value).term());
    }
    let (a, b) = (left.as_f64(), right.as_f64());
    let value = match operator {
        Arithmetic::Add => a + b,
        Arithmetic::Subtract => a - b,
        Arithmetic::Multiply => a * b,
        Arithmetic::Divide => {
            if b == 0.0 && rank < 2 {
                return Err(TypeError);
            }
            a / b
        }
    };
    Ok(if rank == 2 { Number::Double(value) } else { Number::Decimal(value) }.term())
}

fn literal_key(literal: &Literal) -> (&str, &str) {
    (literal.datatype.as_str(), literal.language.as_deref().unwrap_or_default())
}

/// `left` compared with `right` as the operators `<`, `=` and the rest compare them: numbers
/// by value, strings, booleans and date-times by their own order. `None` when they do not
/// compare (a type error for `<`; for `=`, RDF term equality decides).
pub(crate) fn compare(left: &Term, right: &Term) -> Option<Ordering> {
    if let (Some(a), Some(b)) = (number(left), number(right)) {
        return match (a, b) {
            (Number::Integer(a), Number::Integer(b)) => Some(a.cmp(&b)),
            _ => a.as_f64().partial_cmp(&b.as_f64()),
        };
    }
    let (Term::Literal(a), Term::Literal(b)) = (left, right) else {
        return None;
    };
    if literal_key(a) != literal_key(b) {
        return None;
    }
    match a.datatype.as_str() {
        XSD_STRING | RDF_LANG_STRING | XSD_DATE_TIME => Some(a.lexical.cmp(&b.lexical)),
        XSD_BOOLEAN => {
            let value = |text: &str| matches!(text, "true" | "1");
            Some(value(&a.lexical).cmp(&value(&b.lexical)))
        }
        _ => (a.lexical == b.lexical).then_some(Ordering::Equal),
    }
}

/// `left = right`: value equality where the values compare, else RDF term equality, with a
/// type error for two literals of unknown types that differ.
pub(crate) fn equals(left: &Term, right: &Term) -> Result<bool, TypeError> {
    if let Some(order) = compare(left, right) {
        return Ok(order == Ordering::Equal);
    }
    if left == right {
        return Ok(true);
    }
    match (left, right) {
        (Term::Literal(a), Term::Literal(b))
            if !is_string_literal(a) && !is_string_literal(b) && number(left).is_none() && number(right).is_none() =>
        {
            Err(TypeError)
        }
        _ => Ok(false),
    }
}

/// The order of `ORDER BY` (section 15.1): unbound, blank nodes, IRIs, then literals, which
/// compare by value where they can and by their text otherwise.
pub(crate) fn order(left: Option<&Term>, right: Option<&Term>) -> Ordering {
    let rank = |term: Option<&Term>| match term {
        None => 0,
        Some(Term::Blank(_)) => 1,
        Some(Term::Iri(_)) => 2,
        Some(Term::Literal(_)) => 3,
    };
    match (left, right) {
        (Some(left_term @ Term::Literal(a)), Some(right_term @ Term::Literal(b))) => compare(left_term, right_term)
            .unwrap_or_else(|| a.lexical.cmp(&b.lexical).then_with(|| a.datatype.cmp(&b.datatype))),
        (Some(Term::Iri(a)), Some(Term::Iri(b))) | (Some(Term::Blank(a)), Some(Term::Blank(b))) => a.cmp(b),
        _ => rank(left).cmp(&rank(right)),
    }
}

/// The string value of `str()`.
pub(crate) fn string_value(term: &Term) -> Result<String, TypeError> {
    match term {
        Term::Iri(iri) => Ok(iri.clone()),
        Term::Literal(literal) => Ok(literal.lexical.clone()),
        Term::Blank(_) => Err(TypeError),
    }
}

fn string_argument(term: &Term) -> Result<&Literal, TypeError> {
    match term {
        Term::Literal(literal) if is_string_literal(literal) => Ok(literal),
        _ => Err(TypeError),
    }
}

/// A string result that keeps the language tag of `like`, as `UCASE`, `SUBSTR` and the
/// others do.
fn string_like(like: &Literal, text: &str) -> Term {
    match &like.language {
        Some(language) => Term::Literal(Literal::tagged(text, language)),
        None => plain(text),
    }
}

fn compatible(left: &Literal, right: &Literal) -> bool {
    right.language.is_none() || left.language == right.language
}

/// Whether `range` (an RFC 4647 basic range) matches the language `tag`.
pub(crate) fn language_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    let (tag, range) = (tag.to_ascii_lowercase(), range.to_ascii_lowercase());
    tag == range || tag.starts_with(&format!("{range}-"))
}

fn percent_encoded(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

struct DateTime {
    year: i64,
    month: u32,
    day: u32,
    hours: u32,
    minutes: u32,
    seconds: String,
    zone: Option<String>,
}

fn date_time(term: &Term) -> Result<DateTime, TypeError> {
    let Term::Literal(literal) = term else {
        return Err(TypeError);
    };
    if literal.datatype != XSD_DATE_TIME && literal.datatype != format!("{XSD}date") {
        return Err(TypeError);
    }
    let text = literal.lexical.trim();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (date, time) = text.split_once('T').unwrap_or((text, "00:00:00"));
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next().and_then(|part| part.parse().ok()).ok_or(TypeError)?;
    let month: u32 = parts.next().and_then(|part| part.parse().ok()).ok_or(TypeError)?;
    let day_text = parts.next().ok_or(TypeError)?;
    let day: u32 = day_text.get(..2).and_then(|part| part.parse().ok()).ok_or(TypeError)?;
    let zone_at = time.find(['Z', '+']).or_else(|| time.rfind('-').filter(|at| *at > 0));
    let (clock, zone) = match zone_at {
        Some(at) => (&time[..at], Some(time[at..].to_owned())),
        None => (time, day_text.get(2..).filter(|zone| !zone.is_empty()).map(str::to_owned)),
    };
    let mut clock_parts = clock.splitn(3, ':');
    let hours = clock_parts.next().and_then(|part| part.parse().ok()).ok_or(TypeError)?;
    let minutes = clock_parts.next().and_then(|part| part.parse().ok()).ok_or(TypeError)?;
    let seconds = clock_parts.next().unwrap_or("0").to_owned();
    Ok(DateTime {
        year: if negative { -year } else { year },
        month,
        day,
        hours,
        minutes,
        seconds,
        zone,
    })
}

fn day_time_duration(zone: &str) -> Result<String, TypeError> {
    if zone == "Z" {
        return Ok("PT0S".to_owned());
    }
    let negative = zone.starts_with('-');
    let (hours, minutes) = zone[1..].split_once(':').ok_or(TypeError)?;
    let hours: u32 = hours.parse().map_err(|_| TypeError)?;
    let minutes: u32 = minutes.parse().map_err(|_| TypeError)?;
    let mut text = format!("{}PT", if negative { "-" } else { "" });
    if hours > 0 {
        text.push_str(&format!("{hours}H"));
    }
    if minutes > 0 || hours == 0 {
        text.push_str(&format!("{minutes}M"));
    }
    Ok(text)
}

/// Whether `pattern` (a regular expression of XPath's syntax, the common subset: literals,
/// `.`, classes `[...]` with ranges and negation, `\d \w \s` and their negations, anchors `^`
/// and `$`, groups and alternation, and the quantifiers `* + ? {m,n}`) matches somewhere in
/// `text`. `None` when the pattern is malformed or too costly.
pub(crate) fn regex_matches(text: &str, pattern: &str, flags: &str) -> Option<bool> {
    let insensitive = flags.contains('i');
    let fold = |text: &str| if insensitive { text.to_lowercase() } else { text.to_owned() };
    let (text, pattern) = (fold(text), fold(pattern));
    let parsed = crate::sparql_regex::parse(&pattern)?;
    let characters: Vec<char> = text.chars().collect();
    let mut steps = 0usize;
    for start in 0..=characters.len() {
        match crate::sparql_regex::matches_at(&parsed, &characters, start, &mut steps, MAX_REGEX_STEPS) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => return None,
        }
    }
    Some(false)
}

/// The built-in function `name` (lower case) applied to already evaluated `arguments`.
pub(crate) fn call(name: &str, arguments: &[Term]) -> Evaluated {
    let one = || arguments.first().ok_or(TypeError);
    let two = || match arguments {
        [a, b, ..] => Ok((a, b)),
        _ => Err(TypeError),
    };
    match name {
        "str" => string_value(one()?).map(|text| plain(&text)),
        "lang" => match one()? {
            Term::Literal(literal) => Ok(plain(literal.language.as_deref().unwrap_or_default())),
            _ => Err(TypeError),
        },
        "langmatches" => {
            let (tag, range) = two()?;
            Ok(boolean(language_matches(&string_value(tag)?, &string_value(range)?)))
        }
        "datatype" => match one()? {
            Term::Literal(literal) => Ok(Term::Iri(literal.datatype.clone())),
            _ => Err(TypeError),
        },
        "iri" | "uri" => match one()? {
            Term::Iri(iri) => Ok(Term::Iri(iri.clone())),
            Term::Literal(literal) if is_string_literal(literal) && literal.language.is_none() => {
                Ok(Term::Iri(literal.lexical.clone()))
            }
            _ => Err(TypeError),
        },
        "isiri" | "isuri" => Ok(boolean(matches!(one()?, Term::Iri(_)))),
        "isblank" => Ok(boolean(matches!(one()?, Term::Blank(_)))),
        "isliteral" => Ok(boolean(matches!(one()?, Term::Literal(_)))),
        "isnumeric" => Ok(boolean(number(one()?).is_some())),
        "sameterm" => {
            let (a, b) = two()?;
            Ok(boolean(a == b))
        }
        "strlen" => Ok(Number::Integer(string_argument(one()?)?.lexical.chars().count() as i128).term()),
        "ucase" | "lcase" => {
            let literal = string_argument(one()?)?;
            let text = if name == "ucase" { literal.lexical.to_uppercase() } else { literal.lexical.to_lowercase() };
            Ok(string_like(literal, &text))
        }
        "substr" => {
            let literal = string_argument(one()?)?;
            let start = arguments.get(1).and_then(number).ok_or(TypeError)?.as_f64().round();
            let characters: Vec<char> = literal.lexical.chars().collect();
            let end = match arguments.get(2) {
                Some(length) => start + number(length).ok_or(TypeError)?.as_f64().round(),
                None => f64::INFINITY,
            };
            let text: String = characters
                .iter()
                .enumerate()
                .filter(|(at, _)| {
                    let position = (*at + 1) as f64;
                    position >= start && position < end
                })
                .map(|(_, character)| *character)
                .collect();
            Ok(string_like(literal, &text))
        }
        "contains" | "strstarts" | "strends" | "strbefore" | "strafter" => {
            let (a, b) = two()?;
            let (a, b) = (string_argument(a)?, string_argument(b)?);
            if !compatible(a, b) {
                return Err(TypeError);
            }
            Ok(match name {
                "contains" => boolean(a.lexical.contains(&b.lexical)),
                "strstarts" => boolean(a.lexical.starts_with(&b.lexical)),
                "strends" => boolean(a.lexical.ends_with(&b.lexical)),
                "strbefore" => match a.lexical.find(&b.lexical) {
                    Some(at) => string_like(a, &a.lexical[..at]),
                    None => plain(""),
                },
                _ => match a.lexical.find(&b.lexical) {
                    Some(at) => string_like(a, &a.lexical[at + b.lexical.len()..]),
                    None => plain(""),
                },
            })
        }
        "concat" => {
            let mut text = String::new();
            let mut language: Option<Option<String>> = None;
            for argument in arguments {
                let literal = string_argument(argument)?;
                text.push_str(&literal.lexical);
                language = match language {
                    None => Some(literal.language.clone()),
                    Some(known) if known == literal.language => Some(known),
                    Some(_) => Some(None),
                };
            }
            Ok(match language.flatten() {
                Some(language) => Term::Literal(Literal::tagged(&text, &language)),
                None => plain(&text),
            })
        }
        "encode_for_uri" => Ok(plain(&percent_encoded(&string_argument(one()?)?.lexical))),
        "strlang" => {
            let (text, language) = two()?;
            let text = string_argument(text)?;
            if text.language.is_some() {
                return Err(TypeError);
            }
            Ok(Term::Literal(Literal::tagged(&text.lexical, &string_value(language)?)))
        }
        "strdt" => {
            let (text, datatype) = two()?;
            let (Term::Literal(text), Term::Iri(datatype)) = (text, datatype) else {
                return Err(TypeError);
            };
            if text.language.is_some() {
                return Err(TypeError);
            }
            Ok(Term::Literal(Literal::typed(&text.lexical, datatype)))
        }
        "abs" | "ceil" | "floor" | "round" => {
            let value = number(one()?).ok_or(TypeError)?;
            Ok(match value {
                Number::Integer(integer) => Number::Integer(if name == "abs" { integer.abs() } else { integer }),
                Number::Decimal(decimal) | Number::Double(decimal) => {
                    let changed = match name {
                        "abs" => decimal.abs(),
                        "ceil" => decimal.ceil(),
                        "floor" => decimal.floor(),
                        _ => (decimal + 0.5).floor(),
                    };
                    if matches!(value, Number::Decimal(_)) { Number::Decimal(changed) } else { Number::Double(changed) }
                }
            }
            .term())
        }
        "regex" => {
            let text = string_argument(one()?)?;
            let pattern = string_value(arguments.get(1).ok_or(TypeError)?)?;
            let flags = arguments.get(2).map(string_value).transpose()?.unwrap_or_default();
            regex_matches(&text.lexical, &pattern, &flags).map(boolean).ok_or(TypeError)
        }
        "replace" => {
            let text = string_argument(one()?)?;
            let pattern = string_value(arguments.get(1).ok_or(TypeError)?)?;
            let replacement = string_value(arguments.get(2).ok_or(TypeError)?)?;
            let flags = arguments.get(3).map(string_value).transpose()?.unwrap_or_default();
            let replaced = crate::sparql_regex::replace_all(&text.lexical, &pattern, &replacement, flags.contains('i'))
                .ok_or(TypeError)?;
            Ok(string_like(text, &replaced))
        }
        "year" | "month" | "day" | "hours" | "minutes" => {
            let parts = date_time(one()?)?;
            let value = match name {
                "year" => i128::from(parts.year),
                "month" => i128::from(parts.month),
                "day" => i128::from(parts.day),
                "hours" => i128::from(parts.hours),
                _ => i128::from(parts.minutes),
            };
            Ok(Number::Integer(value).term())
        }
        "seconds" => {
            let parts = date_time(one()?)?;
            let value: f64 = parts.seconds.parse().map_err(|_| TypeError)?;
            Ok(Number::Decimal(value).term())
        }
        "timezone" => {
            let zone = date_time(one()?)?.zone.ok_or(TypeError)?;
            Ok(Term::Literal(Literal::typed(&day_time_duration(&zone)?, &format!("{XSD}dayTimeDuration"))))
        }
        "tz" => Ok(plain(&date_time(one()?)?.zone.unwrap_or_default())),
        _ => cast(name, arguments),
    }
}

/// An XSD cast written as a function call, such as `xsd:integer("42")`.
fn cast(name: &str, arguments: &[Term]) -> Evaluated {
    let Some(local) = name.strip_prefix(XSD) else {
        return Err(TypeError);
    };
    let [value] = arguments else {
        return Err(TypeError);
    };
    let text = match value {
        Term::Literal(literal) => literal.lexical.trim().to_owned(),
        Term::Iri(iri) if local == "string" => iri.clone(),
        _ => return Err(TypeError),
    };
    match local {
        "string" => Ok(plain(&text)),
        "boolean" => match number(value) {
            Some(number) => Ok(boolean(number.as_f64() != 0.0 && !number.as_f64().is_nan())),
            None => match text.as_str() {
                "true" | "1" => Ok(boolean(true)),
                "false" | "0" => Ok(boolean(false)),
                _ => Err(TypeError),
            },
        },
        "integer" => match number(value) {
            Some(number) => Ok(Number::Integer(number.as_f64().trunc() as i128).term()),
            None => text.parse::<i128>().map(|parsed| Number::Integer(parsed).term()).map_err(|_| TypeError),
        },
        "decimal" => match number(value) {
            Some(number) => Ok(Number::Decimal(number.as_f64()).term()),
            None => text.parse::<f64>().map(|parsed| Number::Decimal(parsed).term()).map_err(|_| TypeError),
        },
        "double" | "float" => match number(value) {
            Some(number) => Ok(Number::Double(number.as_f64()).term()),
            None => text.parse::<f64>().map(|parsed| Number::Double(parsed).term()).map_err(|_| TypeError),
        },
        "dateTime" => {
            let candidate = Term::Literal(Literal::typed(&text, XSD_DATE_TIME));
            date_time(&candidate).map(|_| candidate)
        }
        _ => Err(TypeError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql_ast::Arithmetic;

    fn integer(value: i64) -> Term {
        Term::Literal(Literal::typed(&value.to_string(), XSD_INTEGER))
    }

    #[test]
    fn arithmetic_promotes_types() {
        assert_eq!(arithmetic(Arithmetic::Add, &integer(2), &integer(3)), Ok(integer(5)));
        assert_eq!(
            arithmetic(Arithmetic::Divide, &integer(1), &integer(2)),
            Ok(Term::Literal(Literal::typed("0.5", XSD_DECIMAL)))
        );
        assert_eq!(arithmetic(Arithmetic::Divide, &integer(1), &integer(0)), Err(TypeError));
        assert_eq!(arithmetic(Arithmetic::Add, &integer(1), &plain("x")), Err(TypeError));
    }

    #[test]
    fn comparisons_and_effective_booleans() {
        assert_eq!(compare(&integer(2), &Term::Literal(Literal::typed("2.0", XSD_DECIMAL))), Some(Ordering::Equal));
        assert_eq!(equals(&plain("a"), &Term::Literal(Literal::tagged("a", "en"))), Ok(false));
        assert_eq!(effective_boolean(&plain("")), Ok(false));
        assert_eq!(effective_boolean(&integer(3)), Ok(true));
        assert_eq!(effective_boolean(&Term::iri("http://x")), Err(TypeError));
        assert_eq!(order(None, Some(&Term::iri("x"))), Ordering::Less);
    }

    #[test]
    fn string_and_date_functions() {
        assert_eq!(call("ucase", &[Term::Literal(Literal::tagged("chat", "fr"))]), Ok(Term::Literal(Literal::tagged("CHAT", "fr"))));
        assert_eq!(call("substr", &[plain("foobar"), integer(4)]), Ok(plain("bar")));
        assert_eq!(call("strafter", &[plain("abc"), plain("b")]), Ok(plain("c")));
        assert_eq!(call("concat", &[plain("a"), plain("b")]), Ok(plain("ab")));
        assert_eq!(call("langmatches", &[plain("en-GB"), plain("en")]), Ok(boolean(true)));
        let stamp = Term::Literal(Literal::typed("2011-01-10T14:45:13.815-05:00", XSD_DATE_TIME));
        assert_eq!(call("year", &[stamp.clone()]), Ok(integer(2011)));
        assert_eq!(call("hours", std::slice::from_ref(&stamp)), Ok(integer(14)));
        assert_eq!(call("tz", &[stamp]), Ok(plain("-05:00")));
        assert_eq!(call("regex", &[plain("Alice"), plain("^ali"), plain("i")]), Ok(boolean(true)));
        assert_eq!(call("replace", &[plain("abcd"), plain("b."), plain("Z")]), Ok(plain("aZd")));
        assert_eq!(call(&format!("{XSD}integer"), &[plain("42")]), Ok(integer(42)));
    }
}
