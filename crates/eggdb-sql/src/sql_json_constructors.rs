//! PostgreSQL 17's SQL/JSON constructors and predicate (D302): `expr IS [NOT] JSON [VALUE |
//! SCALAR | ARRAY | OBJECT] [WITH | WITHOUT UNIQUE [KEYS]]`, `JSON(expr [WITH UNIQUE KEYS])`,
//! `JSON_SCALAR(expr)` and `JSON_SERIALIZE(expr [RETURNING type])`. The parser turns each into
//! an internal function whose clauses are integer or typed-NULL arguments.

use crate::ast::{Expr, ExprKind, Literal};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::json::{self, Json, Node};
use crate::types::{Conversion, SqlType, Value, convert};

/// The internal name of the `IS JSON` predicate.
pub(crate) const IS_JSON: &str = "*is_json*";
/// The internal name of `JSON(...)`.
pub(crate) const JSON: &str = "*json*";
/// The internal name of `JSON_SCALAR(...)`.
pub(crate) const JSON_SCALAR: &str = "*json_scalar*";
/// The internal name of `JSON_SERIALIZE(...)`.
pub(crate) const JSON_SERIALIZE: &str = "*json_serialize*";

/// Which values an `IS JSON` predicate accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonShape {
    /// `IS JSON` or `IS JSON VALUE`: any JSON value.
    Value,
    /// `IS JSON SCALAR`: a string, number, boolean or `null`.
    Scalar,
    /// `IS JSON ARRAY`.
    Array,
    /// `IS JSON OBJECT`.
    Object,
}

impl JsonShape {
    /// The shape an `IS JSON` keyword names, if it names one.
    pub(crate) fn named(word: &str) -> Option<Self> {
        Some(match word {
            "value" => Self::Value,
            "scalar" => Self::Scalar,
            "array" => Self::Array,
            "object" => Self::Object,
            _ => return None,
        })
    }

    fn code(self) -> i64 {
        match self {
            Self::Value => 0,
            Self::Scalar => 1,
            Self::Array => 2,
            Self::Object => 3,
        }
    }

    fn of_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Value,
            1 => Self::Scalar,
            2 => Self::Array,
            3 => Self::Object,
            _ => return None,
        })
    }

    fn holds(self, value: &Json) -> bool {
        match self {
            Self::Value => true,
            Self::Scalar => !matches!(value, Json::Array(_) | Json::Object(_)),
            Self::Array => matches!(value, Json::Array(_)),
            Self::Object => matches!(value, Json::Object(_)),
        }
    }
}

fn code_literal(code: i64, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::Integer(code)),
        position,
    }
}

fn call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

/// `subject IS [NOT] JSON shape [WITH UNIQUE KEYS]`.
pub(crate) fn is_json_call(
    subject: Expr,
    shape: JsonShape,
    unique_keys: bool,
    negated: bool,
    position: usize,
) -> Expr {
    let arguments = vec![
        subject,
        code_literal(shape.code(), position),
        code_literal(i64::from(unique_keys), position),
        code_literal(i64::from(negated), position),
    ];
    call(IS_JSON, arguments, position)
}

/// `JSON(subject [WITH UNIQUE KEYS])`.
pub(crate) fn json_call(subject: Expr, unique_keys: bool, position: usize) -> Expr {
    let arguments = vec![subject, code_literal(i64::from(unique_keys), position)];
    call(JSON, arguments, position)
}

/// `JSON_SCALAR(subject)`.
pub(crate) fn json_scalar_call(subject: Expr, position: usize) -> Expr {
    call(JSON_SCALAR, vec![subject], position)
}

/// Whether `JSON_SERIALIZE` may return `sql_type`: a character string type or `bytea`.
pub(crate) fn serializes_to(sql_type: SqlType) -> bool {
    sql_type.is_character() || sql_type == SqlType::Bytea
}

/// The error for a `RETURNING` type `JSON_SERIALIZE` cannot produce.
pub(crate) fn unsupported_serialize_type(sql_type: SqlType) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "cannot use RETURNING type {} in JSON_SERIALIZE()",
            sql_type.name()
        ),
    )
}

pub(crate) const SERIALIZE_PRETTY: u8 = 1;
pub(crate) const SERIALIZE_ASCII: u8 = 2;
pub(crate) const SERIALIZE_TRUNCATE: u8 = 4;
const LAX_JSON: &str = "\u{0}oracle_lax_json";

