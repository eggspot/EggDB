//! The distributed planner for sharded tables (D1041): a single-table `SELECT` on a table whose
//! rows are spread over shard servers becomes one query every shard runs (its filter, partial
//! aggregates by group, ordering and limit pushed down) and a final query over the shards'
//! answers, which the coordinator runs locally. A filter that fixes the shard key to a literal
//! sends the query to that key's shard alone. The rewrite works on the statement's tokens, so
//! every pushed clause is the text the user wrote.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};
use eggdb_cluster::{Strategy, route};

/// The name the shards' combined answers go by in a plan's final query.
pub(crate) const PARTIALS: &str = "eggdb_partials";

/// A table whose rows are spread over shard servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShardedTable {
    pub(crate) name: String,
    pub(crate) shard_key: String,
    pub(crate) sources: Vec<String>,
}

/// One column of the shards' answers: the key it has in each answer row, the name the final
/// query reads it by, and its type there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartialColumn {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) type_text: String,
}

/// How a query on a sharded table runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DistributedPlan {
    pub(crate) shard_sql: String,
    pub(crate) shards: Vec<usize>,
    pub(crate) columns: Vec<PartialColumn>,
    pub(crate) final_sql: String,
}

const AGGREGATES: [&str; 5] = ["count", "sum", "min", "max", "avg"];

fn unsupported(reason: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!(
            "a query on a sharded table {reason}; the distributed planner runs single-table SELECTs with WHERE, GROUP BY on columns, HAVING, ORDER BY, LIMIT, OFFSET and count, sum, min, max and avg"
        ),
    )
}

/// The shard of a row whose shard key's text is `key`, among `shards`: the hash routing of
/// the cluster's shard maps (D212).
pub(crate) fn shard_of(key: &str, shards: usize) -> usize {
    let shards = u32::try_from(shards.max(1)).unwrap_or(u32::MAX);
    route(&Strategy::Hash { shards }, key.as_bytes()).unwrap_or(0) as usize
}

fn word_is(token: &Token<'_>, word: &str) -> bool {
    matches!(&token.kind, TokenKind::Word(found) if found.eq_ignore_ascii_case(word))
}

fn identifier(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn span(text: &str, tokens: &[Token<'_>]) -> String {
    match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => text[first.start..last.end].to_owned(),
        _ => String::new(),
    }
}

fn depth_change(token: &Token<'_>) -> i32 {
    match token.kind {
        TokenKind::LeftParen | TokenKind::LeftBracket => 1,
        TokenKind::RightParen | TokenKind::RightBracket => -1,
        _ => 0,
    }
}

fn split_top_level<'a, 't>(
    tokens: &'a [Token<'t>],
    at: impl Fn(&Token<'t>) -> bool,
) -> Vec<&'a [Token<'t>]> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if depth == 0 && at(token) {
            parts.push(&tokens[start..index]);
            start = index + 1;
        }
        depth += depth_change(token);
    }
    parts.push(&tokens[start..]);
    parts
}

#[derive(Default)]
struct Clauses<'a, 't> {
    items: &'a [Token<'t>],
    from: &'a [Token<'t>],
    filter: Option<&'a [Token<'t>]>,
    group_by: Option<&'a [Token<'t>]>,
    having: Option<&'a [Token<'t>]>,
    order_by: Option<&'a [Token<'t>]>,
    limit: Option<&'a [Token<'t>]>,
    offset: Option<&'a [Token<'t>]>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    Items,
    From,
    Filter,
    GroupBy,
    Having,
    OrderBy,
    Limit,
    Offset,
}

