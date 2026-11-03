use crate::ast::{BinaryOperator, Expr, ExprKind, Hierarchy, HierarchyOutput};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::memory::WorkMemory;
use crate::session::{OutputColumn, QueryResult};
use crate::temp_tables::HIDDEN_COLUMN_MARKER;
use crate::types::{SqlType, Value, compare_in_directions, compare_values};
use std::cmp::Ordering;

pub(crate) const PARENT_SLOTS: usize = usize::MAX / 4;
pub(crate) const LEVEL_SLOT: usize = usize::MAX / 2;
const ROWS_BETWEEN_CANCEL_CHECKS: usize = 4096;

pub(crate) fn level_column() -> String {
    format!("{HIDDEN_COLUMN_MARKER}level")
}

pub(crate) fn leaf_column() -> String {
    format!("{HIDDEN_COLUMN_MARKER}connect_by_isleaf")
}

pub(crate) fn cycle_column() -> String {
    format!("{HIDDEN_COLUMN_MARKER}connect_by_iscycle")
}

pub(crate) fn output_column(number: usize) -> String {
    format!("{HIDDEN_COLUMN_MARKER}hierarchy {number}")
}

fn loop_in_user_data(position: usize) -> SqlError {
    SqlError::new(SqlState::DATA_EXCEPTION, "CONNECT BY loop in user data")
        .with_oracle_code(1436)
        .at(position)
}

fn separator_in_value(position: usize) -> SqlError {
    SqlError::new(
        SqlState::DATA_EXCEPTION,
        "when using SYS_CONNECT_BY_PATH function, cannot have separator as part of column value",
    )
    .with_oracle_code(30004)
    .at(position)
}

fn level_value(level: usize) -> Value {
    Value::BigInt(i64::try_from(level).unwrap_or(i64::MAX))
}

fn placed(expr: &Expr, inputs: usize, types: &[Option<SqlType>]) -> SqlResult<Expr> {
    if let ExprKind::Slot { index, .. } = expr.kind {
        let (index, sql_type) = if index == LEVEL_SLOT {
            (inputs.saturating_mul(2), Some(SqlType::BigInt))
        } else if index >= PARENT_SLOTS {
            let input = index - PARENT_SLOTS;
            (
                inputs.saturating_add(input),
                types.get(input).copied().flatten(),
            )
        } else {
            (index, types.get(index).copied().flatten())
        };
        return Ok(Expr {
            kind: ExprKind::Slot { index, sql_type },
            position: expr.position,
        });
    }
    crate::aggregate::rebuild(expr, &mut |child| placed(child, inputs, types))
}

fn paired_keys(parent_slot: usize, child_slot: usize) -> Option<(usize, usize)> {
    if child_slot < PARENT_SLOTS && (PARENT_SLOTS..LEVEL_SLOT).contains(&parent_slot) {
        Some((child_slot, parent_slot - PARENT_SLOTS))
    } else {
        None
    }
}

fn equal_keys(connect: &Expr) -> Option<(usize, usize)> {
    let conjuncts: Vec<&Expr> = match &connect.kind {
        ExprKind::And(operands) => operands.iter().collect(),
        _ => vec![connect],
    };
    conjuncts
        .into_iter()
        .find_map(|conjunct| match &conjunct.kind {
            ExprKind::Binary(BinaryOperator::Equal, left, right) => match (&left.kind, &right.kind)
            {
                (ExprKind::Slot { index: a, .. }, ExprKind::Slot { index: b, .. }) => {
                    paired_keys(*a, *b).or_else(|| paired_keys(*b, *a))
                }
                _ => None,
            },
            _ => None,
        })
}

fn mentions_level(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Slot { index, .. } if index == LEVEL_SLOT)
        || crate::eval::children(expr).into_iter().any(mentions_level)
}

fn same_key(left: &Value, right: &Value) -> bool {
    match (left.is_null(), right.is_null()) {
        (true, true) => true,
        (false, false) => compare_values(left, right) == Ordering::Equal,
        _ => false,
    }
}

struct KeyedRows {
    child: usize,
    parent: usize,
    order: Vec<usize>,
}

