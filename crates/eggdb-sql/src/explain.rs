use crate::ast::ExplainStyle;
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::join::JoinMethod;
use crate::plan::{KeyChoice, Use};
use crate::session::{OutputColumn, QueryResult};
use crate::types::{SqlType, Value};

const MYSQL_COLUMNS: [&str; 12] = [
    "id",
    "select_type",
    "table",
    "partitions",
    "type",
    "possible_keys",
    "key",
    "key_len",
    "ref",
    "rows",
    "filtered",
    "Extra",
];

pub(crate) fn columns(style: ExplainStyle, dialect: Dialect) -> Vec<OutputColumn> {
    let text = |name: &str| OutputColumn {
        name: name.to_owned(),
        sql_type: Some(SqlType::Text),
    };
    let number = |name: &str| OutputColumn {
        name: name.to_owned(),
        sql_type: Some(SqlType::BigInt),
    };
    match (style, dialect) {
        (ExplainStyle::QueryPlan, _) | (_, Dialect::Sqlite) => vec![
            number("id"),
            number("parent"),
            number("notused"),
            text("detail"),
        ],
        (ExplainStyle::Plan, Dialect::MySql) => MYSQL_COLUMNS
            .iter()
            .map(|&name| {
                if name == "id" {
                    number(name)
                } else {
                    text(name)
                }
            })
            .collect(),
        (ExplainStyle::Plan, _) => vec![text("QUERY PLAN")],
    }
}

pub(crate) fn render(query: &ExplainedQuery, style: ExplainStyle, dialect: Dialect) -> QueryResult {
    let rows = match (style, dialect) {
        (ExplainStyle::QueryPlan, _) | (_, Dialect::Sqlite) => sqlite_rows(query),
        (ExplainStyle::Plan, Dialect::MySql) => mysql_rows(query),
        (ExplainStyle::Plan, _) => postgresql_lines(query)
            .into_iter()
            .map(|line| vec![Value::Text(line)])
            .collect(),
    };
    let tag = match dialect {
        Dialect::PostgreSql => "EXPLAIN".to_owned(),
        _ => format!("SELECT {}", rows.len()),
    };
    QueryResult::rows(tag, columns(style, dialect), rows)
}

fn key_name(table: &TableDefinition, key: KeyChoice, dialect: Dialect) -> String {
    match (key, dialect) {
        (KeyChoice::Index(position), _) => table.indexes[position].name.clone(),
        (KeyChoice::Primary, Dialect::MySql) => "PRIMARY".to_owned(),
        (KeyChoice::Primary, _) => format!("{}_pkey", table.name),
    }
}

fn named_relation(source: &ExplainedSource) -> String {
    if source.label == source.table.name {
        source.table.name.clone()
    } else {
        format!("{} {}", source.table.name, source.label)
    }
}

fn postgresql_scan(source: &ExplainedSource) -> String {
    let relation = named_relation(source);
    match source.access.key() {
        None => format!("Seq Scan on {relation}"),
        Some(key) => format!(
            "Index Scan{} using {} on {relation}",
            if source.access.backward() {
                " Backward"
            } else {
                ""
            },
            key_name(&source.table, key, Dialect::PostgreSql)
        ),
    }
}

fn postgresql_inner(source: &ExplainedSource, method: JoinMethod) -> Vec<(usize, String)> {
    let relation = named_relation(source);
    match method {
        JoinMethod::PrimaryKey => vec![(
            0,
            format!(
                "Index Scan using {} on {relation}",
                key_name(&source.table, KeyChoice::Primary, Dialect::PostgreSql)
            ),
        )],
        JoinMethod::Index(position) => vec![(
            0,
            format!(
                "Index Scan using {} on {relation}",
                source.table.indexes[position].name
            ),
        )],
        JoinMethod::Hash => vec![
            (0, "Hash".to_owned()),
            (1, format!("Seq Scan on {relation}")),
        ],
        JoinMethod::Loop => vec![(0, format!("Seq Scan on {relation}"))],
    }
}

fn postgresql_tree(query: &ExplainedQuery, upto: usize) -> Vec<(usize, String)> {
    let Some(source) = query.sources.get(upto) else {
        return Vec::new();
    };
    let Some(method) = source.join else {
        return vec![(0, postgresql_scan(source))];
    };
    let mut lines = vec![(
        0,
        match method {
            JoinMethod::Hash => "Hash Join".to_owned(),
            _ => "Nested Loop".to_owned(),
        },
    )];
    let outer = postgresql_tree(query, upto.saturating_sub(1));
    lines.extend(outer.into_iter().map(|(level, text)| (level + 1, text)));
    lines.extend(
        postgresql_inner(source, method)
            .into_iter()
            .map(|(level, text)| (level + 1, text)),
    );
    lines
}

fn postgresql_lines(query: &ExplainedQuery) -> Vec<String> {
    let mut tree = if query.sources.is_empty() {
        vec![(0, "Result".to_owned())]
    } else {
        postgresql_tree(query, query.sources.len() - 1)
    };
    if query.sorted {
        tree = std::iter::once((0, "Sort".to_owned()))
            .chain(tree.into_iter().map(|(level, text)| (level + 1, text)))
            .collect();
    }
    if query.limited {
        tree = std::iter::once((0, "Limit".to_owned()))
            .chain(tree.into_iter().map(|(level, text)| (level + 1, text)))
            .collect();
    }
    tree.into_iter()
        .map(|(level, text)| match level {
            0 => text,
            _ => format!("{}->  {text}", " ".repeat(6 * level - 4)),
        })
        .collect()
}

