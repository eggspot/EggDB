use crate::arrays::{ArrayElement, ArrayValue};
use crate::ast::{Expr, ExprKind, Literal};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::regex::{OptionLetter, Options, Regex, Span, compiled_with};
use crate::types::{SqlType, Value};

pub(crate) const FUNCTIONS: [&str; 9] = [
    "regexp_like",
    "regexp_match",
    "regexp_replace",
    "regexp_split_to_array",
    "regexp_count",
    "regexp_instr",
    "regexp_substr",
    "similar_to_escape",
    "similar_substring_pattern",
];

struct Flags {
    options: Options,
    global: bool,
}

fn invalid_parameter(name: &str, value: i64) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{name}\": {value}"),
    )
}

fn flags_of(text: &str) -> SqlResult<Flags> {
    let mut options = Options::default();
    let mut global = false;
    for letter in text.chars() {
        if letter == 'g' {
            global = true;
            continue;
        }
        if let OptionLetter::Unknown = options.apply(letter)? {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid regular expression option: \"{letter}\""),
            ));
        }
    }
    Ok(Flags { options, global })
}

fn single_match_flags(function: &str, text: &str, hint: Option<&str>) -> SqlResult<Options> {
    let flags = flags_of(text)?;
    if flags.global {
        let error = SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{function}() does not support the \"global\" option"),
        );
        return Err(match hint {
            Some(hint) => error.with_hint(hint),
            None => error,
        });
    }
    Ok(flags.options)
}

fn chars_of(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn piece(chars: &[char], span: Span) -> String {
    chars.get(span.0..span.1).unwrap_or(&[]).iter().collect()
}

fn whole(spans: &[Option<Span>]) -> Option<Span> {
    spans.first().copied().flatten()
}

fn each_match(
    regex: &Regex,
    chars: &[char],
    from: usize,
    global: bool,
    each: &mut dyn FnMut(&[Option<Span>]) -> SqlResult<bool>,
) -> SqlResult<()> {
    let mut finder = regex.finder(chars);
    let mut start = from;
    while start <= chars.len() {
        let Some(spans) = finder.find(start)? else {
            break;
        };
        let Some((begin, end)) = whole(&spans) else {
            break;
        };
        if !each(&spans)? || !global {
            break;
        }
        start = if end == begin { end + 1 } else { end };
    }
    Ok(())
}

fn text_array(items: Vec<Value>) -> SqlResult<Value> {
    let bytes = items
        .iter()
        .map(crate::memory::held_bytes)
        .fold(0usize, usize::saturating_add);
    crate::memory::check_value_bytes(bytes)?;
    Ok(Value::Array(Box::new(ArrayValue::new(
        ArrayElement::Text,
        items,
    ))))
}

fn captured(regex: &Regex, chars: &[char], spans: &[Option<Span>]) -> SqlResult<Value> {
    if regex.groups() == 0 {
        return text_array(vec![Value::Text(whole(spans).map_or_else(String::new, |span| piece(chars, span)))]);
    }
    let items = spans
        .iter()
        .skip(1)
        .map(|span| span.map_or(Value::Null, |span| Value::Text(piece(chars, span))))
        .collect();
    text_array(items)
}

fn texts(values: &[Value]) -> Option<Vec<&str>> {
    values.iter().map(Value::character_text).collect()
}

fn position_argument(value: &Value, name: &str) -> Option<SqlResult<usize>> {
    let number = value.as_integer()?;
    Some(if number < 1 {
        Err(invalid_parameter(name, number))
    } else {
        usize::try_from(number - 1).map_err(|_| invalid_parameter(name, number))
    })
}

fn three_texts(values: &[Value]) -> Option<(&str, &str, &str)> {
    let arguments = texts(values)?;
    Some(match arguments.as_slice() {
        [text, pattern] => (*text, *pattern, ""),
        [text, pattern, flags] => (*text, *pattern, *flags),
        _ => return None,
    })
}

fn regexp_like(values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, pattern, flags) = three_texts(values)?;
    Some(like_of(text, pattern, flags))
}

