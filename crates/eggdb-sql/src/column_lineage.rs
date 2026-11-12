//! Column-level lineage read from a statement's text (D51, D244, D927): for `INSERT ...
//! SELECT`, `CREATE TABLE ... AS` and `CREATE VIEW`, which source columns each target column is
//! computed from.

use crate::ast::{Expr, ExprKind, FromClause, InsertSource, Query, SelectItem, SetExpr, Statement, TableReference};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::{BTreeMap, BTreeSet};

/// The internal function that answers it.
pub(crate) const FUNCTION: &str = "eggdb_column_lineage";
const MOST_DEPTH: usize = 32;

type Sources = BTreeSet<String>;

struct Output {
    name: String,
    sources: Sources,
}

#[derive(Default, Clone)]
struct Scope {
    tables: Vec<(String, String)>,
    derived: BTreeMap<String, Vec<(String, Sources)>>,
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn not_supported() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "column lineage reads INSERT ... SELECT, CREATE TABLE ... AS and CREATE VIEW statements",
    )
}

fn column_refs(expr: &Expr, out: &mut Vec<(Option<String>, String)>, depth: usize) {
    if depth > MOST_DEPTH * 8 {
        return;
    }
    if let ExprKind::Column { qualifier, name } = &expr.kind {
        out.push((qualifier.clone(), name.clone()));
    }
    for child in crate::eval::children(expr) {
        column_refs(child, out, depth + 1);
    }
}

fn add_reference(table: &TableReference, scope: &mut Scope, with: &BTreeMap<String, Vec<(String, Sources)>>, depth: usize) {
    let alias = table.alias.clone().unwrap_or_else(|| crate::schemas::base_name(&table.name.text).to_owned());
    if let Some(derived) = &table.derived {
        let outputs = query_outputs(&derived.query, with, depth + 1);
        scope.derived.insert(alias, outputs.into_iter().map(|output| (output.name, output.sources)).collect());
        return;
    }
    if let Some(columns) = with.get(&table.name.text) {
        scope.derived.insert(alias, columns.clone());
        return;
    }
    scope.tables.push((alias, table.name.text.clone()));
}

fn scope_of(from: Option<&FromClause>, with: &BTreeMap<String, Vec<(String, Sources)>>, depth: usize) -> Scope {
    let mut scope = Scope::default();
    if let Some(from) = from {
        add_reference(&from.first, &mut scope, with, depth);
        for join in &from.joins {
            add_reference(&join.table, &mut scope, with, depth);
        }
    }
    scope
}

fn resolved(scope: &Scope, qualifier: Option<&str>, name: &str) -> Sources {
    let mut sources = Sources::new();
    let derived_column = |columns: &Vec<(String, Sources)>| {
        columns.iter().find(|(column, _)| column == name).map(|(_, found)| found.clone())
    };
    match qualifier {
        Some(qualifier) => {
            if let Some((_, table)) = scope.tables.iter().find(|(alias, _)| alias == qualifier) {
                sources.insert(format!("{table}.{name}"));
            } else if let Some(found) = scope.derived.get(qualifier).and_then(derived_column) {
                sources.extend(found);
            }
        }
        None => {
            let derived: Vec<Sources> = scope.derived.values().filter_map(derived_column).collect();
            match (scope.tables.as_slice(), derived.len()) {
                ([(_, table)], 0) => {
                    sources.insert(format!("{table}.{name}"));
                }
                ([], 1) => sources.extend(derived.into_iter().flatten()),
                (tables, _) => {
                    sources.extend(tables.iter().map(|(_, table)| format!("{table}.{name}")));
                    sources.extend(derived.into_iter().flatten());
                }
            }
        }
    }
    sources
}

fn query_outputs(query: &Query, outer_with: &BTreeMap<String, Vec<(String, Sources)>>, depth: usize) -> Vec<Output> {
    if depth > MOST_DEPTH {
        return Vec::new();
    }
    let mut with = outer_with.clone();
    for common in &query.with {
        let outputs = query_outputs(&common.query, &with, depth + 1);
        with.insert(common.name.text.clone(), outputs.into_iter().map(|output| (output.name, output.sources)).collect());
    }
    set_outputs(&query.body, &with, depth + 1)
}

