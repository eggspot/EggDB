//! The `hstore` type compatible with PostgreSQL's extension of that name: a set of text keys,
//! each with a text value or `NULL`, kept sorted the way PostgreSQL keeps them (by key length,
//! then by bytes) with the first of duplicate keys winning (DNEW-F-citext).

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedValue, SqlType, Value};
use std::cmp::Ordering;

pub(crate) const HSTORE_OID: u32 = 9002;
pub(crate) const MAX_KEY_BYTES: usize = 65_535;

/// A value of type `hstore`: text keys, each with a text value or `NULL`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HstoreValue {
    pairs: Vec<(String, Option<String>)>,
}

impl HstoreValue {
    /// Builds the value from pairs in any order; of pairs with the same key, the first is kept.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, Option<String>)>) -> Self {
        let mut pairs: Vec<_> = pairs.into_iter().collect();
        pairs.sort_by(|a, b| key_order(&a.0, &b.0));
        pairs.dedup_by(|later, earlier| later.0 == earlier.0);
        Self { pairs }
    }

    pub(crate) fn from_sorted(pairs: Vec<(String, Option<String>)>) -> Option<Self> {
        let ascending = pairs
            .windows(2)
            .all(|pair| key_order(&pair[0].0, &pair[1].0) == Ordering::Less);
        (ascending && pairs.iter().all(|(key, _)| key.len() <= MAX_KEY_BYTES))
            .then_some(Self { pairs })
    }

    /// The pairs, sorted by key length and then by key bytes.
    pub fn pairs(&self) -> &[(String, Option<String>)] {
        &self.pairs
    }

    /// The value of `key`: `None` when the key is absent, `Some(None)` when its value is `NULL`.
    pub fn get(&self, key: &str) -> Option<Option<&str>> {
        self.pairs
            .binary_search_by(|(known, _)| key_order(known, key))
            .ok()
            .and_then(|at| self.pairs.get(at))
            .map(|(_, value)| value.as_deref())
    }

    pub(crate) fn held_bytes(&self) -> usize {
        self.pairs
            .iter()
            .map(|(key, value)| {
                key.len()
                    .saturating_add(value.as_ref().map_or(0, String::len))
            })
            .fold(0, usize::saturating_add)
    }
}

pub(crate) fn key_order(left: &str, right: &str) -> Ordering {
    left.len()
        .cmp(&right.len())
        .then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

pub(crate) fn value(hstore: HstoreValue) -> Value {
    Value::Defined(Box::new(DefinedValue::Hstore(hstore)))
}

pub(crate) fn hstore_of(value: &Value) -> Option<&HstoreValue> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Hstore(hstore) => Some(hstore),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn compare(left: &HstoreValue, right: &HstoreValue) -> Ordering {
    for ((left_key, left_value), (right_key, right_value)) in left.pairs.iter().zip(&right.pairs) {
        let ordering =
            key_order(left_key, right_key).then_with(|| match (left_value, right_value) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(a), Some(b)) => a.as_bytes().cmp(b.as_bytes()),
            });
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.pairs.len().cmp(&right.pairs.len())
}

fn push_quoted(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
}

pub(crate) fn to_text(hstore: &HstoreValue) -> String {
    let mut out = String::new();
    for (at, (key, value)) in hstore.pairs.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        push_quoted(&mut out, key);
        out.push_str("=>");
        match value {
            Some(value) => push_quoted(&mut out, value),
            None => out.push_str("NULL"),
        }
    }
    out
}

pub(crate) fn to_json(hstore: &HstoreValue) -> String {
    let mut out = String::from("{");
    for (at, (key, value)) in hstore.pairs.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        crate::json::write_string(key, &mut out);
        out.push_str(": ");
        match value {
            Some(value) => crate::json::write_string(value, &mut out),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    out
}

pub(crate) fn convert_to_json(value: &Value, target: SqlType) -> Option<SqlResult<Value>> {
    let hstore = hstore_of(value)?;
    match target {
        SqlType::Json => Some(Ok(Value::Json(to_json(hstore)))),
        SqlType::Jsonb => Some(crate::json::normalize(&to_json(hstore)).map(Value::Jsonb)),
        _ => None,
    }
}

fn syntax_error(text: &str, at: usize) -> SqlError {
    match text.get(at..).and_then(|rest| rest.chars().next()) {
        Some(found) => SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("syntax error in hstore, near \"{found}\" at position {at}"),
        ),
        None => SqlError::new(
            SqlState::SYNTAX_ERROR,
            "syntax error in hstore: unexpected end of string",
        ),
    }
}

fn key_too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "string too long for hstore key",
    )
}

