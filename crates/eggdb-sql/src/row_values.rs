use crate::ast::{BinaryOperator, Expr, ExprKind};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, and, compare_truth, or};
use crate::types::{DefinedValue, Value};

pub(crate) const ROW_ORDERING: &str = "*row ordering*";
pub(crate) const ROW_MEMBERSHIP: &str = "*row membership*";

pub(crate) fn constructor_fields(expr: &Expr) -> Option<&[Expr]> {
    match &expr.kind {
        ExprKind::Function { name, arguments } if name == crate::records::ROW_FUNCTION => {
            Some(arguments)
        }
        _ => None,
    }
}

pub(crate) fn into_constructor_fields(expr: Expr) -> Result<Vec<Expr>, Expr> {
    match expr.kind {
        ExprKind::Function { name, arguments } if name == crate::records::ROW_FUNCTION => {
            Ok(arguments)
        }
        kind => Err(Expr {
            kind,
            position: expr.position,
        }),
    }
}

pub(crate) fn is_ordering(operator: BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual
    )
}

pub(crate) fn is_row_comparison(operator: BinaryOperator) -> bool {
    is_ordering(operator) || matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
}

pub(crate) fn unequal_entries(dialect: Dialect, left: usize, position: usize) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::CARDINALITY_VIOLATION,
            format!("Operand should contain {left} column(s)"),
        )
        .with_vendor_code(1241),
        Dialect::Sqlite => SqlError::new(SqlState::SYNTAX_ERROR, "row value misused"),
        _ => SqlError::new(
            SqlState::SYNTAX_ERROR,
            "unequal number of entries in row expressions",
        ),
    }
    .at(position)
}

pub(crate) fn misused(dialect: Dialect, fields: &[Expr]) -> Option<SqlError> {
    let position = fields.first().map_or(0, |field| field.position);
    match dialect {
        Dialect::MySql => Some(
            SqlError::new(
                SqlState::CARDINALITY_VIOLATION,
                "Operand should contain 1 column(s)",
            )
            .with_vendor_code(1241)
            .at(position),
        ),
        Dialect::Sqlite => Some(
            SqlError::new(SqlState::SYNTAX_ERROR, "row value misused")
                .with_sqlite_code(1)
                .at(position),
        ),
        _ => None,
    }
}

pub(crate) fn oracle_ordering_refused(position: usize) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, "invalid relational operator")
        .with_oracle_code(920)
        .at(position)
}

pub(crate) fn subquery_width_error(
    dialect: Dialect,
    found: usize,
    expected: usize,
    position: usize,
) -> SqlError {
    let too_many = found > expected;
    match dialect {
        Dialect::Sqlite => SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("sub-select returns {found} columns - expected {expected}"),
        ),
        Dialect::MySql => SqlError::new(
            SqlState::CARDINALITY_VIOLATION,
            format!("Operand should contain {expected} column(s)"),
        )
        .with_vendor_code(1241),
        Dialect::Oracle if too_many => {
            SqlError::new(SqlState::SYNTAX_ERROR, "too many values").with_oracle_code(913)
        }
        Dialect::Oracle => {
            SqlError::new(SqlState::SYNTAX_ERROR, "not enough values").with_oracle_code(947)
        }
        _ if too_many => SqlError::new(SqlState::SYNTAX_ERROR, "subquery has too many columns"),
        _ => SqlError::new(SqlState::SYNTAX_ERROR, "subquery has too few columns"),
    }
    .at(position)
}

fn binary(operator: BinaryOperator, left: Expr, right: Expr) -> Expr {
    let position = left.position;
    Expr {
        kind: ExprKind::Binary(operator, Box::new(left), Box::new(right)),
        position,
    }
}

fn strict(operator: BinaryOperator) -> BinaryOperator {
    match operator {
        BinaryOperator::Less | BinaryOperator::LessOrEqual => BinaryOperator::Less,
        _ => BinaryOperator::Greater,
    }
}

fn row(fields: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: crate::records::ROW_FUNCTION.to_owned(),
            arguments: fields,
        },
        position,
    }
}

fn internal_call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

/// Spells a comparison of two row constructors of the same width with SQL's rules for NULL:
/// `=` becomes the `AND` of the fields' equalities and `<>` the `OR` of their differences, so an
/// index on the fields still serves them; an ordering becomes one flat call decided by the first
/// pair that is not equal, NULL when that pair holds a NULL, however wide the rows are.
pub(crate) fn compare_constructors(
    left: Vec<Expr>,
    operator: BinaryOperator,
    right: Vec<Expr>,
    position: usize,
) -> Expr {
    let kind = match operator {
        BinaryOperator::Equal => ExprKind::And(
            left.into_iter()
                .zip(right)
                .map(|(left, right)| binary(BinaryOperator::Equal, left, right))
                .collect(),
        ),
        BinaryOperator::NotEqual => ExprKind::Or(
            left.into_iter()
                .zip(right)
                .map(|(left, right)| binary(BinaryOperator::NotEqual, left, right))
                .collect(),
        ),
        _ => {
            let symbol = Expr {
                kind: ExprKind::Literal(crate::ast::Literal::String(operator.symbol().to_owned())),
                position,
            };
            return internal_call(
                ROW_ORDERING,
                vec![symbol, row(left, position), row(right, position)],
                position,
            );
        }
    };
    Expr { kind, position }
}

/// Spells `(a, b) IN ((1, 2), (3, 4))` as one call that evaluates the left row once and
/// compares it with each listed row in turn.
pub(crate) fn membership(left: Vec<Expr>, items: Vec<Vec<Expr>>, position: usize) -> Expr {
    let mut arguments = Vec::with_capacity(items.len() + 1);
    arguments.push(row(left, position));
    arguments.extend(items.into_iter().map(|fields| row(fields, position)));
    internal_call(ROW_MEMBERSHIP, arguments, position)
}

