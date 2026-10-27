use crate::ast::{Expr, ExprKind, Literal, OrderItem, SetOperation, SetOperator};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::compare_truth;
use crate::execute::{order_rows, row_count};
use crate::memory::WorkMemory;
use crate::session::{OutputColumn, QueryResult};
use crate::types::{Conversion, SqlType, Value, convert, equality_key};
use std::collections::{HashMap, HashSet};

fn numeric_rank(sql_type: SqlType) -> Option<u8> {
    Some(match sql_type {
        SqlType::SmallInt => 0,
        SqlType::Integer => 1,
        SqlType::BigInt => 2,
        SqlType::Numeric(_) => 3,
        SqlType::Real => 4,
        SqlType::Double => 5,
        _ => return None,
    })
}

fn datetime_rank(sql_type: SqlType) -> Option<u8> {
    Some(match sql_type {
        SqlType::Date => 0,
        SqlType::Timestamp(_) => 1,
        SqlType::TimestampTz(_) => 2,
        _ => return None,
    })
}

pub(crate) fn common_type(left: SqlType, right: SqlType) -> Option<SqlType> {
    if left == right {
        return Some(left);
    }
    if left.is_string() && right.is_string() {
        return Some(SqlType::Text);
    }
    let widest = |rank: fn(SqlType) -> Option<u8>| -> Option<SqlType> {
        let (a, b) = (rank(left)?, rank(right)?);
        Some(match a.cmp(&b) {
            std::cmp::Ordering::Less => right.without_modifier(),
            std::cmp::Ordering::Greater => left.without_modifier(),
            std::cmp::Ordering::Equal => left.without_modifier(),
        })
    };
    widest(numeric_rank).or_else(|| widest(datetime_rank))
}

fn mismatch(operator: SetOperator, left: SqlType, right: SqlType, position: usize) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "{} types {} and {} cannot be matched",
            operator.name(),
            left.name(),
            right.name()
        ),
    )
    .at(position)
}

pub(crate) fn column_count_mismatch(
    operator: SetOperator,
    dialect: Dialect,
    position: usize,
) -> SqlError {
    let message = if dialect == Dialect::Sqlite {
        format!(
            "SELECTs to the left and right of {} do not have the same number of result columns",
            operator.name()
        )
    } else {
        format!(
            "each {} query must have the same number of columns",
            operator.name()
        )
    };
    SqlError::new(SqlState::SYNTAX_ERROR, message).at(position)
}

fn value_types(rows: &[Vec<Value>], index: usize) -> Option<Option<SqlType>> {
    let mut found: Option<SqlType> = None;
    for row in rows {
        let Some(sql_type) = row.get(index).and_then(Value::sql_type) else {
            continue;
        };
        found = Some(match found {
            None => sql_type,
            Some(known) => common_type(known, sql_type)?,
        });
    }
    Some(found)
}

fn column_type(
    declared: Option<SqlType>,
    unknown: bool,
    rows: &[Vec<Value>],
    index: usize,
) -> Option<Option<SqlType>> {
    if unknown {
        return Some(None);
    }
    match declared {
        Some(sql_type) => Some(Some(sql_type)),
        None => value_types(rows, index),
    }
}

pub(crate) struct Branch {
    pub(crate) result: QueryResult,
    pub(crate) unknown: Vec<bool>,
}

fn stored(value: Value, target: SqlType) -> SqlResult<Value> {
    if value.is_null()
        || value.sql_type() == Some(target)
        || (target.is_string() && matches!(value, Value::Text(_)))
    {
        return Ok(value);
    }
    convert(value, target, Conversion::Explicit)
}

pub(crate) fn conform(row: Vec<Value>, columns: &[OutputColumn]) -> SqlResult<Vec<Value>> {
    row.into_iter()
        .zip(columns)
        .map(|(value, column)| match column.sql_type {
            Some(target) => stored(value, target),
            None => Ok(value),
        })
        .collect()
}