fn like_of(text: &str, pattern: &str, flags: &str) -> SqlResult<Value> {
    let options = single_match_flags("regexp_like", flags, None)?;
    let regex = compiled_with(pattern, options)?;
    let chars = chars_of(text);
    Ok(Value::Boolean(regex.finder(&chars).find(0)?.is_some()))
}

fn regexp_match(values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, pattern, flags) = three_texts(values)?;
    Some(match_of(text, pattern, flags))
}

fn match_of(text: &str, pattern: &str, flags: &str) -> SqlResult<Value> {
    let options = single_match_flags(
        "regexp_match",
        flags,
        Some("Use the regexp_matches function instead."),
    )?;
    let regex = compiled_with(pattern, options)?;
    let chars = chars_of(text);
    match regex.finder(&chars).find(0)? {
        Some(spans) => captured(&regex, &chars, &spans),
        None => Ok(Value::Null),
    }
}

pub(crate) fn matches_rows(
    values: &[Value],
    sink: &mut dyn FnMut(Value) -> SqlResult<()>,
) -> SqlResult<()> {
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let Some((text, pattern, flags)) = three_texts(values) else {
        return Ok(());
    };
    let flags = flags_of(flags)?;
    let regex = compiled_with(pattern, flags.options)?;
    let chars = chars_of(text);
    each_match(&regex, &chars, 0, flags.global, &mut |spans| {
        sink(captured(&regex, &chars, spans)?)?;
        Ok(true)
    })
}

fn split_pieces(text: &str, pattern: &str, flags: &str, function: &str) -> SqlResult<Vec<Value>> {
    let options = single_match_flags(function, flags, None)?;
    let regex = compiled_with(pattern, options)?;
    let chars = chars_of(text);
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    let mut previous_end = 0;
    let mut held = 0usize;
    each_match(&regex, &chars, 0, true, &mut |spans| {
        let Some((begin, end)) = whole(spans) else {
            return Ok(false);
        };
        if begin == end && (begin == 0 || begin == chars.len() || begin == previous_end) {
            return Ok(true);
        }
        let found = piece(&chars, (piece_start, begin));
        held = held.saturating_add(found.len());
        crate::memory::check_value_bytes(held)?;
        pieces.push(Value::Text(found));
        piece_start = end;
        previous_end = end;
        Ok(true)
    })?;
    pieces.push(Value::Text(piece(&chars, (piece_start, chars.len()))));
    Ok(pieces)
}

pub(crate) fn split_rows(
    values: &[Value],
    sink: &mut dyn FnMut(Value) -> SqlResult<()>,
) -> SqlResult<()> {
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let Some((text, pattern, flags)) = three_texts(values) else {
        return Ok(());
    };
    for item in split_pieces(text, pattern, flags, "regexp_split_to_table")? {
        sink(item)?;
    }
    Ok(())
}

fn split_to_array(values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, pattern, flags) = three_texts(values)?;
    Some(
        split_pieces(text, pattern, flags, "regexp_split_to_array").and_then(text_array),
    )
}

fn replacement(
    out: &mut String,
    template: &[char],
    chars: &[char],
    spans: &[Option<Span>],
) -> SqlResult<()> {
    let mut at = 0;
    while let Some(&character) = template.get(at) {
        at += 1;
        if character != '\\' {
            out.push(character);
            continue;
        }
        match template.get(at) {
            Some(digit @ '1'..='9') => {
                at += 1;
                let group = digit.to_digit(10).map_or(0, |digit| digit as usize);
                if let Some(Some(span)) = spans.get(group) {
                    out.push_str(&piece(chars, *span));
                }
            }
            Some('&') => {
                at += 1;
                if let Some(span) = whole(spans) {
                    out.push_str(&piece(chars, span));
                }
            }
            Some('\\') => {
                at += 1;
                out.push('\\');
            }
            _ => out.push('\\'),
        }
        crate::memory::check_value_bytes(out.len())?;
    }
    crate::memory::check_value_bytes(out.len())
}

