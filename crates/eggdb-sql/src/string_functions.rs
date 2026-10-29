use crate::ast::Expr;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::types::{SqlType, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ends {
    Leading,
    Trailing,
    Both,
}

fn text_of(value: &Value) -> Option<&str> {
    value.character_text()
}

struct CharacterSet {
    ascii: u128,
    others: Vec<char>,
}

impl CharacterSet {
    fn of(characters: &str) -> Self {
        let mut ascii = 0u128;
        let mut others = Vec::new();
        for character in characters.chars() {
            if character.is_ascii() {
                ascii |= 1u128 << u32::from(character);
            } else {
                others.push(character);
            }
        }
        others.sort_unstable();
        others.dedup();
        Self { ascii, others }
    }

    fn contains(&self, character: char) -> bool {
        if character.is_ascii() {
            return self.ascii & (1u128 << u32::from(character)) != 0;
        }
        self.others.binary_search(&character).is_ok()
    }
}

fn trimmed(text: &str, characters: &str, ends: Ends) -> String {
    let set = CharacterSet::of(characters);
    let in_set = |character: char| set.contains(character);
    let text = if ends != Ends::Trailing {
        text.trim_start_matches(in_set)
    } else {
        text
    };
    let text = if ends != Ends::Leading {
        text.trim_end_matches(in_set)
    } else {
        text
    };
    text.to_owned()
}

pub(crate) const FUNCTIONS: [&str; 23] = [
    "split_part",
    "concat",
    "concat_ws",
    "replace",
    "strpos",
    "position",
    "left",
    "right",
    "lpad",
    "rpad",
    "repeat",
    "initcap",
    "translate",
    "reverse",
    "chr",
    "ascii",
    "md5",
    "starts_with",
    "format",
    "overlay",
    "substr",
    "quote_literal",
    "quote_nullable",
];

const MAX_TEXT_BYTES: usize = (1 << 30) - 1;

fn too_long() -> SqlError {
    SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "requested length too large")
}

fn checked_length(bytes: usize) -> SqlResult<()> {
    if bytes > MAX_TEXT_BYTES {
        return Err(too_long());
    }
    crate::memory::check_value_bytes(bytes)
}

fn output_text(value: &Value) -> Option<String> {
    value.to_text()
}

fn concat(values: &[Value]) -> SqlResult<Value> {
    let mut joined = String::new();
    for text in values.iter().filter_map(output_text) {
        checked_length(joined.len().saturating_add(text.len()))?;
        joined.push_str(&text);
    }
    Ok(Value::Text(joined))
}

fn concat_ws(values: &[Value]) -> SqlResult<Value> {
    let Some((separator, rest)) = values.split_first() else {
        return Ok(Value::Null);
    };
    let Some(separator) = output_text(separator) else {
        return Ok(Value::Null);
    };
    let mut joined = String::new();
    let mut first = true;
    for text in rest.iter().filter_map(output_text) {
        let added = if first { 0 } else { separator.len() };
        checked_length(
            joined
                .len()
                .saturating_add(added)
                .saturating_add(text.len()),
        )?;
        if !first {
            joined.push_str(&separator);
        }
        joined.push_str(&text);
        first = false;
    }
    Ok(Value::Text(joined))
}

