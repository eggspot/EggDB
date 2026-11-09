use super::estimates::{self, Estimate, ROW_COST, TableRows};
use super::expression_text::{ExpressionText, Spelling};
use super::subplans::{Subplan, SubplanPlace};
use super::{
    Analyzed, ExplainedGrouping, Measured, Planned, PlannedStatement, PlannedWrite, Step, key_name,
    measured_steps, milliseconds, source_measured, steps,
};
use crate::ast::{BinaryOperator, Expr, ExprKind};
use crate::dialect::Dialect;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::join::JoinMethod;
use crate::plan::{KeyChoice, Use};
use crate::step_times::{MeasuredSelect, MeasuredSubplan, RowTimes};

const NOT_EXECUTABLE: &str = "<not executable by iterator executor>";

struct TreeLine {
    level: usize,
    text: String,
    estimate: Option<Estimate>,
    measured: Option<Measured>,
}

struct Tree<'q> {
    numbers: Vec<(usize, usize)>,
    measured_subplans: Option<&'q [MeasuredSubplan]>,
    table_rows: &'q TableRows,
    lines: Vec<TreeLine>,
}

#[derive(Default)]
struct QueryMeasures {
    steps: Vec<Option<Measured>>,
    grouped: Option<Measured>,
}

enum Wrapper<'q> {
    Stream,
    Having(&'q Expr),
    Aggregate(&'q ExplainedGrouping),
    Where(&'q Expr),
}

fn streams_results(query: &ExplainedQuery) -> bool {
    query.sorted && !query.appended && (query.grouping.is_some() || query.sources.len() > 1)
}

fn wrappers<'q>(query: &'q ExplainedQuery, filter: Option<&'q Expr>) -> Vec<Wrapper<'q>> {
    let mut wrappers = Vec::new();
    if streams_results(query) {
        wrappers.push(Wrapper::Stream);
    }
    if let Some(grouping) = &query.grouping {
        if let Some(having) = &grouping.having {
            wrappers.push(Wrapper::Having(having));
        }
        wrappers.push(Wrapper::Aggregate(grouping));
    }
    if let Some(filter) = filter {
        wrappers.push(Wrapper::Where(filter));
    }
    wrappers
}

fn child_positions(all: &[(usize, Step<'_>)], at: usize) -> Vec<usize> {
    let Some((level, _)) = all.get(at) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for (offset, (child_level, _)) in all.iter().enumerate().skip(at + 1) {
        if *child_level <= *level {
            break;
        }
        if *child_level == level + 1 {
            children.push(offset);
        }
    }
    children
}

fn groups_measured(select: &MeasuredSelect) -> Measured {
    let Some(source) = select.source else {
        return Measured::NeverExecuted;
    };
    Measured::Ran(RowTimes {
        first_row: source.last_row,
        last_row: source.last_row,
        rows: select.output.rows.saturating_add(select.skipped),
    })
}

fn tree_measures(query: &ExplainedQuery, analyzed: &Analyzed) -> QueryMeasures {
    let mut steps = measured_steps(query, analyzed);
    let mut grouped = None;
    if query.grouping.is_some()
        && !query.appended
        && let Some(select) = analyzed.selects.first()
    {
        let attach = usize::from(query.limited) + usize::from(query.sorted);
        grouped = Some(if attach == 0 {
            steps
                .first()
                .copied()
                .flatten()
                .unwrap_or(Measured::NeverExecuted)
        } else {
            groups_measured(select)
        });
        if let Some(step) = steps.get_mut(attach) {
            *step = Some(source_measured(select));
        }
    }
    QueryMeasures { steps, grouped }
}

fn write_text(query: &ExplainedQuery, write: &PlannedWrite) -> String {
    let tables: Vec<String> = write
        .targets
        .iter()
        .filter_map(|&at| query.sources.get(at))
        .map(|source| format!("{} (buffered)", source.label))
        .collect();
    match write.statement {
        PlannedStatement::Delete => format!("Delete from {}", tables.join(", ")),
        PlannedStatement::Update | PlannedStatement::Select => {
            format!("Update {}", tables.join(", "))
        }
    }
}

fn collect_positions(query: &ExplainedQuery, out: &mut Vec<usize>) {
    for subplan in &query.subplans {
        out.push(subplan.position);
        collect_positions(&subplan.plan, out);
    }
}

fn select_numbers(query: &ExplainedQuery) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    collect_positions(query, &mut positions);
    positions.sort_unstable();
    positions
        .into_iter()
        .enumerate()
        .map(|(at, position)| (position, at + 2))
        .collect()
}

fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::And(parts) => parts.iter().flat_map(conjuncts).collect(),
        _ => vec![expr],
    }
}

pub(super) fn unconsumed_filter(query: &ExplainedQuery) -> Option<&Expr> {
    let filter = query.filter.as_ref()?;
    let consumed = match query.sources.as_slice() {
        [only] if only.join.is_none() && !query.appended => {
            only.access.key().is_some()
                && !super::subplans::mentions_subplan(filter)
                && conjuncts(filter).len() == only.access.conditions.len()
        }
        _ => false,
    };
    (!consumed).then_some(filter)
}

fn names_column(expr: &Expr, label: &str, column: &str) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Column { qualifier, name }
            if name.eq_ignore_ascii_case(column)
                && qualifier.as_deref().is_none_or(|qualifier| qualifier.eq_ignore_ascii_case(label))
    )
}

