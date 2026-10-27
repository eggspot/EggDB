use crate::aggregate::bind;
use crate::ast::{BinaryOperator, Expr, ExprKind, FromClause, JoinCondition, JoinKind};
use crate::encoding::{TableDefinition, decode_row, encode_key_value, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{
    Evaluator, Scope, Source, Sources, check_columns, compare_truth, reject_aggregates,
};
use crate::memory::WorkMemory;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::{Conversion, SqlType, Value, comparable, compare_values, convert, equality_key};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct JoinedFrom<'a> {
    pub(crate) sources: Sources<'a>,
    shapes: Vec<StepShape<'a>>,
}

struct StepShape<'a> {
    kind: JoinKind,
    source: Source<'a>,
    on: Option<&'a Expr>,
    using: Vec<(usize, usize)>,
    visible: Sources<'a>,
}

fn sources_between<'a>(
    list: &[Source<'a>],
    visible: std::ops::Range<usize>,
    merged: &[usize],
) -> Sources<'a> {
    Sources {
        list: list[visible.clone()].to_vec(),
        merged: merged.to_vec(),
        out_of_reach: list[..visible.start]
            .iter()
            .map(|source| source.label)
            .collect(),
        star: Vec::new(),
    }
}

fn columns_of(source: &Source<'_>) -> impl Iterator<Item = usize> {
    source.offset..source.offset + source.table.columns.len()
}

fn type_family(sql_type: SqlType) -> u8 {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => 0,
        SqlType::Real | SqlType::Double => 1,
        SqlType::Text | SqlType::Varchar(_) => 2,
        SqlType::Boolean => 3,
    }
}

fn no_equality(left: SqlType, right: SqlType, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} = {}",
            left.name(),
            right.name()
        ),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
    .at(position)
}

pub(crate) fn resolve<'a>(
    from: &'a FromClause,
    tables: &'a [Arc<TableDefinition>],
) -> SqlResult<JoinedFrom<'a>> {
    let references: Vec<_> = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|join| &join.table))
        .collect();
    let mut list: Vec<Source<'a>> = Vec::with_capacity(references.len());
    let mut offset = 0;
    for (reference, table) in references.iter().zip(tables) {
        let label = reference.alias.as_deref().unwrap_or(&table.name);
        if list.iter().any(|source| source.label == label) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_ALIAS,
                format!("table name \"{label}\" specified more than once"),
            )
            .at(reference.name.position));
        }
        list.push(Source {
            table,
            label,
            offset,
        });
        offset += table.columns.len();
    }
    let mut merged = Vec::new();
    let mut star = Vec::new();
    let mut group_start = 0;
    let mut group_star: Vec<usize> = columns_of(&list[0]).collect();
    let mut shapes = Vec::with_capacity(from.joins.len());
    for (index, join) in from.joins.iter().enumerate() {
        let right_index = index + 1;
        let right = list[right_index];
        if join.kind == JoinKind::Comma {
            star.append(&mut group_star);
            group_start = right_index;
            group_star = columns_of(&right).collect();
            shapes.push(StepShape {
                kind: join.kind,
                source: right,
                on: None,
                using: Vec::new(),
                visible: sources_between(&list, right_index..right_index + 1, &merged),
            });
            continue;
        }
        let visible = sources_between(&list, group_start..right_index + 1, &merged);
        let mut using = Vec::new();
        let mut on = None;
        match &join.condition {
            JoinCondition::On(expr) => {
                reject_aggregates(expr, "JOIN conditions")?;
                check_columns(expr, Scope::Many(&visible))?;
                on = Some(expr);
            }
            JoinCondition::Using(names) => {
                let left = sources_between(&list, group_start..right_index, &merged);
                for (at, name) in names.iter().enumerate() {
                    if names[..at].iter().any(|earlier| earlier.text == name.text) {
                        return Err(SqlError::new(
                            SqlState::DUPLICATE_COLUMN,
                            format!(
                                "column name \"{}\" appears more than once in USING clause",
                                name.text
                            ),
                        )
                        .at(name.position));
                    }
                    let left_offset = Scope::Many(&left)
                        .column(None, &name.text, name.position)
                        .map_err(|error| {
                            if error.state() == SqlState::AMBIGUOUS_COLUMN {
                                SqlError::new(
                                    SqlState::AMBIGUOUS_COLUMN,
                                    format!(
                                        "common column name \"{}\" appears more than once in left table",
                                        name.text
                                    ),
                                )
                                .at(name.position)
                            } else {
                                SqlError::new(
                                    SqlState::UNDEFINED_COLUMN,
                                    format!(
                                        "column \"{}\" specified in USING clause does not exist in left table",
                                        name.text
                                    ),
                                )
                                .at(name.position)
                            }
                        })?;
                    let right_column = right.table.column_index(&name.text).ok_or_else(|| {
                        SqlError::new(
                            SqlState::UNDEFINED_COLUMN,
                            format!(
                                "column \"{}\" specified in USING clause does not exist in right table",
                                name.text
                            ),
                        )
                        .at(name.position)
                    })?;
                    let left_type = Scope::Many(&left).column_type(left_offset);
                    let right_type = right.table.columns[right_column].sql_type;
                    if let Some(left_type) = left_type
                        && !comparable(left_type, right_type)
                    {
                        return Err(no_equality(left_type, right_type, name.position));
                    }
                    using.push((left_offset, right.offset + right_column));
                }
            }
            JoinCondition::Always => {}
        }
        if using.is_empty() {
            group_star.extend(columns_of(&right));
        } else {
            let lefts: Vec<usize> = using.iter().map(|&(left, _)| left).collect();
            let rights: Vec<usize> = using.iter().map(|&(_, right)| right).collect();
            let mut reordered = lefts.clone();
            reordered.extend(group_star.iter().filter(|offset| !lefts.contains(offset)));
            reordered.extend(columns_of(&right).filter(|offset| !rights.contains(offset)));
            group_star = reordered;
            merged.extend(rights);
        }
        shapes.push(StepShape {
            kind: join.kind,
            source: right,
            on,
            using,
            visible,
        });
    }
    star.append(&mut group_star);
    Ok(JoinedFrom {
        sources: Sources {
            list,
            merged,
            out_of_reach: Vec::new(),
            star,
        },
        shapes,
    })
}