pub(crate) fn quote_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 3);
    if text.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for character in text.chars() {
        match character {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

fn format_error(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

struct FormatCursor<'a> {
    chars: Vec<char>,
    at: usize,
    arguments: &'a [Value],
    next: usize,
}

impl<'a> FormatCursor<'a> {
    fn number(&mut self) -> Option<usize> {
        let start = self.at;
        let mut value: usize = 0;
        while let Some(digit) = self.chars.get(self.at).and_then(|c| c.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(digit as usize);
            self.at += 1;
        }
        (self.at > start).then_some(value)
    }

    fn argument(&mut self, position: Option<usize>) -> SqlResult<&'a Value> {
        let index = match position {
            Some(0) => {
                return Err(format_error(
                    "format specifies argument 0, but arguments are numbered from 1",
                ));
            }
            Some(position) => position - 1,
            None => self.next,
        };
        self.next = index + 1;
        self.arguments
            .get(index)
            .ok_or_else(|| format_error("too few arguments for format()"))
    }

    fn position(&mut self) -> Option<usize> {
        let start = self.at;
        match self.number() {
            Some(number) if self.chars.get(self.at) == Some(&'$') => {
                self.at += 1;
                Some(number)
            }
            _ => {
                self.at = start;
                None
            }
        }
    }

    fn width(&mut self) -> SqlResult<Option<(usize, bool)>> {
        if self.chars.get(self.at) == Some(&'*') {
            self.at += 1;
            let position = self.position();
            let value = self.argument(position)?;
            return Ok(match value {
                Value::Null => None,
                other => {
                    let number = other.as_integer().ok_or_else(|| {
                        format_error("width argument of format() must be an integer")
                    })?;
                    let width = usize::try_from(number.unsigned_abs()).unwrap_or(usize::MAX);
                    Some((width, number < 0))
                }
            });
        }
        Ok(self.number().map(|width| (width, false)))
    }
}

fn padded(out: &mut String, text: &str, width: Option<(usize, bool)>, left: bool) -> SqlResult<()> {
    let Some((width, negative)) = width else {
        out.push_str(text);
        return Ok(());
    };
    let length = text.chars().count();
    let fill = width.saturating_sub(length);
    checked_length(out.len().saturating_add(text.len()).saturating_add(fill))?;
    if left || negative {
        out.push_str(text);
        out.extend(std::iter::repeat_n(' ', fill));
    } else {
        out.extend(std::iter::repeat_n(' ', fill));
        out.push_str(text);
    }
    Ok(())
}

fn format_text(values: &[Value]) -> SqlResult<Value> {
    let Some((template, arguments)) = values.split_first() else {
        return Ok(Value::Null);
    };
    let Some(template) = output_text(template) else {
        return Ok(Value::Null);
    };
    let mut cursor = FormatCursor {
        chars: template.chars().collect(),
        at: 0,
        arguments,
        next: 0,
    };
    let mut out = String::new();
    while let Some(&character) = cursor.chars.get(cursor.at) {
        cursor.at += 1;
        if character != '%' {
            out.push(character);
            continue;
        }
        if cursor.chars.get(cursor.at) == Some(&'%') {
            cursor.at += 1;
            out.push('%');
            continue;
        }
        let position = cursor.position();
        let mut left = false;
        while cursor.chars.get(cursor.at) == Some(&'-') {
            left = true;
            cursor.at += 1;
        }
        let width = cursor.width()?;
        let Some(&kind) = cursor.chars.get(cursor.at) else {
            return Err(format_error("unterminated format() type specifier")
                .with_hint("For a single \"%\" use \"%%\"."));
        };
        cursor.at += 1;
        if !matches!(kind, 's' | 'I' | 'L') {
            return Err(
                format_error(format!("unrecognized format() type specifier \"{kind}\""))
                    .with_hint("For a single \"%\" use \"%%\"."),
            );
        }
        let value = cursor.argument(position)?;
        let shown = match (kind, value) {
            ('s', value) => output_text(value).unwrap_or_default(),
            ('I', Value::Null) => {
                return Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "null values cannot be formatted as an SQL identifier",
                ));
            }
            ('I', value) => crate::pg_keywords::quote_ident(&output_text(value).unwrap_or_default()),
            (_, Value::Null) => "NULL".to_owned(),
            (_, value) => quote_literal(&output_text(value).unwrap_or_default()),
        };
        padded(&mut out, &shown, width, left)?;
    }
    Ok(Value::Text(out))
}

fn count_of(value: &Value) -> Option<i64> {
    value.as_integer()
}

fn character_count(count: i64, length: usize) -> usize {
    usize::try_from(count.max(0)).unwrap_or(usize::MAX).min(length)
}

fn kept_end(text: &str, count: i64, from_left: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let length = chars.len();
    let taken = if count >= 0 {
        character_count(count, length)
    } else {
        length.saturating_sub(character_count(count.saturating_neg(), length))
    };
    if from_left {
        chars[..taken].iter().collect()
    } else {
        chars[length - taken..].iter().collect()
    }
}

