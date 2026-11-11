use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

pub(crate) const ESCAPE: &str = "xml_escape";
pub(crate) const ELEMENT: &str = "xml_element";
pub(crate) const FOREST: &str = "xml_forest";
pub(crate) const CONCAT: &str = "xml_concat";
pub(crate) const COMMENT: &str = "xml_comment";
pub(crate) const PI: &str = "xml_pi";
pub(crate) const FRAGMENT: &str = "xml_fragment";
pub(crate) const INTERNAL_NAMES: [&str; 7] = [ESCAPE, ELEMENT, FOREST, CONCAT, COMMENT, PI, FRAGMENT];

/// Whether `name` is one of the internal calls that already produce XML, so their results are
/// nested without escaping.
pub(crate) fn produces_xml(name: &str) -> bool {
    matches!(name, ELEMENT | FOREST | CONCAT | COMMENT | PI | FRAGMENT)
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    INTERNAL_NAMES.contains(&name).then_some(SqlType::Text)
}

fn escaped(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(text.clone()),
        Value::Boolean(flag) => Some(flag.to_string()),
        other => other.to_text().ok(),
    }
}

fn element_name(value: &Value) -> SqlResult<String> {
    let name = text_of(value).unwrap_or_default();
    let valid = name
        .chars()
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == ':')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | ':' | '-' | '.'));
    if !valid {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid XML name: \"{name}\""),
        ));
    }
    Ok(name)
}

/// Evaluates one of the XML calls; `None` for another name.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !INTERNAL_NAMES.contains(&name) {
        return None;
    }
    Some(evaluate(name, values))
}

fn evaluate(name: &str, values: &[Value]) -> SqlResult<Value> {
    Ok(match (name, values) {
        (ESCAPE, [value]) => text_of(value).map_or(Value::Null, |text| Value::Text(escaped(&text, false))),
        (FRAGMENT, [value]) => value.clone(),
        (CONCAT, parts) => {
            let present: Vec<String> = parts.iter().filter_map(text_of).collect();
            if present.is_empty() {
                Value::Null
            } else {
                Value::Text(present.concat())
            }
        }
        (FOREST, pairs) => {
            let mut out = String::new();
            let mut any = false;
            for pair in pairs.chunks(2) {
                let [tag, value] = pair else {
                    continue;
                };
                let Some(content) = text_of(value) else {
                    continue;
                };
                let tag = element_name(tag)?;
                out.push_str(&format!("<{tag}>{content}</{tag}>"));
                any = true;
            }
            if any { Value::Text(out) } else { Value::Null }
        }
        (COMMENT, [text]) => match text_of(text) {
            None => Value::Null,
            Some(text) if text.contains("--") || text.ends_with('-') => {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid XML comment",
                ));
            }
            Some(text) => Value::Text(format!("<!--{text}-->")),
        },
        (PI, [target, rest @ ..]) => {
            let target = element_name(target)?;
            if target.eq_ignore_ascii_case("xml") {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid XML processing instruction target",
                ));
            }
            match rest.first().and_then(text_of) {
                Some(content) if content.contains("?>") => {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "invalid XML processing instruction",
                    ));
                }
                Some(content) => Value::Text(format!("<?{target} {}?>", content.trim_start())),
                None => Value::Text(format!("<?{target}?>")),
            }
        }
        (ELEMENT, [tag, count, rest @ ..]) => {
            let tag = element_name(tag)?;
            let count = count.as_integer().unwrap_or(0).max(0) as usize;
            let (attributes, contents) = rest.split_at((count * 2).min(rest.len()));
            let mut out = format!("<{tag}");
            for pair in attributes.chunks(2) {
                let [attribute, value] = pair else {
                    continue;
                };
                if let Some(value) = text_of(value) {
                    out.push_str(&format!(
                        " {}=\"{}\"",
                        element_name(attribute)?,
                        escaped(&value, true)
                    ));
                }
            }
            let inner: Vec<String> = contents.iter().filter_map(text_of).collect();
            if inner.is_empty()
                && crate::constraints::current_dialect() != crate::dialect::Dialect::Oracle
            {
                out.push_str("/>");
            } else {
                out.push('>');
                out.push_str(&inner.concat());
                out.push_str(&format!("</{tag}>"));
            }
            Value::Text(out)
        }
        _ => Value::Null,
    })
}