enum Access {
    Lookup(Vec<usize>),
    Hash {
        probe: Vec<usize>,
        buckets: HashMap<Vec<u8>, Vec<usize>>,
        rows: Vec<Vec<Value>>,
    },
    Scan(Vec<Vec<Value>>),
}

enum Right<'r> {
    Owned(Vec<Value>),
    Borrowed(&'r [Value]),
}

struct Step<'a> {
    key: RefCell<Vec<u8>>,
    prefix_length: usize,
    preserves_left: bool,
    table: &'a TableDefinition,
    on: Option<Expr>,
    using: &'a [(usize, usize)],
    scope: Scope<'a>,
    access: Access,
}

pub(crate) struct JoinedRows<'a> {
    reader: &'a dyn KeyValueRead,
    first: &'a TableDefinition,
    first_key: Option<Vec<u8>>,
    steps: Vec<Step<'a>>,
}

fn equality_pairs(shape: &StepShape<'_>) -> Vec<(usize, usize)> {
    let scope = Scope::Many(&shape.visible);
    let same_family =
        |outer: usize, inner: usize| match (scope.column_type(outer), scope.column_type(inner)) {
            (Some(x), Some(y)) => type_family(x) == type_family(y),
            _ => false,
        };
    let mut pairs: Vec<(usize, usize)> = shape
        .using
        .iter()
        .copied()
        .filter(|&(outer, inner)| same_family(outer, inner))
        .collect();
    let Some(on) = shape.on else {
        return pairs;
    };
    let right = shape.source.offset..shape.source.offset + shape.source.table.columns.len();
    let conjuncts: Vec<&Expr> = match &on.kind {
        ExprKind::And(operands) => operands.iter().collect(),
        _ => vec![on],
    };
    for conjunct in conjuncts {
        let ExprKind::Binary(BinaryOperator::Equal, left_expr, right_expr) = &conjunct.kind else {
            continue;
        };
        let offset = |expr: &Expr| match &expr.kind {
            ExprKind::Column { qualifier, name } => {
                scope.column(qualifier.as_deref(), name, expr.position).ok()
            }
            _ => None,
        };
        let (Some(a), Some(b)) = (offset(left_expr), offset(right_expr)) else {
            continue;
        };
        let (outer, inner) = match (right.contains(&a), right.contains(&b)) {
            (false, true) => (a, b),
            (true, false) => (b, a),
            _ => continue,
        };
        if same_family(outer, inner) {
            pairs.push((outer, inner));
        }
    }
    pairs
}

fn scan_table(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>,
) -> SqlResult<()> {
    let prefix = rows_prefix(table.id);
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, bytes| visit(decode_row(bytes, &table.columns)?),
    )
}

fn key_of(row: &[Value], offsets: &[usize], key: &mut Vec<u8>) -> bool {
    key.clear();
    for &offset in offsets {
        let value = &row[offset];
        if value.is_null() {
            return false;
        }
        equality_key(key, value);
    }
    true
}

