use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use std::cmp::Ordering;

pub(crate) const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Boolean(bool),
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub(crate) value: Json,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

fn invalid(type_name: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {type_name}"),
    )
    .with_detail(detail.to_owned())
}

struct Parser<'t> {
    text: &'t str,
    bytes: &'t [u8],
    at: usize,
    type_name: &'static str,
}

impl Parser<'_> {
    fn error(&self, detail: &str) -> SqlError {
        invalid(self.type_name, detail)
    }

    fn spaces(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> SqlResult<Node> {
        if depth > MAX_DEPTH {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                format!(
                    "{} value nests deeper than {MAX_DEPTH} levels",
                    self.type_name
                ),
            ));
        }
        self.spaces();
        let start = self.at;
        let value = match self.bytes.get(self.at) {
            None => return Err(self.error("The input string ended unexpectedly.")),
            Some(b'{') => self.object(depth)?,
            Some(b'[') => self.array(depth)?,
            Some(b'"') => Json::String(self.string()?),
            Some(b'-' | b'0'..=b'9') => Json::Number(self.number()?),
            Some(_) => self.word()?,
        };
        Ok(Node {
            value,
            start,
            end: self.at,
        })
    }

    fn word(&mut self) -> SqlResult<Json> {
        let rest = &self.bytes[self.at..];
        for (word, value) in [
            (&b"true"[..], Json::Boolean(true)),
            (b"false", Json::Boolean(false)),
            (b"null", Json::Null),
        ] {
            if rest.starts_with(word)
                && !rest
                    .get(word.len())
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
            {
                self.at += word.len();
                return Ok(value);
            }
        }
        let token: String = self.text[self.at..]
            .chars()
            .take_while(|c| c.is_alphanumeric())
            .take(32)
            .collect();
        let shown = if token.is_empty() {
            self.text[self.at..]
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_default()
        } else {
            token
        };
        Err(self.error(&format!("Token \"{shown}\" is invalid.")))
    }

    fn number(&mut self) -> SqlResult<String> {
        let start = self.at;
        self.take(b'-');
        let digits_from = self.at;
        self.digits();
        let integral = &self.bytes[digits_from..self.at];
        if integral.is_empty() || (integral.len() > 1 && integral[0] == b'0') {
            return Err(self.error("Token is invalid."));
        }
        if self.take(b'.') && self.digits() == 0 {
            return Err(self.error("Token is invalid."));
        }
        if self.take(b'e') || self.take(b'E') {
            if !self.take(b'+') {
                self.take(b'-');
            }
            if self.digits() == 0 {
                return Err(self.error("Token is invalid."));
            }
        }
        if self
            .bytes
            .get(self.at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'.')
        {
            return Err(self.error("Token is invalid."));
        }
        Ok(self.text[start..self.at].to_owned())
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        self.at - start
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> SqlResult<String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let run_start = self.at;
            while self
                .bytes
                .get(self.at)
                .is_some_and(|&byte| byte != b'"' && byte != b'\\' && byte >= 0x20)
            {
                self.at += 1;
            }
            out.push_str(&self.text[run_start..self.at]);
            match self.bytes.get(self.at) {
                None => return Err(self.error("The input string ended unexpectedly.")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    out.push(self.escape()?);
                }
                Some(byte) => {
                    let detail = format!("Character with value 0x{byte:02x} must be escaped.");
                    return Err(self.error(&detail));
                }
            }
        }
    }

    fn hex4(&mut self) -> SqlResult<u32> {
        let digits = self
            .text
            .get(self.at..self.at + 4)
            .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| self.error("\"\\u\" must be followed by four hexadecimal digits."))?;
        self.at += 4;
        u32::from_str_radix(digits, 16)
            .map_err(|_| self.error("\"\\u\" must be followed by four hexadecimal digits."))
    }

    fn escape(&mut self) -> SqlResult<char> {
        let Some(&byte) = self.bytes.get(self.at) else {
            return Err(self.error("The input string ended unexpectedly."));
        };
        self.at += 1;
        Ok(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.hex4()?;
                let code = if (0xD800..0xDC00).contains(&first) {
                    if !(self.take(b'\\') && self.take(b'u')) {
                        return Err(
                            self.error("Unicode high surrogate must not follow a high surrogate.")
                        );
                    }
                    let second = self.hex4()?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(
                            self.error("Unicode low surrogate must follow a high surrogate.")
                        );
                    }
                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                } else if (0xDC00..0xE000).contains(&first) {
                    return Err(self.error("Unicode low surrogate must follow a high surrogate."));
                } else {
                    first
                };
                if code == 0 {
                    return Err(SqlError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        "unsupported Unicode escape sequence",
                    )
                    .with_detail("\\u0000 cannot be converted to text."));
                }
                char::from_u32(code).ok_or_else(|| self.error("Invalid Unicode escape."))?
            }
            _ => {
                let shown = self.text[self.at - 1..].chars().next().unwrap_or('?');
                return Err(self.error(&format!("Escape sequence \"\\{shown}\" is invalid.")));
            }
        })
    }

    fn array(&mut self, depth: usize) -> SqlResult<Json> {
        self.at += 1;
        let mut items = Vec::new();
        self.spaces();
        if self.take(b']') {
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.spaces();
            if self.take(b']') {
                return Ok(Json::Array(items));
            }
            if !self.take(b',') {
                return Err(self.error("Expected \",\" or \"]\", but found something else."));
            }
        }
    }

    fn object(&mut self, depth: usize) -> SqlResult<Json> {
        self.at += 1;
        let mut members = Vec::new();
        self.spaces();
        if self.take(b'}') {
            return Ok(Json::Object(members));
        }
        loop {
            self.spaces();
            if self.bytes.get(self.at) != Some(&b'"') {
                return Err(self.error("Expected string, but found something else."));
            }
            let key = self.string()?;
            self.spaces();
            if !self.take(b':') {
                return Err(self.error("Expected \":\", but found something else."));
            }
            members.push((key, self.value(depth + 1)?));
            self.spaces();
            if self.take(b'}') {
                return Ok(Json::Object(members));
            }
            if !self.take(b',') {
                return Err(self.error("Expected \",\" or \"}\", but found something else."));
            }
        }
    }
}

