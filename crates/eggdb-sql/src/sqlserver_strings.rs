use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::sqlserver_conversion::text_of;
use crate::sqlserver_functions::within_limit;
use crate::sqlserver_types::{SqlServerType, value_type_name};
use crate::types::{SqlType, Value};

const SHORT_VALUE_BYTES: usize = 8000;
const SHORT_NATIONAL_CHARACTERS: usize = 4000;
const MOST_CONCAT_ARGUMENTS: usize = 254;
const UNKNOWN_CHARACTER: i32 = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrimEnds {
    Both,
    Leading,
    Trailing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextFunction {
    Substring,
    Left,
    Right,
    Replace,
    Stuff,
    Replicate,
    Space,
    Reverse,
    Trim(TrimEnds),
    Concat,
    ConcatWs,
    DataLength,
    Ascii,
    Char,
    Unicode,
    NChar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextOrigin {
    pub(crate) national: bool,
    pub(crate) declared: Option<SqlType>,
}

impl TextFunction {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "substring" => Self::Substring,
            "left" => Self::Left,
            "right" => Self::Right,
            "replace" => Self::Replace,
            "stuff" => Self::Stuff,
            "replicate" => Self::Replicate,
            "space" => Self::Space,
            "reverse" => Self::Reverse,
            "trim" => Self::Trim(TrimEnds::Both),
            "ltrim" => Self::Trim(TrimEnds::Leading),
            "rtrim" => Self::Trim(TrimEnds::Trailing),
            "concat" => Self::Concat,
            "concat_ws" => Self::ConcatWs,
            "datalength" => Self::DataLength,
            "ascii" => Self::Ascii,
            "char" => Self::Char,
            "unicode" => Self::Unicode,
            "nchar" => Self::NChar,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Substring => "substring",
            Self::Left => "left",
            Self::Right => "right",
            Self::Replace => "replace",
            Self::Stuff => "stuff",
            Self::Replicate => "replicate",
            Self::Space => "space",
            Self::Reverse => "reverse",
            Self::Trim(TrimEnds::Both) => "trim",
            Self::Trim(TrimEnds::Leading) => "ltrim",
            Self::Trim(TrimEnds::Trailing) => "rtrim",
            Self::Concat => "concat",
            Self::ConcatWs => "concat_ws",
            Self::DataLength => "datalength",
            Self::Ascii => "ascii",
            Self::Char => "char",
            Self::Unicode => "unicode",
            Self::NChar => "nchar",
        }
    }

    pub(crate) fn arguments(self) -> std::ops::RangeInclusive<usize> {
        match self {
            Self::Space
            | Self::Reverse
            | Self::DataLength
            | Self::Ascii
            | Self::Char
            | Self::Unicode
            | Self::NChar => 1..=1,
            Self::Left | Self::Right | Self::Replicate => 2..=2,
            Self::Trim(_) => 1..=2,
            Self::Substring | Self::Replace => 3..=3,
            Self::Stuff => 4..=4,
            Self::Concat => 2..=MOST_CONCAT_ARGUMENTS,
            Self::ConcatWs => 3..=MOST_CONCAT_ARGUMENTS,
        }
    }

    pub(crate) fn needs_declared_type(self) -> bool {
        matches!(
            self,
            Self::Replace | Self::Stuff | Self::Replicate | Self::DataLength
        )
    }

    pub(crate) fn result_type(self, first_argument: Option<SqlType>) -> Option<SqlType> {
        Some(match self {
            Self::Substring | Self::Stuff | Self::Replicate
                if first_argument == Some(SqlType::Bytea) =>
            {
                SqlType::Bytea
            }
            Self::DataLength | Self::Ascii | Self::Unicode => SqlType::Integer,
            _ => SqlType::Text,
        })
    }
}

fn invalid_length(function: TextFunction) -> SqlError {
    let (code, message) = match function {
        TextFunction::Right => (536, "Invalid length parameter passed to the RIGHT function."),
        _ => (
            537,
            "Invalid length parameter passed to the LEFT or SUBSTRING function.",
        ),
    };
    SqlError::new(SqlState::SUBSTRING_ERROR, message).with_sqlserver_code(code)
}

