use super::expression_text::{ExpressionText, Spelling};
use super::subplans::{Subplan, SubplanPlace};
use super::{Analyzed, Measured, Step, key_name, measured_steps, milliseconds, steps};
use crate::ast::{BinaryOperator, Expr, ExprKind};
use crate::dialect::Dialect;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::plan::{KeyChoice, Use};
use crate::step_times::MeasuredSubplan;

struct TreeLine {
    level: usize,
    text: String,
    measured: Option<Measured>,
}

struct Tree<'q> {
    numbers: Vec<(usize, usize)>,
    measured_subplans: Option<&'q [MeasuredSubplan]>,
    lines: Vec<TreeLine>,
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

fn names_column(expr: &Expr, label: &str, column: &str) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Column { qualifier, name }
            if name.eq_ignore_ascii_case(column)
                && qualifier.as_deref().is_none_or(|qualifier| qualifier.eq_ignore_ascii_case(label))
    )
}

fn sql_value_of<'e>(conditions: &[&'e Expr], label: &str, column: &str) -> Option<&'e Expr> {
    conditions.iter().find_map(|&condition| match &condition.kind {
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

    fn lookup(&self, query: &ExplainedQuery, source: &ExplainedSource, columns: &[usize]) -> String {
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

    fn join_source<'q>(
        all: &'q [(usize, Step<'q>)],
        at: usize,
    ) -> Option<&'q ExplainedSource> {
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
                        format!("Inner hash join ({})", self.text(query, condition))
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

    fn filter_needed(query: &ExplainedQuery) -> Option<&Expr> {
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
        for subplan in query.subplans.iter().filter(|subplan| subplan.place == place) {
            let label = self.subplan_label(subplan);
            self.lines.push(TreeLine {
                level,
                text: label,
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
        measured: Option<&[Option<Measured>]>,
        top: Option<Measured>,
    ) {
        let all = steps(query);
        let attach = usize::from(query.limited) + usize::from(query.sorted);
        let filter = Self::filter_needed(query);
        let first = self.lines.len();
        for (at, (step_level, step)) in all.iter().enumerate() {
            let mut line_level = level + step_level;
            if at == attach
                && let Some(filter) = filter
            {
                let text = format!("Filter: {}", self.text(query, filter));
                self.lines.push(TreeLine {
                    level: line_level,
                    text,
                    measured: measured.and_then(|steps| steps.get(at).copied().flatten()),
                });
            }
            if at >= attach && filter.is_some() {
                line_level += 1;
            }
            let text = self.step_text(query, &all, at, step);
            self.lines.push(TreeLine {
                level: line_level,
                text,
                measured: measured.and_then(|steps| steps.get(at).copied().flatten()),
            });
            if at == all.len().saturating_sub(1) {
                self.add_subplans(query, SubplanPlace::Condition, level + attach + 1);
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
        Measured::NeverExecuted => "  (never executed)".to_owned(),
        Measured::Ran(times) => format!(
            "  (actual time={:.3}..{:.3} rows={} loops=1)",
            milliseconds(times.first_row.min(times.last_row)),
            milliseconds(times.last_row),
            times.rows
        ),
        Measured::Looped { times, loops } => format!(
            "  (actual time={:.3}..{:.3} rows={} loops={loops})",
            milliseconds(times.first_row.min(times.last_row)),
            milliseconds(times.last_row),
            times.rows
        ),
    }
}

/// The plan in MySQL's `FORMAT=TREE` form, one `-> ` line per iterator indented four spaces per
/// level, with `(actual time=a..b rows=n loops=n)` on the iterators EggDB measured when the
/// statement was analyzed.
pub(crate) fn tree_text(query: &ExplainedQuery, analyzed: Option<&Analyzed>) -> String {
    let measured = analyzed.map(|analyzed| measured_steps(query, analyzed));
    let mut tree = Tree {
        numbers: select_numbers(query),
        measured_subplans: analyzed.map(|analyzed| analyzed.subplans.as_slice()),
        lines: Vec::new(),
    };
    tree.add_query(query, 0, measured.as_deref(), None);
    let mut text = String::new();
    for line in &tree.lines {
        text.push_str(&"    ".repeat(line.level));
        text.push_str("-> ");
        text.push_str(&line.text);
        if let Some(measured) = line.measured {
            text.push_str(&actual_text(measured));
        }
        text.push('\n');
    }
    text
}
