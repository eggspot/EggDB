mod oracle_plan;
mod showplan;

pub(crate) use oracle_plan::{
    DisplayedRow, MOST_DISPLAYED_ROWS, PLAN_DISPLAY_FUNCTION, PLAN_TABLE, create_plan_table,
    display_lines, display_result, insert_plan_row, missing_plan_lines, plan_rows,
};
pub(crate) use showplan::{showplan_all, showplan_text};

use crate::ast::{ExplainStyle, Statement};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::join::JoinMethod;
use crate::plan::{KeyChoice, Use};
use crate::session::{OutputColumn, QueryResult};
use crate::step_times::{MeasuredSelect, RowTimes};
use crate::store::PageLookups;
use crate::types::{SqlType, Value};
use std::time::Duration;

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

pub(crate) struct Buffers {
    pub(crate) planning: PageLookups,
    pub(crate) execution: PageLookups,
}

pub(crate) struct Analyzed {
    pub(crate) rows: u64,
    pub(crate) planning: Duration,
    pub(crate) execution: Duration,
    pub(crate) buffers: Option<Buffers>,
    pub(crate) selects: Vec<MeasuredSelect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Measured {
    Ran(RowTimes),
    NeverExecuted,
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn actual_group(measured: Measured) -> String {
    match measured {
        Measured::NeverExecuted => " (never executed)".to_owned(),
        Measured::Ran(times) => format!(
            " (actual time={:.3}..{:.3} rows={} loops=1)",
            milliseconds(times.first_row.min(times.last_row)),
            milliseconds(times.last_row),
            times.rows
        ),
    }
}

fn source_measured(select: &MeasuredSelect) -> Measured {
    select.source.map_or(Measured::NeverExecuted, Measured::Ran)
}

fn sort_measured(select: &MeasuredSelect) -> Measured {
    let Some(source) = select.source else {
        return Measured::NeverExecuted;
    };
    Measured::Ran(RowTimes {
        first_row: source.last_row.min(select.output.last_row),
        last_row: select.output.last_row,
        rows: select
            .output
            .rows
            .saturating_add(select.skipped)
            .min(source.rows),
    })
}

fn top_measured(measured: &[Option<Measured>], analyzed: &Analyzed) -> Measured {
    let own = measured.first().copied().flatten();
    let first_ran = measured.iter().take(2).find_map(|step| match step {
        Some(Measured::Ran(times)) => Some(times.first_row),
        _ => None,
    });
    let last_row = match own {
        Some(Measured::Ran(times)) => times.last_row,
        _ => analyzed.execution,
    };
    Measured::Ran(RowTimes {
        first_row: first_ran.unwrap_or(last_row).min(last_row),
        last_row,
        rows: analyzed.rows,
    })
}

fn measured_steps(query: &ExplainedQuery, analyzed: &Analyzed) -> Vec<Option<Measured>> {
    let mut measured = vec![None; steps(query).len()];
    let wrappers = usize::from(query.limited) + usize::from(query.sorted);
    if query.appended {
        let branches = analyzed.selects.iter().take(query.sources.len());
        for (position, select) in branches.enumerate() {
            if let Some(step) = measured.get_mut(wrappers + 1 + position) {
                *step = Some(source_measured(select));
            }
        }
    } else if let Some(select) = analyzed.selects.first() {
        if let Some(step) = measured.get_mut(wrappers) {
            *step = Some(source_measured(select));
        }
        if query.sorted
            && let Some(step) = measured.get_mut(usize::from(query.limited))
        {
            *step = Some(sort_measured(select));
        }
    }
    let top = top_measured(&measured, analyzed);
    if let Some(step) = measured.first_mut() {
        *step = Some(top);
    }
    measured
}

fn buffers_line(indent: &str, lookups: PageLookups) -> Option<String> {
    let mut parts = Vec::new();
    if lookups.hits > 0 {
        parts.push(format!("hit={}", lookups.hits));
    }
    if lookups.reads > 0 {
        parts.push(format!("read={}", lookups.reads));
    }
    (!parts.is_empty()).then(|| format!("{indent}Buffers: shared {}", parts.join(" ")))
}

/// `EXPLAIN ANALYZE` in PostgreSQL's text form: the plan with `(actual time=a..b rows=n
/// loops=1)` on the top node and on each scan, join, sort and append branch the executor
/// measured (milliseconds from the start of execution to the step's first and last row;
/// `(never executed)` for a step it never read), the rows the statement returned or changed on
/// the top line, under `BUFFERS` the page lookups of the whole statement under the top node and
/// those of planning under `Planning:`, and the planning and execution times.
pub(crate) fn render_analyzed(query: &ExplainedQuery, analyzed: Analyzed) -> QueryResult {
    let measured = measured_steps(query, &analyzed);
    let mut lines: Vec<String> = postgresql_lines(query)
        .into_iter()
        .zip(measured)
        .map(|(mut line, measured)| {
            if let Some(measured) = measured {
                line.push_str(&actual_group(measured));
            }
            line
        })
        .collect();
    if let Some(buffers) = &analyzed.buffers {
        if let Some(line) = buffers_line("  ", buffers.execution) {
            lines.insert(1.min(lines.len()), line);
        }
        if let Some(line) = buffers_line("  ", buffers.planning) {
            lines.push("Planning:".to_owned());
            lines.push(line);
        }
    }
    lines.push(format!(
        "Planning Time: {:.3} ms",
        milliseconds(analyzed.planning)
    ));
    lines.push(format!(
        "Execution Time: {:.3} ms",
        milliseconds(analyzed.execution)
    ));
    QueryResult::rows(
        "EXPLAIN".to_owned(),
        columns(ExplainStyle::Plan, Dialect::PostgreSql),
        lines
            .into_iter()
            .map(|line| vec![Value::Text(line)])
            .collect(),
    )
}

fn key_name(table: &TableDefinition, key: KeyChoice, dialect: Dialect) -> String {
    match (key, dialect) {
        (KeyChoice::Index(position), _) => table.indexes[position].name.clone(),
        (KeyChoice::Primary, Dialect::MySql) => "PRIMARY".to_owned(),
        (KeyChoice::Primary, _) => table.primary_key_constraint_name(),
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

enum Step<'q> {
    Result,
    Append,
    Sort,
    Limit,
    NestedLoop,
    HashJoin,
    Hash,
    Scan(&'q ExplainedSource),
    FullScan(&'q ExplainedSource),
    KeyLookup(&'q ExplainedSource, KeyChoice),
}

fn inner_steps(source: &ExplainedSource, method: JoinMethod) -> Vec<(usize, Step<'_>)> {
    match method {
        JoinMethod::PrimaryKey => vec![(0, Step::KeyLookup(source, KeyChoice::Primary))],
        JoinMethod::Index(position) => {
            vec![(0, Step::KeyLookup(source, KeyChoice::Index(position)))]
        }
        JoinMethod::Hash => vec![(0, Step::Hash), (1, Step::FullScan(source))],
        JoinMethod::Loop => vec![(0, Step::FullScan(source))],
    }
}

fn join_steps(query: &ExplainedQuery, upto: usize) -> Vec<(usize, Step<'_>)> {
    let Some(source) = query.sources.get(upto) else {
        return Vec::new();
    };
    let Some(method) = source.join else {
        return vec![(0, Step::Scan(source))];
    };
    let mut steps = vec![(
        0,
        match method {
            JoinMethod::Hash => Step::HashJoin,
            _ => Step::NestedLoop,
        },
    )];
    let outer = join_steps(query, upto.saturating_sub(1));
    steps.extend(outer.into_iter().map(|(level, step)| (level + 1, step)));
    steps.extend(
        inner_steps(source, method)
            .into_iter()
            .map(|(level, step)| (level + 1, step)),
    );
    steps
}

fn under<'q>(top: Step<'q>, steps: Vec<(usize, Step<'q>)>) -> Vec<(usize, Step<'q>)> {
    std::iter::once((0, top))
        .chain(steps.into_iter().map(|(level, step)| (level + 1, step)))
        .collect()
}

fn steps(query: &ExplainedQuery) -> Vec<(usize, Step<'_>)> {
    let mut steps = if query.sources.is_empty() {
        vec![(0, Step::Result)]
    } else if query.appended {
        under(
            Step::Append,
            query
                .sources
                .iter()
                .map(|source| (0, Step::Scan(source)))
                .collect(),
        )
    } else {
        join_steps(query, query.sources.len() - 1)
    };
    if query.sorted {
        steps = under(Step::Sort, steps);
    }
    if query.limited {
        steps = under(Step::Limit, steps);
    }
    steps
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlannedStatement {
    Select,
    Update,
    Delete,
}

impl PlannedStatement {
    pub(crate) fn of(statement: &Statement) -> Option<Self> {
        match statement {
            Statement::Select(_) | Statement::Query(_) => Some(Self::Select),
            Statement::Update(_) => Some(Self::Update),
            Statement::Delete(_) => Some(Self::Delete),
            _ => None,
        }
    }
}

fn steps_without_hashes(query: &ExplainedQuery) -> Vec<(usize, Step<'_>)> {
    let mut ancestors: Vec<(usize, bool)> = Vec::new();
    let mut kept = Vec::new();
    for (level, step) in steps(query) {
        while ancestors.last().is_some_and(|(at, _)| *at >= level) {
            ancestors.pop();
        }
        let hashes = ancestors.iter().filter(|(_, hash)| *hash).count();
        let hash = matches!(step, Step::Hash);
        ancestors.push((level, hash));
        if !hash {
            kept.push((level.saturating_sub(hashes), step));
        }
    }
    kept
}

fn postgresql_step(step: &Step<'_>) -> String {
    match step {
        Step::Result => "Result".to_owned(),
        Step::Append => "Append".to_owned(),
        Step::Sort => "Sort".to_owned(),
        Step::Limit => "Limit".to_owned(),
        Step::NestedLoop => "Nested Loop".to_owned(),
        Step::HashJoin => "Hash Join".to_owned(),
        Step::Hash => "Hash".to_owned(),
        Step::Scan(source) => postgresql_scan(source),
        Step::FullScan(source) => format!("Seq Scan on {}", named_relation(source)),
        Step::KeyLookup(source, key) => format!(
            "Index Scan using {} on {}",
            key_name(&source.table, *key, Dialect::PostgreSql),
            named_relation(source)
        ),
    }
}

fn postgresql_lines(query: &ExplainedQuery) -> Vec<String> {
    steps(query)
        .into_iter()
        .map(|(level, step)| match level {
            0 => postgresql_step(&step),
            _ => format!(
                "{}->  {}",
                " ".repeat(6 * level - 4),
                postgresql_step(&step)
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_print_only_the_counts_that_are_not_zero() {
        let lookups = |hits, reads| PageLookups { hits, reads };
        assert_eq!(
            buffers_line("  ", lookups(3, 2)).as_deref(),
            Some("  Buffers: shared hit=3 read=2")
        );
        assert_eq!(
            buffers_line("  ", lookups(0, 4)).as_deref(),
            Some("  Buffers: shared read=4")
        );
        assert_eq!(
            buffers_line("  ", lookups(5, 0)).as_deref(),
            Some("  Buffers: shared hit=5")
        );
        assert_eq!(buffers_line("  ", lookups(0, 0)), None);
    }

    fn micros(first_row: u64, last_row: u64, rows: u64) -> RowTimes {
        RowTimes {
            first_row: Duration::from_micros(first_row),
            last_row: Duration::from_micros(last_row),
            rows,
        }
    }

    fn analyzed(rows: u64, selects: Vec<MeasuredSelect>) -> Analyzed {
        Analyzed {
            rows,
            planning: Duration::from_micros(7),
            execution: Duration::from_micros(100),
            buffers: None,
            selects,
        }
    }

    fn plan_lines(query: &ExplainedQuery, analyzed: Analyzed) -> Vec<String> {
        render_analyzed(query, analyzed)
            .rows
            .into_iter()
            .filter_map(|row| match row.into_iter().next() {
                Some(Value::Text(line)) => Some(line),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_measured_step_prints_postgresqls_actual_time_rows_and_loops() {
        let measured = MeasuredSelect {
            source: Some(micros(10, 25, 1)),
            output: micros(10, 30, 1),
            skipped: 0,
        };
        let lines = plan_lines(&ExplainedQuery::default(), analyzed(1, vec![measured]));
        assert_eq!(lines[0], "Result (actual time=0.010..0.025 rows=1 loops=1)");
        assert_eq!(lines[1], "Planning Time: 0.007 ms");
        assert_eq!(lines[2], "Execution Time: 0.100 ms");
    }

    #[test]
    fn a_sort_under_a_limit_starts_when_its_input_ends_and_counts_what_it_handed_up() {
        let query = ExplainedQuery {
            sorted: true,
            limited: true,
            ..ExplainedQuery::default()
        };
        let measured = MeasuredSelect {
            source: Some(micros(5, 40, 3)),
            output: micros(5, 60, 1),
            skipped: 1,
        };
        let lines = plan_lines(&query, analyzed(1, vec![measured]));
        assert_eq!(
            lines[..3],
            [
                "Limit (actual time=0.040..0.100 rows=1 loops=1)",
                "  ->  Sort (actual time=0.040..0.060 rows=2 loops=1)",
                "        ->  Result (actual time=0.005..0.040 rows=3 loops=1)",
            ]
        );
    }

    #[test]
    fn a_step_the_executor_never_read_says_never_executed() {
        let query = ExplainedQuery {
            sorted: true,
            limited: true,
            ..ExplainedQuery::default()
        };
        let measured = MeasuredSelect {
            source: None,
            output: micros(50, 50, 0),
            skipped: 0,
        };
        let lines = plan_lines(&query, analyzed(0, vec![measured]));
        assert_eq!(
            lines[..3],
            [
                "Limit (actual time=0.100..0.100 rows=0 loops=1)",
                "  ->  Sort (never executed)",
                "        ->  Result (never executed)",
            ]
        );
    }

    #[test]
    fn without_measured_steps_the_top_node_carries_the_statements_rows_and_time() {
        assert_eq!(
            plan_lines(&ExplainedQuery::default(), analyzed(4, Vec::new()))[0],
            "Result (actual time=0.100..0.100 rows=4 loops=1)"
        );
    }
}
