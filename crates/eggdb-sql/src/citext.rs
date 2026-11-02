//! The `citext` type compatible with PostgreSQL's extension of that name: text that compares,
//! orders, groups and indexes by one full case fold (DNEW-F-citext), with no tie-break, so
//! `'A'` and `'a'` are the same value to every operation that compares.

use crate::types::{DefinedType, DefinedValue, Value};
use std::cmp::Ordering;

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

#[cfg(test)]
mod tests {
    use super::{compare, compare_with_text, fold, text_of, value};
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
}