impl KeyedRows {
    fn new(rows: &[Vec<Value>], child: usize, parent: usize) -> Self {
        let key = |row: usize| rows.get(row).and_then(|values| values.get(child));
        let mut order: Vec<usize> = (0..rows.len())
            .filter(|&row| key(row).is_some_and(|value| !value.is_null()))
            .collect();
        order.sort_by(|&a, &b| match (key(a), key(b)) {
            (Some(left), Some(right)) => compare_values(left, right).then(a.cmp(&b)),
            _ => a.cmp(&b),
        });
        Self {
            child,
            parent,
            order,
        }
    }

    fn candidates(&self, rows: &[Vec<Value>], parent_row: usize) -> Vec<usize> {
        let Some(wanted) = rows
            .get(parent_row)
            .and_then(|values| values.get(self.parent))
            .filter(|value| !value.is_null())
        else {
            return Vec::new();
        };
        let key = |row: usize| {
            rows.get(row)
                .and_then(|values| values.get(self.child))
                .map_or(Ordering::Less, |value| compare_values(value, wanted))
        };
        let first = self
            .order
            .partition_point(|&row| key(row) == Ordering::Less);
        self.order
            .get(first..)
            .unwrap_or(&[])
            .iter()
            .copied()
            .take_while(|&row| key(row) == Ordering::Equal)
            .collect()
    }
}

struct Step {
    row: usize,
    paths: Vec<String>,
}

struct Walk<'w> {
    hierarchy: &'w Hierarchy,
    rows: &'w [Vec<Value>],
    types: Vec<Option<SqlType>>,
    connect: Expr,
    level_dependent: bool,
    keyed: Option<KeyedRows>,
    known_children: Vec<Option<Vec<usize>>>,
    local: Vec<Value>,
    memory: WorkMemory,
}