fn sql_value_of<'e>(conditions: &[&'e Expr], label: &str, column: &str) -> Option<&'e Expr> {
    conditions
        .iter()
        .find_map(|&condition| match &condition.kind {
            ExprKind::Binary(BinaryOperator::Equal, left, right) => {
                if names_column(left, label, column) {
                    Some(&**right)
                } else if names_column(right, label, column) {
                    Some(&**left)
                } else {
                    None
                }
            }
            _ => None,
        })
}

impl Tree<'_> {
    fn text(&self, query: &ExplainedQuery, expr: &Expr) -> String {
        let qualifier = match query.sources.as_slice() {
            [only] => Some(only.label.as_str()),
            _ => None,
        };
        ExpressionText {
            spelling: Spelling::MySql,
            qualifier,
            bare_columns: false,
            subplans: &query.subplans,
            select_numbers: &self.numbers,
        }
        .text(expr)
    }

    fn lookup(
        &self,
        query: &ExplainedQuery,
        source: &ExplainedSource,
        columns: &[usize],
    ) -> String {
        let mut conditions: Vec<&Expr> = query.filter.iter().flat_map(conjuncts).collect();
        for condition in query.join_conditions.iter().flatten() {
            conditions.extend(conjuncts(condition));
        }
        let parts: Vec<String> = columns
            .iter()
            .filter_map(|&column| source.table.columns.get(column))
            .map(|column| {
                let name = column.name.as_str();
                let value = sql_value_of(&conditions, &source.label, name)
                    .map_or_else(|| "?".to_owned(), |value| self.text(query, value));
                format!("{name}={value}")
            })
            .collect();
        parts.join(", ")
    }

    fn key_lookup(
        &self,
        query: &ExplainedQuery,
        source: &ExplainedSource,
        key: KeyChoice,
        single_row: bool,
    ) -> String {
        let columns: Vec<usize> = match key {
            KeyChoice::Primary => source.table.primary_key.clone(),
            KeyChoice::Index(position) => source
                .table
                .indexes
                .get(position)
                .map(|index| index.columns.clone())
                .unwrap_or_default(),
        };
        let kind = if single_row {
            "Single-row index lookup"
        } else {
            "Index lookup"
        };
        format!(
            "{kind} on {} using {} ({})",
            source.label,
            key_name(&source.table, key, Dialect::MySql),
            self.lookup(query, source, &columns)
        )
    }

    fn scan(&self, query: &ExplainedQuery, source: &ExplainedSource) -> String {
        let access = &source.access;
        let Some(key) = access.key() else {
            return format!("Table scan on {}", source.label);
        };
        let reverse = if access.backward() { " (reverse)" } else { "" };
        let equal_columns: Vec<usize> = access
            .conditions
            .iter()
            .filter(|(_, operator)| *operator == "=")
            .map(|(column, _)| *column)
            .collect();
        match access.used {
            Use::Point => format!(
                "Single-row index lookup on {} using {} ({})",
                source.label,
                key_name(&source.table, key, Dialect::MySql),
                self.lookup(query, source, &equal_columns)
            ),
            Use::Equal => format!(
                "Index lookup on {} using {} ({})",
                source.label,
                key_name(&source.table, key, Dialect::MySql),
                self.lookup(query, source, &equal_columns)
            ),
            Use::Range => format!(
                "Index range scan on {} using {}{reverse}",
                source.label,
                key_name(&source.table, key, Dialect::MySql)
            ),
            Use::Order | Use::Scan => format!(
                "Index scan on {} using {}{reverse}",
                source.label,
                key_name(&source.table, key, Dialect::MySql)
            ),
        }
    }

    fn join_source<'q>(all: &'q [(usize, Step<'q>)], at: usize) -> Option<&'q ExplainedSource> {
        let level = all.get(at)?.0;
        let mut inner = None;
        for (offset, (child_level, step)) in all.iter().enumerate().skip(at + 1) {
            if *child_level <= level {
                break;
            }
            if *child_level == level + 1 {
                inner = Some((offset, step));
            }
        }
        match inner? {
            (_, Step::KeyLookup(source, _) | Step::FullScan(source) | Step::Scan(source)) => {
                Some(*source)
            }
            (offset, Step::Hash) => match all.get(offset + 1) {
                Some((_, Step::FullScan(source))) => Some(*source),
                _ => None,
            },
            _ => None,
        }
    }

    fn sort(&self, query: &ExplainedQuery) -> String {
        let keys: Vec<String> = query
            .order_by
            .iter()
            .map(|item| {
                let key = self.text(query, &item.expr);
                if item.descending {
                    format!("{key} DESC")
                } else {
                    key
                }
            })
            .collect();
        let mut text = format!("Sort: {}", keys.join(", "));
        if let Some(limit) = query.row_limit {
            text.push_str(&format!(
                ", limit input to {} row(s) per chunk",
                limit.saturating_add(query.row_offset)
            ));
        }
        text
    }

    fn limit(query: &ExplainedQuery) -> String {
        match (query.row_limit, query.row_offset) {
            (Some(limit), 0) => format!("Limit: {limit} row(s)"),
            (Some(limit), offset) => format!("Limit/Offset: {limit}/{offset} row(s)"),
            (None, _) => "Limit".to_owned(),
        }
    }

    fn step_text(
        &self,
        query: &ExplainedQuery,
        all: &[(usize, Step<'_>)],
        at: usize,
        step: &Step<'_>,
    ) -> String {
        match step {
            Step::Result => "Rows fetched before execution".to_owned(),
            Step::Append => "Append".to_owned(),
            Step::Sort => self.sort(query),
            Step::Limit => Self::limit(query),
            Step::NestedLoop => "Nested loop inner join".to_owned(),
            Step::HashJoin => {
                let condition = Self::join_source(all, at).and_then(|source| {
                    let position = query
                        .sources
                        .iter()
                        .position(|candidate| std::ptr::eq(candidate, source))?;
                    query.join_conditions.get(position)?.as_ref()
                });
                match condition {
                    Some(condition) => {
                        let parts: Vec<String> = conjuncts(condition)
                            .into_iter()
                            .map(|part| parenthesized(self.text(query, part)))
                            .collect();
                        format!("Inner hash join {}", parts.join(", "))
                    }
                    None => "Inner hash join".to_owned(),
                }
            }
            Step::Hash => "Hash".to_owned(),
            Step::Scan(source) => self.scan(query, source),
            Step::FullScan(source) => format!("Table scan on {}", source.label),
            Step::KeyLookup(source, key) => {
                self.key_lookup(query, source, *key, matches!(key, KeyChoice::Primary))
            }
        }
    }

    fn join_fraction(query: &ExplainedQuery, all: &[(usize, Step<'_>)], at: usize) -> f64 {
        let Some(source) = Self::join_source(all, at) else {
            return 1.0;
        };
        if matches!(
            source.join,
            Some(JoinMethod::PrimaryKey | JoinMethod::Index(_))
        ) {
            return 1.0;
        }
        query
            .sources
            .iter()
            .position(|candidate| std::ptr::eq(candidate, source))
            .and_then(|position| query.join_conditions.get(position))
            .and_then(Option::as_ref)
            .map_or(1.0, estimates::selectivity)
    }

    fn step_estimate(
        &self,
        query: &ExplainedQuery,
        all: &[(usize, Step<'_>)],
        at: usize,
        children: &[Estimate],
    ) -> Estimate {
        let first = children.first().copied().unwrap_or_default();
        let Some((_, step)) = all.get(at) else {
            return Estimate::default();
        };
        match step {
            Step::Result => Estimate {
                cost: 0.0,
                rows: 1.0,
            },
            Step::Append => children
                .iter()
                .fold(Estimate::default(), |sum, child| Estimate {
                    cost: sum.cost + child.cost,
                    rows: sum.rows + child.rows,
                }),
            Step::Sort => Estimate {
                cost: first.cost + estimates::sort_cost(first.rows),
                rows: query.row_limit.map_or(first.rows, |limit| {
                    first
                        .rows
                        .min(limit.saturating_add(query.row_offset) as f64)
                }),
            },
            Step::Limit => {
                let left = (first.rows - query.row_offset as f64).max(0.0);
                Estimate {
                    cost: first.cost,
                    rows: query.row_limit.map_or(left, |limit| left.min(limit as f64)),
                }
            }
            Step::Hash => first,
            Step::NestedLoop | Step::HashJoin => {
                let inner = children.get(1).copied().unwrap_or_default();
                let rows = first.rows * inner.rows * Self::join_fraction(query, all, at);
                let cost = if matches!(step, Step::HashJoin) {
                    first.cost + inner.cost + ROW_COST * (first.rows + inner.rows)
                } else {
                    first.cost + first.rows * inner.cost
                };
                Estimate { cost, rows }
            }
            Step::Scan(source) => Estimate::read(estimates::scanned_rows(source, self.table_rows)),
            Step::FullScan(source) => Estimate::read(self.table_rows.of(&source.table)),
            Step::KeyLookup(source, key) => {
                Estimate::read(estimates::lookup_rows(source, *key, self.table_rows))
            }
        }
    }

    fn filter_base(&self, query: &ExplainedQuery, below: Estimate) -> f64 {
        match query.sources.as_slice() {
            [only] if only.join.is_none() && !query.appended => self.table_rows.of(&only.table),
            _ => below.rows,
        }
    }

    fn wrapper_estimates(
        &self,
        query: &ExplainedQuery,
        wrappers: &[Wrapper<'_>],
        source: Estimate,
    ) -> Vec<Estimate> {
        let mut found = vec![Estimate::default(); wrappers.len()];
        let mut below = source;
        for (at, wrapper) in wrappers.iter().enumerate().rev() {
            let estimate = match wrapper {
                Wrapper::Where(filter) => Estimate {
                    cost: below.cost,
                    rows: (self.filter_base(query, below) * estimates::selectivity(filter))
                        .min(below.rows),
                },
                Wrapper::Aggregate(grouping) => estimates::aggregated(below, grouping.keyed),
                Wrapper::Having(having) => Estimate {
                    cost: below.cost,
                    rows: below.rows * estimates::selectivity(having),
                },
                Wrapper::Stream => below,
            };
            if let Some(slot) = found.get_mut(at) {
                *slot = estimate;
            }
            below = estimate;
        }
        found
    }

    fn step_estimates(
        &self,
        query: &ExplainedQuery,
        all: &[(usize, Step<'_>)],
        attach: usize,
        wrappers: &[Wrapper<'_>],
    ) -> (Vec<Estimate>, Vec<Estimate>) {
        let mut own = vec![Estimate::default(); all.len()];
        let mut handed_up = vec![Estimate::default(); all.len()];
        let mut wrapped = Vec::new();
        for at in (0..all.len()).rev() {
            let children: Vec<Estimate> = child_positions(all, at)
                .into_iter()
                .filter_map(|child| handed_up.get(child).copied())
                .collect();
            let estimate = self.step_estimate(query, all, at, &children);
            let top = if at == attach {
                wrapped = self.wrapper_estimates(query, wrappers, estimate);
                wrapped.first().copied().unwrap_or(estimate)
            } else {
                estimate
            };
            if let Some(slot) = own.get_mut(at) {
                *slot = estimate;
            }
            if let Some(slot) = handed_up.get_mut(at) {
                *slot = top;
            }
        }
        (own, wrapped)
    }

    fn aggregate_text(&self, query: &ExplainedQuery, grouping: &ExplainedGrouping) -> String {
        let mut calls: Vec<String> = Vec::new();
        for call in &grouping.aggregates {
            let text = match &call.kind {
                ExprKind::Aggregate {
                    function,
                    argument: None,
                    ..
                } => format!("{}(0)", function.name()),
                _ => self.text(query, call),
            };
            if !calls.contains(&text) {
                calls.push(text);
            }
        }
        match (grouping.keyed, calls.is_empty()) {
            (true, true) => "Group (no aggregates)".to_owned(),
            (true, false) => format!("Group aggregate: {}", calls.join(", ")),
            (false, true) => "Aggregate".to_owned(),
            (false, false) => format!("Aggregate: {}", calls.join(", ")),
        }
    }

    fn wrapper_text(&self, query: &ExplainedQuery, wrapper: &Wrapper<'_>) -> String {
        match wrapper {
            Wrapper::Stream => "Stream results".to_owned(),
            Wrapper::Having(condition) | Wrapper::Where(condition) => {
                format!("Filter: {}", self.text(query, condition))
            }
            Wrapper::Aggregate(grouping) => self.aggregate_text(query, grouping),
        }
    }

    fn subplan_label(&self, subplan: &Subplan) -> String {
        let number = self
            .numbers
            .iter()
            .find(|(position, _)| *position == subplan.position)
            .map_or(2, |(_, number)| *number);
        let place = match subplan.place {
            SubplanPlace::Projection => "projection",
            SubplanPlace::Condition => "condition",
        };
        let runs = if subplan.correlated {
            "dependent"
        } else {
            "run only once"
        };
        format!("Select #{number} (subquery in {place}; {runs})")
    }

    fn subplan_measured(&self, subplan: &Subplan) -> Option<Measured> {
        let measured = self.measured_subplans?;
        Some(super::subplan_measured(measured, subplan.position))
    }

    fn add_subplans(&mut self, query: &ExplainedQuery, place: SubplanPlace, level: usize) {
        for subplan in query
            .subplans
            .iter()
            .filter(|subplan| subplan.place == place)
        {
            let label = self.subplan_label(subplan);
            self.lines.push(TreeLine {
                level,
                text: label,
                estimate: None,
                measured: None,
            });
            let measured = self.subplan_measured(subplan);
            self.add_query(&subplan.plan, level + 1, None, measured);
        }
    }

    fn add_query(
        &mut self,
        query: &ExplainedQuery,
        level: usize,
        measures: Option<&QueryMeasures>,
        top: Option<Measured>,
    ) {
        let all = steps(query);
        let attach = usize::from(query.limited) + usize::from(query.sorted);
        let filter = unconsumed_filter(query);
        let wrappers = wrappers(query, filter);
        let (own_estimates, wrapped) = self.step_estimates(query, &all, attach, &wrappers);
        let step_measured =
            |at: usize| measures.and_then(|measures| measures.steps.get(at).copied().flatten());
        let grouped = measures.and_then(|measures| measures.grouped);
        let where_offset = wrappers
            .iter()
            .position(|wrapper| matches!(wrapper, Wrapper::Where(_)));
        let first = self.lines.len();
        for (at, (step_level, step)) in all.iter().enumerate() {
            let mut line_level = level + step_level;
            if at == attach {
                for (offset, wrapper) in wrappers.iter().enumerate() {
                    let measured = match wrapper {
                        Wrapper::Stream if query.grouping.is_some() => grouped,
                        Wrapper::Stream | Wrapper::Where(_) => step_measured(at),
                        Wrapper::Aggregate(grouping) if grouping.having.is_some() => None,
                        Wrapper::Aggregate(_) | Wrapper::Having(_) => grouped,
                    };
                    let text = self.wrapper_text(query, wrapper);
                    self.lines.push(TreeLine {
                        level: line_level + offset,
                        text,
                        estimate: wrapped.get(offset).copied(),
                        measured,
                    });
                }
            }
            if at >= attach {
                line_level += wrappers.len();
            }
            let text = self.step_text(query, &all, at, step);
            let filtered_here = at == attach && filter.is_some();
            let estimate = if matches!(step, Step::Hash) {
                None
            } else {
                own_estimates.get(at).copied()
            };
            self.lines.push(TreeLine {
                level: line_level,
                text,
                estimate,
                measured: if filtered_here {
                    None
                } else {
                    step_measured(at)
                },
            });
            if at == all.len().saturating_sub(1) {
                let condition_level = level + attach + where_offset.unwrap_or(wrappers.len()) + 1;
                self.add_subplans(query, SubplanPlace::Condition, condition_level);
            }
        }
        if let Some(line) = self.lines.get_mut(first)
            && top.is_some()
        {
            line.measured = top;
        }
        self.add_subplans(query, SubplanPlace::Projection, level + 1);
    }
}

fn actual_text(measured: Measured) -> String {
    match measured {
        Measured::NeverExecuted => "(never executed)".to_owned(),
        Measured::Ran(times) => format!(
            "(actual time={:.3}..{:.3} rows={} loops=1)",
            milliseconds(times.first_row.min(times.last_row)),
            milliseconds(times.last_row),
            times.rows
        ),
        Measured::Looped { times, loops } => format!(
            "(actual time={:.3}..{:.3} rows={} loops={loops})",
            milliseconds(times.first_row.min(times.last_row)),
            milliseconds(times.last_row),
            times.rows
        ),
    }
}

/// MySQL 8's iterator tree for `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE`: one `-> ` line per
/// iterator, four spaces per level, each with EggDB's `(cost=... rows=...)` estimate and, when
/// analyzed, the measured `(actual time=... rows=... loops=...)`. A multi-table `UPDATE` or
/// `DELETE` puts its `Update`/`Delete from` iterator over the join it reads; a single-table one
/// is not run by MySQL's iterator executor, and says so.
pub(crate) fn tree_text(planned: &Planned, analyzed: Option<&Analyzed>) -> String {
    let query = &planned.query;
    let mut tree = Tree {
        numbers: select_numbers(query),
        measured_subplans: analyzed.map(|analyzed| analyzed.subplans.as_slice()),
        table_rows: &planned.table_rows,
        lines: Vec::new(),
    };
    match &planned.write {
        Some(write) if !write.joined => tree.lines.push(TreeLine {
            level: 0,
            text: NOT_EXECUTABLE.to_owned(),
            estimate: None,
            measured: None,
        }),
        Some(write) => {
            let measured = analyzed.map(|analyzed| {
                Measured::Ran(RowTimes {
                    first_row: analyzed.execution,
                    last_row: analyzed.execution,
                    rows: 0,
                })
            });
            tree.lines.push(TreeLine {
                level: 0,
                text: write_text(query, write),
                estimate: None,
                measured,
            });
            tree.add_query(query, 1, None, None);
        }
        None => {
            let measures = analyzed.map(|analyzed| tree_measures(query, analyzed));
            tree.add_query(query, 0, measures.as_ref(), None);
        }
    }
    let mut text = String::new();
    for line in &tree.lines {
        text.push_str(&"    ".repeat(line.level));
        text.push_str("-> ");
        text.push_str(&line.text);
        if let Some(estimate) = line.estimate {
            text.push_str("  ");
            text.push_str(&estimate.group());
        }
        if let Some(measured) = line.measured {
            text.push_str(if line.estimate.is_some() { " " } else { "  " });
            text.push_str(&actual_text(measured));
        }
        text.push('\n');
    }
    text
}

fn parenthesized(text: String) -> String {
    if text.starts_with('(') && text.ends_with(')') {
        text
    } else {
        format!("({text})")
    }
}