struct ReplaceCall<'v> {
    text: &'v str,
    pattern: &'v str,
    template: &'v str,
    start: usize,
    occurrence: Option<i64>,
    flags: &'v str,
}

fn replace_call(values: &[Value]) -> Option<SqlResult<ReplaceCall<'_>>> {
    let text = |at: usize| values.get(at).and_then(Value::character_text);
    let (text_value, pattern, template) = (text(0)?, text(1)?, text(2)?);
    let mut call = ReplaceCall {
        text: text_value,
        pattern,
        template,
        start: 0,
        occurrence: None,
        flags: "",
    };
    match values.get(3..).unwrap_or(&[]) {
        [] => {}
        [flags] if flags.character_text().is_some() => call.flags = flags.character_text()?,
        [start, rest @ ..] if start.as_integer().is_some() && rest.len() <= 2 => {
            call.start = match position_argument(start, "start")? {
                Ok(start) => start,
                Err(error) => return Some(Err(error)),
            };
            if let Some(occurrence) = rest.first() {
                let number = occurrence.as_integer()?;
                if number < 0 {
                    return Some(Err(invalid_parameter("n", number)));
                }
                call.occurrence = Some(number);
            }
            if let Some(flags) = rest.get(1) {
                call.flags = flags.character_text()?;
            }
        }
        _ => return None,
    }
    Some(Ok(call))
}

fn regexp_replace(values: &[Value]) -> Option<SqlResult<Value>> {
    Some(replace_call(values)?.and_then(|call| replaced(&call)))
}

fn replaced(call: &ReplaceCall<'_>) -> SqlResult<Value> {
    let flags = flags_of(call.flags)?;
    let regex = compiled_with(call.pattern, flags.options)?;
    let chars = chars_of(call.text);
    let template = chars_of(call.template);
    let wanted = match call.occurrence {
        Some(0) => None,
        Some(number) => Some(number),
        None if flags.global => None,
        None => Some(1),
    };
    let mut out = String::with_capacity(call.text.len());
    let mut copied = 0;
    let mut seen = 0i64;
    let from = call.start.min(chars.len().saturating_add(1));
    each_match(&regex, &chars, from, true, &mut |spans| {
        let Some((begin, end)) = whole(spans) else {
            return Ok(false);
        };
        seen += 1;
        if wanted.is_some_and(|wanted| wanted != seen) {
            return Ok(true);
        }
        out.push_str(&piece(&chars, (copied, begin)));
        replacement(&mut out, &template, &chars, spans)?;
        copied = end;
        Ok(wanted.is_none())
    })?;
    out.push_str(&piece(&chars, (copied, chars.len())));
    crate::memory::check_value_bytes(out.len())?;
    Ok(Value::Text(out))
}

struct Search<'v> {
    text: &'v str,
    pattern: &'v str,
    start: usize,
    occurrence: i64,
    end_option: i64,
    flags: &'v str,
    group: i64,
}

fn search_arguments<'v>(
    values: &'v [Value],
    layout: &[&str],
) -> Option<SqlResult<Search<'v>>> {
    let mut search = Search {
        text: values.first()?.character_text()?,
        pattern: values.get(1)?.character_text()?,
        start: 0,
        occurrence: 1,
        end_option: 0,
        flags: "",
        group: 0,
    };
    if values.len() > layout.len() + 2 {
        return None;
    }
    for (value, name) in values.iter().skip(2).zip(layout) {
        match *name {
            "flags" => search.flags = value.character_text()?,
            "start" => {
                search.start = match position_argument(value, "start")? {
                    Ok(start) => start,
                    Err(error) => return Some(Err(error)),
                };
            }
            "n" => {
                let number = value.as_integer()?;
                if number < 1 {
                    return Some(Err(invalid_parameter("n", number)));
                }
                search.occurrence = number;
            }
            "endoption" => {
                let number = value.as_integer()?;
                if !(0..=1).contains(&number) {
                    return Some(Err(invalid_parameter("endoption", number)));
                }
                search.end_option = number;
            }
            _ => {
                let number = value.as_integer()?;
                if number < 0 {
                    return Some(Err(invalid_parameter("subexpr", number)));
                }
                search.group = number;
            }
        }
    }
    Some(Ok(search))
}