impl Walk<'_> {
    fn cell(&self, row: usize, input: usize) -> Value {
        self.rows
            .get(row)
            .and_then(|values| values.get(input))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn inputs_of(&self, row: usize) -> &[Value] {
        self.rows
            .get(row)
            .and_then(|values| values.get(..self.hierarchy.inputs))
            .unwrap_or(&[])
    }

    fn starts(&self, row: usize) -> bool {
        match self.hierarchy.start {
            None => true,
            Some(input) => matches!(self.cell(row, input), Value::Boolean(true)),
        }
    }

    fn connects(&mut self, child: usize, parent: usize, level: usize) -> SqlResult<bool> {
        let mut local = std::mem::take(&mut self.local);
        local.clear();
        local.extend_from_slice(self.inputs_of(child));
        local.extend_from_slice(self.inputs_of(parent));
        local.push(level_value(level));
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &local,
            correlation: None,
        };
        let connected = matches!(evaluator.eval(&self.connect, None)?, Value::Boolean(true));
        self.local = local;
        Ok(connected)
    }

    fn in_sibling_order(&self, rows: &mut Vec<usize>) {
        if self.hierarchy.siblings.is_empty() || rows.len() < 2 {
            return;
        }
        let directions: Vec<_> = self
            .hierarchy
            .siblings
            .iter()
            .map(|&(_, direction)| direction)
            .collect();
        let mut keyed: Vec<(Vec<Value>, usize)> = rows
            .iter()
            .map(|&row| {
                let keys = self
                    .hierarchy
                    .siblings
                    .iter()
                    .map(|&(input, _)| self.cell(row, input))
                    .collect();
                (keys, row)
            })
            .collect();
        keyed.sort_by(|left, right| compare_in_directions(&directions, &left.0, &right.0));
        *rows = keyed.into_iter().map(|(_, row)| row).collect();
    }

    fn children(&mut self, parent: usize, level: usize) -> SqlResult<Vec<usize>> {
        if !self.level_dependent
            && let Some(Some(known)) = self.known_children.get(parent)
        {
            return Ok(known.clone());
        }
        crate::cancel::check()?;
        let candidates: Vec<usize> = match &self.keyed {
            Some(keyed) => keyed.candidates(self.rows, parent),
            None => (0..self.rows.len()).collect(),
        };
        let mut found = Vec::new();
        for (count, child) in candidates.into_iter().enumerate() {
            if count % ROWS_BETWEEN_CANCEL_CHECKS == ROWS_BETWEEN_CANCEL_CHECKS - 1 {
                crate::cancel::check()?;
            }
            if self.connects(child, parent, level)? {
                found.push(child);
            }
        }
        self.in_sibling_order(&mut found);
        if !self.level_dependent {
            self.memory
                .charge(found.len().saturating_mul(std::mem::size_of::<usize>()))?;
            if let Some(slot) = self.known_children.get_mut(parent) {
                *slot = Some(found.clone());
            }
        }
        Ok(found)
    }

    fn closes_cycle(&self, child: usize, path: &[Step]) -> bool {
        let keys = &self.hierarchy.prior_inputs;
        !keys.is_empty()
            && path.iter().any(|step| {
                keys.iter()
                    .all(|&input| same_key(&self.cell(child, input), &self.cell(step.row, input)))
            })
    }

    fn step(&self, row: usize, parent: Option<&Step>) -> SqlResult<Step> {
        let mut paths = Vec::with_capacity(self.hierarchy.outputs.len());
        for (number, output) in self.hierarchy.outputs.iter().enumerate() {
            let HierarchyOutput::Path { input, separator } = output else {
                paths.push(String::new());
                continue;
            };
            let value = self.cell(row, *input);
            let declared = self.types.get(*input).copied().flatten();
            let text =
                crate::oracle_functions::text_with_type(&value, declared).unwrap_or_default();
            if text.contains(separator.as_str()) {
                return Err(separator_in_value(self.hierarchy.position));
            }
            let before = parent
                .and_then(|step| step.paths.get(number))
                .map_or("", String::as_str);
            paths.push(format!("{before}{separator}{text}"));
        }
        Ok(Step { row, paths })
    }

    fn emitted(&self, path: &[Step], level: usize, leaf: bool, cycle: bool) -> Vec<Value> {
        let Some(current) = path.last() else {
            return Vec::new();
        };
        let mut values: Vec<Value> = self
            .rows
            .get(current.row)
            .and_then(|row| row.get(self.hierarchy.inputs..))
            .unwrap_or(&[])
            .to_vec();
        values.push(level_value(level));
        values.push(Value::BigInt(i64::from(leaf)));
        values.push(Value::BigInt(i64::from(cycle)));
        for (number, output) in self.hierarchy.outputs.iter().enumerate() {
            values.push(match output {
                HierarchyOutput::Path { .. } => current
                    .paths
                    .get(number)
                    .map_or(Value::Null, |text| Value::Text(text.clone())),
                HierarchyOutput::Root(input) => path
                    .first()
                    .map_or(Value::Null, |root| self.cell(root.row, *input)),
                HierarchyOutput::Prior(input) => path
                    .len()
                    .checked_sub(2)
                    .and_then(|at| path.get(at))
                    .map_or(Value::Null, |parent| self.cell(parent.row, *input)),
            });
        }
        values
    }

    fn run(mut self) -> SqlResult<Vec<Vec<Value>>> {
        let mut roots: Vec<usize> = (0..self.rows.len())
            .filter(|&row| self.starts(row))
            .collect();
        self.in_sibling_order(&mut roots);
        let mut pending: Vec<(usize, usize)> =
            roots.into_iter().rev().map(|row| (row, 1)).collect();
        let mut path: Vec<Step> = Vec::new();
        let mut emitted = Vec::new();
        while let Some((row, level)) = pending.pop() {
            path.truncate(level.saturating_sub(1));
            let step = self.step(row, path.last())?;
            path.push(step);
            let children = self.children(row, level.saturating_add(1))?;
            let mut cycle = false;
            let mut kept = Vec::with_capacity(children.len());
            for &child in &children {
                if self.closes_cycle(child, &path) {
                    if !self.hierarchy.no_cycle {
                        return Err(loop_in_user_data(self.hierarchy.position));
                    }
                    cycle = true;
                } else {
                    kept.push(child);
                }
            }
            let values = self.emitted(&path, level, children.is_empty(), cycle);
            self.memory.charge_row(&values)?;
            emitted.push(values);
            pending.extend(
                kept.into_iter()
                    .rev()
                    .map(|child| (child, level.saturating_add(1))),
            );
        }
        Ok(emitted)
    }
}