fn unify(
    operation: &SetOperation,
    left: &Branch,
    right: &Branch,
) -> SqlResult<(Vec<OutputColumn>, Vec<bool>)> {
    let empty = Vec::new();
    let left_columns = left.result.columns.as_ref().unwrap_or(&empty);
    let right_columns = right.result.columns.as_ref().unwrap_or(&empty);
    if left_columns.len() != right_columns.len() {
        return Err(column_count_mismatch(
            operation.operator,
            operation.dialect,
            operation.position,
        ));
    }
    let mut columns = Vec::with_capacity(left_columns.len());
    let mut texts = Vec::with_capacity(left_columns.len());
    for (index, (left_column, right_column)) in left_columns.iter().zip(right_columns).enumerate() {
        let literals_unknown = operation.dialect == Dialect::PostgreSql;
        let left_unknown = literals_unknown && left.unknown.get(index).copied().unwrap_or(false);
        let right_unknown = literals_unknown && right.unknown.get(index).copied().unwrap_or(false);
        let left_type = column_type(left_column.sql_type, left_unknown, &left.result.rows, index);
        let right_type = column_type(
            right_column.sql_type,
            right_unknown,
            &right.result.rows,
            index,
        );
        let mut as_text = false;
        let sql_type = if operation.dialect == Dialect::Sqlite {
            left_column.sql_type.or(right_column.sql_type)
        } else {
            match (left_type, right_type) {
                (Some(Some(a)), Some(Some(b))) => match common_type(a, b) {
                    Some(found) => Some(found),
                    None if operation.dialect == Dialect::MySql => {
                        as_text = true;
                        Some(SqlType::Text)
                    }
                    None => return Err(mismatch(operation.operator, a, b, operation.position)),
                },
                (Some(Some(a)), Some(None)) | (Some(None), Some(Some(a))) => Some(a),
                (Some(None), Some(None)) => {
                    if left_unknown && right_unknown {
                        Some(SqlType::Text)
                    } else {
                        None
                    }
                }
                _ if operation.dialect == Dialect::MySql => {
                    as_text = true;
                    Some(SqlType::Text)
                }
                _ => {
                    return Err(SqlError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("{} types cannot be matched", operation.operator.name()),
                    )
                    .at(operation.position));
                }
            }
        };
        columns.push(OutputColumn {
            name: left_column.name.clone(),
            sql_type,
        });
        texts.push(as_text);
    }
    Ok((columns, texts))
}

fn converted(
    rows: Vec<Vec<Value>>,
    columns: &[OutputColumn],
    texts: &[bool],
    dialect: Dialect,
) -> SqlResult<Vec<Vec<Value>>> {
    if dialect == Dialect::Sqlite {
        return Ok(rows);
    }
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .zip(columns.iter().zip(texts))
                .map(|(value, (column, &as_text))| {
                    if as_text {
                        return Ok(Dialect::MySql
                            .value_text(&value)
                            .map_or(Value::Null, Value::Text));
                    }
                    match column.sql_type {
                        Some(target) => stored(value, target),
                        None => Ok(value),
                    }
                })
                .collect()
        })
        .collect()
}

fn row_key(row: &[Value]) -> Vec<u8> {
    let mut key = Vec::new();
    for value in row {
        equality_key(&mut key, value);
    }
    key
}

