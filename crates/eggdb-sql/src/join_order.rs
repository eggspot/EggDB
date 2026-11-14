use crate::ast::{
    BinaryOperator, Expr, ExprKind, FromClause, Join, JoinCondition, JoinKind, Literal, Select,
    SelectItem, TableReference,
};
use crate::column_statistics::{ColumnStatistics, EveryColumnStatistics};
use crate::error::SqlResult;
use crate::store::KeyValueRead;
use std::collections::HashMap;

const MOST_REORDERED: usize = 12;
const UNKNOWN_SELECTIVITY: f64 = 0.33;
const UNKNOWN_EQUALITY: f64 = 0.1;

struct Relation {
    reference: TableReference,
    label: String,
    rows: f64,
}

fn label_of(reference: &TableReference) -> String {
    reference
        .alias
        .clone()
        .unwrap_or_else(|| crate::schemas::base_name(&reference.name.text).to_owned())
}

fn reorderable(from: &FromClause) -> bool {
    let plain = |reference: &TableReference| {
        reference.derived.is_none()
            && reference.generated.is_none()
            && reference.partition_selection.is_none()
            && reference.system_time.is_none()
    };
    !from.joins.is_empty()
        && from.joins.len() < MOST_REORDERED
        && plain(&from.first)
        && from.joins.iter().all(|join| {
            matches!(join.kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Comma)
                && matches!(join.condition, JoinCondition::Always | JoinCondition::On(_))
                && plain(&join.table)
        })
}

fn conjuncts(expr: Expr, into: &mut Vec<Expr>) {
    match expr.kind {
        ExprKind::And(parts) => {
            for part in parts {
                conjuncts(part, into);
            }
        }
        _ => into.push(expr),
    }
}

fn qualifiers(expr: &Expr, into: &mut Vec<Option<String>>) {
    match &expr.kind {
        ExprKind::Column { qualifier, .. } => into.push(qualifier.clone()),
        ExprKind::Binary(_, left, right) => {
            qualifiers(left, into);
            qualifiers(right, into);
        }
        ExprKind::And(parts) | ExprKind::Or(parts) => {
            for part in parts {
                qualifiers(part, into);
            }
        }
        ExprKind::IsNull { expr, .. } | ExprKind::Cast { expr, .. } => qualifiers(expr, into),
        ExprKind::InList { expr, list, .. } => {
            qualifiers(expr, into);
            for item in list {
                qualifiers(item, into);
            }
        }
        ExprKind::Literal(_) | ExprKind::Constant { .. } => {}
        _ => into.push(None),
    }
}

fn only_on(expr: &Expr, label: &str) -> bool {
    let mut found = Vec::new();
    qualifiers(expr, &mut found);
    !found.is_empty()
        && found
            .iter()
            .all(|qualifier| qualifier.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case(label)))
}

fn column_of<'e>(expr: &'e Expr) -> Option<(&'e str, &'e str)> {
    match &expr.kind {
        ExprKind::Column {
            qualifier: Some(qualifier),
            name,
        } => Some((qualifier, name)),
        ExprKind::Cast { expr, .. } => column_of(expr),
        _ => None,
    }
}

fn literal_text(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(number)) => Some(number.to_string()),
        ExprKind::Literal(Literal::Decimal(text) | Literal::String(text)) => Some(text.clone()),
        ExprKind::Literal(Literal::Boolean(flag)) => Some(if *flag { "t" } else { "f" }.to_owned()),
        ExprKind::Constant { value, .. } => Some(value.to_text()),
        ExprKind::Cast { expr, .. } => literal_text(expr),
        _ => None,
    }
}

fn distinct_values(column: &ColumnStatistics, rows: f64) -> f64 {
    let distinct = f64::from(column.distinct);
    if distinct < 0.0 {
        (-distinct * rows).max(1.0)
    } else {
        distinct.max(1.0)
    }
}

/// The share of rows whose column equals `value`: its frequency when it is a most common value,
/// else the rest of the rows spread over the other distinct values.
pub(crate) fn equality_selectivity(column: &ColumnStatistics, rows: f64, value: &str) -> f64 {
    if let Some(at) = column.most_common_values.iter().position(|common| common == value) {
        return f64::from(column.most_common_frequencies.get(at).copied().unwrap_or(0.0));
    }
    let common: f64 = column.most_common_frequencies.iter().map(|share| f64::from(*share)).sum();
    let others = (distinct_values(column, rows) - column.most_common_values.len() as f64).max(1.0);
    ((1.0 - common - f64::from(column.null_fraction)).max(0.0) / others).clamp(0.0, 1.0)
}

fn bound_number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok()
}

