use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};

pub(crate) const APPLY: &str = "eggdb_jsonlogic";
const MAX_DEPTH: usize = 128;
const MAX_ITEMS: usize = 100_000;

#[derive(Debug, Clone, PartialEq)]
enum J {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<J>),
    Object(Vec<(String, J)>),
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn of_node(node: &Node) -> J {
    match &node.value {
        Json::Null => J::Null,
        Json::Boolean(flag) => J::Bool(*flag),
        Json::Number(text) => J::Number(text.parse().unwrap_or(0.0)),
        Json::String(text) => J::Text(text.clone()),
        Json::Array(items) => J::Array(items.iter().map(of_node).collect()),
        Json::Object(members) => J::Object(
            members
                .iter()
                .map(|(key, value)| (key.clone(), of_node(value)))
                .collect(),
        ),
    }
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
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
    out
}

fn number_text(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn written(value: &J) -> String {
    match value {
        J::Null => "null".to_owned(),
        J::Bool(flag) => flag.to_string(),
        J::Number(number) if number.is_finite() => number_text(*number),
        J::Number(_) => "null".to_owned(),
        J::Text(text) => quoted(text),
        J::Array(items) => format!(
            "[{}]",
            items.iter().map(written).collect::<Vec<_>>().join(", ")
        ),
        J::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(key, value)| format!("{}: {}", quoted(key), written(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn truthy(value: &J) -> bool {
    match value {
        J::Null => false,
        J::Bool(flag) => *flag,
        J::Number(number) => *number != 0.0 && !number.is_nan(),
        J::Text(text) => !text.is_empty(),
        J::Array(items) => !items.is_empty(),
        J::Object(_) => true,
    }
}

fn text_of(value: &J) -> String {
    match value {
        J::Null => "null".to_owned(),
        J::Bool(flag) => flag.to_string(),
        J::Number(number) => number_text(*number),
        J::Text(text) => text.clone(),
        J::Array(items) => items.iter().map(text_of).collect::<Vec<_>>().join(","),
        J::Object(_) => "[object Object]".to_owned(),
    }
}

fn number_of(value: &J) -> Option<f64> {
    match value {
        J::Number(number) => Some(*number),
        J::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        J::Null => Some(0.0),
        J::Text(text) if text.trim().is_empty() => Some(0.0),
        J::Text(text) => text.trim().parse().ok(),
        J::Array(items) if items.is_empty() => Some(0.0),
        J::Array(items) if items.len() == 1 => number_of(&items[0]),
        _ => None,
    }
}

fn loosely_equal(left: &J, right: &J) -> bool {
    match (left, right) {
        (J::Null, J::Null) => true,
        (J::Null, _) | (_, J::Null) => false,
        (J::Text(a), J::Text(b)) => a == b,
        (J::Bool(a), J::Bool(b)) => a == b,
        (J::Array(_) | J::Object(_), _) | (_, J::Array(_) | J::Object(_)) => left == right,
        _ => match (number_of(left), number_of(right)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

fn compared(left: &J, right: &J) -> Option<std::cmp::Ordering> {
    if let (J::Text(a), J::Text(b)) = (left, right) {
        return Some(a.cmp(b));
    }
    number_of(left)?.partial_cmp(&number_of(right)?)
}

fn lookup(data: &J, path: &str) -> Option<J> {
    if path.is_empty() {
        return Some(data.clone());
    }
    let mut current = data;
    for part in path.split('.') {
        current = match current {
            J::Object(members) => members.iter().find(|(key, _)| key == part).map(|(_, value)| value)?,
            J::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current.clone())
}

fn murmur3(bytes: &[u8]) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let mut hash: u32 = 0;
    let mut chunks = bytes.chunks_exact(4);
    for chunk in &mut chunks {
        let mut k = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        hash ^= k;
        hash = hash.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = chunks.remainder();
    let mut k: u32 = 0;
    for (at, byte) in tail.iter().enumerate() {
        k |= u32::from(*byte) << (8 * at);
    }
    if !tail.is_empty() {
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        hash ^= k;
    }
    hash ^= bytes.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

fn version_parts(text: &str) -> Option<(Vec<u64>, String)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, pre.split('+').next().unwrap_or("").to_owned()),
        None => (text.split('+').next().unwrap_or(""), String::new()),
    };
    let mut numbers: Vec<u64> = core
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    if numbers.is_empty() || numbers.len() > 3 {
        return None;
    }
    numbers.resize(3, 0);
    Some((numbers, pre))
}

fn version_order(left: &(Vec<u64>, String), right: &(Vec<u64>, String)) -> std::cmp::Ordering {
    left.0.cmp(&right.0).then_with(|| match (left.1.is_empty(), right.1.is_empty()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => left.1.cmp(&right.1),
    })
}

struct Evaluator {
    steps: usize,
}

impl Evaluator {
    fn apply(&mut self, rule: &J, data: &J, depth: usize) -> SqlResult<J> {
        if depth > MAX_DEPTH {
            return Err(invalid("the JsonLogic rule is nested too deeply"));
        }
        self.steps += 1;
        if self.steps > MAX_ITEMS * 10 {
            return Err(invalid("the JsonLogic rule takes too many steps"));
        }
        let (operator, raw) = match rule {
            J::Array(items) => {
                return items
                    .iter()
                    .map(|item| self.apply(item, data, depth + 1))
                    .collect::<SqlResult<Vec<J>>>()
                    .map(J::Array);
            }
            J::Object(members) if members.len() == 1 => (members[0].0.as_str(), &members[0].1),
            other => return Ok(other.clone()),
        };
        let raw_arguments: Vec<J> = match raw {
            J::Array(items) => items.clone(),
            single => vec![single.clone()],
        };
        match operator {
            "if" | "?:" => {
                let mut at = 0;
                while at + 1 < raw_arguments.len() {
                    if truthy(&self.apply(&raw_arguments[at], data, depth + 1)?) {
                        return self.apply(&raw_arguments[at + 1], data, depth + 1);
                    }
                    at += 2;
                }
                return match raw_arguments.get(at) {
                    Some(otherwise) => self.apply(otherwise, data, depth + 1),
                    None => Ok(J::Null),
                };
            }
            "and" => {
                let mut last = J::Bool(true);
                for argument in &raw_arguments {
                    last = self.apply(argument, data, depth + 1)?;
                    if !truthy(&last) {
                        return Ok(last);
                    }
                }
                return Ok(last);
            }
            "or" => {
                let mut last = J::Bool(false);
                for argument in &raw_arguments {
                    last = self.apply(argument, data, depth + 1)?;
                    if truthy(&last) {
                        return Ok(last);
                    }
                }
                return Ok(last);
            }
            "map" | "filter" | "all" | "none" | "some" | "reduce" => {
                return self.iterate(operator, &raw_arguments, data, depth);
            }
            _ => {}
        }
        let arguments: Vec<J> = raw_arguments
            .iter()
            .map(|argument| self.apply(argument, data, depth + 1))
            .collect::<SqlResult<_>>()?;
        let first = arguments.first().cloned().unwrap_or(J::Null);
        let second = arguments.get(1).cloned().unwrap_or(J::Null);
        Ok(match operator {
            "var" => {
                let path = match &first {
                    J::Number(number) => number_text(*number),
                    J::Null => String::new(),
                    other => text_of(other),
                };
                lookup(data, &path)
                    .filter(|found| *found != J::Null)
                    .unwrap_or_else(|| arguments.get(1).cloned().unwrap_or(J::Null))
            }
            "missing" => {
                let keys: Vec<J> = match &first {
                    J::Array(items) => items.clone(),
                    _ => arguments.clone(),
                };
                J::Array(
                    keys.into_iter()
                        .filter(|key| {
                            matches!(lookup(data, &text_of(key)), None | Some(J::Null))
                                || lookup(data, &text_of(key)) == Some(J::Text(String::new()))
                        })
                        .collect(),
                )
            }
            "missing_some" => {
                let needed = number_of(&first).unwrap_or(0.0) as usize;
                let J::Array(keys) = &second else {
                    return Ok(J::Array(Vec::new()));
                };
                let missing: Vec<J> = keys
                    .iter()
                    .filter(|key| matches!(lookup(data, &text_of(key)), None | Some(J::Null)))
                    .cloned()
                    .collect();
                if keys.len() - missing.len() >= needed {
                    J::Array(Vec::new())
                } else {
                    J::Array(missing)
                }
            }
            "==" => J::Bool(loosely_equal(&first, &second)),
            "!=" => J::Bool(!loosely_equal(&first, &second)),
            "===" => J::Bool(first == second),
            "!==" => J::Bool(first != second),
            "!" => J::Bool(!truthy(&first)),
            "!!" => J::Bool(truthy(&first)),
            ">" | ">=" | "<" | "<=" => {
                let holds = |left: &J, right: &J| match compared(left, right) {
                    Some(order) => match operator {
                        ">" => order.is_gt(),
                        ">=" => order.is_ge(),
                        "<" => order.is_lt(),
                        _ => order.is_le(),
                    },
                    None => false,
                };
                if arguments.len() == 3 && matches!(operator, "<" | "<=") {
                    J::Bool(holds(&first, &second) && holds(&second, &arguments[2]))
                } else {
                    J::Bool(holds(&first, &second))
                }
            }
            "in" => J::Bool(match &second {
                J::Array(items) => items.iter().any(|item| item == &first),
                J::Text(text) => text.contains(&text_of(&first)),
                _ => false,
            }),
            "cat" => J::Text(arguments.iter().map(text_of).collect()),
            "substr" => {
                let text: Vec<char> = text_of(&first).chars().collect();
                let length = text.len() as i64;
                let start = number_of(&second).unwrap_or(0.0) as i64;
                let start = if start < 0 { (length + start).max(0) } else { start.min(length) };
                let end = match arguments.get(2).and_then(number_of) {
                    Some(count) if count < 0.0 => (length + count as i64).max(start),
                    Some(count) => (start + count as i64).min(length),
                    None => length,
                };
                J::Text(text[start as usize..end.max(start) as usize].iter().collect())
            }
            "+" => J::Number(arguments.iter().filter_map(number_of).sum()),
            "*" => J::Number(arguments.iter().filter_map(number_of).product()),
            "-" => match arguments.len() {
                1 => J::Number(-number_of(&first).unwrap_or(0.0)),
                _ => J::Number(number_of(&first).unwrap_or(0.0) - number_of(&second).unwrap_or(0.0)),
            },
            "/" => J::Number(number_of(&first).unwrap_or(0.0) / number_of(&second).unwrap_or(0.0)),
            "%" => J::Number(number_of(&first).unwrap_or(0.0) % number_of(&second).unwrap_or(0.0)),
            "min" | "max" => {
                let numbers: Vec<f64> = arguments.iter().filter_map(number_of).collect();
                if numbers.is_empty() {
                    J::Null
                } else if operator == "min" {
                    J::Number(numbers.into_iter().fold(f64::INFINITY, f64::min))
                } else {
                    J::Number(numbers.into_iter().fold(f64::NEG_INFINITY, f64::max))
                }
            }
            "merge" => J::Array(
                arguments
                    .into_iter()
                    .flat_map(|argument| match argument {
                        J::Array(items) => items,
                        single => vec![single],
                    })
                    .collect(),
            ),
            "starts_with" => J::Bool(text_of(&first).starts_with(&text_of(&second))),
            "ends_with" => J::Bool(text_of(&first).ends_with(&text_of(&second))),
            "sem_ver" => {
                let operator = text_of(&second);
                let wanted = arguments.get(2).map(text_of).unwrap_or_default();
                match (version_parts(&text_of(&first)), version_parts(&wanted)) {
                    (Some(have), Some(want)) => {
                        let order = version_order(&have, &want);
                        J::Bool(match operator.as_str() {
                            "=" => order.is_eq(),
                            "!=" => !order.is_eq(),
                            ">" => order.is_gt(),
                            ">=" => order.is_ge(),
                            "<" => order.is_lt(),
                            "<=" => order.is_le(),
                            "^" => have.0[0] == want.0[0],
                            "~" => have.0[0] == want.0[0] && have.0[1] == want.0[1],
                            _ => false,
                        })
                    }
                    _ => J::Null,
                }
            }
            "fractional" => self.fractional(&arguments, data),
            other => return Err(invalid(format!("unknown JsonLogic operation {other}"))),
        })
    }

    fn fractional(&self, arguments: &[J], data: &J) -> J {
        let (key, buckets) = match arguments.first() {
            Some(J::Text(key)) => (key.clone(), &arguments[1..]),
            _ => {
                let flag = lookup(data, "$flagd.flagKey").map(|key| text_of(&key)).unwrap_or_default();
                let target = lookup(data, "targetingKey").map(|key| text_of(&key)).unwrap_or_default();
                (format!("{flag}{target}"), arguments)
            }
        };
        let weighted: Vec<(String, f64)> = buckets
            .iter()
            .filter_map(|bucket| match bucket {
                J::Array(items) => {
                    let name = text_of(items.first()?);
                    let weight = items.get(1).and_then(number_of).unwrap_or(1.0).max(0.0);
                    Some((name, weight))
                }
                _ => None,
            })
            .collect();
        let total: f64 = weighted.iter().map(|(_, weight)| weight).sum();
        if weighted.is_empty() || total <= 0.0 {
            return J::Null;
        }
        let ratio = f64::from(murmur3(key.as_bytes())) / f64::from(u32::MAX);
        let point = ratio * 100.0;
        let mut reached = 0.0;
        for (name, weight) in &weighted {
            reached += weight / total * 100.0;
            if point < reached {
                return J::Text(name.clone());
            }
        }
        J::Text(weighted.last().map(|(name, _)| name.clone()).unwrap_or_default())
    }

    fn iterate(&mut self, operator: &str, raw: &[J], data: &J, depth: usize) -> SqlResult<J> {
        let scope = match raw.first() {
            Some(rule) => self.apply(rule, data, depth + 1)?,
            None => J::Null,
        };
        let items = match scope {
            J::Array(items) => items,
            _ => Vec::new(),
        };
        if items.len() > MAX_ITEMS {
            return Err(invalid("JsonLogic iterates over at most 100,000 items"));
        }
        let body = raw.get(1).cloned().unwrap_or(J::Null);
        Ok(match operator {
            "map" => J::Array(
                items
                    .iter()
                    .map(|item| self.apply(&body, item, depth + 1))
                    .collect::<SqlResult<_>>()?,
            ),
            "filter" => {
                let mut kept = Vec::new();
                for item in items {
                    if truthy(&self.apply(&body, &item, depth + 1)?) {
                        kept.push(item);
                    }
                }
                J::Array(kept)
            }
            "all" => {
                if items.is_empty() {
                    return Ok(J::Bool(false));
                }
                for item in &items {
                    if !truthy(&self.apply(&body, item, depth + 1)?) {
                        return Ok(J::Bool(false));
                    }
                }
                J::Bool(true)
            }
            "none" | "some" => {
                let mut any = false;
                for item in &items {
                    if truthy(&self.apply(&body, item, depth + 1)?) {
                        any = true;
                        break;
                    }
                }
                J::Bool(if operator == "some" { any } else { !any })
            }
            _ => {
                let mut accumulator = match raw.get(2) {
                    Some(initial) => self.apply(initial, data, depth + 1)?,
                    None => J::Null,
                };
                for item in items {
                    let scope = J::Object(vec![
                        ("current".to_owned(), item),
                        ("accumulator".to_owned(), accumulator),
                    ]);
                    accumulator = self.apply(&body, &scope, depth + 1)?;
                }
                accumulator
            }
        })
    }
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    (name == APPLY).then_some(SqlType::Jsonb)
}

fn json_of(value: &Value) -> SqlResult<J> {
    let text = match value {
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.clone(),
        other => other.to_text().unwrap_or_default(),
    };
    parse_document(&text)
        .map(|node| of_node(&node))
        .map_err(|error| invalid(error.message))
}

/// `eggdb_jsonlogic(rule, data)`: applies a JsonLogic rule (jsonlogic.com's operations, with
/// flagd's `fractional`, `starts_with`, `ends_with` and `sem_ver`) to `data`.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name != APPLY {
        return None;
    }
    Some((|| {
        let [rule, data] = values else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                "eggdb_jsonlogic takes a rule and its data",
            ));
        };
        if rule.is_null() {
            return Ok(Value::Null);
        }
        let rule = json_of(rule)?;
        let data = if data.is_null() { J::Null } else { json_of(data)? };
        let result = Evaluator { steps: 0 }.apply(&rule, &data, 0)?;
        let node = crate::json::parse(&written(&result), true)?;
        Ok(Value::Jsonb(crate::json::normalized_value(&node.value)?))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(rule: &str, data: &str) -> String {
        match function(
            APPLY,
            &[Value::Jsonb(rule.to_owned()), Value::Jsonb(data.to_owned())],
        )
        .unwrap()
        .unwrap()
        {
            Value::Jsonb(text) => text,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rules_follow_jsonlogic_org() {
        assert_eq!(apply(r#"{"==": [1, 1]}"#, "{}"), "true");
        assert_eq!(apply(r#"{"var": "a.b"}"#, r#"{"a": {"b": 3}}"#), "3");
        assert_eq!(
            apply(r#"{"if": [{"<": [{"var": "t"}, 0]}, "freezing", {"<": [{"var": "t"}, 100]}, "liquid", "gas"]}"#, r#"{"t": 55}"#),
            "\"liquid\""
        );
        assert_eq!(apply(r#"{"in": ["Ringo", ["John", "Paul", "Ringo"]]}"#, "{}"), "true");
        assert_eq!(apply(r#"{"cat": ["I love", " pie"]}"#, "{}"), "\"I love pie\"");
        assert_eq!(apply(r#"{"missing": ["a", "b"]}"#, r#"{"a": "apple"}"#), "[\"b\"]");
        assert_eq!(
            apply(r#"{"reduce": [{"var": "n"}, {"+": [{"var": "current"}, {"var": "accumulator"}]}, 0]}"#, r#"{"n": [1, 2, 3, 4]}"#),
            "10"
        );
        assert_eq!(apply(r#"{"sem_ver": ["1.4.2", ">=", "1.4.0"]}"#, "{}"), "true");
        assert_eq!(apply(r#"{"starts_with": [{"var": "email"}, "admin"]}"#, r#"{"email": "admin@x.io"}"#), "true");
    }

    #[test]
    fn fractional_splits_steadily() {
        let rule = r#"{"fractional": [["red", 50], ["blue", 50]]}"#;
        let first = apply(rule, r#"{"targetingKey": "user-1", "$flagd": {"flagKey": "color"}}"#);
        let again = apply(rule, r#"{"targetingKey": "user-1", "$flagd": {"flagKey": "color"}}"#);
        assert_eq!(first, again);
        assert!(first == "\"red\"" || first == "\"blue\"");
        assert_eq!(murmur3(b""), 0);
        assert_eq!(murmur3(b"hello"), 0x248b_fa47);
    }
}
