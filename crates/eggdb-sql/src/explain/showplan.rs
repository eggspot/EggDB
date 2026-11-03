use super::{PlannedStatement, Step, key_name, steps_without_hashes};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::plan::KeyChoice;
use crate::session::{OutputColumn, QueryResult};
use crate::types::{SqlType, Value};

const STATEMENT_NODE: i32 = 1;

const SHOWPLAN_ALL_COLUMNS: [(&str, SqlType); 18] = [
    ("StmtText", SqlType::Text),
    ("StmtId", SqlType::Integer),
    ("NodeId", SqlType::Integer),
    ("Parent", SqlType::Integer),
    ("PhysicalOp", SqlType::Text),
    ("LogicalOp", SqlType::Text),
    ("Argument", SqlType::Text),
    ("DefinedValues", SqlType::Text),
    ("EstimateRows", SqlType::Real),
    ("EstimateIO", SqlType::Real),
    ("EstimateCPU", SqlType::Real),
    ("AvgRowSize", SqlType::Integer),
    ("TotalSubtreeCost", SqlType::Real),
    ("OutputList", SqlType::Text),
    ("Warnings", SqlType::Text),
    ("Type", SqlType::Text),
    ("Parallel", SqlType::Boolean),
    ("EstimateExecutions", SqlType::Real),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Operator {
    depth: usize,
    physical: &'static str,
    logical: &'static str,
    argument: Option<String>,
}

fn bracketed(name: &str) -> String {
    name.split(crate::schemas::QUALIFIER)
        .map(|part| format!("[{}]", part.replace(']', "]]")))
        .collect::<Vec<_>>()
        .join(".")
}

fn alias(source: &ExplainedSource) -> String {
    if source.label == source.table.name {
        String::new()
    } else {
        format!(" AS {}", bracketed(&source.label))
    }
}

fn clustered(table: &TableDefinition) -> bool {
    !table.primary_key.is_empty()
}

fn scan(depth: usize, source: &ExplainedSource) -> Operator {
    let table = &source.table;
    if clustered(table) {
        Operator {
            depth,
            physical: "Clustered Index Scan",
            logical: "Clustered Index Scan",
            argument: Some(format!(
                "OBJECT:({}.{}{})",
                bracketed(&table.name),
                bracketed(&key_name(table, KeyChoice::Primary, Dialect::SqlServer)),
                alias(source)
            )),
        }
    } else {
        Operator {
            depth,
            physical: "Table Scan",
            logical: "Table Scan",
            argument: Some(format!(
                "OBJECT:({}{})",
                bracketed(&table.name),
                alias(source)
            )),
        }
    }
}

fn seek(depth: usize, source: &ExplainedSource, key: KeyChoice, backward: bool) -> Operator {
    let operation = match key {
        KeyChoice::Primary => "Clustered Index Seek",
        KeyChoice::Index(_) => "Index Seek",
    };
    Operator {
        depth,
        physical: operation,
        logical: operation,
        argument: Some(format!(
            "OBJECT:({}.{}{}) ORDERED {}",
            bracketed(&source.table.name),
            bracketed(&key_name(&source.table, key, Dialect::SqlServer)),
            alias(source),
            if backward { "BACKWARD" } else { "FORWARD" }
        )),
    }
}

fn change(statement: PlannedStatement, source: &ExplainedSource) -> Option<Operator> {
    let clustered = clustered(&source.table);
    let (physical, logical) = match (statement, clustered) {
        (PlannedStatement::Select, _) => return None,
        (PlannedStatement::Update, true) => ("Clustered Index Update", "Update"),
        (PlannedStatement::Update, false) => ("Table Update", "Update"),
        (PlannedStatement::Delete, true) => ("Clustered Index Delete", "Delete"),
        (PlannedStatement::Delete, false) => ("Table Delete", "Delete"),
    };
    Some(Operator {
        depth: 0,
        physical,
        logical,
        argument: Some(format!("OBJECT:({})", bracketed(&source.table.name))),
    })
}

fn operators(query: &ExplainedQuery, statement: PlannedStatement) -> Vec<Operator> {
    let mut out: Vec<Operator> = query
        .sources
        .first()
        .and_then(|source| change(statement, source))
        .into_iter()
        .collect();
    let base = out.len();
    let plain = |depth, physical, logical| Operator {
        depth,
        physical,
        logical,
        argument: None,
    };
    for (level, step) in steps_without_hashes(query) {
        let depth = base + level;
        out.push(match step {
            Step::Result => plain(depth, "Constant Scan", "Constant Scan"),
            Step::Append => plain(depth, "Concatenation", "Concatenation"),
            Step::Sort => plain(depth, "Sort", "Sort"),
            Step::Limit => plain(depth, "Top", "Top"),
            Step::NestedLoop => plain(depth, "Nested Loops", "Inner Join"),
            Step::HashJoin | Step::Hash => plain(depth, "Hash Match", "Inner Join"),
            Step::FullScan(source) => scan(depth, source),
            Step::Scan(source) => match source.access.key() {
                None => scan(depth, source),
                Some(key) => seek(depth, source, key, source.access.backward()),
            },
            Step::KeyLookup(source, key) => seek(depth, source, key, false),
        });
    }
    out
}

fn operator_text(operator: &Operator) -> String {
    let inside: Vec<&str> = [
        (operator.logical != operator.physical).then_some(operator.logical),
        operator.argument.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if inside.is_empty() {
        operator.physical.to_owned()
    } else {
        format!("{}({})", operator.physical, inside.join(", "))
    }
}

fn tree_lines(operators: &[Operator]) -> Vec<String> {
    let continues: Vec<bool> = operators
        .iter()
        .enumerate()
        .map(|(at, operator)| {
            operators
                .get(at + 1..)
                .unwrap_or_default()
                .iter()
                .take_while(|later| later.depth >= operator.depth)
                .any(|later| later.depth == operator.depth)
        })
        .collect();
    let mut ancestors: Vec<usize> = Vec::new();
    let mut lines = Vec::with_capacity(operators.len());
    for (at, operator) in operators.iter().enumerate() {
        ancestors.truncate(operator.depth);
        let mut line = String::from("  ");
        for &ancestor in &ancestors {
            line.push_str(if continues.get(ancestor).copied().unwrap_or(false) {
                "|    "
            } else {
                "     "
            });
        }
        line.push_str("|--");
        line.push_str(&operator_text(operator));
        lines.push(line);
        ancestors.push(at);
    }
    lines
}

fn statement_type(text: &str) -> String {
    text.split_whitespace()
        .next()
        .map(str::to_ascii_uppercase)
        .unwrap_or_default()
}

fn statement_text_result(text: &str) -> QueryResult {
    QueryResult::rows(
        "SELECT 1".to_owned(),
        vec![OutputColumn {
            name: "StmtText".to_owned(),
            sql_type: Some(SqlType::Text),
        }],
        vec![vec![Value::Text(text.to_owned())]],
    )
}

/// What `SET SHOWPLAN_TEXT ON` returns for a statement it does not run: a result holding the
/// statement's text, then, when EggDB can plan it, a result with one `StmtText` row per
/// operator drawn as SQL Server's `|--` tree.
pub(crate) fn showplan_text(
    plan: Option<(&ExplainedQuery, PlannedStatement)>,
    text: &str,
) -> Vec<QueryResult> {
    let mut results = vec![statement_text_result(text)];
    if let Some((query, statement)) = plan {
        let lines = tree_lines(&operators(query, statement));
        results.push(QueryResult::rows(
            format!("SELECT {}", lines.len()),
            vec![OutputColumn {
                name: "StmtText".to_owned(),
                sql_type: Some(SqlType::Text),
            }],
            lines
                .into_iter()
                .map(|line| vec![Value::Text(line)])
                .collect(),
        ));
    }
    results
}

fn node(number: usize) -> Value {
    Value::Integer(i32::try_from(number).unwrap_or(i32::MAX).saturating_add(2))
}

fn counted(number: u64) -> Value {
    Value::BigInt(i64::try_from(number).unwrap_or(i64::MAX))
}

/// What `SET SHOWPLAN_ALL ON` returns for a statement it does not run, in SQL Server's 18
/// columns: the statement's row (`Type` is its verb), then one `PLAN_ROW` per operator with
/// its `NodeId` and its `Parent`'s. EggDB has no estimates, so those columns are NULL. With
/// `profiled_rows`, the statement ran under `SET STATISTICS PROFILE ON`: `Rows` and
/// `Executes` come first, given for the statement and its top operator.
pub(crate) fn showplan_all(
    plan: Option<(&ExplainedQuery, PlannedStatement)>,
    text: &str,
    profiled_rows: Option<u64>,
) -> QueryResult {
    let operators = plan.map_or_else(Vec::new, |(query, statement)| operators(query, statement));
    let lines = tree_lines(&operators);
    let mut columns: Vec<OutputColumn> = Vec::new();
    if profiled_rows.is_some() {
        for name in ["Rows", "Executes"] {
            columns.push(OutputColumn {
                name: name.to_owned(),
                sql_type: Some(SqlType::BigInt),
            });
        }
    }
    columns.extend(
        SHOWPLAN_ALL_COLUMNS
            .iter()
            .map(|(name, sql_type)| OutputColumn {
                name: (*name).to_owned(),
                sql_type: Some(*sql_type),
            }),
    );
    let measured = |row: &mut Vec<Value>, measured: bool| {
        if let Some(rows) = profiled_rows {
            if measured {
                row.extend([counted(rows), counted(1)]);
            } else {
                row.extend([Value::Null, Value::Null]);
            }
        }
    };
    let nulls = |row: &mut Vec<Value>, count: usize| row.extend((0..count).map(|_| Value::Null));
    let mut rows = Vec::with_capacity(operators.len() + 1);
    let mut statement_row = Vec::with_capacity(columns.len());
    measured(&mut statement_row, true);
    statement_row.extend([
        Value::Text(text.to_owned()),
        Value::Integer(1),
        Value::Integer(STATEMENT_NODE),
        Value::Integer(0),
    ]);
    nulls(&mut statement_row, 11);
    statement_row.push(Value::Text(statement_type(text)));
    nulls(&mut statement_row, 2);
    rows.push(statement_row);
    let mut open: Vec<usize> = Vec::new();
    for (at, (operator, line)) in operators.iter().zip(lines).enumerate() {
        open.truncate(operator.depth);
        let parent = open
            .last()
            .map_or(Value::Integer(STATEMENT_NODE), |&parent| node(parent));
        open.push(at);
        let mut row = Vec::with_capacity(columns.len());
        measured(&mut row, at == 0);
        row.extend([
            Value::Text(line),
            Value::Integer(1),
            node(at),
            parent,
            Value::Text(operator.physical.to_owned()),
            Value::Text(operator.logical.to_owned()),
            operator.argument.clone().map_or(Value::Null, Value::Text),
        ]);
        nulls(&mut row, 8);
        row.push(Value::Text("PLAN_ROW".to_owned()));
        row.push(Value::Boolean(false));
        row.push(Value::Null);
        rows.push(row);
    }
    QueryResult::rows(format!("SELECT {}", rows.len()), columns, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator(depth: usize, physical: &'static str, logical: &'static str) -> Operator {
        Operator {
            depth,
            physical,
            logical,
            argument: None,
        }
    }

    #[test]
    fn the_text_tree_draws_a_bar_down_to_each_later_sibling() {
        let lines = tree_lines(&[
            operator(0, "Sort", "Sort"),
            operator(1, "Hash Match", "Inner Join"),
            Operator {
                argument: Some("OBJECT:([a])".to_owned()),
                ..operator(2, "Table Scan", "Table Scan")
            },
            operator(2, "Table Scan", "Table Scan"),
        ]);
        assert_eq!(
            lines,
            [
                "  |--Sort",
                "       |--Hash Match(Inner Join)",
                "            |--Table Scan(OBJECT:([a]))",
                "            |--Table Scan",
            ]
        );
        let branched = tree_lines(&[
            operator(0, "Nested Loops", "Inner Join"),
            operator(1, "Sort", "Sort"),
            operator(2, "Table Scan", "Table Scan"),
            operator(1, "Index Seek", "Index Seek"),
        ]);
        assert_eq!(branched[2], "       |    |--Table Scan");
        assert_eq!(branched[3], "       |--Index Seek");
    }

    #[test]
    fn names_are_bracketed_part_by_part_and_brackets_doubled() {
        assert_eq!(bracketed("t"), "[t]");
        assert_eq!(bracketed("sales\0order]s"), "[sales].[order]]s]");
        assert_eq!(statement_type("  select 1"), "SELECT");
        assert_eq!(statement_type(""), "");
    }
}
