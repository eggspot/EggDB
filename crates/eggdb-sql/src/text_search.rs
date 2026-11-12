use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use eggdb_search::{Analyzer, Config, HeadlineOptions, Lexer, TsQuery, TsVector, Weight};
use std::sync::Arc;

const DEFAULT_CONFIG: Config = Config::English;

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.clone()),
        other => crate::Dialect::PostgreSql.value_text(other),
    }
}

enum Chosen {
    BuiltIn(Config),
    Defined(Arc<Analyzer>),
}

fn config_named(name: &str) -> SqlResult<Chosen> {
    if let Some(config) = Config::named(name) {
        return Ok(Chosen::BuiltIn(config));
    }
    match crate::text_search_catalog::analyzer(name) {
        Some(analyzer) => analyzer.map(Chosen::Defined),
        None => Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("text search configuration \"{name}\" does not exist"),
        )),
    }
}

fn syntax_error(text: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("syntax error in tsquery: \"{text}\""),
    )
}

fn vector_of(value: &str) -> TsVector {
    if value.trim_start().starts_with('\'')
        && let Some(vector) = TsVector::parse(value)
    {
        return vector;
    }
    TsVector::from_text(DEFAULT_CONFIG, value)
}

fn query_of(value: &str) -> TsQuery {
    if value.contains('\'')
        && let Some(query) = TsQuery::parse_literal(value)
    {
        return query;
    }
    TsQuery::plain(DEFAULT_CONFIG, value)
}

/// `vector @@ query`: a text-search vector (or text, read with the default configuration)
/// against a query (or text, read as `plainto_tsquery` reads it).
pub(crate) fn matches(left: &Value, right: &Value) -> SqlResult<Value> {
    let textual = |value: &Value| matches!(value, Value::Null | Value::Text(_) | Value::Bpchar(_));
    if !textual(left) || !textual(right) {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "operator does not exist: @@ matches a tsvector or text with a tsquery or text",
        ));
    }
    let (Some(left), Some(right)) = (text(left), text(right)) else {
        return Ok(Value::Null);
    };
    Ok(Value::Boolean(query_of(&right).matches(&vector_of(&left))))
}

fn with_config(values: &[Value]) -> SqlResult<Option<(Chosen, String)>> {
    match values {
        [value] => Ok(text(value).map(|text| (Chosen::BuiltIn(DEFAULT_CONFIG), text))),
        [config, value] => match (text(config), text(value)) {
            (Some(config), Some(value)) => Ok(Some((config_named(&config)?, value))),
            _ => Ok(None),
        },
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "text search functions take an optional configuration and one text",
        )),
    }
}

fn rank_weights(value: &Value) -> SqlResult<[f64; 4]> {
    let numbers: Vec<Option<f64>> = match value {
        Value::Array(array) => array
            .items
            .iter()
            .map(|item| text(item).and_then(|text| text.trim().parse().ok()))
            .collect(),
        other => text(other)
            .unwrap_or_default()
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .map(|item| item.trim().parse().ok())
            .collect(),
    };
    let refused = || {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "array of weight must be one-dimensional with four non-null values between 0 and 1",
        )
    };
    let numbers: Vec<f64> = numbers.into_iter().collect::<Option<Vec<f64>>>().ok_or_else(refused)?;
    let [d, c, b, a] = numbers.as_slice() else {
        return Err(refused());
    };
    let weights = [*d, *c, *b, *a];
    if weights.iter().any(|weight| !(0.0..=1.0).contains(weight)) {
        return Err(refused());
    }
    Ok(weights)
}

fn setweight(values: &[Value]) -> SqlResult<Value> {
    let (vector, weight, only) = match values {
        [vector, weight] => (vector, weight, None),
        [vector, weight, lexemes] => (vector, weight, Some(lexemes)),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                "function setweight takes a tsvector, a weight and optionally an array of lexemes",
            ));
        }
    };
    let (Some(vector), Some(weight)) = (text(vector), text(weight)) else {
        return Ok(Value::Null);
    };
    let mut letters = weight.chars();
    let (Some(letter), None) = (letters.next(), letters.next()) else {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unrecognized weight: \"{weight}\""),
        ));
    };
    let weight = Weight::named(letter).ok_or_else(|| {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unrecognized weight: {}", u32::from(letter)),
        )
    })?;
    let listed: Option<Vec<String>> = match only {
        None => None,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Array(array)) => Some(array.items.iter().filter_map(text).collect()),
        Some(other) => Some(
            text(other)
                .unwrap_or_default()
                .trim()
                .trim_start_matches('{')
                .trim_end_matches('}')
                .split(',')
                .map(|item| item.trim().trim_matches('"').to_owned())
                .filter(|item| !item.is_empty())
                .collect(),
        ),
    };
    Ok(Value::Text(
        vector_of(&vector)
            .with_weight(weight, listed.as_deref())
            .to_string(),
    ))
}