fn mysql_rows(query: &ExplainedQuery) -> Vec<Vec<Value>> {
    let null = || Value::Null;
    let text = |text: &str| Value::Text(text.to_owned());
    query
        .sources
        .iter()
        .enumerate()
        .map(|(position, source)| {
            let table = &source.table;
            let (kind, key) = match source.join {
                Some(JoinMethod::PrimaryKey) => ("eq_ref", Some(KeyChoice::Primary)),
                Some(JoinMethod::Index(index)) => ("ref", Some(KeyChoice::Index(index))),
                Some(JoinMethod::Hash | JoinMethod::Loop) => ("ALL", None),
                None => (
                    match source.access.used {
                        Use::Scan => "ALL",
                        Use::Point => "const",
                        Use::Equal => "ref",
                        Use::Range => "range",
                        Use::Order => "index",
                    },
                    source.access.key(),
                ),
            };
            let mut extra = Vec::new();
            if query.filtered && kind != "const" {
                extra.push("Using where");
            }
            if source.join == Some(JoinMethod::Hash) {
                extra.push("Using join buffer (hash join)");
            }
            if source.join.is_none() && source.access.backward() {
                extra.push("Backward index scan");
            }
            if position == 0 && query.sorted {
                extra.push("Using filesort");
            }
            let key = key.map(|key| key_name(table, key, Dialect::MySql));
            vec![
                Value::BigInt(1),
                text("SIMPLE"),
                text(&source.label),
                null(),
                text(kind),
                key.as_deref().map_or(Value::Null, text),
                key.as_deref().map_or(Value::Null, text),
                null(),
                null(),
                null(),
                text("100.00"),
                if extra.is_empty() {
                    Value::Null
                } else {
                    text(&extra.join("; "))
                },
            ]
        })
        .collect()
}

fn sqlite_conditions(
    table: &TableDefinition,
    conditions: &[(usize, &'static str)],
    rowid: bool,
) -> String {
    let parts: Vec<String> = conditions
        .iter()
        .map(|&(column, operator)| {
            let name = if rowid {
                "rowid"
            } else {
                table.columns[column].name.as_str()
            };
            let operator = match operator {
                ">=" => ">",
                "<=" => "<",
                other => other,
            };
            format!("{name}{operator}?")
        })
        .collect();
    format!("({})", parts.join(" AND "))
}

fn rowid_key(table: &TableDefinition) -> bool {
    matches!(table.primary_key.as_slice(), [only] if table.columns[*only].auto_increment)
}

fn sqlite_detail(source: &ExplainedSource) -> String {
    let table = &source.table;
    let label = &source.label;
    if let Some(method) = source.join {
        return match method {
            JoinMethod::PrimaryKey if rowid_key(table) => {
                format!("SEARCH {label} USING INTEGER PRIMARY KEY (rowid=?)")
            }
            JoinMethod::PrimaryKey => {
                let conditions: Vec<(usize, &'static str)> = table
                    .primary_key
                    .iter()
                    .map(|&column| (column, "="))
                    .collect();
                format!(
                    "SEARCH {label} USING PRIMARY KEY {}",
                    sqlite_conditions(table, &conditions, false)
                )
            }
            JoinMethod::Index(position) => {
                let index = &table.indexes[position];
                format!(
                    "SEARCH {label} USING INDEX {} ({}=?)",
                    index.name, table.columns[index.columns[0]].name
                )
            }
            JoinMethod::Hash | JoinMethod::Loop => format!("SCAN {label}"),
        };
    }
    let access = &source.access;
    match (access.key(), access.used) {
        (None, _) | (Some(KeyChoice::Primary), Use::Order) => format!("SCAN {label}"),
        (Some(KeyChoice::Index(position)), Use::Order) => {
            format!("SCAN {label} USING INDEX {}", table.indexes[position].name)
        }
        (Some(KeyChoice::Primary), _) if rowid_key(table) => format!(
            "SEARCH {label} USING INTEGER PRIMARY KEY {}",
            sqlite_conditions(table, &access.conditions, true)
        ),
        (Some(KeyChoice::Primary), _) => format!(
            "SEARCH {label} USING PRIMARY KEY {}",
            sqlite_conditions(table, &access.conditions, false)
        ),
        (Some(KeyChoice::Index(position)), _) => format!(
            "SEARCH {label} USING INDEX {} {}",
            table.indexes[position].name,
            sqlite_conditions(table, &access.conditions, false)
        ),
    }
}

fn sqlite_rows(query: &ExplainedQuery) -> Vec<Vec<Value>> {
    let mut details: Vec<String> = query.sources.iter().map(sqlite_detail).collect();
    if query.sorted {
        details.push("USE TEMP B-TREE FOR ORDER BY".to_owned());
    }
    details
        .into_iter()
        .enumerate()
        .map(|(position, detail)| {
            vec![
                Value::BigInt(position as i64 + 2),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Text(detail),
            ]
        })
        .collect()
}
