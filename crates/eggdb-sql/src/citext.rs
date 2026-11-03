//! The `citext` type compatible with PostgreSQL's extension of that name: text that compares,
//! orders, groups and indexes by one full case fold (D259), with no tie-break, so
//! `'A'` and `'a'` are the same value to every operation that compares.

mod folded;

use crate::error::SqlResult;
use crate::types::{DefinedType, DefinedValue, SqlType, Value};
use folded::FoldedText;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Range;

pub(crate) const CITEXT_OID: u32 = 9000;

pub(crate) fn value(text: String) -> Value {
    Value::Defined(Box::new(DefinedValue::Citext(text)))
}

pub(crate) fn text_of(value: &Value) -> Option<&str> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Citext(text) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_citext(defined: DefinedType) -> bool {
    defined == DefinedType::CITEXT
}

pub(crate) fn fold(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        eggdb_text::case_fold(text)
    }
}

pub(crate) fn compare(left: &str, right: &str) -> Ordering {
    if left.is_ascii() && right.is_ascii() {
        let left = left.bytes().map(|byte| byte.to_ascii_lowercase());
        let right = right.bytes().map(|byte| byte.to_ascii_lowercase());
        return left.cmp(right);
    }
    fold(left).as_bytes().cmp(fold(right).as_bytes())
}

fn character_or_citext(value: &Value) -> Option<&str> {
    text_of(value).or_else(|| value.character_text())
}

pub(crate) fn compare_with_text(left: &Value, right: &Value) -> Ordering {
    match (character_or_citext(left), character_or_citext(right)) {
        (Some(a), Some(b)) => a.as_bytes().cmp(b.as_bytes()),
        _ => Ordering::Equal,
    }
}

pub(crate) fn like_operands(value: Value, pattern: Value) -> (Value, Value, bool) {
    let folded = text_of(&value).is_some() || text_of(&pattern).is_some();
    if !folded {
        return (value, pattern, false);
    }
    let as_text = |value: Value| match text_of(&value) {
        Some(text) => Value::Text(text.to_owned()),
        None => value,
    };
    (as_text(value), as_text(pattern), true)
}

pub(crate) fn regex_operands(text: Value, pattern: Value) -> (Value, Value, bool) {
    let folded = text_of(&text).is_some();
    let as_text = |value: Value| match text_of(&value) {
        Some(text) => Value::Text(text.to_owned()),
        None => value,
    };
    (as_text(text), as_text(pattern), folded)
}

pub(crate) fn is_citext_type(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Defined(defined) if is_citext(defined))
}

const CASE_INSENSITIVE_REGEXP_FUNCTIONS: [&str; 5] = [
    "regexp_match",
    "regexp_matches",
    "regexp_replace",
    "regexp_split_to_array",
    "regexp_split_to_table",
];

const CASE_INSENSITIVE_STRING_FUNCTIONS: [&str; 4] =
    ["replace", "split_part", "strpos", "translate"];

fn as_text(value: &Value) -> Value {
    match text_of(value) {
        Some(text) => Value::Text(text.to_owned()),
        None => value.clone(),
    }
}

fn subject_is_citext(values: &[Value]) -> bool {
    values.first().and_then(text_of).is_some()
}

fn flags_position(name: &str) -> usize {
    if name == "regexp_replace" { 3 } else { 2 }
}

fn add_case_insensitive_flag(name: &str, values: &mut Vec<Value>) {
    let flags_at = flags_position(name);
    if values.len() == flags_at {
        values.push(Value::Text("i".to_owned()));
    } else if values.len() == flags_at + 1
        && let Some(Value::Text(flags)) = values.last_mut()
        && !flags.contains('c')
    {
        flags.push('i');
    }
}

pub(crate) fn regexp_arguments<'v>(name: &str, values: &'v [Value]) -> Cow<'v, [Value]> {
    if !values.iter().any(|value| text_of(value).is_some()) {
        return Cow::Borrowed(values);
    }
    let mut texts: Vec<Value> = values.iter().map(as_text).collect();
    if subject_is_citext(values) && CASE_INSENSITIVE_REGEXP_FUNCTIONS.contains(&name) {
        add_case_insensitive_flag(name, &mut texts);
    }
    Cow::Owned(texts)
}

fn text_argument(values: &[Value], at: usize) -> Option<&str> {
    values.get(at).and_then(Value::character_text)
}