/// Oracle's lax JSON as standard JSON: the subject's text with unquoted keys quoted, single-quoted
/// strings double-quoted, `TRUE`/`FALSE`/`NULL` in any case lowered and numbers such as `+1`, `.5`
/// and `1.` written out; text that is not lax JSON is left as it is, so the strict check refuses it.
pub(crate) fn lax_json_call(subject: Expr, position: usize) -> Expr {
    call(LAX_JSON, vec![subject], position)
}

fn read_quoted(characters: &[char], at: &mut usize, quote: char) -> Option<String> {
    let mut out = String::new();
    *at += 1;
    while let Some(&character) = characters.get(*at) {
        *at += 1;
        match character {
            '\\' => {
                let next = *characters.get(*at)?;
                *at += 1;
                if next == '\'' {
                    out.push('\'');
                } else {
                    out.push('\\');
                    out.push(next);
                }
            }
            found if found == quote => return Some(out),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    None
}

fn strict_text(text: &str) -> Option<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut at = 0;
    while let Some(&character) = characters.get(at) {
        match character {
            '"' => {
                let inner = read_quoted(&characters, &mut at, '"')?;
                out.push('"');
                out.push_str(&inner);
                out.push('"');
            }
            '\'' => {
                let inner = read_quoted(&characters, &mut at, '\'')?;
                out.push('"');
                out.push_str(&inner);
                out.push('"');
            }
            '{' | '}' | '[' | ']' | ',' | ':' => {
                out.push(character);
                at += 1;
            }
            space if space.is_whitespace() => {
                out.push(space);
                at += 1;
            }
            '+' | '-' | '.' | '0'..='9' => {
                let start = at;
                while characters
                    .get(at)
                    .is_some_and(|next| next.is_ascii_digit() || matches!(next, '+' | '-' | '.' | 'e' | 'E'))
                {
                    at += 1;
                }
                let mut number: String = characters[start..at].iter().collect();
                if let Some(rest) = number.strip_prefix('+') {
                    number = rest.to_owned();
                }
                if number.starts_with('.') {
                    number.insert(0, '0');
                } else if let Some(rest) = number.strip_prefix("-.") {
                    number = format!("-0.{rest}");
                }
                if number.ends_with('.') {
                    number.push('0');
                }
                out.push_str(&number);
            }
            letter if letter.is_alphabetic() || letter == '_' || letter == '$' => {
                let start = at;
                while characters
                    .get(at)
                    .is_some_and(|next| next.is_alphanumeric() || matches!(next, '_' | '$'))
                {
                    at += 1;
                }
                let word: String = characters[start..at].iter().collect();
                let lowered = word.to_ascii_lowercase();
                let mut ahead = at;
                while characters.get(ahead).is_some_and(|next| next.is_whitespace()) {
                    ahead += 1;
                }
                if characters.get(ahead) == Some(&':') {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else if matches!(lowered.as_str(), "true" | "false" | "null") {
                    out.push_str(&lowered);
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn lax_json(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [subject] = arguments else {
        return Err(malformed(IS_JSON));
    };
    let value = evaluator.eval(subject, Some(SqlType::Text))?;
    Ok(match value {
        Value::Text(text) => Value::Text(strict_text(&text).unwrap_or(text)),
        other => other,
    })
}

fn pretty(node: &crate::json::Node, depth: usize, out: &mut String) {
    let indent = |depth: usize| "  ".repeat(depth);
    match &node.value {
        crate::json::Json::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (at, item) in items.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                pretty(item, depth + 1, out);
                if at + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&indent(depth));
            out.push(']');
        }
        crate::json::Json::Object(members) if !members.is_empty() => {
            out.push_str("{\n");
            for (at, (key, value)) in members.iter().enumerate() {
                out.push_str(&indent(depth + 1));
                out.push_str(&json_string(key));
                out.push_str(" : ");
                pretty(value, depth + 1, out);
                if at + 1 < members.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&indent(depth));
            out.push('}');
        }
        crate::json::Json::Array(_) => out.push_str("[]"),
        crate::json::Json::Object(_) => out.push_str("{}"),
        crate::json::Json::String(text) => out.push_str(&json_string(text)),
        crate::json::Json::Number(text) => out.push_str(text),
        crate::json::Json::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        crate::json::Json::Null => out.push_str("null"),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn ascii_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            out.push(character);
        } else {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{:04X}", unit));
            }
        }
    }
    out
}

/// `JSON_SERIALIZE(subject [RETURNING returning] [PRETTY] [ASCII] [TRUNCATE])`.
pub(crate) fn json_serialize_call(
    subject: Expr,
    returning: Option<SqlType>,
    options: u8,
    position: usize,
) -> Expr {
    let null = Expr {
        kind: ExprKind::Literal(Literal::Null),
        position,
    };
    let returning = match returning {
        Some(sql_type) => Expr {
            kind: ExprKind::Cast {
                expr: Box::new(null),
                sql_type,
            },
            position,
        },
        None => null,
    };
    call(
        JSON_SERIALIZE,
        vec![subject, returning, code_literal(i64::from(options), position)],
        position,
    )
}

/// Whether `name` is one of this module's internal functions.
pub(crate) fn is_constructor(name: &str) -> bool {
    matches!(name, IS_JSON | JSON | JSON_SCALAR | JSON_SERIALIZE | LAX_JSON)
}

/// The column name PostgreSQL gives a select-list item that is one of these calls.
pub(crate) fn column_name(name: &str) -> Option<&'static str> {
    match name {
        JSON => Some("json"),
        JSON_SCALAR => Some("json_scalar"),
        JSON_SERIALIZE => Some("json_serialize"),
        _ => None,
    }
}

fn serialized_type(arguments: &[Expr], scope: Scope<'_>) -> SqlType {
    arguments
        .get(1)
        .and_then(|argument| crate::typing::static_type(argument, scope))
        .unwrap_or(SqlType::Text)
}

/// The result type of one of these calls.
pub(crate) fn result_type(name: &str, arguments: &[Expr], scope: Scope<'_>) -> Option<SqlType> {
    match name {
        IS_JSON => Some(SqlType::Boolean),
        JSON | JSON_SCALAR => Some(SqlType::Json),
        JSON_SERIALIZE => Some(serialized_type(arguments, scope)),
        LAX_JSON => Some(SqlType::Text),
        _ => None,
    }
}

fn malformed(name: &str) -> SqlError {
    let label = match name {
        IS_JSON => "IS JSON",
        JSON => "JSON",
        JSON_SCALAR => "JSON_SCALAR",
        _ => "JSON_SERIALIZE",
    };
    SqlError::new(SqlState::SYNTAX_ERROR, format!("malformed {label} call"))
}

fn code_at(name: &str, arguments: &[Expr], index: usize) -> SqlResult<i64> {
    match arguments.get(index).map(|argument| &argument.kind) {
        Some(ExprKind::Literal(Literal::Integer(code))) => Ok(*code),
        _ => Err(malformed(name)),
    }
}

fn flag_at(name: &str, arguments: &[Expr], index: usize) -> SqlResult<bool> {
    match code_at(name, arguments, index)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(malformed(name)),
    }
}

enum JsonText {
    Text(String),
    Normalized(String),
}

fn json_text(value: Value) -> Option<SqlResult<JsonText>> {
    Some(Ok(match value {
        Value::Text(text) | Value::Bpchar(text) | Value::Json(text) => JsonText::Text(text),
        Value::Jsonb(text) => JsonText::Normalized(text),
        Value::Bytea(bytes) => match String::from_utf8(bytes) {
            Ok(text) => JsonText::Text(text),
            Err(_) => {
                return Some(Err(SqlError::new(
                    SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\"",
                )));
            }
        },
        other => JsonText::Text(crate::citext::text_of(&other)?.to_owned()),
    }))
}

fn has_duplicate_keys(node: &Node) -> bool {
    match &node.value {
        Json::Array(elements) => elements.iter().any(has_duplicate_keys),
        Json::Object(members) => {
            let mut keys: Vec<&str> = members.iter().map(|(key, _)| key.as_str()).collect();
            keys.sort_unstable();
            keys.windows(2).any(|pair| pair[0] == pair[1])
                || members.iter().any(|(_, member)| has_duplicate_keys(member))
        }
        _ => false,
    }
}

fn duplicate_key() -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_JSON_OBJECT_KEY_VALUE,
        "duplicate JSON object key value",
    )
}