fn clauses<'a, 't>(tokens: &'a [Token<'t>]) -> SqlResult<Option<Clauses<'a, 't>>> {
    if !tokens.first().is_some_and(|token| word_is(token, "select")) {
        return Ok(None);
    }
    let mut marks: Vec<(Clause, usize, usize)> = vec![(Clause::Items, 0, 1)];
    let mut depth = 0;
    let mut index = 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if word_is(token, "over") {
            return Err(unsupported("uses a window function"));
        }
        if depth == 0 {
            let next_is_by = tokens.get(index + 1).is_some_and(|next| word_is(next, "by"));
            let found = if word_is(token, "from") {
                Some((Clause::From, 1))
            } else if word_is(token, "where") {
                Some((Clause::Filter, 1))
            } else if word_is(token, "group") && next_is_by {
                Some((Clause::GroupBy, 2))
            } else if word_is(token, "having") {
                Some((Clause::Having, 1))
            } else if word_is(token, "order") && next_is_by {
                Some((Clause::OrderBy, 2))
            } else if word_is(token, "limit") {
                Some((Clause::Limit, 1))
            } else if word_is(token, "offset") {
                Some((Clause::Offset, 1))
            } else {
                for refused in ["union", "intersect", "except", "window", "for", "fetch", "into"] {
                    if word_is(token, refused) {
                        return Err(unsupported(&format!("uses {}", refused.to_ascii_uppercase())));
                    }
                }
                None
            };
            if let Some((clause, width)) = found {
                marks.push((clause, index, index + width));
                index += width;
                continue;
            }
        }
        depth += depth_change(token);
        index += 1;
    }
    let mut found = Clauses::default();
    for (position, (clause, _, body_start)) in marks.iter().enumerate() {
        let body_end = marks.get(position + 1).map_or(tokens.len(), |next| next.1);
        let body = &tokens[*body_start..body_end];
        let slot = match clause {
            Clause::Items => {
                found.items = body;
                continue;
            }
            Clause::From => {
                found.from = body;
                continue;
            }
            Clause::Filter => &mut found.filter,
            Clause::GroupBy => &mut found.group_by,
            Clause::Having => &mut found.having,
            Clause::OrderBy => &mut found.order_by,
            Clause::Limit => &mut found.limit,
            Clause::Offset => &mut found.offset,
        };
        if slot.replace(body).is_some() {
            return Err(SqlError::new(SqlState::SYNTAX_ERROR, "a clause appears twice"));
        }
    }
    Ok(Some(found))
}

fn table_of(from: &[Token<'_>]) -> Option<(String, Option<String>)> {
    let (name, rest) = match from {
        [schema, dot, table, rest @ ..] if matches!(dot.kind, TokenKind::Dot) => (
            format!("{}.{}", identifier(schema)?, identifier(table)?),
            rest,
        ),
        [table, rest @ ..] => (identifier(table)?, rest),
        [] => return None,
    };
    let alias = match rest {
        [] => None,
        [as_word, alias] if word_is(as_word, "as") => Some(identifier(alias)?),
        [alias] => Some(identifier(alias)?),
        _ => return None,
    };
    Some((name, alias))
}

struct Call {
    start: usize,
    end: usize,
    function: String,
    argument: String,
    text: String,
}

fn aggregate_calls(text: &str, tokens: &[Token<'_>]) -> SqlResult<Vec<Call>> {
    let mut calls = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let function = match &tokens[index].kind {
            TokenKind::Word(word) if AGGREGATES.contains(&word.to_ascii_lowercase().as_str()) => {
                word.to_ascii_lowercase()
            }
            _ => {
                index += 1;
                continue;
            }
        };
        if !tokens.get(index + 1).is_some_and(|next| matches!(next.kind, TokenKind::LeftParen)) {
            index += 1;
            continue;
        }
        let mut depth = 0;
        let mut close = None;
        for (offset, token) in tokens[index + 1..].iter().enumerate() {
            depth += depth_change(token);
            if depth == 0 {
                close = Some(index + 1 + offset);
                break;
            }
        }
        let close = close.ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "an aggregate call is not closed"))?;
        let inner = &tokens[index + 2..close];
        if inner.first().is_some_and(|token| word_is(token, "distinct")) {
            return Err(unsupported(&format!("uses {}(DISTINCT ...)", function)));
        }
        calls.push(Call {
            start: tokens[index].start,
            end: tokens[close].end,
            function,
            argument: span(text, inner),
            text: span(text, &tokens[index..=close]),
        });
        index = close + 1;
    }
    Ok(calls)
}

fn rewritten(text: &str, tokens: &[Token<'_>], replacements: &[(usize, usize, String)]) -> String {
    let (Some(first), Some(last)) = (tokens.first(), tokens.last()) else {
        return String::new();
    };
    let mut ordered: Vec<&(usize, usize, String)> = replacements
        .iter()
        .filter(|(start, end, _)| *start >= first.start && *end <= last.end)
        .collect();
    ordered.sort_by_key(|(start, _, _)| *start);
    let mut out = String::new();
    let mut at = first.start;
    for (start, end, replacement) in ordered {
        if *start < at {
            continue;
        }
        out.push_str(&text[at..*start]);
        out.push_str(replacement);
        at = *end;
    }
    out.push_str(&text[at..last.end]);
    out
}

fn split_alias<'a, 't>(item: &'a [Token<'t>]) -> SqlResult<(&'a [Token<'t>], Option<String>)> {
    match item {
        [expression @ .., as_word, alias] if word_is(as_word, "as") && !expression.is_empty() => Ok((
            expression,
            Some(identifier(alias).ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "AS needs a name"))?),
        )),
        _ => Ok((item, None)),
    }
}