impl<'a> JoinedRows<'a> {
    pub(crate) fn new(
        reader: &'a dyn KeyValueRead,
        joined: &'a JoinedFrom<'a>,
        first_key: Option<Vec<u8>>,
        memory: &mut WorkMemory,
    ) -> SqlResult<Self> {
        let mut steps = Vec::with_capacity(joined.shapes.len());
        for shape in &joined.shapes {
            let table = shape.source.table;
            let offset = shape.source.offset;
            let pairs = equality_pairs(shape);
            let lookup: Option<Vec<usize>> = if table.primary_key.is_empty() {
                None
            } else {
                table
                    .primary_key
                    .iter()
                    .map(|&index| {
                        pairs
                            .iter()
                            .find(|&&(_, inner)| inner == offset + index)
                            .map(|&(outer, _)| outer)
                    })
                    .collect()
            };
            let access = if let Some(lefts) = lookup {
                Access::Lookup(lefts)
            } else if !pairs.is_empty() {
                let probe: Vec<usize> = pairs.iter().map(|&(outer, _)| outer).collect();
                let build: Vec<usize> = pairs.iter().map(|&(_, inner)| inner - offset).collect();
                let mut buckets: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
                let mut rows = Vec::new();
                let mut key = Vec::new();
                scan_table(reader, table, &mut |row| {
                    if key_of(&row, &build, &mut key) {
                        memory.charge_row(&row)?;
                        memory.charge(key.len())?;
                        buckets.entry(key.clone()).or_default().push(rows.len());
                        rows.push(row);
                    }
                    Ok(true)
                })?;
                Access::Hash {
                    probe,
                    buckets,
                    rows,
                }
            } else {
                let mut rows = Vec::new();
                scan_table(reader, table, &mut |row| {
                    memory.charge_row(&row)?;
                    rows.push(row);
                    Ok(true)
                })?;
                Access::Scan(rows)
            };
            let prefix = rows_prefix(table.id);
            steps.push(Step {
                prefix_length: prefix.len(),
                key: RefCell::new(prefix),
                preserves_left: shape.kind == JoinKind::Left,
                table,
                on: shape
                    .on
                    .map(|on| bind(on, Scope::Many(&shape.visible)))
                    .transpose()?,
                using: &shape.using,
                scope: Scope::Many(&shape.visible),
                access,
            });
        }
        Ok(Self {
            reader,
            first: joined.sources.list[0].table,
            first_key,
            steps,
        })
    }

    pub(crate) fn for_each(
        &self,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        if let Some(key) = &self.first_key {
            let mut found = None;
            self.reader.get_with(key, &mut |bytes| {
                found = Some(decode_row(bytes, &self.first.columns)?);
                Ok(())
            })?;
            if let Some(mut row) = found {
                self.extend(0, &mut row, visit)?;
            }
            return Ok(());
        }
        scan_table(self.reader, self.first, &mut |mut row| {
            self.extend(0, &mut row, visit)
        })
    }

    fn extend(
        &self,
        level: usize,
        row: &mut Vec<Value>,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<bool> {
        let Some(step) = self.steps.get(level) else {
            return visit(row);
        };
        let base = row.len();
        let mut matched = false;
        let mut attempt = |row: &mut Vec<Value>, right: Right<'_>| -> SqlResult<bool> {
            match right {
                Right::Owned(values) => row.extend(values),
                Right::Borrowed(values) => row.extend_from_slice(values),
            }
            let keep_going = if step.matches(row)? {
                matched = true;
                self.extend(level + 1, row, visit)?
            } else {
                true
            };
            row.truncate(base);
            Ok(keep_going)
        };
        match &step.access {
            Access::Lookup(lefts) => {
                if let Some(right) = self.lookup(step, lefts, row)?
                    && !attempt(row, Right::Owned(right))?
                {
                    return Ok(false);
                }
            }
            Access::Hash {
                probe,
                buckets,
                rows,
            } => {
                let mut key = Vec::new();
                if key_of(row, probe, &mut key)
                    && let Some(candidates) = buckets.get(&key)
                {
                    for &candidate in candidates {
                        if !attempt(row, Right::Borrowed(&rows[candidate]))? {
                            return Ok(false);
                        }
                    }
                }
            }
            Access::Scan(rows) => {
                for right in rows {
                    if !attempt(row, Right::Borrowed(right))? {
                        return Ok(false);
                    }
                }
            }
        }
        if !matched && step.preserves_left {
            row.resize(base + step.table.columns.len(), Value::Null);
            let keep_going = self.extend(level + 1, row, visit)?;
            row.truncate(base);
            return Ok(keep_going);
        }
        Ok(true)
    }

    fn lookup(
        &self,
        step: &Step<'_>,
        lefts: &[usize],
        row: &[Value],
    ) -> SqlResult<Option<Vec<Value>>> {
        let table = step.table;
        let mut key = step.key.borrow_mut();
        key.truncate(step.prefix_length);
        for (&index, &left) in table.primary_key.iter().zip(lefts) {
            let original = &row[left];
            if original.is_null() {
                return Ok(None);
            }
            let Ok(converted) = convert(
                original.clone(),
                table.columns[index].sql_type,
                Conversion::Assignment,
            ) else {
                return Ok(None);
            };
            if compare_values(&converted, original) != Ordering::Equal {
                return Ok(None);
            }
            encode_key_value(&mut key, &converted);
        }
        let key = key.as_slice();
        let mut found = None;
        self.reader.get_with(key, &mut |bytes| {
            found = Some(decode_row(bytes, &table.columns)?);
            Ok(())
        })?;
        Ok(found)
    }
}

impl Step<'_> {
    fn matches(&self, row: &[Value]) -> SqlResult<bool> {
        for &(left, right) in self.using {
            if compare_truth(&row[left], &row[right], "=")? != Some(true) {
                return Ok(false);
            }
        }
        let Some(on) = &self.on else {
            return Ok(true);
        };
        let evaluator = Evaluator {
            scope: self.scope,
            row,
        };
        Ok(evaluator.boolean(on, "JOIN/ON")? == Some(true))
    }
}
