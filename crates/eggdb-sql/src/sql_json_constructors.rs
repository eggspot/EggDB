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

/// `JSON_SERIALIZE(subject [RETURNING returning])`.
pub(crate) fn json_serialize_call(
    subject: Expr,
    returning: Option<SqlType>,
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
    call(JSON_SERIALIZE, vec![subject, returning], position)
}

/// Whether `name` is one of this module's internal functions.
pub(crate) fn is_constructor(name: &str) -> bool {
    matches!(name, IS_JSON | JSON | JSON_SCALAR | JSON_SERIALIZE)
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
        Some(Ok(JsonText::Text(text))) => json::parse(&text, false).is_ok_and(|node| {
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
    let [subject, _] = arguments else {
        return Err(malformed(JSON_SERIALIZE));
    };
    let returning = serialized_type(arguments, evaluator.scope);
    if !serializes_to(returning) {
        return Err(unsupported_serialize_type(returning));
    }
    let value = evaluator.eval(subject, Some(SqlType::Text))?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let text = validated_text(value, false)?;
    if returning == SqlType::Bytea {
        return Ok(Value::Bytea(text.into_bytes()));
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