fn column_reference(tokens: &[Token<'_>], alias: Option<&str>, table: &str) -> Option<String> {
    match tokens {
        [column] => identifier(column),
        [qualifier, dot, column] if matches!(dot.kind, TokenKind::Dot) => {
            let qualifier = identifier(qualifier)?;
            let table_name = table.rsplit('.').next().unwrap_or(table);
            (alias == Some(qualifier.as_str()) || qualifier == table_name).then(|| identifier(column)).flatten()
        }
        _ => None,
    }
}

fn routed_shard(filter: Option<&[Token<'_>]>, shard_key: &str, shards: usize) -> Option<usize> {
    fixed_in(filter?, shard_key).map(|key| shard_of(&key, shards))
}

/// The literal `text`'s top-level `WHERE` fixes `column` to, when it is a `SELECT` whose filter
/// is a conjunction with `column = <literal>` in it: what a global index on the column can
/// look up.
pub(crate) fn fixed_value(text: &str, column: &str) -> SqlResult<Option<String>> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let tokens: Vec<Token<'_>> = tokens
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
        .collect();
    let Some(clauses) = clauses(&tokens)? else {
        return Ok(None);
    };
    Ok(clauses.filter.and_then(|filter| fixed_in(filter, column)))
}

fn fixed_in(filter: &[Token<'_>], shard_key: &str) -> Option<String> {
    if split_top_level(filter, |token| word_is(token, "or")).len() > 1 {
        return None;
    }
    for condition in split_top_level(filter, |token| word_is(token, "and")) {
        let key = match condition {
            [column, equals, value] | [value, equals, column]
                if matches!(equals.kind, TokenKind::Operator("=")) && identifier(column).as_deref() == Some(shard_key) =>
            {
                match &value.kind {
                    TokenKind::Integer(digits) => Some((*digits).to_owned()),
                    TokenKind::String(content) => Some(content.to_string()),
                    _ => None,
                }
            }
            _ => None,
        };
        if key.is_some() {
            return key;
        }
    }
    None
}

fn literal_number(tokens: Option<&[Token<'_>]>) -> Option<u64> {
    match tokens? {
        [Token { kind: TokenKind::Integer(digits), .. }] => digits.parse().ok(),
        _ => None,
    }
}

/// The plan for `text` when it is a `SELECT` whose `FROM` names `table`; `None` when it reads
/// something else. `columns` are the table's columns with their types, as the coordinator
/// declares the table.
pub(crate) fn plan_distributed(
    text: &str,
    table: &ShardedTable,
    columns: &[(String, String)],
) -> SqlResult<Option<DistributedPlan>> {
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let tokens: Vec<Token<'_>> = tokens
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
        .collect();
    let Some(clauses) = clauses(&tokens)? else {
        return Ok(None);
    };
    let Some((name, alias)) = table_of(clauses.from) else {
        let mentions = clauses
            .from
            .iter()
            .any(|token| identifier(token).is_some_and(|word| word == table.name || table.name.ends_with(&format!(".{word}"))));
        return if mentions { Err(unsupported("joins it or reads it through a subquery")) } else { Ok(None) };
    };
    if name != table.name && !table.name.ends_with(&format!(".{name}")) {
        return Ok(None);
    }
    if clauses.items.first().is_some_and(|token| word_is(token, "distinct")) {
        return Err(unsupported("uses SELECT DISTINCT"));
    }
    let shards: Vec<usize> = match routed_shard(clauses.filter, &table.shard_key, table.sources.len()) {
        Some(shard) => vec![shard],
        None => (0..table.sources.len()).collect(),
    };
    let type_of = |column: &str| -> String {
        columns
            .iter()
            .find(|(name, _)| name == column)
            .map_or_else(|| "text".to_owned(), |(_, type_text)| type_text.clone())
    };
    let from_text = span(text, clauses.from);
    let filter_text = clauses.filter.map(|filter| format!(" WHERE {}", span(text, filter))).unwrap_or_default();
    let mut item_calls = Vec::new();
    for clause in [Some(clauses.items), clauses.having, clauses.order_by].into_iter().flatten() {
        item_calls.extend(aggregate_calls(text, clause)?);
    }
    let star = matches!(clauses.items, [Token { kind: TokenKind::Operator("*"), .. }]);
    if item_calls.is_empty() && clauses.group_by.is_none() {
        let mut shard_items = Vec::new();
        let mut partials = Vec::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        if star {
            for (index, (column, type_text)) in columns.iter().enumerate() {
                let key = format!("c{index}");
                shard_items.push(format!("{} AS {key}", quoted(column)));
                partials.push(PartialColumn { key: key.clone(), name: column.clone(), type_text: type_text.clone() });
                aliases.push((column.clone(), key));
            }
        } else {
            for (index, item) in split_top_level(clauses.items, |token| matches!(token.kind, TokenKind::Comma)).into_iter().enumerate() {
                let (expression, alias_name) = split_alias(item)?;
                let column = column_reference(expression, alias.as_deref(), &table.name);
                let output = alias_name.clone().or_else(|| column.clone()).unwrap_or_else(|| "?column?".to_owned());
                let key = format!("c{index}");
                shard_items.push(format!("{} AS {key}", span(text, expression)));
                partials.push(PartialColumn {
                    key: key.clone(),
                    name: output.clone(),
                    type_text: column.as_deref().map_or_else(|| "text".to_owned(), type_of),
                });
                aliases.push((output, key));
            }
        }
        let limit = literal_number(clauses.limit);
        let offset = literal_number(clauses.offset).unwrap_or(0);
        let shard_order = clauses.order_by.map(|order| {
            let replacements: Vec<(usize, usize, String)> = order
                .iter()
                .filter_map(|token| {
                    let word = identifier(token)?;
                    let (_, key) = aliases.iter().find(|(name, _)| *name == word)?;
                    Some((token.start, token.end, key.clone()))
                })
                .collect();
            format!(" ORDER BY {}", rewritten(text, order, &replacements))
        });
        let shard_limit = limit.map_or_else(String::new, |limit| format!(" LIMIT {}", limit.saturating_add(offset)));
        let shard_sql = format!(
            "SELECT {} FROM {from_text}{filter_text}{}{shard_limit}",
            shard_items.join(", "),
            shard_order.unwrap_or_default()
        );
        let outputs: Vec<String> = partials.iter().map(|column| quoted(&column.name)).collect();
        let mut final_sql = format!("SELECT {} FROM {PARTIALS}", outputs.join(", "));
        for (keyword, clause) in [("ORDER BY", clauses.order_by), ("LIMIT", clauses.limit), ("OFFSET", clauses.offset)] {
            if let Some(clause) = clause {
                final_sql.push_str(&format!(" {keyword} {}", span(text, clause)));
            }
        }
        return Ok(Some(DistributedPlan { shard_sql, shards, columns: partials, final_sql }));
    }
    if star {
        return Err(unsupported("selects * with aggregates"));
    }
    let mut group_columns = Vec::new();
    if let Some(group_by) = clauses.group_by {
        for key in split_top_level(group_by, |token| matches!(token.kind, TokenKind::Comma)) {
            let column = column_reference(key, alias.as_deref(), &table.name)
                .ok_or_else(|| unsupported("groups by an expression that is not a column"))?;
            group_columns.push(column);
        }
    }
    let mut partials = Vec::new();
    let mut shard_items = Vec::new();
    for (index, column) in group_columns.iter().enumerate() {
        let key = format!("g{index}");
        shard_items.push(format!("{} AS {key}", quoted(column)));
        partials.push(PartialColumn { key: key.clone(), name: key, type_text: type_of(column) });
    }
    let mut combined: Vec<(String, String)> = Vec::new();
    for call in &item_calls {
        if combined.iter().any(|(text, _)| *text == call.text) {
            continue;
        }
        let first = format!("p{}", partials.len());
        let argument_type = if call.argument.trim() == "*" {
            "text".to_owned()
        } else {
            let argument_tokens = tokenize_in(&call.argument, Dialect::PostgreSql)?;
            let argument_tokens: Vec<Token<'_>> =
                argument_tokens.into_iter().filter(|token| !matches!(token.kind, TokenKind::End)).collect();
            column_reference(&argument_tokens, alias.as_deref(), &table.name)
                .map_or_else(|| "numeric".to_owned(), |column| type_of(&column))
        };
        let combine = match call.function.as_str() {
            "count" => {
                shard_items.push(format!("{} AS {first}", call.text));
                partials.push(PartialColumn { key: first.clone(), name: first.clone(), type_text: "bigint".to_owned() });
                format!("sum({first})::bigint")
            }
            "sum" => {
                shard_items.push(format!("{} AS {first}", call.text));
                partials.push(PartialColumn { key: first.clone(), name: first.clone(), type_text: "numeric".to_owned() });
                format!("sum({first})")
            }
            "avg" => {
                let second = format!("p{}", partials.len() + 1);
                shard_items.push(format!("sum({}) AS {first}", call.argument));
                shard_items.push(format!("count({}) AS {second}", call.argument));
                partials.push(PartialColumn { key: first.clone(), name: first.clone(), type_text: "numeric".to_owned() });
                partials.push(PartialColumn { key: second.clone(), name: second.clone(), type_text: "bigint".to_owned() });
                format!("(sum({first}) / nullif(sum({second}), 0))")
            }
            function => {
                shard_items.push(format!("{} AS {first}", call.text));
                partials.push(PartialColumn { key: first.clone(), name: first.clone(), type_text: argument_type });
                format!("{function}({first})")
            }
        };
        combined.push((call.text.clone(), combine));
    }
    let replacements_in = |tokens: &[Token<'_>]| -> SqlResult<Vec<(usize, usize, String)>> {
        let calls = aggregate_calls(text, tokens)?;
        let mut replacements: Vec<(usize, usize, String)> = calls
            .iter()
            .filter_map(|call| {
                combined
                    .iter()
                    .find(|(text, _)| *text == call.text)
                    .map(|(_, combine)| (call.start, call.end, combine.clone()))
            })
            .collect();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];
            let inside_call = calls.iter().any(|call| token.start >= call.start && token.end <= call.end);
            if !inside_call {
                let qualified = tokens.get(index + 1).is_some_and(|dot| matches!(dot.kind, TokenKind::Dot));
                let reference = if qualified { tokens.get(index..index + 3) } else { tokens.get(index..index + 1) };
                if let Some(reference) = reference
                    && let Some(column) = column_reference(reference, alias.as_deref(), &table.name)
                    && let Some(position) = group_columns.iter().position(|group| *group == column)
                    && !tokens.get(index + 1).is_some_and(|next| matches!(next.kind, TokenKind::LeftParen))
                {
                    let last = &reference[reference.len() - 1];
                    replacements.push((token.start, last.end, format!("g{position}")));
                    index += reference.len();
                    continue;
                }
            }
            index += 1;
        }
        Ok(replacements)
    };
    let mut final_items = Vec::new();
    for item in split_top_level(clauses.items, |token| matches!(token.kind, TokenKind::Comma)) {
        let (expression, alias_name) = split_alias(item)?;
        let column = column_reference(expression, alias.as_deref(), &table.name);
        if let Some(column) = &column
            && !group_columns.contains(column)
        {
            return Err(SqlError::new(
                SqlState::GROUPING_ERROR,
                format!("column \"{column}\" must appear in the GROUP BY clause or be used in an aggregate function"),
            ));
        }
        let output = alias_name.or(column).unwrap_or_else(|| {
            aggregate_calls(text, expression)
                .ok()
                .filter(|calls| calls.len() == 1 && calls[0].text == span(text, expression))
                .map_or_else(|| "?column?".to_owned(), |calls| calls[0].function.clone())
        });
        let replacements = replacements_in(expression)?;
        final_items.push(format!("{} AS {}", rewritten(text, expression, &replacements), quoted(&output)));
    }
    let shard_group = if group_columns.is_empty() {
        String::new()
    } else {
        format!(" GROUP BY {}", group_columns.iter().map(|column| quoted(column)).collect::<Vec<_>>().join(", "))
    };
    let shard_sql = format!("SELECT {} FROM {from_text}{filter_text}{shard_group}", shard_items.join(", "));
    let mut final_sql = format!("SELECT {} FROM {PARTIALS}", final_items.join(", "));
    if !group_columns.is_empty() {
        let keys: Vec<String> = (0..group_columns.len()).map(|index| format!("g{index}")).collect();
        final_sql.push_str(&format!(" GROUP BY {}", keys.join(", ")));
    }
    if let Some(having) = clauses.having {
        final_sql.push_str(&format!(" HAVING {}", rewritten(text, having, &replacements_in(having)?)));
    }
    if let Some(order) = clauses.order_by {
        final_sql.push_str(&format!(" ORDER BY {}", rewritten(text, order, &replacements_in(order)?)));
    }
    for (keyword, clause) in [("LIMIT", clauses.limit), ("OFFSET", clauses.offset)] {
        if let Some(clause) = clause {
            final_sql.push_str(&format!(" {keyword} {}", span(text, clause)));
        }
    }
    Ok(Some(DistributedPlan { shard_sql, shards, columns: partials, final_sql }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orders() -> ShardedTable {
        ShardedTable {
            name: "orders".to_owned(),
            shard_key: "customer".to_owned(),
            sources: vec!["postgresql://a@s1/db".to_owned(), "postgresql://a@s2/db".to_owned(), "postgresql://a@s3/db".to_owned()],
        }
    }

    fn columns() -> Vec<(String, String)> {
        vec![
            ("id".to_owned(), "integer".to_owned()),
            ("customer".to_owned(), "text".to_owned()),
            ("total".to_owned(), "numeric".to_owned()),
        ]
    }

    fn plan(text: &str) -> DistributedPlan {
        plan_distributed(text, &orders(), &columns()).unwrap().unwrap()
    }

    #[test]
    fn a_filtered_scan_pushes_its_filter_order_and_limit_to_every_shard() {
        let planned = plan("SELECT id, total AS amount FROM orders WHERE total > 10 ORDER BY amount DESC LIMIT 5 OFFSET 2");
        assert_eq!(planned.shards, vec![0, 1, 2]);
        assert_eq!(
            planned.shard_sql,
            "SELECT id AS c0, total AS c1 FROM orders WHERE total > 10 ORDER BY c1 DESC LIMIT 7"
        );
        assert_eq!(
            planned.final_sql,
            "SELECT \"id\", \"amount\" FROM eggdb_partials ORDER BY amount DESC LIMIT 5 OFFSET 2"
        );
        assert_eq!(planned.columns[1].type_text, "numeric");
    }

    #[test]
    fn a_fixed_shard_key_routes_to_one_shard() {
        let planned = plan("SELECT * FROM orders WHERE customer = 'ada' AND total > 1");
        assert_eq!(planned.shards, vec![shard_of("ada", 3)]);
        assert_eq!(planned.columns.len(), 3);
        let either = plan("SELECT id FROM orders WHERE customer = 'ada' OR total > 1");
        assert_eq!(either.shards.len(), 3);
    }

    #[test]
    fn aggregates_are_split_into_partials_and_combined() {
        let planned = plan(
            "SELECT customer, count(*), avg(total) AS mean FROM orders GROUP BY customer HAVING sum(total) > 100 ORDER BY count(*) DESC",
        );
        assert_eq!(
            planned.shard_sql,
            "SELECT \"customer\" AS g0, count(*) AS p1, sum(total) AS p2, count(total) AS p3, sum(total) AS p4 FROM orders GROUP BY \"customer\""
        );
        assert_eq!(
            planned.final_sql,
            "SELECT g0 AS \"customer\", sum(p1)::bigint AS \"count\", (sum(p2) / nullif(sum(p3), 0)) AS \"mean\" FROM eggdb_partials GROUP BY g0 HAVING sum(p4) > 100 ORDER BY sum(p1)::bigint DESC"
        );
    }

    #[test]
    fn other_tables_pass_and_unsupported_shapes_are_named() {
        assert_eq!(plan_distributed("SELECT 1", &orders(), &columns()).unwrap(), None);
        assert_eq!(plan_distributed("SELECT * FROM lines", &orders(), &columns()).unwrap(), None);
        assert_eq!(plan_distributed("UPDATE orders SET total = 1", &orders(), &columns()).unwrap(), None);
        for text in [
            "SELECT * FROM orders JOIN lines ON lines.id = orders.id",
            "SELECT DISTINCT customer FROM orders",
            "SELECT count(DISTINCT customer) FROM orders",
            "SELECT customer, row_number() OVER () FROM orders",
            "SELECT id FROM orders UNION SELECT id FROM orders",
            "SELECT customer, count(*) FROM orders GROUP BY lower(customer)",
        ] {
            assert_eq!(
                plan_distributed(text, &orders(), &columns()).unwrap_err().state(),
                SqlState::FEATURE_NOT_SUPPORTED,
                "{text}"
            );
        }
        assert_eq!(
            plan_distributed("SELECT id, count(*) FROM orders GROUP BY customer", &orders(), &columns())
                .unwrap_err()
                .state(),
            SqlState::GROUPING_ERROR
        );
    }

    #[test]
    fn shard_keys_spread_over_every_shard() {
        let mut seen = [0usize; 4];
        for key in 0..400 {
            seen[shard_of(&key.to_string(), 4)] += 1;
        }
        assert!(seen.iter().all(|count| *count > 50), "{seen:?}");
    }
}