pub(crate) fn combine(
    operation: &SetOperation,
    left: Branch,
    right: Branch,
) -> SqlResult<QueryResult> {
    let (columns, texts) = unify(operation, &left, &right)?;
    let mut memory = WorkMemory::new();
    for row in left.result.rows.iter().chain(&right.result.rows) {
        memory.charge_row(row)?;
    }
    let left_rows = converted(left.result.rows, &columns, &texts, operation.dialect)?;
    let right_rows = converted(right.result.rows, &columns, &texts, operation.dialect)?;
    let mut output = Vec::new();
    match (operation.operator, operation.all) {
        (SetOperator::Union, true) => {
            output = left_rows;
            output.extend(right_rows);
        }
        (SetOperator::Union, false) => {
            let mut seen = HashSet::new();
            for row in left_rows.into_iter().chain(right_rows) {
                let key = row_key(&row);
                if seen.contains(&key) {
                    continue;
                }
                memory.charge(key.len() + 32)?;
                seen.insert(key);
                output.push(row);
            }
        }
        (SetOperator::Intersect | SetOperator::Except, all) => {
            let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
            for row in &right_rows {
                let key = row_key(row);
                memory.charge(key.len() + 40)?;
                *counts.entry(key).or_insert(0) += 1;
            }
            drop(right_rows);
            let mut emitted = HashSet::new();
            let intersect = operation.operator == SetOperator::Intersect;
            for row in left_rows {
                let key = row_key(&row);
                let available = counts.get_mut(&key);
                let keep = match (intersect, all, available) {
                    (true, true, Some(count)) if *count > 0 => {
                        *count -= 1;
                        true
                    }
                    (true, false, Some(_)) => !emitted.contains(&key),
                    (true, _, _) => false,
                    (false, true, Some(count)) if *count > 0 => {
                        *count -= 1;
                        false
                    }
                    (false, true, _) => true,
                    (false, false, Some(_)) => false,
                    (false, false, None) => !emitted.contains(&key),
                };
                if keep {
                    if !all {
                        memory.charge(key.len() + 32)?;
                        emitted.insert(key);
                    }
                    output.push(row);
                }
            }
        }
    }
    Ok(QueryResult::rows(
        format!("SELECT {}", output.len()),
        columns,
        output,
    ))
}

pub(crate) fn unknown_columns(items: &[crate::ast::SelectItem]) -> Vec<bool> {
    items
        .iter()
        .map(|item| {
            matches!(
                item,
                crate::ast::SelectItem::Expr {
                    expr: Expr {
                        kind: ExprKind::Literal(Literal::String(_) | Literal::Null),
                        ..
                    },
                    ..
                }
            )
        })
        .collect()
}

fn invalid_order(position: usize) -> SqlError {
    SqlError::not_supported("invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
        .with_detail("Only result column names can be used, not expressions or functions.")
        .with_hint(
            "Add the expression/function to every SELECT, or move the UNION into a FROM clause.",
        )
        .at(position)
}

pub(crate) fn order_and_limit(
    mut result: QueryResult,
    order_by: &[OrderItem],
    limit: Option<&Expr>,
    offset: Option<&Expr>,
) -> SqlResult<QueryResult> {
    if order_by.is_empty() && limit.is_none() && offset.is_none() {
        return Ok(result);
    }
    let columns = result.columns.clone().unwrap_or_default();
    let mut keys = Vec::with_capacity(order_by.len());
    for item in order_by {
        let index = match &item.expr.kind {
            ExprKind::Literal(Literal::Integer(position)) => usize::try_from(*position)
                .ok()
                .filter(|&index| index >= 1 && index <= columns.len())
                .map(|index| index - 1)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!("ORDER BY position {position} is not in select list"),
                    )
                    .at(item.expr.position)
                })?,
            ExprKind::Column {
                qualifier: None,
                name,
            } => columns
                .iter()
                .position(|column| &column.name == name)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{name}\" does not exist"),
                    )
                    .at(item.expr.position)
                })?,
            _ => return Err(invalid_order(item.expr.position)),
        };
        keys.push(index);
    }
    if !keys.is_empty() {
        for (item, &index) in order_by.iter().zip(&keys) {
            let mut seen: Option<&Value> = None;
            for row in &result.rows {
                let Some(value) = row.get(index).filter(|value| !value.is_null()) else {
                    continue;
                };
                match seen {
                    Some(previous) => {
                        compare_truth(previous, value, "<")
                            .map_err(|error| error.at(item.expr.position))?;
                    }
                    None => seen = Some(value),
                }
            }
        }
        order_rows(&mut result.rows, order_by, &keys);
    }
    let skip = row_count(offset, "OFFSET")?.unwrap_or(0);
    let take = row_count(limit, "LIMIT")?;
    let skip = usize::try_from(skip).unwrap_or(usize::MAX);
    let take = take.map_or(usize::MAX, |take| {
        usize::try_from(take).unwrap_or(usize::MAX)
    });
    if skip > 0 || take < result.rows.len() {
        result.rows = std::mem::take(&mut result.rows)
            .into_iter()
            .skip(skip)
            .take(take)
            .collect();
    }
    result.tag = format!("SELECT {}", result.rows.len());
    Ok(result)
}
