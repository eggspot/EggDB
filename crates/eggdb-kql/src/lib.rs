//! EggDB's translator of the Kusto Query Language (D242), written from Microsoft's public KQL
//! documentation: a table followed by `|` operators (`where`, `project`, `extend`, `summarize
//! ... by`, `order by`/`sort by`, `take`/`limit`, `top ... by`, `count`, `distinct`) becomes one
//! SQL query in the PostgreSQL dialect, each operator wrapping the one before as a subquery, so
//! KQL runs on every EggDB table. Expressions translate KQL's operators (`==`, `contains`,
//! `startswith`, `endswith`, `has`, `in`, `between`), literals (`datetime(...)`, timespans such as
//! `1h`), `ago()`, `now()` and `bin()`. Kusto is a trademark of Microsoft; EggDB is compatible
//! with its query language.

mod expression;

use expression::{Tokens, translate_expression};

/// Why a KQL query could not be translated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KqlError(pub String);

impl std::fmt::Display for KqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KQL: {}", self.0)
    }
}

impl std::error::Error for KqlError {}

fn error(message: impl Into<String>) -> KqlError {
    KqlError(message.into())
}

fn split_pipes(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (at, character) in text.char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            (None, '|') => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn split_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut quote = None;
    for (at, character) in text.char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                parts.push(text[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

fn identifier(name: &str) -> Result<String, KqlError> {
    let name = name.trim().trim_matches(|character| character == '[' || character == ']').trim_matches('\'').trim_matches('"');
    let mut characters = name.chars();
    let valid = characters.next().is_some_and(|first| first.is_alphabetic() || first == '_') && characters.all(|character| character.is_alphanumeric() || character == '_');
    if valid { Ok(format!("\"{name}\"")) } else { Err(error(format!("{name} is not a column or table name"))) }
}

fn named_expression(item: &str) -> Result<String, KqlError> {
    match split_assignment(item) {
        Some((name, expression)) => Ok(format!("{} AS {}", translate_expression(&mut Tokens::new(expression)?)?, identifier(name)?)),
        None => translate_expression(&mut Tokens::new(item)?),
    }
}

fn split_assignment(item: &str) -> Option<(&str, &str)> {
    let at = item.find('=')?;
    let next = item[at + 1..].chars().next();
    let before = item[..at].chars().last();
    if matches!(next, Some('=' | '~')) || matches!(before, Some('!' | '<' | '>' | '=')) {
        return None;
    }
    Some((item[..at].trim(), item[at + 1..].trim()))
}

fn aggregate(item: &str) -> Result<String, KqlError> {
    let (name, call) = split_assignment(item).map_or((None, item), |(name, call)| (Some(name), call));
    let call = call.trim();
    let open = call.find('(').ok_or_else(|| error(format!("{call} is not an aggregation")))?;
    let function = call[..open].trim().to_ascii_lowercase();
    let inner = call[open + 1..].strip_suffix(')').ok_or_else(|| error(format!("{call} is not closed")))?.trim();
    let argument = || -> Result<String, KqlError> { translate_expression(&mut Tokens::new(inner)?) };
    let sql = match function.as_str() {
        "count" => "count(*)".to_owned(),
        "countif" => format!("count(*) FILTER (WHERE {})", argument()?),
        "sum" => format!("sum({})", argument()?),
        "avg" => format!("avg({})", argument()?),
        "min" => format!("min({})", argument()?),
        "max" => format!("max({})", argument()?),
        "dcount" => format!("count(DISTINCT {})", argument()?),
        other => return Err(error(format!("the aggregation {other} is not supported yet"))),
    };
    let default_name = match function.as_str() {
        "count" => "count_".to_owned(),
        other => format!("{other}_{}", inner.trim_matches(|character: char| !character.is_alphanumeric() && character != '_')),
    };
    Ok(format!("{sql} AS {}", identifier(name.unwrap_or(&default_name))?))
}

fn order_items(text: &str) -> Result<String, KqlError> {
    let mut items = Vec::new();
    for item in split_commas(text) {
        let mut words = item.split_whitespace();
        let column = translate_expression(&mut Tokens::new(words.next().unwrap_or_default())?)?;
        let direction = match words.next().map(str::to_ascii_lowercase).as_deref() {
            Some("asc") => "ASC",
            Some("desc") | None => "DESC",
            Some(other) => return Err(error(format!("{other} is not asc or desc"))),
        };
        items.push(format!("{column} {direction}"));
    }
    Ok(items.join(", "))
}

/// Translates a KQL query into one SQL query.
pub fn translate(kql: &str) -> Result<String, KqlError> {
    let stages = split_pipes(kql.trim().trim_end_matches(';'));
    let source = stages.first().map(|stage| stage.trim()).filter(|stage| !stage.is_empty()).ok_or_else(|| error("a query starts with a table name"))?;
    let mut sql = format!("SELECT * FROM {}", identifier(source)?);
    for (index, stage) in stages.iter().enumerate().skip(1) {
        let stage = stage.trim();
        let (operator, rest) = stage.split_once(char::is_whitespace).unwrap_or((stage, ""));
        let rest = rest.trim();
        let alias = format!("q{index}");
        sql = match operator.to_ascii_lowercase().as_str() {
            "where" | "filter" => format!("SELECT * FROM ({sql}) AS {alias} WHERE {}", translate_expression(&mut Tokens::new(rest)?)?),
            "project" => format!("SELECT {} FROM ({sql}) AS {alias}", split_commas(rest).into_iter().map(named_expression).collect::<Result<Vec<_>, _>>()?.join(", ")),
            "extend" => format!("SELECT *, {} FROM ({sql}) AS {alias}", split_commas(rest).into_iter().map(named_expression).collect::<Result<Vec<_>, _>>()?.join(", ")),
            "summarize" => {
                let (aggregations, groups) = match rest.to_ascii_lowercase().rfind(" by ") {
                    Some(at) => (&rest[..at], Some(&rest[at + 4..])),
                    None if rest.to_ascii_lowercase().starts_with("by ") => ("", Some(&rest[3..])),
                    None => (rest, None),
                };
                let mut selected: Vec<String> = Vec::new();
                let mut grouped = Vec::new();
                for group in groups.map(split_commas).unwrap_or_default() {
                    let named = named_expression(group)?;
                    grouped.push((selected.len() + 1).to_string());
                    selected.push(if split_assignment(group).is_none() && group.trim_start().to_ascii_lowercase().starts_with("bin(") {
                        let column = group.trim_start()[4..].split(',').next().unwrap_or_default().trim().to_owned();
                        format!("{named} AS {}", identifier(&column)?)
                    } else {
                        named
                    });
                }
                for item in split_commas(aggregations) {
                    selected.push(aggregate(item)?);
                }
                if selected.is_empty() {
                    return Err(error("summarize needs an aggregation or a by clause"));
                }
                let group_by = if grouped.is_empty() { String::new() } else { format!(" GROUP BY {}", grouped.join(", ")) };
                format!("SELECT {} FROM ({sql}) AS {alias}{group_by}", selected.join(", "))
            }
            "order" | "sort" => {
                let rest = rest.strip_prefix("by").ok_or_else(|| error(format!("{operator} is followed by by")))?;
                format!("SELECT * FROM ({sql}) AS {alias} ORDER BY {}", order_items(rest)?)
            }
            "take" | "limit" => format!("SELECT * FROM ({sql}) AS {alias} LIMIT {}", rest.parse::<u64>().map_err(|_| error(format!("{operator} takes a count, not {rest}")))?),
            "top" => {
                let (count, by) = rest.split_once(" by ").ok_or_else(|| error("top takes a count and by"))?;
                let count = count.trim().parse::<u64>().map_err(|_| error(format!("top takes a count, not {count}")))?;
                format!("SELECT * FROM ({sql}) AS {alias} ORDER BY {} LIMIT {count}", order_items(by)?)
            }
            "count" => format!("SELECT count(*) AS \"Count\" FROM ({sql}) AS {alias}"),
            "distinct" => format!("SELECT DISTINCT {} FROM ({sql}) AS {alias}", split_commas(rest).into_iter().map(identifier).collect::<Result<Vec<_>, _>>()?.join(", ")),
            other => return Err(error(format!("the operator {other} is not supported yet"))),
        };
    }
    Ok(sql)
}

#[cfg(test)]
mod tests;