pub(crate) fn is_internal_call(name: &str) -> bool {
    name == ROW_ORDERING || name == ROW_MEMBERSHIP
}

fn lost_shape() -> SqlError {
    SqlError::new(SqlState::INTERNAL_ERROR, "a row comparison lost its rows")
}

fn ordering_operator(symbol: &Expr) -> SqlResult<BinaryOperator> {
    match &symbol.kind {
        ExprKind::Literal(crate::ast::Literal::String(symbol)) => match symbol.as_str() {
            "<" => Ok(BinaryOperator::Less),
            "<=" => Ok(BinaryOperator::LessOrEqual),
            ">" => Ok(BinaryOperator::Greater),
            ">=" => Ok(BinaryOperator::GreaterOrEqual),
            _ => Err(lost_shape()),
        },
        _ => Err(lost_shape()),
    }
}

pub(crate) fn evaluate_internal_call(
    evaluator: &Evaluator<'_>,
    name: &str,
    arguments: &[Expr],
) -> SqlResult<Value> {
    let truth = if name == ROW_ORDERING {
        ordering(evaluator, arguments)?
    } else {
        member(evaluator, arguments)?
    };
    Ok(crate::eval::truth_value(truth))
}

fn ordering(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Option<bool>> {
    let [symbol, left, right] = arguments else {
        return Err(lost_shape());
    };
    let operator = ordering_operator(symbol)?;
    let (Some(left), Some(right)) = (constructor_fields(left), constructor_fields(right)) else {
        return Err(lost_shape());
    };
    if left.len() != right.len() {
        return Err(lost_shape());
    }
    for (left, right) in left.iter().zip(right) {
        let (left, right) = evaluator.pair(left, right)?;
        match compare_truth(&left, &right, "=")? {
            Some(true) => {}
            None => return Ok(None),
            Some(false) => return compare_truth(&left, &right, strict(operator).symbol()),
        }
    }
    Ok(Some(matches!(
        operator,
        BinaryOperator::LessOrEqual | BinaryOperator::GreaterOrEqual
    )))
}

fn member(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Option<bool>> {
    let Some((left, items)) = arguments.split_first() else {
        return Err(lost_shape());
    };
    let Some(left) = constructor_fields(left) else {
        return Err(lost_shape());
    };
    let left_values = left
        .iter()
        .map(|field| {
            if crate::eval::is_unknown_literal(field) {
                Ok(None)
            } else {
                evaluator.eval(field, None).map(Some)
            }
        })
        .collect::<SqlResult<Vec<_>>>()?;
    let mut result = Some(false);
    for item in items {
        let Some(fields) = constructor_fields(item) else {
            return Err(lost_shape());
        };
        if fields.len() != left.len() {
            return Err(lost_shape());
        }
        let mut matched = Some(true);
        for ((left, known), field) in left.iter().zip(&left_values).zip(fields) {
            let equal = match known {
                Some(value) => {
                    let other = evaluator.eval(field, evaluator.type_of(value, left))?;
                    compare_truth(value, &other, "=")?
                }
                None => {
                    let (value, other) = evaluator.pair(left, field)?;
                    compare_truth(&value, &other, "=")?
                }
            };
            matched = and(matched, equal);
            if matched == Some(false) {
                break;
            }
        }
        result = or(result, matched);
        if result == Some(true) {
            break;
        }
    }
    Ok(result)
}

pub(crate) fn evaluate_fields(
    evaluator: &Evaluator<'_>,
    fields: &[Expr],
    sample: Option<&Value>,
) -> SqlResult<Vec<Value>> {
    let sample_fields = sample.and_then(record_fields).unwrap_or_default();
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let hint = sample_fields.get(index).and_then(Value::sql_type);
            evaluator.eval(field, hint)
        })
        .collect()
}

fn record_fields(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Record(record) => Some(&record.fields),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn compare_with_record(
    fields: &[Value],
    record: &Value,
    operator: BinaryOperator,
) -> SqlResult<Option<bool>> {
    match record_fields(record) {
        Some(other) => compare_rows(fields, other, operator),
        None => Ok(None),
    }
}

/// Compares two rows of values of the same width with SQL's rules for NULL.
pub(crate) fn compare_rows(
    left: &[Value],
    right: &[Value],
    operator: BinaryOperator,
) -> SqlResult<Option<bool>> {
    match operator {
        BinaryOperator::Equal => {
            let mut result = Some(true);
            for (left, right) in left.iter().zip(right) {
                result = and(result, compare_truth(left, right, "=")?);
                if result == Some(false) {
                    break;
                }
            }
            Ok(result)
        }
        BinaryOperator::NotEqual => {
            let mut result = Some(false);
            for (left, right) in left.iter().zip(right) {
                result = or(result, compare_truth(left, right, "<>")?);
                if result == Some(true) {
                    break;
                }
            }
            Ok(result)
        }
        _ if is_ordering(operator) => {
            for (left, right) in left.iter().zip(right) {
                match compare_truth(left, right, "=")? {
                    Some(true) => {}
                    None => return Ok(None),
                    Some(false) => return compare_truth(left, right, strict(operator).symbol()),
                }
            }
            Ok(Some(matches!(
                operator,
                BinaryOperator::LessOrEqual | BinaryOperator::GreaterOrEqual
            )))
        }
        _ => Err(SqlError::not_supported(format!(
            "the {} operator between rows",
            operator.symbol()
        ))),
    }
}