/// The share of rows whose column is below `value` (`below`) or above it, read from the
/// histogram's equal-population bounds (D1136); `None` when there is no histogram.
pub(crate) fn range_selectivity(column: &ColumnStatistics, value: &str, below: bool) -> Option<f64> {
    let bounds = &column.histogram_bounds;
    if bounds.len() < 2 {
        return None;
    }
    let buckets = (bounds.len() - 1) as f64;
    let numeric = bound_number(value);
    let position = |bound: &String| match (numeric, bound_number(bound)) {
        (Some(wanted), Some(at)) => at.partial_cmp(&wanted),
        _ => Some(bound.as_str().cmp(value)),
    };
    let mut below_share = 1.0;
    for (index, pair) in bounds.windows(2).enumerate() {
        let upper_above = position(&pair[1]).is_some_and(|ordering| ordering.is_ge());
        if upper_above {
            let within = match (numeric, bound_number(&pair[0]), bound_number(&pair[1])) {
                (Some(wanted), Some(low), Some(high)) if high > low => ((wanted - low) / (high - low)).clamp(0.0, 1.0),
                _ => 0.5,
            };
            below_share = (index as f64 + within) / buckets;
            break;
        }
    }
    if position(&bounds[0]).is_some_and(|ordering| ordering.is_ge()) {
        below_share = 0.0;
    }
    let present = 1.0 - f64::from(column.null_fraction);
    let share = if below { below_share } else { 1.0 - below_share };
    Some((share * present).clamp(0.0, 1.0))
}

fn selectivity(condition: &Expr, statistics: Option<&Vec<ColumnStatistics>>, rows: f64) -> f64 {
    let column_named = |name: &str| statistics.and_then(|columns| columns.iter().find(|column| column.column == name));
    match &condition.kind {
        ExprKind::Binary(operator, left, right) => {
            let (column, value, operator) = match (column_of(left), literal_text(right)) {
                (Some((_, name)), Some(value)) => (name, value, *operator),
                _ => match (column_of(right), literal_text(left)) {
                    (Some((_, name)), Some(value)) => (
                        name,
                        value,
                        match operator {
                            BinaryOperator::Less => BinaryOperator::Greater,
                            BinaryOperator::LessOrEqual => BinaryOperator::GreaterOrEqual,
                            BinaryOperator::Greater => BinaryOperator::Less,
                            BinaryOperator::GreaterOrEqual => BinaryOperator::LessOrEqual,
                            other => *other,
                        },
                    ),
                    _ => return UNKNOWN_SELECTIVITY,
                },
            };
            let Some(stats) = column_named(column) else {
                return if operator == BinaryOperator::Equal { UNKNOWN_EQUALITY } else { UNKNOWN_SELECTIVITY };
            };
            match operator {
                BinaryOperator::Equal => equality_selectivity(stats, rows, &value),
                BinaryOperator::NotEqual => 1.0 - equality_selectivity(stats, rows, &value),
                BinaryOperator::Less | BinaryOperator::LessOrEqual => {
                    range_selectivity(stats, &value, true).unwrap_or(UNKNOWN_SELECTIVITY)
                }
                BinaryOperator::Greater | BinaryOperator::GreaterOrEqual => {
                    range_selectivity(stats, &value, false).unwrap_or(UNKNOWN_SELECTIVITY)
                }
                _ => UNKNOWN_SELECTIVITY,
            }
        }
        ExprKind::IsNull { expr, negated } => match column_of(expr).and_then(|(_, name)| column_named(name)) {
            Some(stats) if *negated => 1.0 - f64::from(stats.null_fraction),
            Some(stats) => f64::from(stats.null_fraction),
            None => UNKNOWN_SELECTIVITY,
        },
        ExprKind::InList { expr, list, negated: false } => {
            match column_of(expr).and_then(|(_, name)| column_named(name)) {
                Some(stats) => list
                    .iter()
                    .filter_map(literal_text)
                    .map(|value| equality_selectivity(stats, rows, &value))
                    .sum::<f64>()
                    .min(1.0),
                None => (UNKNOWN_EQUALITY * list.len() as f64).min(1.0),
            }
        }
        _ => UNKNOWN_SELECTIVITY,
    }
}

fn joins(condition: &Expr, first: &str, second: &str) -> bool {
    let ExprKind::Binary(BinaryOperator::Equal, left, right) = &condition.kind else {
        return false;
    };
    match (column_of(left), column_of(right)) {
        (Some((a, _)), Some((b, _))) => {
            (a.eq_ignore_ascii_case(first) && b.eq_ignore_ascii_case(second))
                || (a.eq_ignore_ascii_case(second) && b.eq_ignore_ascii_case(first))
        }
        _ => false,
    }
}

