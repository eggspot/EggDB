use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_functions::text_of;
use crate::types::Value;

pub(crate) const MAX_RESULT_BYTES: usize = 32_767;
pub(crate) const DEFAULT_TRUNCATION_INDICATOR: &str = "...";

fn too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "result of string concatenation is too long",
    )
    .with_oracle_code(1489)
}

struct Overflow {
    truncate: bool,
    indicator: String,
    with_count: bool,
}

impl Overflow {
    fn of(options: &[Value]) -> Self {
        Self {
            truncate: matches!(options.first(), Some(Value::Boolean(true))),
            indicator: options
                .get(1)
                .and_then(text_of)
                .unwrap_or_default(),
            with_count: !matches!(options.get(2), Some(Value::Boolean(false))),
        }
    }
}

fn truncated(items: &[String], delimiter: &str, overflow: &Overflow) -> SqlResult<String> {
    let count_room = if overflow.with_count {
        items.len().to_string().len() + 2
    } else {
        0
    };
    let reserved = delimiter.len() + overflow.indicator.len() + count_room;
    if reserved > MAX_RESULT_BYTES {
        return Err(too_long());
    }
    let mut out = String::new();
    let mut kept = 0;
    for item in items {
        let separator = if kept == 0 { 0 } else { delimiter.len() };
        if out.len() + separator + item.len() + reserved > MAX_RESULT_BYTES {
            break;
        }
        if kept > 0 {
            out.push_str(delimiter);
        }
        out.push_str(item);
        kept += 1;
    }
    if kept > 0 {
        out.push_str(delimiter);
    }
    out.push_str(&overflow.indicator);
    if overflow.with_count {
        out.push_str(&format!("({})", items.len() - kept));
    }
    Ok(out)
}

pub(crate) fn joined(parts: &Value, options: &[Value]) -> SqlResult<Value> {
    let Value::Array(array) = parts else {
        return Ok(Value::Null);
    };
    let delimiter = options.first().and_then(text_of).unwrap_or_default();
    let items: Vec<String> = array.items.iter().filter_map(text_of).collect();
    if items.is_empty() {
        return Ok(Value::Null);
    }
    let total = items
        .iter()
        .map(String::len)
        .fold(0usize, usize::saturating_add)
        .saturating_add(delimiter.len().saturating_mul(items.len() - 1));
    if total <= MAX_RESULT_BYTES {
        crate::memory::check_value_bytes(total)?;
        return Ok(Value::Text(items.join(&delimiter)));
    }
    let overflow = Overflow::of(options.get(1..).unwrap_or(&[]));
    if !overflow.truncate {
        return Err(too_long());
    }
    truncated(&items, &delimiter, &overflow).map(Value::Text)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESULT_BYTES, joined};
    use crate::arrays::{ArrayElement, ArrayValue};
    use crate::types::Value;

    fn parts(items: &[Option<&str>]) -> Value {
        Value::Array(Box::new(ArrayValue::new(
            ArrayElement::Text,
            items
                .iter()
                .map(|item| item.map_or(Value::Null, |text| Value::Text(text.to_owned())))
                .collect(),
        )))
    }

    fn options(delimiter: &str, truncate: bool, indicator: &str, with_count: bool) -> Vec<Value> {
        vec![
            Value::Text(delimiter.to_owned()),
            Value::Boolean(truncate),
            Value::Text(indicator.to_owned()),
            Value::Boolean(with_count),
        ]
    }

    #[test]
    fn values_join_with_the_delimiter_and_nulls_are_left_out() {
        assert_eq!(
            joined(
                &parts(&[Some("Raphaely"), None, Some("Khoo")]),
                &options("; ", false, "...", true)
            ),
            Ok(Value::Text("Raphaely; Khoo".to_owned()))
        );
        assert_eq!(
            joined(&parts(&[None]), &options(",", false, "...", true)),
            Ok(Value::Null)
        );
        assert_eq!(joined(&Value::Null, &[]), Ok(Value::Null));
        assert_eq!(
            joined(&parts(&[Some("a"), Some("b")]), &[Value::Null]),
            Ok(Value::Text("ab".to_owned()))
        );
    }

    #[test]
    fn overflow_is_an_error_or_a_truncation_with_its_count() {
        let long = "x".repeat(MAX_RESULT_BYTES / 2);
        let items = [Some(long.as_str()), Some(long.as_str()), Some("tail")];
        let error = joined(&parts(&items), &options(",", false, "...", true)).expect_err("long");
        assert_eq!(error.vendor_code(), Some(1489));
        let Ok(Value::Text(text)) = joined(&parts(&items), &options(",", true, "~~~", true)) else {
            panic!("truncated");
        };
        assert!(text.ends_with(",~~~(2)"));
        assert!(text.len() <= MAX_RESULT_BYTES);
        let Ok(Value::Text(text)) = joined(&parts(&items), &options(",", true, "", false)) else {
            panic!("truncated");
        };
        assert!(text.ends_with('x') || text.ends_with(','));
    }
}