fn is_json(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [subject, ..] = arguments else {
        return Err(malformed(IS_JSON));
    };
    if arguments.len() != 4 {
        return Err(malformed(IS_JSON));
    }
    let shape =
        JsonShape::of_code(code_at(IS_JSON, arguments, 1)?).ok_or_else(|| malformed(IS_JSON))?;
    let unique_keys = flag_at(IS_JSON, arguments, 2)?;
    let negated = flag_at(IS_JSON, arguments, 3)?;
    let value = evaluator.eval(subject, None)?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let type_name = value.type_name();
    let holds = match json_text(value) {
        None => {
            return Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("cannot use type {type_name} in IS JSON predicate"),
            ));
        }
        Some(Err(_)) => false,
        Some(Ok(JsonText::Normalized(text))) => {
            json::parse(&text, true).is_ok_and(|node| shape.holds(&node.value))
        }
        Some(Ok(JsonText::Text(text))) => json::parse_json_input(&text).is_ok_and(|node| {
            shape.holds(&node.value) && !(unique_keys && has_duplicate_keys(&node))
        }),
    };
    Ok(Value::Boolean(holds != negated))
}

fn non_string_subject(type_name: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("cannot use non-string types with implicit FORMAT JSON clause (found {type_name})"),
    )
}

fn validated_text(value: Value, unique_keys: bool) -> SqlResult<String> {
    let type_name = value.type_name();
    match json_text(value).ok_or_else(|| non_string_subject(&type_name))?? {
        JsonText::Normalized(text) => Ok(text),
        JsonText::Text(text) => {
            let node = json::parse(&text, false)?;
            if unique_keys && has_duplicate_keys(&node) {
                return Err(duplicate_key());
            }
            Ok(text)
        }
    }
}