fn greedy_order(relations: &[Relation], conditions: &[Expr]) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::with_capacity(relations.len());
    let mut left: Vec<usize> = (0..relations.len()).collect();
    while !left.is_empty() {
        let connected: Vec<usize> = left
            .iter()
            .copied()
            .filter(|&candidate| {
                chosen.iter().any(|&earlier| {
                    conditions
                        .iter()
                        .any(|condition| joins(condition, &relations[earlier].label, &relations[candidate].label))
                })
            })
            .collect();
        let pool = if chosen.is_empty() || connected.is_empty() { &left } else { &connected };
        let Some(&next) = pool.iter().min_by(|a, b| {
            relations[**a]
                .rows
                .partial_cmp(&relations[**b].rows)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(b))
        }) else {
            break;
        };
        chosen.push(next);
        left.retain(|&index| index != next);
    }
    chosen
}

/// Orders the tables of a `SELECT` whose `FROM` holds only inner and cross joins of plain tables
/// by estimated size (D1136): each table's rows from `ANALYZE` times the selectivity of its own
/// filters, from most common values and histograms; then, starting from the smallest, the
/// smallest table joined by an equality to one already placed. The `ON` conditions move to
/// `WHERE`, where the join step still finds its key equalities. A query whose output order
/// depends on the `FROM` order (`*`), or with a table never analyzed, is left as written.
pub(crate) fn order_inner_joins(reader: &dyn KeyValueRead, select: &mut Select) -> SqlResult<bool> {
    let Some(from) = &select.from else {
        return Ok(false);
    };
    if !reorderable(from)
        || select
            .items
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard { qualifier: None, .. }))
    {
        return Ok(false);
    }
    let counts: HashMap<String, u64> = crate::statistics::every_row_count(reader)?;
    let references: Vec<&TableReference> = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|join| &join.table))
        .collect();
    if references.iter().any(|reference| !counts.contains_key(&reference.name.text)) {
        return Ok(false);
    }
    let columns: EveryColumnStatistics = crate::statistics::every_column_statistics(reader)?;
    let Some(from) = select.from.take() else {
        return Ok(false);
    };
    let mut conditions = Vec::new();
    if let Some(filter) = select.filter.take() {
        conjuncts(filter, &mut conditions);
    }
    let FromClause { first, joins: written } = from;
    let mut relations = vec![first];
    for Join { table, condition, .. } in written {
        if let JoinCondition::On(on) = condition {
            conjuncts(on, &mut conditions);
        }
        relations.push(table);
    }
    let relations: Vec<Relation> = relations
        .into_iter()
        .map(|reference| {
            let label = label_of(&reference);
            let total = counts.get(&reference.name.text).copied().unwrap_or(0) as f64;
            let statistics = columns.get(&reference.name.text);
            let share: f64 = conditions
                .iter()
                .filter(|condition| only_on(condition, &label))
                .map(|condition| selectivity(condition, statistics, total))
                .product();
            Relation {
                reference,
                label,
                rows: (total * share).max(1.0),
            }
        })
        .collect();
    let order = greedy_order(&relations, &conditions);
    let mut slots: Vec<Option<Relation>> = relations.into_iter().map(Some).collect();
    let mut ordered = order.iter().filter_map(|&index| slots[index].take().map(|relation| relation.reference));
    let Some(first) = ordered.next() else {
        return Ok(false);
    };
    let joins = ordered
        .map(|table| Join {
            kind: JoinKind::Cross,
            table,
            condition: JoinCondition::Always,
        })
        .collect();
    select.from = Some(FromClause { first, joins });
    select.filter = match conditions.len() {
        0 => None,
        1 => conditions.pop(),
        _ => Some(Expr {
            position: conditions[0].position,
            kind: ExprKind::And(conditions),
        }),
    };
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{equality_selectivity, range_selectivity};
    use crate::column_statistics::ColumnStatistics;

    fn statistics() -> ColumnStatistics {
        ColumnStatistics {
            column: "amount".into(),
            null_fraction: 0.0,
            average_width: 4,
            distinct: 100.0,
            most_common_values: vec!["7".into()],
            most_common_frequencies: vec![0.5],
            histogram_bounds: vec!["0".into(), "10".into(), "20".into(), "30".into(), "40".into()],
            correlation: None,
        }
    }

    #[test]
    fn common_values_use_their_frequency_and_others_share_the_rest() {
        let column = statistics();
        assert!((equality_selectivity(&column, 1000.0, "7") - 0.5).abs() < 1e-9);
        assert!((equality_selectivity(&column, 1000.0, "8") - 0.5 / 99.0).abs() < 1e-9);
    }

    #[test]
    fn ranges_read_the_histogram_buckets() {
        let column = statistics();
        assert!((range_selectivity(&column, "20", true).unwrap() - 0.5).abs() < 1e-9);
        assert!((range_selectivity(&column, "35", false).unwrap() - 0.125).abs() < 1e-9);
        assert_eq!(range_selectivity(&column, "-5", true), Some(0.0));
    }
}