/// Walks the rows of a hierarchical query's source (its `START WITH` and `CONNECT BY` inputs
/// first, then the source's own columns) depth first, as Oracle returns them, and gives each
/// row its `LEVEL`, `CONNECT_BY_ISLEAF`, `CONNECT_BY_ISCYCLE` and the `SYS_CONNECT_BY_PATH`,
/// `CONNECT_BY_ROOT` and `PRIOR` values the query reads, as hidden columns.
pub(crate) fn expanded(hierarchy: &Hierarchy, inner: QueryResult) -> SqlResult<QueryResult> {
    let inner_columns = inner.columns.unwrap_or_default();
    let inputs = hierarchy.inputs.min(inner_columns.len());
    let types: Vec<Option<SqlType>> = inner_columns
        .get(..inputs)
        .unwrap_or(&[])
        .iter()
        .map(|column| column.sql_type)
        .collect();
    let mut columns: Vec<OutputColumn> = inner_columns.get(inputs..).unwrap_or(&[]).to_vec();
    for name in [level_column(), leaf_column(), cycle_column()] {
        columns.push(OutputColumn {
            name,
            sql_type: Some(SqlType::BigInt),
        });
    }
    for (number, output) in hierarchy.outputs.iter().enumerate() {
        let sql_type = match output {
            HierarchyOutput::Path { .. } => Some(SqlType::Text),
            HierarchyOutput::Root(input) | HierarchyOutput::Prior(input) => {
                types.get(*input).copied().flatten()
            }
        };
        columns.push(OutputColumn {
            name: output_column(number),
            sql_type,
        });
    }
    let connect = placed(&hierarchy.connect, inputs, &types)?;
    let keyed = equal_keys(&hierarchy.connect)
        .filter(|&(child, parent)| {
            let child_type = types.get(child).copied().flatten();
            child_type.is_some() && child_type == types.get(parent).copied().flatten()
        })
        .map(|(child, parent)| KeyedRows::new(&inner.rows, child, parent));
    let walk = Walk {
        hierarchy,
        rows: &inner.rows,
        types,
        level_dependent: mentions_level(&hierarchy.connect),
        connect,
        keyed,
        known_children: vec![None; inner.rows.len()],
        local: Vec::with_capacity(inputs.saturating_mul(2).saturating_add(1)),
        memory: WorkMemory::new(),
    };
    let rows = walk.run()?;
    Ok(QueryResult::rows(
        format!("SELECT {}", rows.len()),
        columns,
        rows,
    ))
}

#[cfg(test)]
mod tests {
    use super::{KeyedRows, LEVEL_SLOT, PARENT_SLOTS, equal_keys, mentions_level};
    use crate::ast::{BinaryOperator, Expr, ExprKind};
    use crate::types::Value;

    fn slot(index: usize) -> Expr {
        Expr {
            kind: ExprKind::Slot {
                index,
                sql_type: None,
            },
            position: 0,
        }
    }

    fn equal(left: Expr, right: Expr) -> Expr {
        Expr {
            kind: ExprKind::Binary(BinaryOperator::Equal, Box::new(left), Box::new(right)),
            position: 0,
        }
    }

    #[test]
    fn an_equality_between_a_prior_input_and_a_child_input_keys_the_children() {
        assert_eq!(
            equal_keys(&equal(slot(PARENT_SLOTS), slot(1))),
            Some((1, 0))
        );
        assert_eq!(
            equal_keys(&equal(slot(1), slot(PARENT_SLOTS + 2))),
            Some((1, 2))
        );
        assert_eq!(equal_keys(&equal(slot(0), slot(1))), None);
        assert_eq!(equal_keys(&equal(slot(LEVEL_SLOT), slot(1))), None);
    }

    #[test]
    fn a_condition_that_reads_level_is_level_dependent() {
        assert!(mentions_level(&equal(slot(LEVEL_SLOT), slot(0))));
        assert!(!mentions_level(&equal(slot(PARENT_SLOTS), slot(0))));
    }

    #[test]
    fn keyed_rows_find_every_child_with_the_parent_key_in_row_order() {
        let rows = vec![
            vec![Value::BigInt(1), Value::Null],
            vec![Value::BigInt(2), Value::BigInt(1)],
            vec![Value::BigInt(3), Value::BigInt(1)],
            vec![Value::BigInt(4), Value::BigInt(2)],
        ];
        let keyed = KeyedRows::new(&rows, 1, 0);
        assert_eq!(keyed.candidates(&rows, 0), vec![1, 2]);
        assert_eq!(keyed.candidates(&rows, 1), vec![3]);
        assert!(keyed.candidates(&rows, 3).is_empty());
    }
}