fn nth_match(
    function: &str,
    search: &Search<'_>,
) -> SqlResult<(Vec<char>, Option<Vec<Option<Span>>>)> {
    let options = single_match_flags(function, search.flags, None)?;
    let regex = compiled_with(search.pattern, options)?;
    let chars = chars_of(search.text);
    if search.start > chars.len() {
        return Ok((chars, None));
    }
    let mut seen = 0;
    let mut found = None;
    each_match(&regex, &chars, search.start, true, &mut |spans| {
        seen += 1;
        if seen == search.occurrence {
            found = Some(spans.to_vec());
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok((chars, found))
}

fn chosen_group(spans: &[Option<Span>], group: i64) -> Option<Span> {
    usize::try_from(group)
        .ok()
        .and_then(|group| spans.get(group).copied().flatten())
}

fn regexp_count(values: &[Value]) -> Option<SqlResult<Value>> {
    Some(search_arguments(values, &["start", "flags"])?.and_then(|search| counted(&search)))
}

fn counted(search: &Search<'_>) -> SqlResult<Value> {
    let options = single_match_flags("regexp_count", search.flags, None)?;
    let regex = compiled_with(search.pattern, options)?;
    let chars = chars_of(search.text);
    let mut count = 0i64;
    if search.start <= chars.len() {
        each_match(&regex, &chars, search.start, true, &mut |_| {
            count += 1;
            Ok(true)
        })?;
    }
    crate::types::integer_value(count, SqlType::Integer)
}

fn regexp_instr(values: &[Value]) -> Option<SqlResult<Value>> {
    let layout = ["start", "n", "endoption", "flags", "subexpr"];
    Some(search_arguments(values, &layout)?.and_then(|search| position_of(&search)))
}

fn position_of(search: &Search<'_>) -> SqlResult<Value> {
    let (_, found) = nth_match("regexp_instr", search)?;
    let position = found
        .as_deref()
        .and_then(|spans| chosen_group(spans, search.group))
        .map_or(0, |(begin, end)| {
            let at = if search.end_option == 1 { end } else { begin };
            at as i64 + 1
        });
    crate::types::integer_value(position, SqlType::Integer)
}

fn regexp_substr(values: &[Value]) -> Option<SqlResult<Value>> {
    let layout = ["start", "n", "flags", "subexpr"];
    Some(search_arguments(values, &layout)?.and_then(|search| substring_of(&search)))
}

fn substring_of(search: &Search<'_>) -> SqlResult<Value> {
    let (chars, found) = nth_match("regexp_substr", search)?;
    Ok(found
        .as_deref()
        .and_then(|spans| chosen_group(spans, search.group))
        .map_or(Value::Null, |span| Value::Text(piece(&chars, span))))
}

fn escape_character(value: Option<&Value>) -> SqlResult<Option<char>> {
    let Some(value) = value else {
        return Ok(Some('\\'));
    };
    let text = value.character_text().unwrap_or_default();
    let mut characters = text.chars();
    match (characters.next(), characters.next()) {
        (None, _) => Ok(None),
        (Some(only), None) => Ok(Some(only)),
        _ => Err(SqlError::new(
            SqlState::INVALID_ESCAPE_SEQUENCE,
            "invalid escape string",
        )
        .with_hint("Escape string must be empty or one character.")),
    }
}

fn separated_parts(pattern: &str, escape: Option<char>) -> SqlResult<Vec<String>> {
    let mut parts = vec![String::new()];
    let mut after_escape = false;
    let mut in_class = false;
    for character in pattern.chars() {
        if after_escape {
            after_escape = false;
            if character == '"' && !in_class {
                if parts.len() == 3 {
                    return Err(SqlError::new(
                        SqlState::INVALID_REGULAR_EXPRESSION,
                        "SQL regular expression may not contain more than two escape-double-quote separators",
                    ));
                }
                parts.push(String::new());
                continue;
            }
            if let (Some(part), Some(escape)) = (parts.last_mut(), escape) {
                part.push(escape);
                part.push(character);
            }
            continue;
        }
        if Some(character) == escape {
            after_escape = true;
            continue;
        }
        if in_class && character == ']' {
            in_class = false;
        } else if !in_class && character == '[' {
            in_class = true;
        }
        if let Some(part) = parts.last_mut() {
            part.push(character);
        }
    }
    if after_escape {
        return Err(SqlError::new(
            SqlState::INVALID_ESCAPE_SEQUENCE,
            "invalid escape string",
        )
        .with_hint("Escape string must be empty or one character."));
    }
    Ok(parts)
}

fn converted_part(part: &str, escape: Option<char>, lazy: bool) -> String {
    let mut out = String::with_capacity(part.len() + 8);
    let mut after_escape = false;
    let mut in_class = false;
    let mut class_start = false;
    let mut characters = part.chars().peekable();
    while let Some(character) = characters.next() {
        if after_escape {
            after_escape = false;
            out.push('\\');
            out.push(character);
            continue;
        }
        if Some(character) == escape {
            after_escape = true;
            continue;
        }
        if in_class {
            if character == '\\' {
                out.push_str("\\\\");
            } else {
                out.push(character);
            }
            if character == ']' && !class_start {
                in_class = false;
            }
            class_start = class_start && character == '^';
            continue;
        }
        match character {
            '[' => {
                out.push('[');
                in_class = true;
                class_start = true;
            }
            '%' => out.push_str(if lazy { ".*?" } else { ".*" }),
            '_' => out.push('.'),
            '(' => out.push_str("(?:"),
            '\\' | '.' | '^' | '$' => {
                out.push('\\');
                out.push(character);
            }
            '*' | '+' | '?' | '}' => {
                out.push(character);
                if lazy && characters.peek() != Some(&'?') {
                    out.push('?');
                }
            }
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn similar_to_escape_text(pattern: &str, escape: Option<char>) -> SqlResult<String> {
    let parts = separated_parts(pattern, escape)?;
    let converted: Vec<String> = parts
        .iter()
        .map(|part| converted_part(part, escape, false))
        .collect();
    let mut out = String::from("^(?:");
    for (at, part) in converted.iter().enumerate() {
        match at {
            1 => out.push_str("){1,1}?("),
            2 => out.push_str("){1,1}(?:"),
            _ => {}
        }
        out.push_str(part);
    }
    out.push_str(")$");
    Ok(out)
}

fn substring_pattern(pattern: &str, escape: Option<char>) -> SqlResult<String> {
    let parts = separated_parts(pattern, escape)?;
    Ok(match parts.as_slice() {
        [middle] => format!("^({})$", converted_part(middle, escape, false)),
        [first, middle] => format!(
            "^(?:{})({})$",
            converted_part(first, escape, true),
            converted_part(middle, escape, false)
        ),
        [first, middle, last, ..] => format!(
            "^(?:{})({})(?:{})$",
            converted_part(first, escape, true),
            converted_part(middle, escape, false),
            converted_part(last, escape, false)
        ),
        [] => "^()$".to_owned(),
    })
}

fn similar_to_escape(values: &[Value]) -> Option<SqlResult<Value>> {
    let pattern = values.first()?.character_text()?;
    Some(
        escape_character(values.get(1))
            .and_then(|escape| similar_to_escape_text(pattern, escape))
            .map(Value::Text),
    )
}

fn similar_substring_pattern(values: &[Value]) -> Option<SqlResult<Value>> {
    let pattern = values.first()?.character_text()?;
    Some(
        escape_character(values.get(1))
            .and_then(|escape| substring_pattern(pattern, escape))
            .map(Value::Text),
    )
}

fn pattern_substring(text: &str, pattern: &str) -> SqlResult<Value> {
    let regex = compiled_with(pattern, Options::default())?;
    let chars = chars_of(text);
    let Some(spans) = regex.finder(&chars).find(0)? else {
        return Ok(Value::Null);
    };
    let span = if regex.groups() > 0 {
        spans.get(1).copied().flatten()
    } else {
        whole(&spans)
    };
    Ok(span.map_or(Value::Null, |span| Value::Text(piece(&chars, span))))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    match name {
        "regexp_like" => regexp_like(values),
        "regexp_match" => regexp_match(values),
        "regexp_replace" => regexp_replace(values),
        "regexp_split_to_array" => split_to_array(values),
        "regexp_count" => regexp_count(values),
        "regexp_instr" => regexp_instr(values),
        "regexp_substr" => regexp_substr(values),
        "similar_to_escape" => similar_to_escape(values),
        "similar_substring_pattern" => similar_substring_pattern(values),
        _ => None,
    }
}

fn is_pattern_argument(evaluator: &Evaluator<'_>, expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(Literal::String(_)) => true,
        _ => crate::typing::static_type(expr, evaluator.scope).is_some_and(SqlType::is_character),
    }
}

pub(crate) fn substring(
    evaluator: &Evaluator<'_>,
    arguments: &[Expr],
) -> Option<SqlResult<Value>> {
    let (text, pattern, escape) = match arguments {
        [text, pattern] => (text, pattern, None),
        [text, pattern, escape] => (text, pattern, Some(escape)),
        _ => return None,
    };
    if !is_pattern_argument(evaluator, pattern) {
        return None;
    }
    Some(evaluated_substring(evaluator, text, pattern, escape))
}

fn evaluated_substring(
    evaluator: &Evaluator<'_>,
    text: &Expr,
    pattern: &Expr,
    escape: Option<&Expr>,
) -> SqlResult<Value> {
    let text = evaluator.eval(text, Some(SqlType::Text))?;
    let pattern = evaluator.eval(pattern, Some(SqlType::Text))?;
    let escape = match escape {
        Some(escape) => Some(evaluator.eval(escape, Some(SqlType::Text))?),
        None => None,
    };
    if text.is_null() || pattern.is_null() || escape.as_ref().is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    let (Some(subject), Some(written)) = (text.character_text(), pattern.character_text()) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "function substring({}, {}) does not exist",
                text.type_name(),
                pattern.type_name()
            ),
        ));
    };
    match escape {
        Some(escape) => {
            let escape = escape_character(Some(&escape))?;
            pattern_substring(subject, &substring_pattern(written, escape)?)
        }
        None => pattern_substring(subject, written),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: SqlResult<Value>) -> String {
        match value {
            Ok(value) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Err(error) => format!("error {}", error.state().code()),
        }
    }

    fn call(name: &str, arguments: &[&str]) -> String {
        let values: Vec<Value> = arguments
            .iter()
            .map(|argument| Value::Text((*argument).to_owned()))
            .collect();
        text(function(name, &values).unwrap_or_else(|| panic!("{name} took no {arguments:?}")))
    }

    #[test]
    fn the_manuals_regexp_examples_give_its_results() {
        assert_eq!(call("regexp_match", &["foobarbequebaz", "bar.*que"]), "{barbeque}");
        assert_eq!(call("regexp_match", &["foobarbequebaz", "(bar)(beque)"]), "{bar,beque}");
        assert_eq!(call("regexp_match", &["foobarbequebaz", "x"]), "NULL");
        assert_eq!(call("regexp_replace", &["Thomas", ".[mN]a.", "M"]), "ThM");
        assert_eq!(call("regexp_replace", &["foobarbaz", "b(..)", "X\\1Y", "g"]), "fooXarYXazY");
        assert_eq!(
            call("regexp_split_to_array", &["hello world", "\\s+"]),
            "{hello,world}"
        );
        assert_eq!(call("regexp_split_to_array", &["abc", ""]), "{a,b,c}");
        assert_eq!(call("regexp_like", &["Hello World", "world$", "i"]), "t");
        assert_eq!(call("regexp_like", &["Hello World", "world$"]), "f");
        assert_eq!(call("similar_to_escape", &["abc%"]), "^(?:abc.*)$");
    }

    #[test]
    fn counting_and_positions_follow_the_manual() {
        let values = [
            Value::Text("ABCABCAXYaxy".into()),
            Value::Text("A.".into()),
            Value::Integer(1),
            Value::Text("i".into()),
        ];
        assert_eq!(text(function("regexp_count", &values).unwrap()), "4");
        let values = [
            Value::Text("ABCDEFGHI".into()),
            Value::Text("(c..)(...)".into()),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(0),
            Value::Text("i".into()),
            Value::Integer(2),
        ];
        assert_eq!(text(function("regexp_instr", &values).unwrap()), "6");
        let values = [
            Value::Text("ABCDEFGHI".into()),
            Value::Text("(c..)(...)".into()),
            Value::Integer(1),
            Value::Integer(1),
            Value::Text("i".into()),
            Value::Integer(2),
        ];
        assert_eq!(text(function("regexp_substr", &values).unwrap()), "FGH");
        let values = [Value::Text("x".into()), Value::Text("x".into()), Value::Integer(0)];
        assert_eq!(text(function("regexp_count", &values).unwrap()), "error 22023");
    }

    #[test]
    fn similar_patterns_convert_to_anchored_regular_expressions() {
        assert_eq!(substring_pattern("%#\"o_b#\"%", Some('#')).unwrap(), "^(?:.*?)(o.b)(?:.*)$");
        assert_eq!(text(pattern_substring("foobar", "^(?:.*?)(o.b)(?:.*)$")), "oob");
        assert!(separated_parts("a#\"b#\"c#\"d", Some('#')).is_err());
        assert_eq!(similar_to_escape_text("a.b(c|d)_", Some('\\')).unwrap(), "^(?:a\\.b(?:c|d).)$");
        assert_eq!(escape_character(Some(&Value::Text("ab".into()))).unwrap_err().state().code(), "22025");
    }

    #[test]
    fn global_flags_are_refused_where_the_manual_refuses_them() {
        assert_eq!(call("regexp_match", &["a", "a", "g"]), "error 22023");
        assert_eq!(call("regexp_like", &["a", "a", "z"]), "error 22023");
        assert_eq!(call("regexp_split_to_array", &["a", "a", "g"]), "error 22023");
    }

    #[test]
    fn seeded_hostile_patterns_and_templates_never_panic() {
        const PIECES: [&str; 18] = [
            "a", "b", "(", ")", "\\1", "(?=", "(?<!", "*", "+?", "|", "[", "]", "\\m", "%", "#\"",
            "_", "{2}", "$",
        ];
        let mut seed = 0x5EED_0036_u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for _ in 0..3_000 {
            let pattern: String = (0..next(8))
                .map(|_| PIECES[next(PIECES.len() as u64) as usize])
                .collect();
            let text: String = (0..next(16)).map(|_| ['a', 'b', ' ', '#'][next(4) as usize]).collect();
            let values = [Value::Text(text.clone()), Value::Text(pattern.clone()), Value::Text("X\\1\\&".into()), Value::Text("g".into())];
            let _ = function("regexp_replace", &values);
            let _ = function("regexp_match", &values[..2]);
            let _ = function("regexp_split_to_array", &values[..2]);
            let _ = function("similar_to_escape", &[Value::Text(pattern.clone()), Value::Text("#".into())]);
            let _ = substring_pattern(&pattern, Some('#'));
            let _ = matches_rows(&[Value::Text(text), Value::Text(pattern), Value::Text("g".into())], &mut |_| Ok(()));
        }
    }
}