fn argument_type_invalid(value: &Value, argument: usize, function: TextFunction) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "Argument data type {} is invalid for argument {argument} of {} function.",
            value_type_name(value),
            function.name()
        ),
    )
    .with_sqlserver_code(8116)
}

fn overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "Arithmetic overflow error converting expression to data type int.",
    )
    .with_sqlserver_code(8115)
}

fn text(value: &Value) -> SqlResult<Option<String>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(text.clone()),
        other => Some(text_of(other, None, None)?),
    })
}

fn count(value: &Value, argument: usize, function: TextFunction) -> SqlResult<Option<i64>> {
    Ok(match value {
        Value::Null => None,
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value.as_integer(),
        Value::Boolean(flag) => Some(i64::from(*flag)),
        Value::Numeric(number) => Some(whole_of(number)?),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN).trunc();
            if !number.is_finite() || number.abs() > 9e18 {
                return Err(overflow());
            }
            Some(number as i64)
        }
        Value::Text(_) | Value::Bpchar(_) => crate::sqlserver_conversion::convert(
            value.clone(),
            SqlServerType::BigInt,
            None,
            None,
        )?
        .as_integer(),
        other => return Err(argument_type_invalid(other, argument, function)),
    })
}

fn whole_of(number: &Numeric) -> SqlResult<i64> {
    number
        .truncated(0)
        .ok()
        .and_then(|whole| whole.integral_value())
        .and_then(|whole| i64::try_from(whole).ok())
        .ok_or_else(overflow)
}

fn is_large(origin: TextOrigin) -> bool {
    origin.declared == Some(SqlType::Text)
}

fn short_text(mut text: String, origin: TextOrigin) -> String {
    if is_large(origin) {
        return text;
    }
    let cut = if origin.national {
        text.char_indices()
            .nth(SHORT_NATIONAL_CHARACTERS)
            .map(|(at, _)| at)
    } else if text.len() > SHORT_VALUE_BYTES {
        let mut at = SHORT_VALUE_BYTES;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        Some(at)
    } else {
        None
    };
    if let Some(at) = cut {
        text.truncate(at);
    }
    text
}

fn character_range(length: usize, start: i64, count: i64) -> (usize, usize) {
    let first = start.max(1);
    let end = start.saturating_add(count);
    if end <= first {
        return (0, 0);
    }
    let from = usize::try_from(first - 1).unwrap_or(usize::MAX).min(length);
    let to = usize::try_from(end - 1).unwrap_or(usize::MAX).min(length);
    (from, to.max(from))
}

fn characters_between(text: &str, from: usize, to: usize) -> String {
    text.chars().skip(from).take(to - from).collect()
}

