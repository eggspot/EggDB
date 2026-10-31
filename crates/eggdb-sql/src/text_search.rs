use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use eggdb_search::{Config, TsQuery, TsVector};

const DEFAULT_CONFIG: Config = Config::English;

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.clone()),
        other => crate::Dialect::PostgreSql.value_text(other),
    }
}

fn config_named(name: &str) -> SqlResult<Config> {
    Config::named(name).ok_or_else(|| {
        SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("text search configuration \"{name}\" does not exist"),
        )
    })
}

fn syntax_error(text: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("syntax error in tsquery: \"{text}\""),
    )
}

fn vector_of(value: &str) -> TsVector {
    if value.trim_start().starts_with('\'') {
        if let Some(vector) = TsVector::parse(value) {
            return vector;
        }
    }
    TsVector::from_text(DEFAULT_CONFIG, value)
}

fn query_of(value: &str) -> TsQuery {
    if value.contains('\'') {
        if let Some(query) = TsQuery::parse_literal(value) {
            return query;
        }
    }
    TsQuery::plain(DEFAULT_CONFIG, value)
}

/// `vector @@ query`: a text-search vector (or text, read with the default configuration)
/// against a query (or text, read as `plainto_tsquery` reads it).
pub(crate) fn matches(left: &Value, right: &Value) -> SqlResult<Value> {
    let (Some(left), Some(right)) = (text(left), text(right)) else {
        return Ok(Value::Null);
    };
    Ok(Value::Boolean(query_of(&right).matches(&vector_of(&left))))
}

fn with_config(values: &[Value]) -> SqlResult<Option<(Config, String)>> {
    match values {
        [value] => Ok(text(value).map(|text| (DEFAULT_CONFIG, text))),
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

/// The text-search functions: `to_tsvector`, `to_tsquery`, `plainto_tsquery`,
/// `phraseto_tsquery`, `websearch_to_tsquery`, `ts_rank` and `ts_rank_cd`.
pub(crate) fn function(name: &str, values: &[Value]) -> SqlResult<Value> {
    match name {
        "ts_rank" | "ts_rank_cd" => {
            let (vector, query) = match values {
                [vector, query] | [vector, query, _] => (vector, query),
                [_, vector, query] | [_, vector, query, _] => (vector, query),
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
            Ok(Value::Real(eggdb_search::ts_rank(&vector_of(&vector), &query_of(&query)) as f32))
        }
        _ => {
            let Some((config, input)) = with_config(values)? else {
                return Ok(Value::Null);
            };
            let written = match name {
                "to_tsvector" => TsVector::from_text(config, &input).to_string(),
                "to_tsquery" => TsQuery::parse(config, &input).ok_or_else(|| syntax_error(&input))?.to_string(),
                "plainto_tsquery" => TsQuery::plain(config, &input).to_string(),
                "phraseto_tsquery" => TsQuery::phrase(config, &input).to_string(),
                _ => TsQuery::web(config, &input).to_string(),
            };
            Ok(Value::Text(written))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &[&str]) -> Value {
        let values: Vec<Value> = arguments.iter().map(|text| Value::Text((*text).to_owned())).collect();
        function(name, &values).unwrap()
    }

    #[test]
    fn functions_give_postgresql_text_forms_and_match() {
        assert_eq!(
            call("to_tsvector", &["english", "The fat cats ate fat rats"]),
            Value::Text("'ate':4 'cat':3 'fat':2,5 'rat':6".to_owned())
        );
        assert_eq!(call("to_tsquery", &["fat & rats"]), Value::Text("'fat' & 'rat'".to_owned()));
        let vector = call("to_tsvector", &["a fat rat"]);
        let query = call("plainto_tsquery", &["fat rats"]);
        assert_eq!(matches(&vector, &query).unwrap(), Value::Boolean(true));
        assert_eq!(
            matches(&Value::Text("a cat".to_owned()), &Value::Text("rat".to_owned())).unwrap(),
            Value::Boolean(false)
        );
        assert!(function("to_tsquery", &[Value::Text("fat &".to_owned())]).is_err());
        assert!(function("to_tsvector", &[Value::Text("klingon".to_owned()), Value::Text("x".to_owned())]).is_err());
        assert_eq!(matches(&Value::Null, &query).unwrap(), Value::Null);
    }
}
