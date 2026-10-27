use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, UnaryOperator};
use crate::eval::Scope;
use crate::types::SqlType;

fn wider_integer(left: SqlType, right: SqlType) -> SqlType {
    let rank = |sql_type| match sql_type {
        SqlType::SmallInt => 0,
        SqlType::Integer => 1,
        _ => 2,
    };
    if rank(left) >= rank(right) {
        left
    } else {
        right
    }
}

fn is_integer(sql_type: SqlType) -> bool {
    matches!(
        sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    )
}

fn is_untyped_literal(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Null)
    )
}

fn arithmetic_type(
    operator: BinaryOperator,
    left: &Expr,
    right: &Expr,
    scope: Scope<'_>,
) -> Option<SqlType> {
    let (left_type, right_type) = match (is_untyped_literal(left), is_untyped_literal(right)) {
        (true, false) => {
            let other = static_type(right, scope)?;
            (other, other)
        }
        (false, true) => {
            let other = static_type(left, scope)?;
            (other, other)
        }
        _ => (static_type(left, scope)?, static_type(right, scope)?),
    };
    if !left_type.is_numeric() || !right_type.is_numeric() {
        return None;
    }
    if operator == BinaryOperator::Power {
        return Some(SqlType::Double);
    }
    if is_integer(left_type) && is_integer(right_type) {
        return Some(wider_integer(left_type, right_type));
    }
    if left_type == SqlType::Real && right_type == SqlType::Real {
        Some(SqlType::Real)
    } else {
        Some(SqlType::Double)
    }
}

fn function_type(name: &str, arguments: &[Expr], scope: Scope<'_>) -> Option<SqlType> {
    match (name, arguments) {
        ("length" | "char_length" | "character_length", [_]) => Some(SqlType::Integer),
        ("lower" | "upper", [_]) => Some(SqlType::Text),
        ("abs", [argument]) => {
            static_type(argument, scope).filter(|sql_type| sql_type.is_numeric())
        }
        ("nullif", [left, _]) => static_type(left, scope),
        ("coalesce", [first, rest @ ..]) => {
            let first_type = static_type(first, scope)?;
            rest.iter()
                .all(|argument| {
                    matches!(argument.kind, ExprKind::Literal(Literal::Null))
                        || static_type(argument, scope) == Some(first_type)
                })
                .then_some(first_type)
        }
        _ => None,
    }
}

/// The type an expression's values have, when it is known before any row is read: what a
/// client is told in a result's column description. `None` when the type depends on the
/// values, as for a NULL or a string constant of unknown type.
pub(crate) fn static_type(expr: &Expr, scope: Scope<'_>) -> Option<SqlType> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(number)) => Some(if i32::try_from(*number).is_ok() {
            SqlType::Integer
        } else {
            SqlType::BigInt
        }),
        ExprKind::Literal(Literal::Boolean(_)) => Some(SqlType::Boolean),
        ExprKind::Literal(Literal::String(_)) => Some(SqlType::Text),
        ExprKind::Literal(Literal::Null | Literal::Decimal(_)) => None,
        ExprKind::Column { qualifier, name } => scope
            .column(qualifier.as_deref(), name, expr.position)
            .ok()
            .and_then(|offset| scope.column_type(offset)),
        ExprKind::Slot { sql_type, .. } => *sql_type,
        ExprKind::Cast { sql_type, .. } => Some(*sql_type),
        ExprKind::SessionValue { .. } => Some(SqlType::Text),
        ExprKind::Unary(UnaryOperator::Not, _)
        | ExprKind::And(_)
        | ExprKind::Or(_)
        | ExprKind::IsNull { .. }
        | ExprKind::IsTruth { .. }
        | ExprKind::IsDistinctFrom { .. }
        | ExprKind::Between { .. }
        | ExprKind::InList { .. }
        | ExprKind::Like { .. } => Some(SqlType::Boolean),
        ExprKind::Unary(UnaryOperator::Minus | UnaryOperator::Plus, operand) => {
            static_type(operand, scope).filter(|sql_type| sql_type.is_numeric())
        }
        ExprKind::Binary(operator, left, right) => match operator {
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual => Some(SqlType::Boolean),
            BinaryOperator::Concat => Some(SqlType::Text),
            _ => arithmetic_type(*operator, left, right, scope),
        },
        ExprKind::Case {
            branches,
            otherwise,
            ..
        } => {
            let mut results = branches
                .iter()
                .map(|(_, result)| result)
                .chain(otherwise.as_deref())
                .filter(|result| !matches!(result.kind, ExprKind::Literal(Literal::Null)));
            let first = results.next()?;
            let first_type = static_type(first, scope)?;
            results
                .all(|result| static_type(result, scope) == Some(first_type))
                .then_some(first_type)
        }
        ExprKind::Function { name, arguments } => function_type(name, arguments, scope),
        ExprKind::Aggregate { .. } => None,
    }
}
