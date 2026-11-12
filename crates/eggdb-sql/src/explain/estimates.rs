use crate::ast::{BinaryOperator, Expr, ExprKind, UnaryOperator};
use crate::encoding::{TableDefinition, prefix_end, rows_prefix};
use crate::error::SqlResult;
use crate::execute::{ExplainedQuery, ExplainedSource};
use crate::plan::{KeyChoice, Use};
use crate::store::{KeyValueRead, ScanDirection};
use std::collections::HashMap;

pub(crate) const SEEK_COST: f64 = 0.25;
pub(crate) const ROW_COST: f64 = 0.1;
pub(crate) const EQUAL_SELECTIVITY: f64 = 0.1;
pub(crate) const RANGE_SELECTIVITY: f64 = 1.0 / 3.0;
pub(crate) const MOST_COUNTED_ROWS: u64 = 10_000;
const MOST_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Estimate {
    pub(crate) cost: f64,
    pub(crate) rows: f64,
}

impl Estimate {
    pub(crate) fn read(rows: f64) -> Self {
        Self {
            cost: SEEK_COST + ROW_COST * rows,
            rows,
        }
    }

    pub(crate) fn group(self) -> String {
        format!(
            "(cost={} rows={})",
            readable(self.cost),
            readable(self.rows)
        )
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TableRows {
    counts: HashMap<u32, f64>,
    equal_selectivity: HashMap<(u32, usize), f64>,
}

fn equal_selectivity_of(distinct: f32, rows: f64) -> Option<f64> {
    let values = if distinct > 0.0 {
        f64::from(distinct)
    } else if distinct < 0.0 {
        -f64::from(distinct) * rows
    } else {
        return None;
    };
    (values >= 1.0).then(|| (1.0 / values).clamp(0.0, 1.0))
}

fn collect_tables<'q>(query: &'q ExplainedQuery, out: &mut Vec<&'q TableDefinition>) {
    out.extend(query.sources.iter().map(|source| source.table.as_ref()));
    for subplan in &query.subplans {
        collect_tables(&subplan.plan, out);
    }
}

fn counted_rows(reader: &dyn KeyValueRead, table: &TableDefinition) -> SqlResult<u64> {
    let prefix = rows_prefix(table.id);
    let mut rows = 0u64;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, _| {
            rows = rows.saturating_add(1);
            Ok(rows < MOST_COUNTED_ROWS)
        },
    )?;
    Ok(rows)
}

impl TableRows {
    /// The row count of every table the plan reads: the one `ANALYZE` recorded, or else the
    /// table's rows counted up to [`MOST_COUNTED_ROWS`].
    pub(crate) fn read(reader: &dyn KeyValueRead, query: &ExplainedQuery) -> SqlResult<Self> {
        let mut tables = Vec::new();
        collect_tables(query, &mut tables);
        if tables.is_empty() {
            return Ok(Self::default());
        }
        let analyzed = crate::statistics::every_row_count(reader)?;
        let column_statistics = crate::statistics::every_column_statistics(reader)?;
        let mut counts = HashMap::with_capacity(tables.len());
        let mut equal_selectivity = HashMap::new();
        for table in tables {
            if counts.contains_key(&table.id) {
                continue;
            }
            let rows = match analyzed.get(&table.name) {
                Some(&rows) => rows,
                None => counted_rows(reader, table)?,
            };
            counts.insert(table.id, rows as f64);
            if let Some(columns) = column_statistics.get(&table.name) {
                for (at, column) in table.columns.iter().enumerate() {
                    if let Some(found) = columns.iter().find(|found| found.column == column.name)
                        && let Some(selectivity) = equal_selectivity_of(found.distinct, rows as f64)
                    {
                        equal_selectivity.insert((table.id, at), selectivity);
                    }
                }
            }
        }
        Ok(Self {
            counts,
            equal_selectivity,
        })
    }