pub(crate) fn parse(text: &str, binary: bool) -> SqlResult<Node> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        at: 0,
        type_name: if binary { "jsonb" } else { "json" },
    };
    let node = parser.value(0)?;
    parser.spaces();
    if parser.at != text.len() {
        return Err(parser.error("Expected end of input, but found something else."));
    }
    Ok(node)
}

pub(crate) fn validate(text: &str) -> SqlResult<()> {
    parse(text, false).map(|_| ())
}

pub(crate) fn normalize(text: &str) -> SqlResult<String> {
    let node = parse(text, true)?;
    let mut out = String::with_capacity(text.len());
    write_normalized(&node.value, &mut out, size_limit(text.len()), false)?;
    Ok(out)
}

pub(crate) fn comparable(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('.') {
        return std::borrow::Cow::Borrowed(text);
    }
    let canonical = parse(text, true).and_then(|node| {
        let mut out = String::with_capacity(text.len());
        write_normalized(&node.value, &mut out, size_limit(text.len()), true)?;
        Ok(out)
    });
    match canonical {
        Ok(out) => std::borrow::Cow::Owned(out),
        Err(_) => std::borrow::Cow::Borrowed(text),
    }
}

const MAX_NORMALIZED_GROWTH: usize = 16;
const NORMALIZED_ALLOWANCE: usize = 1 << 20;

fn size_limit(input: usize) -> usize {
    input
        .saturating_mul(MAX_NORMALIZED_GROWTH)
        .saturating_add(NORMALIZED_ALLOWANCE)
}

fn too_large() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "jsonb value is more than {MAX_NORMALIZED_GROWTH} times larger than its text once its numbers are written out"
        ),
    )
}