fn spliced(text: &str, found: &[Range<usize>], to: &str) -> SqlResult<String> {
    let removed: usize = found.iter().map(|range| range.end - range.start).sum();
    let length = text
        .len()
        .saturating_sub(removed)
        .saturating_add(to.len().saturating_mul(found.len()));
    crate::string_functions::checked_length(length)?;
    let mut out = String::with_capacity(length);
    let mut copied = 0;
    for range in found {
        out.push_str(text.get(copied..range.start).unwrap_or_default());
        out.push_str(to);
        copied = range.end;
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Ok(out)
}

fn replaced(values: &[Value]) -> Option<SqlResult<Value>> {
    let [text, from, to] = [0, 1, 2].map(|at| text_argument(values, at));
    let (text, from, to) = (text?, from?, to?);
    let found = FoldedText::new(text).matches(from, usize::MAX);
    Some(spliced(text, &found, to).map(Value::Text))
}

fn split_part(values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, delimiter) = (text_argument(values, 0)?, text_argument(values, 1)?);
    let field = values.get(2)?.as_integer()?;
    let Some(at) = usize::try_from(field)
        .ok()
        .and_then(|field| field.checked_sub(1))
    else {
        return Some(Ok(Value::Null));
    };
    if delimiter.is_empty() {
        let character = text.chars().nth(at);
        return Some(Ok(character.map_or(Value::Null, |character| {
            Value::Text(character.to_string())
        })));
    }
    let found = FoldedText::new(text).matches(delimiter, at.saturating_add(1));
    let start = match at.checked_sub(1) {
        None => Some(0),
        Some(before) => found.get(before).map(|range| range.end),
    };
    let end = found.get(at).map_or(text.len(), |range| range.start);
    let piece = start.and_then(|start| text.get(start..end));
    Some(Ok(piece.map_or(Value::Null, |piece| Value::Text(piece.to_owned()))))
}

fn position(values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, wanted) = (text_argument(values, 0)?, text_argument(values, 1)?);
    let start = if wanted.is_empty() {
        Some(0)
    } else {
        FoldedText::new(text)
            .matches(wanted, 1)
            .first()
            .map(|range| range.start)
    };
    let position = start
        .and_then(|start| text.get(..start))
        .map_or(0, |head| {
            i64::try_from(head.chars().count())
                .unwrap_or(i64::MAX)
                .saturating_add(1)
        });
    Some(crate::types::integer_value(position, SqlType::Integer))
}