fn substring(arguments: &[Value]) -> SqlResult<Value> {
    let function = TextFunction::Substring;
    let value = arguments.first().unwrap_or(&Value::Null);
    let start = count(arguments.get(1).unwrap_or(&Value::Null), 2, function)?;
    let length = count(arguments.get(2).unwrap_or(&Value::Null), 3, function)?;
    let (Some(start), Some(length)) = (start, length) else {
        return Ok(Value::Null);
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if length < 0 {
        return Err(invalid_length(function));
    }
    if let Value::Bytea(bytes) = value {
        let (from, to) = character_range(bytes.len(), start, length);
        return Ok(Value::Bytea(bytes[from..to].to_vec()));
    }
    let Some(text) = text(value)? else {
        return Ok(Value::Null);
    };
    let (from, to) = character_range(text.chars().count(), start, length);
    Ok(Value::Text(characters_between(&text, from, to)))
}

fn left_or_right(function: TextFunction, arguments: &[Value]) -> SqlResult<Value> {
    let Some(text) = text(arguments.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let Some(wanted) = count(arguments.get(1).unwrap_or(&Value::Null), 2, function)? else {
        return Ok(Value::Null);
    };
    if wanted < 0 {
        return Err(invalid_length(function));
    }
    let length = text.chars().count();
    let wanted = usize::try_from(wanted).unwrap_or(usize::MAX).min(length);
    Ok(Value::Text(if function == TextFunction::Left {
        characters_between(&text, 0, wanted)
    } else {
        characters_between(&text, length - wanted, length)
    }))
}

fn replace(arguments: &[Value], origin: TextOrigin) -> SqlResult<Value> {
    let mut parts = Vec::with_capacity(3);
    for argument in arguments.iter().take(3) {
        match text(argument)? {
            Some(part) => parts.push(part),
            None => return Ok(Value::Null),
        }
    }
    let [subject, pattern, replacement] = parts.as_slice() else {
        return Ok(Value::Null);
    };
    if pattern.is_empty() {
        return Ok(Value::Text(short_text(subject.clone(), origin)));
    }
    let found = subject.matches(pattern.as_str()).count();
    let grown = replacement.len().saturating_sub(pattern.len());
    within_limit(subject.len().saturating_add(found.saturating_mul(grown)))?;
    Ok(Value::Text(short_text(
        subject.replace(pattern.as_str(), replacement),
        origin,
    )))
}

fn stuff(arguments: &[Value], origin: TextOrigin) -> SqlResult<Value> {
    let function = TextFunction::Stuff;
    let subject = arguments.first().unwrap_or(&Value::Null);
    let start = count(arguments.get(1).unwrap_or(&Value::Null), 2, function)?;
    let length = count(arguments.get(2).unwrap_or(&Value::Null), 3, function)?;
    let (Some(start), Some(length)) = (start, length) else {
        return Ok(Value::Null);
    };
    if subject.is_null() || length < 0 || start < 1 {
        return Ok(Value::Null);
    }
    let inserted = arguments.get(3).unwrap_or(&Value::Null);
    if let Value::Bytea(bytes) = subject {
        let from = usize::try_from(start - 1).unwrap_or(usize::MAX);
        if from >= bytes.len() {
            return Ok(Value::Null);
        }
        let to = from
            .saturating_add(usize::try_from(length).unwrap_or(usize::MAX))
            .min(bytes.len());
        let middle = match inserted {
            Value::Null => Vec::new(),
            Value::Bytea(more) => more.clone(),
            other => text(other)?.unwrap_or_default().into_bytes(),
        };
        within_limit(bytes.len().saturating_add(middle.len()))?;
        let mut out = Vec::with_capacity(bytes.len() + middle.len());
        out.extend_from_slice(&bytes[..from]);
        out.extend_from_slice(&middle);
        out.extend_from_slice(&bytes[to..]);
        return Ok(Value::Bytea(out));
    }
    let Some(subject) = text(subject)? else {
        return Ok(Value::Null);
    };
    let characters = subject.chars().count();
    let from = usize::try_from(start - 1).unwrap_or(usize::MAX);
    if from >= characters {
        return Ok(Value::Null);
    }
    let to = from
        .saturating_add(usize::try_from(length).unwrap_or(usize::MAX))
        .min(characters);
    let middle = text(inserted)?.unwrap_or_default();
    within_limit(subject.len().saturating_add(middle.len()))?;
    let mut out = String::with_capacity(subject.len() + middle.len());
    out.extend(subject.chars().take(from));
    out.push_str(&middle);
    out.extend(subject.chars().skip(to));
    Ok(Value::Text(short_text(out, origin)))
}

fn replicate(arguments: &[Value], origin: TextOrigin) -> SqlResult<Value> {
    let function = TextFunction::Replicate;
    let subject = arguments.first().unwrap_or(&Value::Null);
    let Some(times) = count(arguments.get(1).unwrap_or(&Value::Null), 2, function)? else {
        return Ok(Value::Null);
    };
    if subject.is_null() || times < 0 {
        return Ok(Value::Null);
    }
    let times = usize::try_from(times).unwrap_or(usize::MAX);
    if let Value::Bytea(bytes) = subject {
        if bytes.is_empty() {
            return Ok(Value::Bytea(Vec::new()));
        }
        let kept = times.min(SHORT_VALUE_BYTES / bytes.len() + 1);
        let mut out = bytes.repeat(kept);
        out.truncate(SHORT_VALUE_BYTES);
        return Ok(Value::Bytea(out));
    }
    let Some(piece) = text(subject)? else {
        return Ok(Value::Null);
    };
    if piece.is_empty() {
        return Ok(Value::Text(String::new()));
    }
    let kept = if is_large(origin) {
        within_limit(piece.len().saturating_mul(times))?;
        times
    } else if origin.national {
        times.min(SHORT_NATIONAL_CHARACTERS / piece.chars().count() + 1)
    } else {
        times.min(SHORT_VALUE_BYTES / piece.len() + 1)
    };
    Ok(Value::Text(short_text(piece.repeat(kept), origin)))
}

fn space(arguments: &[Value]) -> SqlResult<Value> {
    let Some(count) = count(
        arguments.first().unwrap_or(&Value::Null),
        1,
        TextFunction::Space,
    )?
    else {
        return Ok(Value::Null);
    };
    if count < 0 {
        return Ok(Value::Null);
    }
    let count = usize::try_from(count)
        .unwrap_or(usize::MAX)
        .min(SHORT_VALUE_BYTES);
    Ok(Value::Text(" ".repeat(count)))
}

fn reverse(arguments: &[Value]) -> SqlResult<Value> {
    Ok(match text(arguments.first().unwrap_or(&Value::Null))? {
        None => Value::Null,
        Some(text) => Value::Text(text.chars().rev().collect()),
    })
}

fn trim(ends: TrimEnds, arguments: &[Value]) -> SqlResult<Value> {
    let Some(subject) = text(arguments.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let removed: Vec<char> = match arguments.get(1) {
        None => vec![' '],
        Some(characters) => match text(characters)? {
            None => return Ok(Value::Null),
            Some(characters) => characters.chars().collect(),
        },
    };
    let removable = |c: char| removed.contains(&c);
    let kept = match ends {
        TrimEnds::Both => subject.trim_matches(removable),
        TrimEnds::Leading => subject.trim_start_matches(removable),
        TrimEnds::Trailing => subject.trim_end_matches(removable),
    };
    Ok(Value::Text(kept.to_owned()))
}

fn joined<'v>(parts: impl Iterator<Item = &'v Value>, separator: &str) -> SqlResult<Value> {
    let mut out = String::new();
    let mut first = true;
    for part in parts {
        let Some(part) = text(part)? else {
            continue;
        };
        let separator = if first { "" } else { separator };
        within_limit(
            out.len()
                .saturating_add(separator.len())
                .saturating_add(part.len()),
        )?;
        out.push_str(separator);
        out.push_str(&part);
        first = false;
    }
    Ok(Value::Text(out))
}

fn concat(arguments: &[Value]) -> SqlResult<Value> {
    let mut out = String::new();
    for argument in arguments {
        if let Some(part) = text(argument)? {
            within_limit(out.len().saturating_add(part.len()))?;
            out.push_str(&part);
        }
    }
    Ok(Value::Text(out))
}

fn concat_ws(arguments: &[Value]) -> SqlResult<Value> {
    let Some((separator, rest)) = arguments.split_first() else {
        return Ok(Value::Text(String::new()));
    };
    let separator = text(separator)?.unwrap_or_default();
    joined(rest.iter(), &separator)
}

fn decimal_bytes(precision: u16) -> i32 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

fn scaled_bytes(scale: Option<u8>, small: i32) -> i32 {
    match scale.unwrap_or(7) {
        0..=2 => small,
        3 | 4 => small + 1,
        _ => small + 2,
    }
}

fn data_length(arguments: &[Value], origin: TextOrigin) -> SqlResult<Value> {
    let value = arguments.first().unwrap_or(&Value::Null);
    let bytes = match value {
        Value::Null => return Ok(Value::Null),
        Value::Text(text) | Value::Bpchar(text) if origin.national => {
            text.encode_utf16().count().saturating_mul(2)
        }
        Value::Text(text) | Value::Bpchar(text) => text.len(),
        Value::Bytea(bytes) => bytes.len(),
        Value::Boolean(_) => 1,
        Value::SmallInt(_) => 2,
        Value::Integer(_) | Value::Real(_) => 4,
        Value::BigInt(_) | Value::Double(_) => 8,
        Value::Uuid(_) => 16,
        Value::Date(_) => 3,
        Value::Numeric(_) => {
            let precision = match origin.declared {
                Some(SqlType::Numeric(Some(modifier))) => modifier.precision,
                _ => crate::sqlserver_numbers::value_decimal_type(value)
                    .map_or(38, |decimal| decimal.precision),
            };
            return Ok(Value::Integer(decimal_bytes(precision)));
        }
        Value::Time(_) => {
            let scale = match origin.declared {
                Some(SqlType::Time(scale)) => scale,
                _ => None,
            };
            return Ok(Value::Integer(scaled_bytes(scale, 3)));
        }
        Value::Timestamp(_) => {
            return Ok(Value::Integer(match origin.declared {
                Some(SqlType::Timestamp(Some(0))) => 4,
                Some(SqlType::Timestamp(Some(3))) => 8,
                Some(SqlType::Timestamp(scale)) => scaled_bytes(scale, 6),
                _ => 8,
            }));
        }
        Value::TimestampTz(_) => {
            let scale = match origin.declared {
                Some(SqlType::TimestampTz(scale)) => scale,
                _ => None,
            };
            return Ok(Value::Integer(scaled_bytes(scale, 8)));
        }
        other => text_of(other, None, None)?.len(),
    };
    Ok(Value::Integer(i32::try_from(bytes).unwrap_or(i32::MAX)))
}

fn first_character(arguments: &[Value]) -> SqlResult<Option<char>> {
    Ok(text(arguments.first().unwrap_or(&Value::Null))?.and_then(|text| text.chars().next()))
}

fn ascii(arguments: &[Value]) -> SqlResult<Value> {
    Ok(match first_character(arguments)? {
        None => Value::Null,
        Some(c) if u32::from(c) <= 255 => Value::Integer(u32::from(c) as i32),
        Some(_) => Value::Integer(UNKNOWN_CHARACTER),
    })
}

fn unicode(arguments: &[Value]) -> SqlResult<Value> {
    Ok(match first_character(arguments)? {
        None => Value::Null,
        Some(c) => Value::Integer(u32::from(c) as i32),
    })
}

fn character_of(function: TextFunction, arguments: &[Value]) -> SqlResult<Value> {
    let Some(code) = count(arguments.first().unwrap_or(&Value::Null), 1, function)? else {
        return Ok(Value::Null);
    };
    let most = if function == TextFunction::Char {
        255
    } else {
        0x10_FFFF
    };
    Ok(u32::try_from(code)
        .ok()
        .filter(|code| *code <= most)
        .and_then(char::from_u32)
        .map_or(Value::Null, |c| Value::Text(c.to_string())))
}

fn separator_not_one_character() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "Procedure expects parameter 'separator' of type 'nchar(1)/nvarchar(1)'.",
    )
    .with_sqlserver_code(214)
}