fn padded_to(text: &str, length: i64, fill: &str, on_left: bool) -> SqlResult<Value> {
    let wanted = usize::try_from(length.max(0)).unwrap_or(usize::MAX);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() >= wanted {
        return Ok(Value::Text(chars[..wanted].iter().collect()));
    }
    let widest = fill.chars().map(char::len_utf8).max().unwrap_or(1).max(4);
    checked_length(wanted.saturating_mul(widest))?;
    if fill.is_empty() {
        return Ok(Value::Text(text.to_owned()));
    }
    let padding: String = fill.chars().cycle().take(wanted - chars.len()).collect();
    Ok(Value::Text(if on_left {
        padding + text
    } else {
        text.to_owned() + &padding
    }))
}

fn repeated(text: &str, count: i64) -> SqlResult<Value> {
    let count = usize::try_from(count.max(0)).unwrap_or(usize::MAX);
    checked_length(text.len().saturating_mul(count))?;
    Ok(Value::Text(text.repeat(count)))
}

fn initcap(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_word = false;
    for character in text.chars() {
        if in_word {
            out.extend(character.to_lowercase());
        } else {
            out.extend(character.to_uppercase());
        }
        in_word = character.is_alphanumeric();
    }
    out
}

fn translated(text: &str, from: &str, to: &str) -> String {
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    text.chars()
        .filter_map(
            |character| match from.iter().position(|found| *found == character) {
                Some(at) => to.get(at).copied(),
                None => Some(character),
            },
        )
        .collect()
}

fn character_of(code: i64) -> SqlResult<Value> {
    if code == 0 {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "null character not permitted",
        ));
    }
    let Ok(code) = u32::try_from(code) else {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {code}"),
        ));
    };
    if code > 0x10FFFF {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {code}"),
        ));
    }
    char::from_u32(code)
        .map(|character| Value::Text(character.to_string()))
        .ok_or_else(|| {
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("requested character not valid for encoding: {code}"),
            )
        })
}

fn split_part(text: &str, delimiter: &str, field: i64) -> SqlResult<Value> {
    if field == 0 {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "field position must not be zero",
        ));
    }
    if delimiter.is_empty() {
        return Ok(Value::Text(if field == 1 || field == -1 {
            text.to_owned()
        } else {
            String::new()
        }));
    }
    let parts: Vec<&str> = text.split(delimiter).collect();
    let index = if field > 0 {
        usize::try_from(field - 1).ok()
    } else {
        usize::try_from(field.saturating_neg())
            .ok()
            .and_then(|back| parts.len().checked_sub(back))
    };
    Ok(Value::Text(
        index
            .and_then(|index| parts.get(index))
            .map_or_else(String::new, |part| (*part).to_owned()),
    ))
}

fn position_of(text: &str, wanted: &str) -> SqlResult<Value> {
    let position = match text.find(wanted) {
        Some(byte) => text[..byte].chars().count() as i64 + 1,
        None => 0,
    };
    crate::types::integer_value(position, SqlType::Integer)
}

fn replaced(text: &str, from: &str, to: &str) -> SqlResult<Value> {
    if from.is_empty() {
        return Ok(Value::Text(text.to_owned()));
    }
    let count = text.matches(from).count();
    let grown = to.len().saturating_sub(from.len()).saturating_mul(count);
    checked_length(text.len().saturating_add(grown))?;
    Ok(Value::Text(text.replace(from, to)))
}

fn overlaid(text: &str, placed: &str, start: i64, count: Option<i64>) -> SqlResult<Value> {
    let chars: Vec<char> = text.chars().collect();
    let count = count.unwrap_or(placed.chars().count() as i64);
    if start < 1 || count < 0 {
        return Err(SqlError::new(
            SqlState::SUBSTRING_ERROR,
            "negative substring length not allowed",
        ));
    }
    let head = usize::try_from(start - 1)
        .unwrap_or(usize::MAX)
        .min(chars.len());
    let tail = usize::try_from(start.saturating_add(count) - 1)
        .unwrap_or(usize::MAX)
        .min(chars.len());
    let mut out: String = chars[..head].iter().collect();
    out.push_str(placed);
    out.extend(&chars[tail..]);
    checked_length(out.len())?;
    Ok(Value::Text(out))
}

