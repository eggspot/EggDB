use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_functions::{out_of_range, text_of, text_or_null, whole_of};
use crate::regex::{CaseSensitivity, Options, Regex, Span, compiled_with};
use crate::regexp_functions::{each_match, replacement};
use crate::types::Value;
use std::rc::Rc;

const MOST_SUBEXPRESSION: i64 = 9;

fn illegal_argument() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "illegal argument for function",
    )
    .with_oracle_code(1760)
}

fn options_of(match_parameter: Option<String>) -> SqlResult<Options> {
    let mut options = Options {
        leftmost_first: true,
        ..Options::default()
    };
    let mut dot_matches_newline = false;
    for letter in match_parameter.unwrap_or_default().chars() {
        match letter {
            'i' => options.case = CaseSensitivity::Insensitive,
            'c' => options.case = CaseSensitivity::Sensitive,
            'n' => dot_matches_newline = true,
            'm' => options.line_anchors = true,
            'x' => options.expanded = true,
            _ => return Err(illegal_argument()),
        }
    }
    options.newline_excluded = !dot_matches_newline;
    Ok(options)
}

fn given_text(value: Option<&Value>) -> Option<String> {
    value.and_then(text_of).filter(|text| !text.is_empty())
}

fn number_at(values: &[Value], at: usize, default: i64) -> SqlResult<Option<i64>> {
    match values.get(at) {
        None => Ok(Some(default)),
        Some(value) => whole_of(value),
    }
}

fn at_least(number: i64, least: i64) -> SqlResult<i64> {
    if number < least {
        return Err(out_of_range(&number.to_string()));
    }
    Ok(number)
}

struct Search {
    chars: Vec<char>,
    regex: Rc<Regex>,
    start: usize,
}

impl Search {
    fn prepared(
        values: &[Value],
        position_at: usize,
        match_parameter_at: usize,
    ) -> SqlResult<Option<Self>> {
        let (Some(source), Some(pattern)) = (given_text(values.first()), given_text(values.get(1)))
        else {
            return Ok(None);
        };
        let Some(position) = number_at(values, position_at, 1)? else {
            return Ok(None);
        };
        let position = at_least(position, 1)?;
        let options = options_of(values.get(match_parameter_at).and_then(text_of))?;
        let regex = compiled_with(&pattern, options)?;
        Ok(Some(Self {
            chars: source.chars().collect(),
            regex,
            start: usize::try_from(position - 1).unwrap_or(usize::MAX),
        }))
    }