pub(crate) fn split_rows(
    values: &[Value],
    sink: &mut dyn FnMut(Value) -> SqlResult<()>,
) -> SqlResult<()> {
    let separator = text(values.get(1).unwrap_or(&Value::Null))?;
    let Some(separator) = separator else {
        return Ok(());
    };
    let mut characters = separator.chars();
    let (Some(separator), None) = (characters.next(), characters.next()) else {
        return Err(separator_not_one_character());
    };
    let Some(subject) = text(values.first().unwrap_or(&Value::Null))? else {
        return Ok(());
    };
    for piece in subject.split(separator) {
        sink(Value::Text(piece.to_owned()))?;
    }
    Ok(())
}

pub(crate) fn call(
    function: TextFunction,
    arguments: &[Value],
    origin: TextOrigin,
) -> SqlResult<Value> {
    match function {
        TextFunction::Substring => substring(arguments),
        TextFunction::Left | TextFunction::Right => left_or_right(function, arguments),
        TextFunction::Replace => replace(arguments, origin),
        TextFunction::Stuff => stuff(arguments, origin),
        TextFunction::Replicate => replicate(arguments, origin),
        TextFunction::Space => space(arguments),
        TextFunction::Reverse => reverse(arguments),
        TextFunction::Trim(ends) => trim(ends, arguments),
        TextFunction::Concat => concat(arguments),
        TextFunction::ConcatWs => concat_ws(arguments),
        TextFunction::DataLength => data_length(arguments, origin),
        TextFunction::Ascii => ascii(arguments),
        TextFunction::Unicode => unicode(arguments),
        TextFunction::Char | TextFunction::NChar => character_of(function, arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::{TextFunction, TextOrigin, TrimEnds, call};
    use crate::types::{SqlType, Value};

    const PLAIN: TextOrigin = TextOrigin {
        national: false,
        declared: None,
    };

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn run(function: TextFunction, arguments: &[Value]) -> String {
        run_from(function, arguments, PLAIN)
    }

    fn run_from(function: TextFunction, arguments: &[Value], origin: TextOrigin) -> String {
        match call(function, arguments, origin) {
            Ok(Value::Null) => "NULL".to_owned(),
            Ok(Value::Text(text)) => text,
            Ok(Value::Integer(number)) => number.to_string(),
            Ok(Value::Bytea(bytes)) => format!("{bytes:?}"),
            Ok(other) => format!("{other:?}"),
            Err(error) => format!("error {}", error.vendor_code().unwrap_or(0)),
        }
    }

    #[test]
    fn substring_counts_from_one_and_clips_before_the_start() {
        let int = Value::Integer;
        assert_eq!(
            run(TextFunction::Substring, &[text("abcdef"), int(2), int(3)]),
            "bcd"
        );
        assert_eq!(
            run(TextFunction::Substring, &[text("abcdef"), int(0), int(3)]),
            "ab"
        );
        assert_eq!(
            run(TextFunction::Substring, &[text("abcdef"), int(-5), int(3)]),
            ""
        );
        assert_eq!(
            run(TextFunction::Substring, &[text("é😀z"), int(2), int(5)]),
            "😀z"
        );
        assert_eq!(
            run(TextFunction::Substring, &[text("abc"), int(1), int(-1)]),
            "error 537"
        );
        assert_eq!(
            run(
                TextFunction::Substring,
                &[Value::Bytea(vec![1, 2, 3]), int(2), int(1)]
            ),
            "[2]"
        );
        assert_eq!(
            run(
                TextFunction::Substring,
                &[text("abc"), int(i32::MAX), int(i32::MAX)]
            ),
            ""
        );
    }

    #[test]
    fn left_right_and_reverse_work_on_characters() {
        let int = Value::Integer;
        assert_eq!(run(TextFunction::Left, &[text("abcdef"), int(2)]), "ab");
        assert_eq!(run(TextFunction::Right, &[text("abcdef"), int(2)]), "ef");
        assert_eq!(run(TextFunction::Right, &[text("ab"), int(9)]), "ab");
        assert_eq!(run(TextFunction::Right, &[text("ab"), int(-1)]), "error 536");
        assert_eq!(run(TextFunction::Left, &[text("ab"), int(-1)]), "error 537");
        assert_eq!(run(TextFunction::Reverse, &[text("é😀z")]), "z😀é");
    }

    #[test]
    fn replace_stuff_and_trim_follow_the_reference() {
        assert_eq!(
            run(
                TextFunction::Replace,
                &[text("abcdefghicde"), text("cde"), text("xxx")]
            ),
            "abxxxfghixxx"
        );
        assert_eq!(
            run(TextFunction::Replace, &[text("abc"), text(""), text("x")]),
            "abc"
        );
        assert_eq!(
            run(
                TextFunction::Replace,
                &[text("abc"), Value::Null, text("x")]
            ),
            "NULL"
        );
        let int = Value::Integer;
        assert_eq!(
            run(
                TextFunction::Stuff,
                &[text("abcdef"), int(2), int(3), text("ijklmn")]
            ),
            "aijklmnef"
        );
        assert_eq!(
            run(
                TextFunction::Stuff,
                &[text("abc"), int(4), int(1), text("x")]
            ),
            "NULL"
        );
        assert_eq!(
            run(
                TextFunction::Stuff,
                &[text("abc"), int(2), int(9), Value::Null]
            ),
            "a"
        );
        assert_eq!(
            run(TextFunction::Trim(TrimEnds::Both), &[text("   test    ")]),
            "test"
        );
        assert_eq!(
            run(
                TextFunction::Trim(TrimEnds::Both),
                &[text(".,! test    ."), text(".,! ")]
            ),
            "test"
        );
        assert_eq!(
            run(TextFunction::Trim(TrimEnds::Leading), &[text("  a  ")]),
            "a  "
        );
        assert_eq!(
            run(TextFunction::Trim(TrimEnds::Trailing), &[text("  a  ")]),
            "  a"
        );
    }

    #[test]
    fn replicate_and_space_stop_at_eight_thousand_bytes_unless_the_value_is_large() {
        let int = Value::Integer;
        assert_eq!(run(TextFunction::Replicate, &[text("ab"), int(3)]), "ababab");
        assert_eq!(run(TextFunction::Replicate, &[text("ab"), int(-1)]), "NULL");
        let short = run(TextFunction::Replicate, &[text("ab"), int(10_000)]);
        assert_eq!(short.len(), 8000);
        let large = TextOrigin {
            national: false,
            declared: Some(SqlType::Text),
        };
        let long = run_from(TextFunction::Replicate, &[text("ab"), int(10_000)], large);
        assert_eq!(long.len(), 20_000);
        let national = TextOrigin {
            national: true,
            declared: None,
        };
        let cut = run_from(TextFunction::Replicate, &[text("é"), int(10_000)], national);
        assert_eq!(cut.chars().count(), 4000);
        assert_eq!(
            run_from(
                TextFunction::Replicate,
                &[text("ab"), int(i32::MAX)],
                large
            ),
            "error 7119"
        );
        assert_eq!(run(TextFunction::Space, &[int(3)]), "   ");
        assert_eq!(run(TextFunction::Space, &[int(i32::MAX)]).len(), 8000);
        assert_eq!(run(TextFunction::Space, &[int(-1)]), "NULL");
    }

    #[test]
    fn concat_skips_nulls_and_concat_ws_skips_them_with_their_separator() {
        assert_eq!(
            run(
                TextFunction::Concat,
                &[text("a"), Value::Null, Value::Integer(1), Value::Boolean(true)]
            ),
            "a11"
        );
        assert_eq!(
            run(
                TextFunction::ConcatWs,
                &[text(","), text("a"), Value::Null, text("b")]
            ),
            "a,b"
        );
        assert_eq!(
            run(TextFunction::ConcatWs, &[text(","), Value::Null, Value::Null]),
            ""
        );
    }

    #[test]
    fn datalength_counts_bytes_of_each_type() {
        let national = TextOrigin {
            national: true,
            declared: None,
        };
        assert_eq!(run(TextFunction::DataLength, &[text("abc")]), "3");
        assert_eq!(run_from(TextFunction::DataLength, &[text("abc")], national), "6");
        assert_eq!(run(TextFunction::DataLength, &[Value::Integer(1)]), "4");
        assert_eq!(run(TextFunction::DataLength, &[Value::BigInt(1)]), "8");
        assert_eq!(run(TextFunction::DataLength, &[Value::Date(0)]), "3");
        assert_eq!(run(TextFunction::DataLength, &[Value::Null]), "NULL");
    }

    #[test]
    fn string_split_returns_every_piece_and_wants_a_one_character_separator() {
        let mut pieces = Vec::new();
        let split = super::split_rows(&[text("a,,b"), text(",")], &mut |value| {
            pieces.push(value);
            Ok(())
        });
        assert!(split.is_ok());
        assert_eq!(pieces, vec![text("a"), text(""), text("b")]);
        let wide = super::split_rows(&[text("a"), text("--")], &mut |_| Ok(()));
        assert_eq!(wide.err().and_then(|error| error.vendor_code()), Some(214));
        let mut none = 0;
        let empty = super::split_rows(&[Value::Null, text(",")], &mut |_| {
            none += 1;
            Ok(())
        });
        assert!(empty.is_ok());
        assert_eq!(none, 0);
    }

    #[test]
    fn character_codes_go_both_ways() {
        assert_eq!(run(TextFunction::Ascii, &[text("A")]), "65");
        assert_eq!(run(TextFunction::Ascii, &[text("")]), "NULL");
        assert_eq!(run(TextFunction::Char, &[Value::Integer(65)]), "A");
        assert_eq!(run(TextFunction::Char, &[Value::Integer(256)]), "NULL");
        assert_eq!(run(TextFunction::Unicode, &[text("😀")]), "128512");
        assert_eq!(run(TextFunction::NChar, &[Value::Integer(0xD800)]), "NULL");
    }
}