fn translated(values: &[Value]) -> Option<SqlResult<Value>> {
    let [text, from, to] = [0, 1, 2].map(|at| text_argument(values, at));
    let (text, from, to) = (text?, from?, to?);
    let to: Vec<char> = to.chars().collect();
    let mut first_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut key = String::new();
    for (index, character) in from.chars().enumerate() {
        key.clear();
        eggdb_text::case_fold_into(character, &mut key);
        first_of.entry(key.clone()).or_insert(index);
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        key.clear();
        eggdb_text::case_fold_into(character, &mut key);
        match first_of.get(key.as_str()) {
            Some(index) => out.extend(to.get(*index)),
            None => out.push(character),
        }
    }
    Some(Ok(Value::Text(out)))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !values.iter().any(|value| text_of(value).is_some()) {
        return None;
    }
    let regexp = crate::regexp_functions::FUNCTIONS.contains(&name);
    if !regexp && !CASE_INSENSITIVE_STRING_FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let texts = regexp_arguments(name, values);
    if regexp {
        return crate::regexp_functions::function(name, &texts);
    }
    if !subject_is_citext(values) {
        return crate::string_functions::function(name, &texts);
    }
    match name {
        "replace" => replaced(&texts),
        "split_part" => split_part(&texts),
        "strpos" => position(&texts),
        "translate" => translated(&texts),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{compare, compare_with_text, fold, function, regexp_arguments, text_of, value};
    use crate::types::Value;
    use std::cmp::Ordering;

    #[test]
    fn case_variants_compare_equal_with_no_tie_break() {
        assert_eq!(compare("ABC", "abc"), Ordering::Equal);
        assert_eq!(compare("Straße", "STRASSE"), Ordering::Equal);
        assert_eq!(compare("ΣΑΣ", "σας"), compare("σασ", "σας"));
        assert_eq!(compare("\u{212A}elvin", "kELVIN"), Ordering::Equal);
        assert_eq!(compare("", ""), Ordering::Equal);
    }

    #[test]
    fn ordering_follows_the_folded_text() {
        assert_eq!(compare("apple", "Banana"), Ordering::Less);
        assert_eq!(compare("Banana", "apple"), Ordering::Greater);
        assert_eq!(compare("ab", "ABC"), Ordering::Less);
        assert_eq!(compare("Zeta", "zet"), Ordering::Greater);
        assert_eq!(compare("[", "a"), "[".cmp("a"));
        assert_eq!(compare("[", "A"), "[".cmp("a"));
    }

    #[test]
    fn the_ascii_shortcut_agrees_with_the_full_fold() {
        let samples = [
            "", "a", "A", "Az", "aZ", "[", "_", "@", "abc", "ABD", "zz", "Z",
        ];
        for left in samples {
            for right in samples {
                assert_eq!(
                    compare(left, right),
                    fold(left).as_bytes().cmp(fold(right).as_bytes()),
                    "{left} vs {right}"
                );
                assert_eq!(fold(left), eggdb_text::case_fold(left));
            }
        }
    }

    #[test]
    fn values_print_as_written_and_compare_with_text_by_bytes() {
        let stored = value("MiXeD".to_owned());
        assert_eq!(text_of(&stored), Some("MiXeD"));
        assert_eq!(stored.to_text().as_deref(), Some("MiXeD"));
        assert_eq!(
            compare_with_text(&stored, &Value::Text("mixed".to_owned())),
            "MiXeD".cmp("mixed")
        );
        assert_eq!(
            compare_with_text(&Value::Text("MiXeD".to_owned()), &stored),
            Ordering::Equal
        );
        assert_eq!(text_of(&Value::Text("plain".to_owned())), None);
    }

    #[test]
    fn citext_subjects_add_the_case_insensitive_flag_unless_c_is_given() {
        let text = |text: &str| Value::Text(text.to_owned());
        let flags = |name: &str, values: &[Value]| {
            regexp_arguments(name, values)
                .iter()
                .map(|value| value.to_text().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            flags("regexp_match", &[value("A".to_owned()), text("a")]),
            ["A", "a", "i"]
        );
        assert_eq!(
            flags(
                "regexp_match",
                &[value("A".to_owned()), text("a"), text("g")]
            ),
            ["A", "a", "gi"]
        );
        assert_eq!(
            flags(
                "regexp_match",
                &[value("A".to_owned()), text("a"), text("c")]
            ),
            ["A", "a", "c"]
        );
        assert_eq!(
            flags(
                "regexp_replace",
                &[value("A".to_owned()), text("a"), text("b")]
            ),
            ["A", "a", "b", "i"]
        );
        assert_eq!(
            flags(
                "regexp_replace",
                &[
                    value("A".to_owned()),
                    text("a"),
                    text("b"),
                    Value::Integer(1)
                ]
            ),
            ["A", "a", "b", "1"]
        );
        assert_eq!(
            flags("regexp_match", &[text("A"), value("a".to_owned())]),
            ["A", "a"]
        );
        assert_eq!(
            flags("regexp_like", &[value("A".to_owned()), text("a")]),
            ["A", "a"]
        );
    }

    fn called(name: &str, arguments: &[Value]) -> String {
        match function(name, arguments) {
            Some(Ok(value)) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Some(Err(error)) => format!("error {}", error.state().code()),
            None => "none".to_owned(),
        }
    }

    fn texts(subject: &str, rest: &[&str]) -> Vec<Value> {
        let mut values = vec![value(subject.to_owned())];
        values.extend(rest.iter().map(|text| Value::Text((*text).to_owned())));
        values
    }

    #[test]
    fn string_functions_on_citext_fold_case_beyond_ascii_like_equality() {
        assert_eq!(called("replace", &texts("CAFÉ café", &["é", "e"])), "CAFe cafe");
        assert_eq!(called("replace", &texts("Straße", &["SS", "s"])), "Strase");
        assert_eq!(called("replace", &texts("ß", &["s", "x"])), "ß");
        assert_eq!(called("replace", &texts("abc", &["", "x"])), "abc");
        let mut split = texts("aÉbéc", &["é"]);
        split.push(Value::Integer(2));
        assert_eq!(called("split_part", &split), "b");
        split[2] = Value::Integer(3);
        assert_eq!(called("split_part", &split), "c");
        split[2] = Value::Integer(4);
        assert_eq!(called("split_part", &split), "NULL");
        split[2] = Value::Integer(0);
        assert_eq!(called("split_part", &split), "NULL");
        assert_eq!(called("strpos", &texts("Café", &["É"])), "4");
        assert_eq!(called("strpos", &texts("Straße", &["SS"])), "5");
        assert_eq!(called("strpos", &texts("Straße", &["s"])), "1");
        assert_eq!(called("strpos", &texts("ß", &["s"])), "0");
        assert_eq!(called("strpos", &texts("abc", &[""])), "1");
        assert_eq!(called("translate", &texts("ÉCOLE école", &["é", "e"])), "eCOLE ecole");
        assert_eq!(called("translate", &texts("İa", &["İA", "XY"])), "XY");
        assert_eq!(called("translate", &texts("abc", &["B", ""])), "ac");
    }
}