fn key_order(left: &str, right: &str) -> Ordering {
    left.len()
        .cmp(&right.len())
        .then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

fn normalized_members(members: &[(String, Node)]) -> Vec<(&str, &Json)> {
    let mut sorted: Vec<(usize, &str, &Json)> = members
        .iter()
        .enumerate()
        .map(|(at, (key, node))| (at, key.as_str(), &node.value))
        .collect();
    sorted.sort_by(|left, right| key_order(left.1, right.1).then(right.0.cmp(&left.0)));
    sorted.dedup_by(|later, earlier| later.1 == earlier.1);
    sorted
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .collect()
}

pub(crate) fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn write_normalized(
    value: &Json,
    out: &mut String,
    limit: usize,
    canonical: bool,
) -> SqlResult<()> {
    match value {
        Json::Null => out.push_str("null"),
        Json::Boolean(true) => out.push_str("true"),
        Json::Boolean(false) => out.push_str("false"),
        Json::Number(text) => {
            let number = Numeric::parse(text)?.to_string();
            if canonical && number.contains('.') {
                out.push_str(number.trim_end_matches('0').trim_end_matches('.'));
            } else {
                out.push_str(&number);
            }
            if out.len() > limit {
                return Err(too_large());
            }
        }
        Json::String(text) => write_string(text, out),
        Json::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                write_normalized(&item.value, out, limit, canonical)?;
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (at, (key, value)) in normalized_members(members).into_iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_normalized(value, out, limit, canonical)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

pub(crate) fn type_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Boolean(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Key(String),
    Index(i64),
}

pub(crate) fn child<'n>(node: &'n Node, step: &Step, binary: bool) -> Option<&'n Node> {
    match (&node.value, step) {
        (Json::Object(members), Step::Key(key)) => {
            let mut found = members.iter().filter(|(name, _)| name == key);
            if binary {
                found.next_back().map(|(_, value)| value)
            } else {
                found.next().map(|(_, value)| value)
            }
        }
        (Json::Array(items), Step::Index(index)) => {
            let position = if *index < 0 {
                i64::try_from(items.len()).ok()?.checked_add(*index)?
            } else {
                *index
            };
            items.get(usize::try_from(position).ok()?)
        }
        (Json::Array(items), Step::Key(key)) => {
            let index: i64 = key.parse().ok()?;
            let position = if index < 0 {
                i64::try_from(items.len()).ok()?.checked_add(index)?
            } else {
                index
            };
            items.get(usize::try_from(position).ok()?)
        }
        _ => None,
    }
}

pub(crate) fn render(text: &str, node: &Node, binary: bool) -> SqlResult<String> {
    if binary {
        let mut out = String::new();
        write_normalized(&node.value, &mut out, size_limit(text.len()), false)?;
        Ok(out)
    } else {
        Ok(text
            .get(node.start..node.end)
            .unwrap_or_default()
            .to_owned())
    }
}

pub(crate) fn render_text(text: &str, node: &Node, binary: bool) -> SqlResult<Option<String>> {
    Ok(match &node.value {
        Json::Null => None,
        Json::String(value) => Some(value.clone()),
        _ => Some(render(text, node, binary)?),
    })
}

pub(crate) fn parse_path(text: &str) -> SqlResult<Vec<String>> {
    let malformed = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("malformed array literal: \"{text}\""),
        )
    };
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(malformed)?;
    let mut steps = Vec::new();
    if inner.trim().is_empty() {
        return Ok(steps);
    }
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        let mut item = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err(malformed()),
                    Some('"') => break,
                    Some('\\') => item.push(chars.next().ok_or_else(malformed)?),
                    Some(other) => item.push(other),
                }
            }
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                chars.next();
            }
        } else {
            while let Some(&next) = chars.peek() {
                if next == ',' || next == '"' || next == '{' || next == '}' {
                    break;
                }
                item.push(next);
                chars.next();
            }
            item = item.trim().to_owned();
            if item.is_empty() {
                return Err(malformed());
            }
        }
        steps.push(item);
        match chars.next() {
            None => return Ok(steps),
            Some(',') => {}
            Some(_) => return Err(malformed()),
        }
    }
}

pub(crate) fn contains(container: &Json, contained: &Json) -> bool {
    match (container, contained) {
        (Json::Object(have), Json::Object(want)) => {
            let have = normalized_members(have);
            normalized_members(want).into_iter().all(|(key, value)| {
                have.iter()
                    .find(|(name, _)| *name == key)
                    .is_some_and(|(_, found)| contains(found, value))
            })
        }
        (Json::Array(have), Json::Array(want)) => want.iter().all(|wanted| {
            have.iter().any(|item| match (&item.value, &wanted.value) {
                (Json::Array(_), Json::Array(_)) | (Json::Object(_), Json::Object(_)) => {
                    contains(&item.value, &wanted.value)
                }
                (left, right) => scalar_equal(left, right),
            })
        }),
        (Json::Array(have), scalar) if !matches!(scalar, Json::Object(_)) => {
            have.iter().any(|item| scalar_equal(&item.value, scalar))
        }
        (left, right) => scalar_equal(left, right),
    }
}