struct Parser<'t> {
    text: &'t str,
    at: usize,
}

enum Token {
    Quoted(String),
    Bare(String),
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.text
            .get(self.at..)
            .and_then(|rest| rest.chars().next())
    }

    fn advance(&mut self, character: char) {
        self.at += character.len_utf8();
    }

    fn skip_space(&mut self) {
        while let Some(character) = self.peek() {
            if !character.is_ascii_whitespace() {
                break;
            }
            self.advance(character);
        }
    }

    fn escaped(&mut self, out: &mut String) -> SqlResult<()> {
        self.advance('\\');
        let Some(character) = self.peek() else {
            return Err(syntax_error(self.text, self.at));
        };
        out.push(character);
        self.advance(character);
        Ok(())
    }

    fn token(&mut self, is_key: bool) -> SqlResult<Token> {
        match self.peek() {
            Some('"') => {
                self.advance('"');
                let mut out = String::new();
                loop {
                    match self.peek() {
                        None => return Err(syntax_error(self.text, self.at)),
                        Some('"') => {
                            self.advance('"');
                            return Ok(Token::Quoted(out));
                        }
                        Some('\\') => self.escaped(&mut out)?,
                        Some(character) => {
                            out.push(character);
                            self.advance(character);
                        }
                    }
                }
            }
            None | Some(',') => Err(syntax_error(self.text, self.at)),
            Some('=') if is_key => Err(syntax_error(self.text, self.at)),
            Some(_) => {
                let mut out = String::new();
                while let Some(character) = self.peek() {
                    let ends = character.is_ascii_whitespace()
                        || character == ','
                        || character == '"'
                        || (is_key && character == '=');
                    if ends {
                        break;
                    }
                    if character == '\\' {
                        self.escaped(&mut out)?;
                    } else {
                        out.push(character);
                        self.advance(character);
                    }
                }
                Ok(Token::Bare(out))
            }
        }
    }

    fn arrow(&mut self) -> SqlResult<()> {
        self.skip_space();
        for expected in ['=', '>'] {
            if self.peek() != Some(expected) {
                return Err(syntax_error(self.text, self.at));
            }
            self.advance(expected);
        }
        self.skip_space();
        Ok(())
    }

    fn pairs(&mut self) -> SqlResult<Vec<(String, Option<String>)>> {
        let mut pairs = Vec::new();
        loop {
            self.skip_space();
            if self.peek().is_none() {
                return Ok(pairs);
            }
            let key = match self.token(true)? {
                Token::Quoted(key) | Token::Bare(key) => key,
            };
            if key.len() > MAX_KEY_BYTES {
                return Err(key_too_long());
            }
            self.arrow()?;
            let value = match self.token(false)? {
                Token::Bare(bare) if bare.eq_ignore_ascii_case("null") => None,
                Token::Quoted(value) | Token::Bare(value) => Some(value),
            };
            pairs.push((key, value));
            self.skip_space();
            match self.peek() {
                None => return Ok(pairs),
                Some(',') => self.advance(','),
                Some(_) => return Err(syntax_error(self.text, self.at)),
            }
        }
    }
}

pub(crate) fn checked(pairs: Vec<(String, Option<String>)>) -> SqlResult<HstoreValue> {
    if pairs.iter().any(|(key, _)| key.len() > MAX_KEY_BYTES) {
        return Err(key_too_long());
    }
    Ok(HstoreValue::from_pairs(pairs))
}

pub(crate) fn parse_text(text: &str) -> SqlResult<HstoreValue> {
    let mut parser = Parser { text, at: 0 };
    parser.pairs().map(HstoreValue::from_pairs)
}

#[cfg(test)]
mod tests {
    use super::{HstoreValue, MAX_KEY_BYTES, compare, parse_text, to_json, to_text};
    use std::cmp::Ordering;

    fn pairs(text: &str) -> Vec<(String, Option<String>)> {
        parse_text(text).unwrap().pairs().to_vec()
    }