fn set_outputs(body: &SetExpr, with: &BTreeMap<String, Vec<(String, Sources)>>, depth: usize) -> Vec<Output> {
    if depth > MOST_DEPTH {
        return Vec::new();
    }
    match body {
        SetExpr::Query(query) => query_outputs(query, with, depth + 1),
        SetExpr::Operation(operation) => {
            let mut left = set_outputs(&operation.left, with, depth + 1);
            let right = set_outputs(&operation.right, with, depth + 1);
            for (output, other) in left.iter_mut().zip(right) {
                output.sources.extend(other.sources);
            }
            left
        }
        SetExpr::Select(select) => {
            let scope = scope_of(select.from.as_ref(), with, depth);
            let mut outputs = Vec::new();
            for item in &select.items {
                match item {
                    SelectItem::Wildcard { qualifier, .. } => {
                        for (alias, table) in &scope.tables {
                            if qualifier.as_deref().is_none_or(|wanted| wanted == alias) {
                                outputs.push(Output {
                                    name: "*".to_owned(),
                                    sources: Sources::from([format!("{table}.*")]),
                                });
                            }
                        }
                        for (alias, columns) in &scope.derived {
                            if qualifier.as_deref().is_none_or(|wanted| wanted == alias) {
                                outputs.extend(columns.iter().map(|(name, sources)| Output {
                                    name: name.clone(),
                                    sources: sources.clone(),
                                }));
                            }
                        }
                    }
                    SelectItem::Expr { expr, alias } => {
                        let mut references = Vec::new();
                        column_refs(expr, &mut references, 0);
                        let mut sources = Sources::new();
                        for (qualifier, name) in &references {
                            sources.extend(resolved(&scope, qualifier.as_deref(), name));
                        }
                        let name = match (alias, &expr.kind) {
                            (Some(alias), _) => alias.clone(),
                            (None, ExprKind::Column { name, .. }) => name.clone(),
                            (None, _) => "?column?".to_owned(),
                        };
                        outputs.push(Output { name, sources });
                    }
                }
            }
            outputs
        }
    }
}


/// The lineage of `sql` as JSON: `{"target": table, "columns": [{"column", "sources": [...]}]}`,
/// each source `table.column` (`table.*` for a `*`), a column naming no table in a query over
/// several tables traced to each of them.
pub(crate) fn lineage(sql: &str) -> SqlResult<String> {
    let tokens = crate::lexer::tokenize_in(sql, Dialect::PostgreSql)?;
    let mut parser = crate::parser::Parser::new(sql, &tokens).in_dialect(Dialect::PostgreSql);
    let statement = parser.statement()?;
    let empty = BTreeMap::new();
    let (target, declared, outputs) = match &statement {
        Statement::Insert(insert) => {
            let InsertSource::Query(query) = &insert.source else {
                return Err(not_supported());
            };
            let declared: Vec<String> = insert.columns.iter().flatten().map(|name| name.text.clone()).collect();
            (insert.table.name.text.clone(), declared, query_outputs(query, &empty, 0))
        }
        Statement::CreateTableAs(create) => {
            let outputs = match &create.query {
                Statement::Query(query) => query_outputs(query, &empty, 0),
                Statement::Select(select) => set_outputs(&SetExpr::Select(Box::new(select.clone())), &empty, 0),
                _ => return Err(not_supported()),
            };
            let declared = create.column_names.iter().map(|name| name.text.clone()).collect();
            (create.name.text.clone(), declared, outputs)
        }
        Statement::CreateView(view) => {
            let declared = view.columns.iter().map(|name| name.text.clone()).collect();
            (view.name.text.clone(), declared, query_outputs(&view.query, &empty, 0))
        }
        _ => return Err(not_supported()),
    };
    let columns: Vec<String> = outputs
        .iter()
        .enumerate()
        .map(|(at, output)| {
            let name = declared.get(at).cloned().unwrap_or_else(|| output.name.clone());
            let sources: Vec<String> = output.sources.iter().map(|source| json_string(source)).collect();
            format!("{{\"column\":{},\"sources\":[{}]}}", json_string(&name), sources.join(","))
        })
        .collect();
    Ok(format!("{{\"target\":{},\"columns\":[{}]}}", json_string(&target), columns.join(",")))
}

#[cfg(test)]
mod tests {
    use super::lineage;

    #[test]
    fn target_columns_trace_to_their_sources() {
        assert_eq!(
            lineage("INSERT INTO mart.revenue (customer, total) SELECT c.name, sum(o.amount * o.rate) FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.name").unwrap(),
            r#"{"target":"mart.revenue","columns":[{"column":"customer","sources":["customers.name"]},{"column":"total","sources":["orders.amount","orders.rate"]}]}"#
        );
        assert_eq!(
            lineage("CREATE TABLE big AS WITH recent AS (SELECT id, amount AS value FROM orders) SELECT r.value * 2 AS doubled FROM recent r").unwrap(),
            r#"{"target":"big","columns":[{"column":"doubled","sources":["orders.amount"]}]}"#
        );
        assert!(lineage("SELECT 1").is_err());
    }
}