fn ts_headline(values: &[Value]) -> SqlResult<Value> {
    let options_like = |value: &Value| text(value).is_some_and(|text| text.contains('='));
    let (config, document, query, options) = match values {
        [document, query] => (None, document, query, None),
        [document, query, options] if options_like(options) => {
            (None, document, query, Some(options))
        }
        [config, document, query] => (Some(config), document, query, None),
        [config, document, query, options] => (Some(config), document, query, Some(options)),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                "function ts_headline takes an optional configuration, a document, a tsquery and optional options",
            ));
        }
    };
    let config = match config.map(text) {
        None => Chosen::BuiltIn(DEFAULT_CONFIG),
        Some(None) => return Ok(Value::Null),
        Some(Some(name)) => config_named(&name)?,
    };
    let (Some(document), Some(query)) = (text(document), text(query)) else {
        return Ok(Value::Null);
    };
    let options = match options.map(text) {
        None => HeadlineOptions::default(),
        Some(None) => return Ok(Value::Null),
        Some(Some(written)) => HeadlineOptions::parse(&written)
            .map_err(|message| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message))?,
    };
    let query = query_of(&query);
    Ok(Value::Text(match &config {
        Chosen::BuiltIn(config) => eggdb_search::headline(*config, &document, &query, &options),
        Chosen::Defined(analyzer) => eggdb_search::headline(&**analyzer, &document, &query, &options),
    }))
}

fn written(name: &str, config: impl Lexer, input: &str) -> SqlResult<String> {
    Ok(match name {
        "to_tsvector" => TsVector::from_text(config, input).to_string(),
        "to_tsquery" => TsQuery::parse(config, input)
            .ok_or_else(|| syntax_error(input))?
            .to_string(),
        "plainto_tsquery" => TsQuery::plain(config, input).to_string(),
        "phraseto_tsquery" => TsQuery::phrase(config, input).to_string(),
        _ => TsQuery::web(config, input).to_string(),
    })
}

/// The text-search functions: `to_tsvector`, `to_tsquery`, `plainto_tsquery`,
/// `phraseto_tsquery`, `websearch_to_tsquery`, `ts_rank` and `ts_rank_cd`.
pub(crate) fn function(name: &str, values: &[Value]) -> SqlResult<Value> {
    match name {
        "setweight" => setweight(values),
        "ts_headline" => ts_headline(values),
        "ts_rank" | "ts_rank_cd" => {
            let weights_given =
                |value: &Value| match value {
                    Value::Array(_) => true,
                    Value::Text(text) => text.trim_start().starts_with('{'),
                    _ => false,
                };
            let (weights, vector, query) = match values {
                [vector, query] => (None, vector, query),
                [weights, vector, query] | [weights, vector, query, _] if weights_given(weights) => {
                    (Some(rank_weights(weights)?), vector, query)
                }
                [vector, query, _] => (None, vector, query),
                _ => {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("function {name} takes a tsvector and a tsquery"),
                    ));
                }
            };
            let (Some(vector), Some(query)) = (text(vector), text(query)) else {
                return Ok(Value::Null);
            };
            let weights = weights.unwrap_or(eggdb_search::DEFAULT_RANK_WEIGHTS);
            Ok(Value::Real(eggdb_search::ts_rank_weighted(
                &vector_of(&vector),
                &query_of(&query),
                weights,
            ) as f32))
        }
        _ => {
            let Some((config, input)) = with_config(values)? else {
                return Ok(Value::Null);
            };
            let written = match &config {
                Chosen::BuiltIn(config) => written(name, *config, &input)?,
                Chosen::Defined(analyzer) => written(name, &**analyzer, &input)?,
            };
            Ok(Value::Text(written))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &[&str]) -> Value {
        let values: Vec<Value> = arguments
            .iter()
            .map(|text| Value::Text((*text).to_owned()))
            .collect();
        function(name, &values).unwrap()
    }

    #[test]
    fn functions_give_postgresql_text_forms_and_match() {
        assert_eq!(
            call("to_tsvector", &["english", "The fat cats ate fat rats"]),
            Value::Text("'ate':4 'cat':3 'fat':2,5 'rat':6".to_owned())
        );
        assert_eq!(
            call("to_tsquery", &["fat & rats"]),
            Value::Text("'fat' & 'rat'".to_owned())
        );
        let vector = call("to_tsvector", &["a fat rat"]);
        let query = call("plainto_tsquery", &["fat rats"]);
        assert_eq!(matches(&vector, &query).unwrap(), Value::Boolean(true));
        assert_eq!(
            matches(
                &Value::Text("a cat".to_owned()),
                &Value::Text("rat".to_owned())
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert!(function("to_tsquery", &[Value::Text("fat &".to_owned())]).is_err());
        assert!(
            function(
                "to_tsvector",
                &[
                    Value::Text("klingon".to_owned()),
                    Value::Text("x".to_owned())
                ]
            )
            .is_err()
        );
        assert_eq!(matches(&Value::Null, &query).unwrap(), Value::Null);
    }
}