fn scalar_equal(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => match (Numeric::parse(a), Numeric::parse(b)) {
            (Ok(a), Ok(b)) => a.compare(&b) == Ordering::Equal,
            _ => false,
        },
        (Json::Array(_) | Json::Object(_), _) | (_, Json::Array(_) | Json::Object(_)) => false,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonb_normalizes_like_postgresql() {
        let cases = [
            (r#"{"b": 1, "a": 2}"#, r#"{"a": 2, "b": 1}"#),
            (r#"{"aa":1,"b":2}"#, r#"{"b": 2, "aa": 1}"#),
            (r#"{"a":1,"a":2}"#, r#"{"a": 2}"#),
            (" [1, 2.50,1e2 , -0] ", "[1, 2.50, 100, 0]"),
            (r#""\u00e9\n\/""#, "\"\u{e9}\\n/\""),
            ("null", "null"),
            (
                r#"{"x":[true,false,{"y":null}]}"#,
                r#"{"x": [true, false, {"y": null}]}"#,
            ),
        ];
        for (input, output) in cases {
            assert_eq!(normalize(input).unwrap(), output, "{input}");
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "01",
            "1.",
            "tru",
            "\"a",
            "\"\\x\"",
            "nul l",
            "{} x",
            "\"\\u0000\"",
            "\"\\ud800\"",
            "-",
            "1e",
        ] {
            assert!(normalize(bad).is_err(), "{bad}");
            assert!(validate(bad).is_err(), "{bad}");
        }
        assert!(validate(&"[".repeat(MAX_DEPTH + 2)).is_err());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(validate(&deep).is_ok());
    }

    #[test]
    fn numbers_compare_by_value_while_keeping_their_written_scale() {
        let one = normalize(r#"{"a": 1.0, "b": [2.50, "1.0"]}"#).unwrap();
        assert_eq!(one, r#"{"a": 1.0, "b": [2.50, "1.0"]}"#);
        assert_eq!(comparable(&one), r#"{"a": 1, "b": [2.5, "1.0"]}"#);
        assert_eq!(comparable(r#"{"a": 1}"#), r#"{"a": 1}"#);
        assert_eq!(comparable("[0.00, -0.0]"), "[0, 0]");
    }

    #[test]
    fn paths_reach_members_and_elements() {
        let text = r#"{"a": {"b": [10, "x", null]}, "a": 5}"#;
        let node = parse(text, false).unwrap();
        let first = child(&node, &Step::Key("a".into()), false).unwrap();
        assert_eq!(
            render(text, first, false).unwrap(),
            r#"{"b": [10, "x", null]}"#
        );
        let last = child(&node, &Step::Key("a".into()), true).unwrap();
        assert_eq!(render(text, last, true).unwrap(), "5");
        let list = child(first, &Step::Key("b".into()), false).unwrap();
        let x = child(list, &Step::Index(-2), false).unwrap();
        assert_eq!(render_text(text, x, false).unwrap().as_deref(), Some("x"));
        let null = child(list, &Step::Index(2), false).unwrap();
        assert_eq!(render_text(text, null, false).unwrap(), None);
        assert!(child(list, &Step::Index(3), false).is_none());
        assert!(child(list, &Step::Index(i64::MIN), false).is_none());
        assert_eq!(parse_path("{a, \"b c\",2}").unwrap(), ["a", "b c", "2"]);
        assert!(parse_path("a,b").is_err());
        assert!(parse_path("{a,,b}").is_err());
    }

    #[test]
    fn containment_follows_postgresql_rules() {
        let holds = |left: &str, right: &str| {
            contains(
                &parse(left, true).unwrap().value,
                &parse(right, true).unwrap().value,
            )
        };
        assert!(holds(r#"{"a":1,"b":[1,2,3]}"#, r#"{"b":[3,1]}"#));
        assert!(holds(r#"[1,2,[3,4]]"#, r#"[[4]]"#));
        assert!(holds(r#"["a", 1]"#, r#""a""#));
        assert!(holds(r#"{"n": 1.0}"#, r#"{"n": 1}"#));
        assert!(!holds(r#"{"a":1}"#, r#"{"a":1,"b":2}"#));
        assert!(!holds(r#"[1,2]"#, r#"[[1]]"#));
        assert!(!holds(r#""a""#, r#"["a"]"#));
    }

    #[test]
    fn hostile_text_is_an_error_never_a_panic() {
        let mut seed = 0xDEAD_BEEF_1234_5678u64;
        let alphabet = b"{}[]\":,\\u0123456789abcdefnulltruefalse -.eE \xc3\xa9";
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 40) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|at| alphabet[((seed >> (at % 40)) as usize + at * 13) % alphabet.len()])
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            let _ = normalize(&text);
            if let Ok(node) = parse(&text, false) {
                let _ = render(&text, &node, false);
            }
            let _ = parse_path(&text);
        }
    }
}
