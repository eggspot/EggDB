//! Materialized views kept up to date incrementally at commit (D1033): `CREATE INCREMENTAL
//! MATERIALIZED VIEW name AS query` creates the view as usual and a row trigger on its one base
//! table that, inside the changing transaction, recomputes only the view rows the changed base
//! rows touch: the row of the same key for a projection, the group of the same group values for
//! a `GROUP BY` with any aggregates. The trigger also notifies `<view>_changes`, so a client
//! that `LISTEN`s runs a continuous query.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};

const REFUSED_WORDS: [&str; 12] = ["join", "union", "intersect", "except", "distinct", "having", "order", "limit", "offset", "over", "window", "fetch"];

/// How a view's rows follow its base table's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IncrementalShape {
    /// Each view row is one base row, identified by the base table's key columns, named in the
    /// view as these columns.
    Rows { base_keys: Vec<String>, view_keys: Vec<String> },
    /// Each view row is one group, identified by these group expressions over the base table,
    /// named in the view as these columns.
    Groups { group_by: Vec<String>, view_keys: Vec<String> },
}

/// What the trigger of an incremental view needs to know about its query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncrementalPlan {
    pub(crate) base_table: String,
    pub(crate) alias: String,
    pub(crate) select_list: String,
    pub(crate) filter: Option<String>,
    pub(crate) group_by_text: Option<String>,
    pub(crate) shape: IncrementalShape,
}

fn not_incremental(reason: &str) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("an incremental materialized view reads one table, with an optional WHERE and GROUP BY: {reason}"),
    )
}

