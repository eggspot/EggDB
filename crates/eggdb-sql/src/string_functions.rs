use crate::ast::Expr;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::types::{SqlType, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ends {
    Leading,
    Trailing,
    Both,
}

fn text_of(value: &Value) -> Option<&str> {
    value.character_text()
}

struct CharacterSet {
    ascii: u128,
    others: Vec<char>,
}

impl CharacterSet {
    fn of(characters: &str) -> Self {
        let mut ascii = 0u128;
        let mut others = Vec::new();
        for character in characters.chars() {
            if character.is_ascii() {
                ascii |= 1u128 << u32::from(character);
            } else {
                others.push(character);
            }
        }
        others.sort_unstable();
        others.dedup();
        Self { ascii, others }
    }

    fn contains(&self, character: char) -> bool {
        if character.is_ascii() {
            return self.ascii & (1u128 << u32::from(character)) != 0;
        }
        self.others.binary_search(&character).is_ok()
    }
}

fn trimmed(text: &str, characters: &str, ends: Ends) -> String {
    let set = CharacterSet::of(characters);
    let in_set = |character: char| set.contains(character);
    let text = if ends != Ends::Trailing {
        text.trim_start_matches(in_set)
    } else {
        text
    };
    let text = if ends != Ends::Leading {
        text.trim_end_matches(in_set)
    } else {
        text
    };
    text.to_owned()
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let ends = match name {
        "btrim" => Ends::Both,
        "ltrim" => Ends::Leading,
        "rtrim" => Ends::Trailing,
        "quote_ident" => {
            return match values {
                [Value::Null] => Some(Ok(Value::Null)),
                [value] => {
                    let text = text_of(value)?;
                    Some(
                        crate::memory::check_value_bytes(
                            text.len().saturating_mul(2).saturating_add(2),
                        )
                        .map(|()| Value::Text(crate::pg_keywords::quote_ident(text))),
                    )
                }
                _ => None,
            };
        }
        _ => return None,
    };
    match values {
        [Value::Null] | [Value::Null, _] | [_, Value::Null] => Some(Ok(Value::Null)),
        [value] => Some(Ok(Value::Text(trimmed(text_of(value)?, " ", ends)))),
        [value, characters] => Some(Ok(Value::Text(trimmed(
            text_of(value)?,
            text_of(characters)?,
            ends,
        )))),
        _ => None,
    }
}

pub(crate) fn collated(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [argument] = arguments else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "COLLATE is missing its operand",
        ));
    };
    let value = evaluator.eval(argument, Some(SqlType::Text))?;
    let sql_type = value
        .sql_type()
        .or_else(|| crate::typing::static_type(argument, evaluator.scope));
    if let Some(sql_type) = sql_type
        && !sql_type.is_character()
    {
        return Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("collations are not supported by type {}", sql_type.name()),
        )
        .at(argument.position));
    }
    Ok(value)
}
