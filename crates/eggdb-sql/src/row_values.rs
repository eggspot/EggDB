use crate::ast::{BinaryOperator, Expr, ExprKind};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, and, compare_truth, or};
use crate::types::{DefinedValue, Value};

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

/// Spells a comparison of two row constructors of the same width as comparisons of their
/// fields, with SQL's rules for NULL: `=` holds when every pair is equal, `<>` when any pair
/// differs, and an ordering is decided by the first pair that is not equal, NULL when that
/// pair holds a NULL.
pub(crate) fn compare_constructors(
    left: Vec<Expr>,
    operator: BinaryOperator,
    right: Vec<Expr>,
    position: usize,
) -> Expr {
    let pairs: Vec<(Expr, Expr)> = left.into_iter().zip(right).collect();
    let kind = match operator {
        BinaryOperator::Equal => ExprKind::And(
            pairs
                .into_iter()
                .map(|(left, right)| binary(BinaryOperator::Equal, left, right))
                .collect(),
        ),
        BinaryOperator::NotEqual => ExprKind::Or(
            pairs
                .into_iter()
                .map(|(left, right)| binary(BinaryOperator::NotEqual, left, right))
                .collect(),
        ),
        _ => return ordered(pairs, operator, position),
    };
    Expr { kind, position }
}

fn ordered(mut pairs: Vec<(Expr, Expr)>, operator: BinaryOperator, position: usize) -> Expr {
    let Some((last_left, last_right)) = pairs.pop() else {
        return Expr {
            kind: ExprKind::Literal(crate::ast::Literal::Boolean(matches!(
                operator,
                BinaryOperator::LessOrEqual | BinaryOperator::GreaterOrEqual
            ))),
            position,
        };
    };
    let mut result = binary(operator, last_left, last_right);
    while let Some((left, right)) = pairs.pop() {
        let equal = binary(BinaryOperator::Equal, left.clone(), right.clone());
        let decided = binary(strict(operator), left, right);
        result = Expr {
            kind: ExprKind::Or(vec![
                decided,
                Expr {
                    kind: ExprKind::And(vec![equal, result]),
                    position,
                },
            ]),
            position,
        };
    }
    result.position = position;
    result
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