fn text_function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "concat" => return Some(concat(values)),
        "concat_ws" => return Some(concat_ws(values)),
        "format" => return Some(format_text(values)),
        "quote_nullable" => {
            return Some(Ok(match values {
                [Value::Null] => Value::Text("NULL".to_owned()),
                [value] => Value::Text(quote_literal(&output_text(value)?)),
                _ => return None,
            }));
        }
        "substr" => return crate::bytea::function("substring", values),
        _ => {}
    }
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    if name == "md5"
        && let [Value::Bytea(bytes)] = values
    {
        return Some(Ok(Value::Text(crate::md5::hex_digest(bytes))));
    }
    let text = |at: usize| values.get(at).and_then(Value::character_text);
    let number = |at: usize| values.get(at).and_then(count_of);
    Some(match (name, values.len()) {
        ("split_part", 3) => split_part(text(0)?, text(1)?, number(2)?),
        ("replace", 3) => replaced(text(0)?, text(1)?, text(2)?),
        ("strpos" | "position", 2) => position_of(text(0)?, text(1)?),
        ("left", 2) => Ok(Value::Text(kept_end(text(0)?, number(1)?, true))),
        ("right", 2) => Ok(Value::Text(kept_end(text(0)?, number(1)?, false))),
        ("lpad" | "rpad", 2) => padded_to(text(0)?, number(1)?, " ", name == "lpad"),
        ("lpad" | "rpad", 3) => padded_to(text(0)?, number(1)?, text(2)?, name == "lpad"),
        ("repeat", 2) => repeated(text(0)?, number(1)?),
        ("initcap", 1) => Ok(Value::Text(initcap(text(0)?))),
        ("translate", 3) => Ok(Value::Text(translated(text(0)?, text(1)?, text(2)?))),
        ("reverse", 1) => Ok(Value::Text(text(0)?.chars().rev().collect())),
        ("chr", 1) => character_of(number(0)?),
        ("ascii", 1) => Ok(Value::Integer(
            text(0)?.chars().next().map_or(0, |first| first as i32),
        )),
        ("md5", 1) => Ok(Value::Text(crate::md5::hex_digest(text(0)?.as_bytes()))),
        ("starts_with", 2) => Ok(Value::Boolean(text(0)?.starts_with(text(1)?))),
        ("overlay", 3) => overlaid(text(0)?, text(1)?, number(2)?, None),
        ("overlay", 4) => overlaid(text(0)?, text(1)?, number(2)?, Some(number(3)?)),
        ("quote_literal", 1) => Ok(Value::Text(quote_literal(&output_text(&values[0])?))),
        _ => return None,
    })
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if let Some(result) = text_function(name, values) {
        return Some(result);
    }
    let ends = match name {
        "btrim" => Ends::Both,
        "ltrim" => Ends::Leading,
        "rtrim" => Ends::Trailing,
        "quote_ident" => {
            return match values {
                [Value::Null] => Some(Ok(Value::Null)),
                [value] => {
                    let text = text_of(value)?;
                    Some(
                        crate::memory::check_value_bytes(
                            text.len().saturating_mul(2).saturating_add(2),
                        )
                        .map(|()| Value::Text(crate::pg_keywords::quote_ident(text))),
                    )
                }
                _ => None,
            };
        }
        _ => return None,
    };
    match values {
        [Value::Null] | [Value::Null, _] | [_, Value::Null] => Some(Ok(Value::Null)),
        [value] => Some(Ok(Value::Text(trimmed(text_of(value)?, " ", ends)))),
        [value, characters] => Some(Ok(Value::Text(trimmed(
            text_of(value)?,
            text_of(characters)?,
            ends,
        )))),
        _ => None,
    }
}

pub(crate) fn collated(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [argument] = arguments else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "COLLATE is missing its operand",
        ));
    };
    let value = evaluator.eval(argument, Some(SqlType::Text))?;
    let sql_type = value
        .sql_type()
        .or_else(|| crate::typing::static_type(argument, evaluator.scope));
    if let Some(sql_type) = sql_type
        && !sql_type.is_character()
    {
        return Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("collations are not supported by type {}", sql_type.name()),
        )
        .at(argument.position));
    }
    Ok(value)
}