    pub(crate) fn of(&self, table: &TableDefinition) -> f64 {
        self.counts.get(&table.id).copied().unwrap_or(0.0)
    }

    fn condition_selectivity(&self, table: &TableDefinition, column: usize, operator: &str) -> f64 {
        if operator == "=" {
            self.equal_selectivity
                .get(&(table.id, column))
                .copied()
                .unwrap_or(EQUAL_SELECTIVITY)
        } else {
            RANGE_SELECTIVITY
        }
    }
}

pub(crate) fn scanned_rows(source: &ExplainedSource, table_rows: &TableRows) -> f64 {
    let table = table_rows.of(&source.table);
    match source.access.used {
        Use::Scan | Use::Order => table,
        Use::Point => table.min(1.0),
        Use::Equal | Use::Range => {
            let fraction: f64 = source
                .access
                .conditions
                .iter()
                .map(|(column, operator)| {
                    table_rows.condition_selectivity(&source.table, *column, operator)
                })
                .product();
            (table * fraction).min(table)
        }
    }
}

pub(crate) fn lookup_rows(source: &ExplainedSource, key: KeyChoice, table_rows: &TableRows) -> f64 {
    let table = table_rows.of(&source.table);
    match key {
        KeyChoice::Primary => table.min(1.0),
        KeyChoice::Index(_) => table * EQUAL_SELECTIVITY,
    }
}

pub(crate) fn sort_cost(rows: f64) -> f64 {
    ROW_COST * rows * rows.max(2.0).log2()
}

pub(crate) fn aggregated(input: Estimate, grouped: bool) -> Estimate {
    let rows = if grouped {
        (input.rows * EQUAL_SELECTIVITY).max(input.rows.min(1.0))
    } else {
        1.0
    };
    Estimate {
        cost: input.cost + ROW_COST * input.rows,
        rows,
    }
}

pub(crate) fn selectivity(expr: &Expr) -> f64 {
    selectivity_at(expr, 0).clamp(0.0, 1.0)
}

fn selectivity_at(expr: &Expr, depth: usize) -> f64 {
    if depth > MOST_DEPTH {
        return RANGE_SELECTIVITY;
    }
    let depth = depth + 1;
    let fraction = match &expr.kind {
        ExprKind::And(parts) => parts
            .iter()
            .map(|part| selectivity_at(part, depth))
            .product(),
        ExprKind::Or(parts) => {
            1.0 - parts
                .iter()
                .map(|part| 1.0 - selectivity_at(part, depth))
                .product::<f64>()
        }
        ExprKind::Unary(UnaryOperator::Not, operand) => 1.0 - selectivity_at(operand, depth),
        ExprKind::Binary(BinaryOperator::Equal, _, _) | ExprKind::IsNull { negated: false, .. } => {
            EQUAL_SELECTIVITY
        }
        ExprKind::Binary(BinaryOperator::NotEqual, _, _)
        | ExprKind::IsNull { negated: true, .. } => 1.0 - EQUAL_SELECTIVITY,
        ExprKind::InList { list, negated, .. } => {
            let found = (EQUAL_SELECTIVITY * list.len() as f64).min(0.5);
            if *negated { 1.0 - found } else { found }
        }
        ExprKind::Between { negated, .. } => {
            let found = RANGE_SELECTIVITY * RANGE_SELECTIVITY;
            if *negated { 1.0 - found } else { found }
        }
        _ => RANGE_SELECTIVITY,
    };
    fraction.clamp(0.0, 1.0)
}

fn rounded_to_significant(value: f64, digits: i32) -> (f64, usize) {
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = (digits - 1 - magnitude).clamp(0, 12);
    let factor = 10f64.powi(decimals);
    ((value * factor).round() / factor, decimals as usize)
}