fn word_of(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut current = String::new();
    for character in text.chars() {
        match character {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                parts.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

fn output_name(item: &str) -> String {
    let lowered = item.to_ascii_lowercase();
    if let Some(at) = lowered.rfind(" as ") {
        return item[at + 4..].trim().trim_matches('"').to_ascii_lowercase();
    }
    let last = item.rsplit(['.', ' ']).next().unwrap_or(item);
    last.trim().trim_matches('"').to_ascii_lowercase()
}

fn bare_column(expression: &str, alias: &str) -> Option<String> {
    let expression = expression.trim();
    let column = match expression.split_once('.') {
        Some((qualifier, column)) if qualifier.trim().eq_ignore_ascii_case(alias) => column,
        Some(_) => return None,
        None => expression,
    };
    let column = column.trim().trim_matches('"');
    column.chars().all(|character| character.is_ascii_alphanumeric() || character == '_').then(|| column.to_ascii_lowercase())
}

/// The plan of an incremental view over `query`, given the base table's key columns, or why
/// the query cannot be maintained incrementally.
pub(crate) fn plan(query: &str, key_columns_of: &mut dyn FnMut(&str) -> SqlResult<Vec<String>>) -> SqlResult<IncrementalPlan> {
    let tokens = tokenize_in(query, Dialect::PostgreSql)?;
    if let Some(found) = tokens.iter().filter_map(word_of).find(|word| REFUSED_WORDS.contains(&word.as_str())) {
        return Err(not_incremental(&format!("{} is not supported", found.to_ascii_uppercase())));
    }
    let lowered = query.to_ascii_lowercase();
    if lowered.matches("select").count() != 1 {
        return Err(not_incremental("subqueries are not supported"));
    }
    let select_at = tokens.iter().position(|token| word_of(token).as_deref() == Some("select")).ok_or_else(|| not_incremental("no SELECT"))?;
    let from_at = tokens.iter().position(|token| word_of(token).as_deref() == Some("from")).ok_or_else(|| not_incremental("no FROM"))?;
    let where_at = tokens.iter().position(|token| word_of(token).as_deref() == Some("where"));
    let group_at = tokens.iter().position(|token| word_of(token).as_deref() == Some("group"));
    let select_list = query[tokens[select_at].end..tokens[from_at].start].trim().to_owned();
    let from_end = where_at.or(group_at).map_or(query.len(), |at| tokens[at].start);
    let from_text = query[tokens[from_at].end..from_end].trim().trim_end_matches(';').trim();
    if from_text.contains(',') || from_text.contains('(') {
        return Err(not_incremental("FROM names one table"));
    }
    let from_words: Vec<&str> = from_text.split_whitespace().filter(|word| !word.eq_ignore_ascii_case("as")).collect();
    let (base_table, alias) = match from_words.as_slice() {
        [table] => (table.to_string(), table.rsplit('.').next().unwrap_or(table).to_string()),
        [table, alias] => (table.to_string(), alias.to_string()),
        _ => return Err(not_incremental("FROM names one table")),
    };
    let filter = where_at.map(|at| {
        let end = group_at.map_or(query.len(), |group| tokens[group].start);
        query[tokens[at].end..end].trim().trim_end_matches(';').trim().to_owned()
    });
    let group_by_text = match group_at {
        Some(at) => {
            if tokens.get(at + 1).and_then(word_of).as_deref() != Some("by") {
                return Err(not_incremental("GROUP must be GROUP BY"));
            }
            Some(query[tokens[at + 1].end..].trim().trim_end_matches(';').trim().to_owned())
        }
        None => None,
    };
    let items = split_top_level(&select_list);
    if items.iter().any(|item| item.trim() == "*") {
        return Err(not_incremental("name the view's columns instead of *"));
    }
    let shape = match &group_by_text {
        Some(groups) => {
            let group_by = split_top_level(groups);
            let mut view_keys = Vec::new();
            for group in &group_by {
                let found = items
                    .iter()
                    .find(|item| {
                        let expression = item.to_ascii_lowercase();
                        let expression = expression.split(" as ").next().unwrap_or(&expression).trim().to_owned();
                        expression == group.to_ascii_lowercase()
                    })
                    .ok_or_else(|| not_incremental(&format!("the group expression {group} is in the select list")))?;
                view_keys.push(output_name(found));
            }
            IncrementalShape::Groups { group_by, view_keys }
        }
        None => {
            let base_keys = key_columns_of(&base_table)?;
            if base_keys.is_empty() {
                return Err(not_incremental("the table needs a primary key"));
            }
            let mut view_keys = Vec::new();
            for key in &base_keys {
                let found = items
                    .iter()
                    .find(|item| {
                        let expression = item.split_whitespace().next().unwrap_or_default();
                        bare_column(expression, &alias).as_deref() == Some(key.as_str())
                    })
                    .ok_or_else(|| not_incremental(&format!("the key column {key} is in the select list")))?;
                view_keys.push(output_name(found));
            }
            IncrementalShape::Rows { base_keys: base_keys.clone(), view_keys }
        }
    };
    Ok(IncrementalPlan { base_table, alias, select_list, filter, group_by_text, shape })
}

fn quoted(name: &str) -> String {
    crate::schema::quoted(name)
}

/// The trigger function body that keeps `view` up to date with `plan`'s base table.
pub(crate) fn trigger_body(view: &str, plan: &IncrementalPlan) -> String {
    let filter = plan.filter.as_deref().map_or_else(|| "TRUE".to_owned(), |filter| format!("({filter})"));
    let refresh = |record: &str| -> String {
        match &plan.shape {
            IncrementalShape::Rows { base_keys, view_keys } => {
                let delete = view_keys
                    .iter()
                    .zip(base_keys)
                    .map(|(view_key, base_key)| format!("{} IS NOT DISTINCT FROM {record}.{}", quoted(view_key), quoted(base_key)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let select = base_keys
                    .iter()
                    .map(|base_key| format!("{}.{} IS NOT DISTINCT FROM {record}.{}", plan.alias, quoted(base_key), quoted(base_key)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                format!(
                    "DELETE FROM {view} WHERE {delete}; INSERT INTO {view} SELECT {} FROM {} AS {} WHERE {filter} AND {select};",
                    plan.select_list, plan.base_table, plan.alias
                )
            }
            IncrementalShape::Groups { group_by, view_keys } => {
                let record_group = |group: &str| -> String {
                    match bare_column(group, &plan.alias) {
                        Some(column) => format!("{record}.{}", quoted(&column)),
                        None => group.to_owned(),
                    }
                };
                let delete = view_keys
                    .iter()
                    .zip(group_by)
                    .map(|(view_key, group)| format!("{} IS NOT DISTINCT FROM {}", quoted(view_key), record_group(group)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let select = group_by
                    .iter()
                    .map(|group| format!("({group}) IS NOT DISTINCT FROM {}", record_group(group)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                format!(
                    "DELETE FROM {view} WHERE {delete}; INSERT INTO {view} SELECT {} FROM {} AS {} WHERE {filter} AND {select} GROUP BY {};",
                    plan.select_list,
                    plan.base_table,
                    plan.alias,
                    plan.group_by_text.as_deref().unwrap_or_default()
                )
            }
        }
    };
    format!(
        "BEGIN IF TG_OP <> 'INSERT' THEN {} END IF; IF TG_OP <> 'DELETE' THEN {} END IF; PERFORM pg_notify({}, TG_OP); RETURN NULL; END",
        refresh("OLD"),
        refresh("NEW"),
        crate::dump::sql_string(&format!("{}_changes", view.rsplit('.').next().unwrap_or(view).trim_matches('"')))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projections_and_groups_plan_their_triggers() {
        let mut keys = |_: &str| Ok(vec!["id".to_owned()]);
        let rows = plan("SELECT o.id, o.total AS amount FROM orders o WHERE o.status = 'paid'", &mut keys).unwrap();
        assert_eq!(rows.shape, IncrementalShape::Rows { base_keys: vec!["id".into()], view_keys: vec!["id".into()] });
        let body = trigger_body("paid", &rows);
        assert!(body.contains("DELETE FROM paid WHERE id IS NOT DISTINCT FROM OLD.id;"));
        assert!(body.contains("INSERT INTO paid SELECT o.id, o.total AS amount FROM orders AS o WHERE (o.status = 'paid') AND o.id IS NOT DISTINCT FROM NEW.id;"));
        let groups = plan("SELECT customer, count(*) AS orders, sum(total) AS spent FROM orders GROUP BY customer", &mut keys).unwrap();
        assert_eq!(groups.shape, IncrementalShape::Groups { group_by: vec!["customer".into()], view_keys: vec!["customer".into()] });
        assert!(trigger_body("spend", &groups).contains("GROUP BY customer;"));
        assert!(plan("SELECT a FROM x JOIN y ON x.id = y.id", &mut keys).is_err());
        assert!(plan("SELECT total FROM orders", &mut keys).is_err());
    }
}
