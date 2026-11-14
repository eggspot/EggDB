//! PostgreSQL's `tsvector`, `tsquery` and `jsonpath` as types of their own (D195, D289): each
//! value is checked when it is made and kept in its canonical text form, so it prints, compares,
//! stores and travels over the wire as PostgreSQL's would, and functions taking text still read it.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedType, DefinedValue, Value};
use eggdb_search::{TsQuery, TsVector};

pub(crate) const TSVECTOR_OID: u32 = 3614;
pub(crate) const TSQUERY_OID: u32 = 3615;
pub(crate) const JSONPATH_OID: u32 = 4072;

fn invalid(type_name: &str, text: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("invalid input syntax for type {type_name}: \"{text}\""),
    )
}

fn quoted_lexeme(word: &str) -> String {
    format!("'{}'", word.replace('\'', "''"))
}

/// The canonical text of a `tsvector` written as `text`: quoted lexemes with positions as
/// PostgreSQL prints them, or bare words, which become lexemes without positions.
pub(crate) fn tsvector_text(text: &str) -> SqlResult<String> {
    if let Some(vector) = TsVector::parse(text) {
        return Ok(vector.to_string());
    }
    let mut words: Vec<&str> = text.split_whitespace().collect();
    words.sort_unstable();
    words.dedup();
    let quoted = words.iter().map(|word| quoted_lexeme(word)).collect::<Vec<_>>().join(" ");
    TsVector::parse(&quoted)
        .map(|vector| vector.to_string())
        .ok_or_else(|| invalid("tsvector", text))
}

/// The canonical text of a `tsquery` written as `text`.
pub(crate) fn tsquery_text(text: &str) -> SqlResult<String> {
    TsQuery::parse_literal(text)
        .or_else(|| {
            let trimmed = text.trim();
            (!trimmed.is_empty() && !trimmed.contains(char::is_whitespace))
                .then(|| TsQuery::parse_literal(&quoted_lexeme(trimmed)))
                .flatten()
        })
        .map(|query| query.to_string())
        .ok_or_else(|| invalid("tsquery", text))
}

/// A `jsonpath` written as `text`, checked by the path parser and kept as written.
pub(crate) fn jsonpath_text(text: &str) -> SqlResult<String> {
    crate::json_path::parse(text)?;
    Ok(text.trim().to_owned())
}

/// The value of base type `defined` (one of these three) written as `text`.
pub(crate) fn parse(text: &str, defined: DefinedType) -> Option<SqlResult<Value>> {
    let made = if defined == DefinedType::TSVECTOR {
        tsvector_text(text).map(DefinedValue::TsVector)
    } else if defined == DefinedType::TSQUERY {
        tsquery_text(text).map(DefinedValue::TsQuery)
    } else if defined == DefinedType::JSONPATH {
        jsonpath_text(text).map(DefinedValue::JsonPath)
    } else {
        return None;
    };
    Some(made.map(|value| Value::Defined(Box::new(value))))
}

/// Whether `defined` is one of these three types.
pub(crate) fn is_text_type(defined: DefinedType) -> bool {
    defined == DefinedType::TSVECTOR || defined == DefinedType::TSQUERY || defined == DefinedType::JSONPATH
}

pub(crate) fn tsvector_value(text: String) -> Value {
    Value::Defined(Box::new(DefinedValue::TsVector(text)))
}

pub(crate) fn tsquery_value(text: String) -> Value {
    Value::Defined(Box::new(DefinedValue::TsQuery(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_and_queries_take_their_canonical_forms() {
        assert_eq!(tsvector_text("a fat cat sat on a mat").unwrap(), "'a' 'cat' 'fat' 'mat' 'on' 'sat'");
        assert_eq!(tsvector_text("'b':2 'a':1").unwrap(), "'a':1 'b':2");
        assert!(tsvector_text("'open").is_err());
        assert_eq!(tsquery_text("fat & rat").unwrap(), "'fat' & 'rat'");
        assert!(tsquery_text("fat &").is_err());
        assert_eq!(jsonpath_text(" $.a ").unwrap(), "$.a");
        assert!(jsonpath_text("$.").is_err());
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn columns_of_these_types_keep_them_and_their_values_work_as_before() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE docs (id integer PRIMARY KEY, v tsvector, q tsquery, p jsonpath);
             INSERT INTO docs VALUES (1, 'fat cat', 'fat & cat', '$.a'), (2, to_tsvector('english', 'rats ran'), plainto_tsquery('english', 'rat'), '$.b[*]')",
        );
        assert_eq!(
            answer(&mut session, "SELECT pg_typeof(v), pg_typeof(q), pg_typeof(p) FROM docs WHERE id = 1"),
            "pg_typeof|pg_typeof|pg_typeof\ntsvector|tsquery|jsonpath\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT id FROM docs WHERE v @@ q ORDER BY id"),
            "id\n1\n2\nSELECT 2"
        );
        assert_eq!(
            answer(&mut session, "SELECT v FROM docs WHERE id = 1"),
            "v\n'cat' 'fat'\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT '{\"b\": [1, 2]}'::jsonb @? p FROM docs WHERE id = 2"),
            "?column?\nt\nSELECT 1"
        );
    }
}