fn json_constructor(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [subject, _] = arguments else {
        return Err(malformed(JSON));
    };
    let unique_keys = flag_at(JSON, arguments, 1)?;
    let value = evaluator.eval(subject, Some(SqlType::Text))?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    validated_text(value, unique_keys).map(Value::Json)
}

fn json_scalar(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [subject] = arguments else {
        return Err(malformed(JSON_SCALAR));
    };
    let value = evaluator.eval(subject, None)?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    crate::json_functions::to_json_text(&value).map(Value::Json)
}

fn json_serialize(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let (subject, options) = match arguments {
        [subject, _] => (subject, 0),
        [subject, _, options] => (
            subject,
            match &options.kind {
                ExprKind::Literal(Literal::Integer(code)) => *code as u8,
                _ => 0,
            },
        ),
        _ => return Err(malformed(JSON_SERIALIZE)),
    };
    let returning = serialized_type(arguments, evaluator.scope);
    if !serializes_to(returning) {
        return Err(unsupported_serialize_type(returning));
    }
    let value = evaluator.eval(subject, Some(SqlType::Text))?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut text = validated_text(value, false)?;
    if options & SERIALIZE_PRETTY != 0
        && let Ok(node) = crate::json::parse_document(&text)
    {
        let mut out = String::new();
        pretty(&node, 0, &mut out);
        text = out;
    }
    if options & SERIALIZE_ASCII != 0 {
        text = ascii_only(&text);
    }
    if returning == SqlType::Bytea {
        return Ok(Value::Bytea(text.into_bytes()));
    }
    if options & SERIALIZE_TRUNCATE != 0
        && let SqlType::Varchar(Some(length)) | SqlType::Bpchar(Some(length)) = returning
    {
        text = text.chars().take(length as usize).collect();
    }
    convert(Value::Text(text), returning, Conversion::Explicit)
}

/// Evaluates one of this module's internal functions.
pub(crate) fn function(
    evaluator: &Evaluator<'_>,
    name: &str,
    arguments: &[Expr],
) -> SqlResult<Value> {
    match name {
        IS_JSON => is_json(evaluator, arguments),
        JSON => json_constructor(evaluator, arguments),
        JSON_SCALAR => json_scalar(evaluator, arguments),
        LAX_JSON => lax_json(evaluator, arguments),
        _ => json_serialize(evaluator, arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonShape, has_duplicate_keys};
    use crate::json;

    fn duplicated(text: &str) -> bool {
        has_duplicate_keys(&json::parse(text, false).unwrap())
    }

    #[test]
    fn duplicate_keys_are_found_at_any_depth() {
        assert!(!duplicated(r#"{"a":1,"b":2}"#));
        assert!(duplicated(r#"{"a":1,"a":2}"#));
        assert!(duplicated(r#"[1,{"x":{"k":1,"k":2}}]"#));
        assert!(!duplicated(r#"[{"a":1},{"a":2}]"#));
        assert!(!duplicated("3"));
    }

    #[test]
    fn shapes_accept_what_their_keyword_names() {
        let shape = |word: &str, text: &str| {
            JsonShape::named(word)
                .unwrap()
                .holds(&json::parse(text, false).unwrap().value)
        };
        assert!(shape("value", "[1]"));
        assert!(shape("scalar", r#""x""#));
        assert!(!shape("scalar", "{}"));
        assert!(shape("array", "[]"));
        assert!(!shape("array", "{}"));
        assert!(shape("object", r#"{"a":null}"#));
        assert!(!shape("object", "null"));
        assert_eq!(JsonShape::named("other"), None);
    }
}