fn trimmed(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A cost or row estimate the way MySQL's tree prints one: three significant digits below 100,
/// whole numbers up to 100,000, and engineering notation (`99.9e+3`, `1.01e+6`) above.
pub(crate) fn readable(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0".to_owned();
    }
    if value < 100.0 {
        let (rounded, decimals) = rounded_to_significant(value, 3);
        return trimmed(format!("{rounded:.decimals$}"));
    }
    if value < 100_000.0 {
        return format!("{:.0}", value.round());
    }
    let mut exponent = (value.log10().floor() as i32 / 3) * 3;
    let (mut mantissa, mut decimals) = rounded_to_significant(value / 10f64.powi(exponent), 3);
    if mantissa >= 1000.0 {
        exponent += 3;
        (mantissa, decimals) = rounded_to_significant(mantissa / 1000.0, 3);
    }
    format!("{}e+{exponent}", trimmed(format!("{mantissa:.decimals$}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Literal;

    fn expr(kind: ExprKind) -> Expr {
        Expr { kind, position: 0 }
    }

    fn column(name: &str) -> Box<Expr> {
        Box::new(expr(ExprKind::Column {
            qualifier: None,
            name: name.to_owned(),
        }))
    }

    fn one() -> Box<Expr> {
        Box::new(expr(ExprKind::Literal(Literal::Integer(1))))
    }

    #[test]
    fn estimates_print_with_three_significant_digits_and_engineering_notation_when_large() {
        assert_eq!(readable(0.55), "0.55");
        assert_eq!(readable(0.35000000000000003), "0.35");
        assert_eq!(readable(1.0), "1");
        assert_eq!(readable(1.666_666), "1.67");
        assert_eq!(readable(3.5), "3.5");
        assert_eq!(readable(0.012_63), "0.0126");
        assert_eq!(readable(33.333), "33.3");
        assert_eq!(readable(1006.4), "1006");
        assert_eq!(readable(99_510.0), "99510");
        assert_eq!(readable(997_632.0), "998e+3");
        assert_eq!(readable(999_700.0), "1e+6");
        assert_eq!(readable(1_010_000.0), "1.01e+6");
        assert_eq!(readable(0.0), "0");
        assert_eq!(readable(f64::NAN), "0");
    }

    #[test]
    fn equalities_keep_a_tenth_ranges_a_third_and_conjunctions_multiply() {
        let equal = expr(ExprKind::Binary(BinaryOperator::Equal, column("a"), one()));
        let greater = expr(ExprKind::Binary(
            BinaryOperator::Greater,
            column("b"),
            one(),
        ));
        assert!((selectivity(&equal) - 0.1).abs() < 1e-12);
        assert!((selectivity(&greater) - 1.0 / 3.0).abs() < 1e-12);
        let both = expr(ExprKind::And(vec![equal.clone(), greater.clone()]));
        assert!((selectivity(&both) - 0.1 / 3.0).abs() < 1e-12);
        let either = expr(ExprKind::Or(vec![equal, greater]));
        assert!((selectivity(&either) - (1.0 - 0.9 * (2.0 / 3.0))).abs() < 1e-12);
    }

    #[test]
    fn a_deeply_nested_condition_stops_at_the_depth_limit() {
        let mut condition = expr(ExprKind::Binary(BinaryOperator::Equal, column("a"), one()));
        for _ in 0..200 {
            condition = expr(ExprKind::Unary(UnaryOperator::Not, Box::new(condition)));
        }
        let fraction = selectivity(&condition);
        assert!((0.0..=1.0).contains(&fraction));
    }

    #[test]
    fn an_aggregate_without_groups_gives_one_row_and_groups_a_tenth_of_their_input() {
        let input = Estimate {
            cost: 1.0,
            rows: 50.0,
        };
        assert_eq!(aggregated(input, false).rows, 1.0);
        assert_eq!(aggregated(input, true).rows, 5.0);
        assert!((aggregated(input, true).cost - 6.0).abs() < 1e-12);
        let tiny = Estimate {
            cost: 0.0,
            rows: 3.0,
        };
        assert_eq!(aggregated(tiny, true).rows, 1.0);
    }
}