    fn pair(key: &str, value: Option<&str>) -> (String, Option<String>) {
        (key.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn pairs_sort_by_key_length_then_bytes_and_the_first_duplicate_wins() {
        assert_eq!(
            pairs("bb=>2, a=>1, ab=>3, a=>9"),
            vec![
                pair("a", Some("1")),
                pair("ab", Some("3")),
                pair("bb", Some("2"))
            ]
        );
        assert_eq!(pairs(""), Vec::new());
        assert_eq!(pairs("   "), Vec::new());
    }

    #[test]
    fn quoted_and_bare_tokens_escapes_and_null_parse_as_postgresql_reads_them() {
        assert_eq!(
            pairs(r#""a b"=>"x\"y", c=>NULL, d=>"NULL", e => null ,f=>\,g"#),
            vec![
                pair("c", None),
                pair("d", Some("NULL")),
                pair("e", None),
                pair("f", Some(",g")),
                pair("a b", Some("x\"y")),
            ]
        );
        assert_eq!(pairs("a=>b=c"), vec![pair("a", Some("b=c"))]);
        assert_eq!(pairs("a=>1,"), vec![pair("a", Some("1"))]);
        assert_eq!(
            pairs("ключ=>значение"),
            vec![pair("ключ", Some("значение"))]
        );
    }

    #[test]
    fn text_form_quotes_every_key_and_value_and_round_trips() {
        let parsed = parse_text(r#"b=>NULL, a=>1, "q\"\\"=>"v""#).unwrap();
        let text = to_text(&parsed);
        assert_eq!(text, r#""a"=>"1", "b"=>NULL, "q\"\\"=>"v""#);
        assert_eq!(parse_text(&text).unwrap(), parsed);
        assert_eq!(to_text(&HstoreValue::default()), "");
    }

    #[test]
    fn json_form_names_each_key_with_its_value_or_null() {
        let parsed = parse_text(r#"a=>1, b=>NULL, "c\""=>x"#).unwrap();
        assert_eq!(to_json(&parsed), r#"{"a": "1", "b": null, "c\"": "x"}"#);
        assert_eq!(to_json(&HstoreValue::default()), "{}");
    }

    #[test]
    fn lookups_find_values_and_nulls() {
        let parsed = parse_text("a=>1, b=>NULL").unwrap();
        assert_eq!(parsed.get("a"), Some(Some("1")));
        assert_eq!(parsed.get("b"), Some(None));
        assert_eq!(parsed.get("c"), None);
    }

    #[test]
    fn comparison_is_consistent_with_equality() {
        let a = parse_text("a=>1").unwrap();
        let b = parse_text("a=>2").unwrap();
        let ab = parse_text("a=>1, b=>1").unwrap();
        let null = parse_text("a=>NULL").unwrap();
        assert_eq!(compare(&a, &parse_text("a=>1").unwrap()), Ordering::Equal);
        assert_eq!(compare(&a, &b), Ordering::Less);
        assert_eq!(compare(&a, &ab), Ordering::Less);
        assert_eq!(compare(&null, &a), Ordering::Less);
        assert_eq!(compare(&b, &a), Ordering::Greater);
    }

    #[test]
    fn hostile_input_returns_errors_never_panics() {
        for bad in [
            "a",
            "a=>",
            "=>1",
            "a=1",
            "a>1",
            "a=>1 b=>2",
            "\"a",
            "a=>\"1",
            "a\\",
            "a=>1\\",
            ",",
            "a=>1,,b=>2",
            "a=>,",
            "\"a\"\"b\"=>1",
            "a=>\"1\"x",
            "é=>",
            "a=>1 ,, ",
        ] {
            assert!(parse_text(bad).is_err(), "{bad}");
        }
        let long_key = format!("{}=>1", "k".repeat(MAX_KEY_BYTES + 1));
        assert_eq!(parse_text(&long_key).unwrap_err().state().code(), "54000");
        assert_eq!(parse_text("a b").unwrap_err().state().code(), "42601");
        let samples = ["a=>1, \"b\"=>NULL", "\"x\\\"\"=>\"y\\\\\"", "ü=>ö,"];
        for sample in samples {
            for cut in 0..=sample.len() {
                if let Some(prefix) = sample.get(..cut) {
                    let _ = parse_text(prefix);
                }
            }
        }
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let alphabet = ['a', '"', '\\', '=', '>', ',', ' ', 'N', 'U', 'L', 'é', '\0'];
        for _ in 0..2_000 {
            let mut text = String::new();
            for _ in 0..(state % 24) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                text.push(alphabet[(state % alphabet.len() as u64) as usize]);
            }
            if let Ok(parsed) = parse_text(&text) {
                assert_eq!(parse_text(&to_text(&parsed)).unwrap(), parsed);
            }
        }
    }

    #[test]
    fn sorted_pairs_are_checked_when_decoded() {
        assert!(HstoreValue::from_sorted(vec![pair("a", None), pair("ab", None)]).is_some());
        assert!(HstoreValue::from_sorted(vec![pair("ab", None), pair("a", None)]).is_none());
        assert!(HstoreValue::from_sorted(vec![pair("a", None), pair("a", None)]).is_none());
        assert!(HstoreValue::from_sorted(Vec::new()).is_some());
    }
}