    fn nth(&self, occurrence: i64) -> SqlResult<Option<Vec<Option<Span>>>> {
        if self.start > self.chars.len() {
            return Ok(None);
        }
        let mut seen = 0;
        let mut found = None;
        each_match(&self.regex, &self.chars, self.start, true, &mut |spans| {
            seen += 1;
            if seen == occurrence {
                found = Some(spans.to_vec());
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    fn count(&self) -> SqlResult<i64> {
        let mut count = 0;
        if self.start <= self.chars.len() {
            each_match(&self.regex, &self.chars, self.start, true, &mut |_| {
                count += 1;
                Ok(true)
            })?;
        }
        Ok(count)
    }
}

fn subexpression_at(values: &[Value], at: usize) -> SqlResult<Option<usize>> {
    let Some(number) = number_at(values, at, 0)? else {
        return Ok(None);
    };
    if !(0..=MOST_SUBEXPRESSION).contains(&number) {
        return Err(out_of_range(&number.to_string()));
    }
    Ok(usize::try_from(number).ok())
}

fn chosen(spans: Option<Vec<Option<Span>>>, subexpression: usize) -> Option<Span> {
    spans.and_then(|spans| spans.get(subexpression).copied().flatten())
}

fn like(values: &[Value]) -> SqlResult<Value> {
    let Some(search) = Search::prepared(values, usize::MAX, 2)? else {
        return Ok(Value::Null);
    };
    Ok(Value::Boolean(search.nth(1)?.is_some()))
}

fn count(values: &[Value]) -> SqlResult<Value> {
    let Some(search) = Search::prepared(values, 2, 3)? else {
        return Ok(Value::Null);
    };
    Ok(Value::BigInt(search.count()?))
}

fn substring(values: &[Value]) -> SqlResult<Value> {
    let Some(search) = Search::prepared(values, 2, 4)? else {
        return Ok(Value::Null);
    };
    let (Some(occurrence), Some(subexpression)) =
        (number_at(values, 3, 1)?, subexpression_at(values, 5)?)
    else {
        return Ok(Value::Null);
    };
    let occurrence = at_least(occurrence, 1)?;
    Ok(match chosen(search.nth(occurrence)?, subexpression) {
        Some((begin, end)) => {
            text_or_null(search.chars.get(begin..end).unwrap_or(&[]).iter().collect())
        }
        None => Value::Null,
    })
}

fn position(values: &[Value]) -> SqlResult<Value> {
    let Some(search) = Search::prepared(values, 2, 5)? else {
        return Ok(Value::Null);
    };
    let (Some(occurrence), Some(return_option), Some(subexpression)) = (
        number_at(values, 3, 1)?,
        number_at(values, 4, 0)?,
        subexpression_at(values, 6)?,
    ) else {
        return Ok(Value::Null);
    };
    let occurrence = at_least(occurrence, 1)?;
    let after_match = at_least(return_option, 0)? > 0;
    let found = chosen(search.nth(occurrence)?, subexpression).map_or(0, |(begin, end)| {
        let at = if after_match { end } else { begin };
        i64::try_from(at).map_or(i64::MAX, |at| at.saturating_add(1))
    });
    Ok(Value::BigInt(found))
}

fn replaced(values: &[Value]) -> SqlResult<Value> {
    let Some(source) = given_text(values.first()) else {
        return Ok(Value::Null);
    };
    let Some(pattern) = given_text(values.get(1)) else {
        return Ok(Value::Text(source));
    };
    let template: Vec<char> = given_text(values.get(2))
        .unwrap_or_default()
        .chars()
        .collect();
    let (Some(position), Some(occurrence)) = (number_at(values, 3, 1)?, number_at(values, 4, 0)?)
    else {
        return Ok(Value::Null);
    };
    let position = at_least(position, 1)?;
    let occurrence = at_least(occurrence, 0)?;
    let options = options_of(values.get(5).and_then(text_of))?;
    let regex = compiled_with(&pattern, options)?;
    let chars: Vec<char> = source.chars().collect();
    let from = usize::try_from(position - 1)
        .unwrap_or(usize::MAX)
        .min(chars.len().saturating_add(1));
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    let mut seen = 0;
    each_match(&regex, &chars, from, true, &mut |spans| {
        let Some((begin, end)) = spans.first().copied().flatten() else {
            return Ok(false);
        };
        seen += 1;
        if occurrence != 0 && seen != occurrence {
            return Ok(true);
        }
        out.extend(chars.get(copied..begin).unwrap_or(&[]));
        replacement(&mut out, &template, &chars, spans)?;
        copied = end;
        Ok(occurrence == 0)
    })?;
    out.extend(chars.get(copied..).unwrap_or(&[]));
    crate::memory::check_value_bytes(out.len())?;
    Ok(text_or_null(out))
}

/// Evaluates one of Oracle's `REGEXP_*` functions, under its internal name, over its evaluated
/// arguments in Oracle's order.
pub(crate) fn call(name: &str, values: &[Value]) -> SqlResult<Value> {
    match name {
        "oracle_regexp_like" => like(values),
        "oracle_regexp_count" => count(values),
        "oracle_regexp_substr" => substring(values),
        "oracle_regexp_instr" => position(values),
        _ => replaced(values),
    }
}

#[cfg(test)]
mod tests {
    use super::call;
    use crate::types::Value;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn number(value: i64) -> Value {
        Value::BigInt(value)
    }

    #[test]
    fn the_reference_examples_give_oracles_results() {
        assert_eq!(
            call(
                "oracle_regexp_substr",
                &[
                    text("500 Oracle Parkway, Redwood Shores, CA"),
                    text(",[^,]+,")
                ]
            ),
            Ok(text(", Redwood Shores,"))
        );
        assert_eq!(
            call(
                "oracle_regexp_instr",
                &[
                    text("500 Oracle Parkway, Redwood Shores, CA"),
                    text("[^ ]+"),
                    number(1),
                    number(6)
                ]
            ),
            Ok(number(37))
        );
        assert_eq!(
            call(
                "oracle_regexp_instr",
                &[
                    text("1234567890"),
                    text("(123)(4(56)(78))"),
                    number(1),
                    number(1),
                    number(0),
                    text("i"),
                    number(4)
                ]
            ),
            Ok(number(7))
        );
        assert_eq!(
            call(
                "oracle_regexp_substr",
                &[
                    text("1234567890"),
                    text("(123)(4(56)(78))"),
                    number(1),
                    number(1),
                    text("i"),
                    number(4)
                ]
            ),
            Ok(text("78"))
        );
        assert_eq!(
            call(
                "oracle_regexp_count",
                &[text("123123123123123"), text("(12)3"), number(1), text("i")]
            ),
            Ok(number(5))
        );
        assert_eq!(
            call(
                "oracle_regexp_replace",
                &[
                    text("515.123.4567"),
                    text("([[:digit:]]{3})\\.([[:digit:]]{3})\\.([[:digit:]]{4})"),
                    text("(\\1) \\2-\\3")
                ]
            ),
            Ok(text("(515) 123-4567"))
        );
        assert_eq!(
            call(
                "oracle_regexp_replace",
                &[text("Peru"), text("(.)"), text("\\1 ")]
            ),
            Ok(text("P e r u "))
        );
    }

    #[test]
    fn alternation_takes_the_first_alternative_that_matches() {
        assert_eq!(
            call("oracle_regexp_substr", &[text("ab"), text("a|ab")]),
            Ok(text("a"))
        );
        assert_eq!(
            call("oracle_regexp_substr", &[text("aaa"), text("a+?")]),
            Ok(text("a"))
        );
    }

    #[test]
    fn match_parameters_follow_oracles_letters() {
        let lines = text("a\nb");
        assert_eq!(
            call("oracle_regexp_like", &[lines.clone(), text("a.b")]),
            Ok(Value::Boolean(false))
        );
        assert_eq!(
            call(
                "oracle_regexp_like",
                &[lines.clone(), text("a.b"), text("n")]
            ),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            call(
                "oracle_regexp_count",
                &[lines.clone(), text("^b"), number(1), text("m")]
            ),
            Ok(number(1))
        );
        assert_eq!(
            call("oracle_regexp_count", &[lines, text("^b")]),
            Ok(number(0))
        );
        assert_eq!(
            call(
                "oracle_regexp_like",
                &[text("ABC"), text("abc"), text("ci")]
            ),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            call(
                "oracle_regexp_like",
                &[text("ABC"), text("abc"), text("ic")]
            ),
            Ok(Value::Boolean(false))
        );
        let refused = call("oracle_regexp_like", &[text("ABC"), text("abc"), text("g")]);
        assert_eq!(
            refused.map_err(|error| error.vendor_code()),
            Err(Some(1760))
        );
    }

    #[test]
    fn empty_text_is_null_and_occurrence_zero_replaces_every_match() {
        assert_eq!(
            call("oracle_regexp_substr", &[text("abc"), text("x")]),
            Ok(Value::Null)
        );
        assert_eq!(
            call("oracle_regexp_replace", &[text("aaa"), text("a")]),
            Ok(Value::Null)
        );
        assert_eq!(
            call("oracle_regexp_like", &[Value::Null, text("a")]),
            Ok(Value::Null)
        );
        assert_eq!(
            call(
                "oracle_regexp_replace",
                &[text("a.b.c"), text("\\."), text("-"), number(1), number(2)]
            ),
            Ok(text("a.b-c"))
        );
        assert_eq!(
            call(
                "oracle_regexp_replace",
                &[text("a.b.c"), text("\\."), text("-")]
            ),
            Ok(text("a-b-c"))
        );
        let refused = call("oracle_regexp_substr", &[text("abc"), text("b"), number(0)]);
        assert_eq!(
            refused.map_err(|error| error.vendor_code()),
            Err(Some(1428))
        );
    }
}
