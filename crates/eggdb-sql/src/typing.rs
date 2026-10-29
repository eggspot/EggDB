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
    if crate::temporal::is_time_or_interval(left_type)
        || crate::temporal::is_time_or_interval(right_type)
        || (operator == BinaryOperator::Subtract
            && left_type.is_datetime()
            && right_type.is_datetime()
            && (left_type, right_type) != (SqlType::Date, SqlType::Date))
    {
        return crate::temporal::arithmetic_type(operator, left_type, right_type);
    }
    if left_type.is_datetime() || right_type.is_datetime() {
        return match (operator, left_type, right_type) {
            (BinaryOperator::Add, SqlType::Date, other)
            | (BinaryOperator::Add, other, SqlType::Date)
                if is_integer(other) =>
            {
                Some(SqlType::Date)
            }
            (BinaryOperator::Subtract, SqlType::Date, other) if is_integer(other) => {
                Some(SqlType::Date)
            }
            (BinaryOperator::Subtract, SqlType::Date, SqlType::Date) => Some(SqlType::Integer),
            _ => None,
        };
    }
    if !left_type.is_numeric() || !right_type.is_numeric() {
        return None;
    }
    let exact = |sql_type| matches!(sql_type, SqlType::Numeric(_));
    if (exact(left_type) || exact(right_type)) && !left_type.is_float() && !right_type.is_float() {
        return Some(SqlType::Numeric(None));
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
    if let Some(sql_type) = crate::sequences::result_type(name) {
        return Some(sql_type);
    }
    if let Some(sql_type) = crate::arrays::function_type(name, arguments, scope) {
        return sql_type;
    }
    match (name, arguments) {
        ("btrim" | "ltrim" | "rtrim" | "quote_ident", [_, ..]) => Some(SqlType::Text),
        ("collate", [argument]) => static_type(argument, scope),
        ("length" | "char_length" | "character_length", [_]) => Some(SqlType::Integer),
        ("lower" | "upper", [_]) => Some(SqlType::Text),
        ("abs", [argument]) => static_type(argument, scope)
            .filter(|sql_type| sql_type.is_numeric())
            .map(SqlType::without_modifier),
        (
            "now"
            | "current_timestamp"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp",
            [],
        ) => Some(SqlType::TimestampTz(None)),
        ("localtimestamp" | "mysql_now" | "mysql_utc_now", []) => Some(SqlType::Timestamp(None)),
        ("current_date", []) => Some(SqlType::Date),
        ("extract" | "oracle_extract", [_, _]) => Some(SqlType::Numeric(None)),
        ("mysql_extract", [_, _]) => Some(SqlType::BigInt),
        ("date_part", [_, _]) => Some(SqlType::Double),
        ("date_trunc", [_, argument]) => match static_type(argument, scope)? {
            SqlType::Timestamp(_) => Some(SqlType::Timestamp(None)),
            SqlType::Date | SqlType::TimestampTz(_) => Some(SqlType::TimestampTz(None)),
            _ => None,
        },
        ("json_typeof" | "jsonb_typeof", [_]) => Some(SqlType::Text),
        ("justify_days" | "justify_hours" | "justify_interval" | "age", _) => {
            Some(SqlType::INTERVAL)
        }
        ("current_time", []) => Some(SqlType::TimeTz(None)),
        ("localtime" | "mysql_curtime", []) => Some(SqlType::Time(None)),
        ("gen_random_uuid", []) => Some(SqlType::Uuid),
        ("encode", [_, _]) => Some(SqlType::Text),
        ("decode", [_, _]) => Some(SqlType::Bytea),
        ("get_byte", [_, _]) | ("octet_length", [_]) => Some(SqlType::Integer),
        ("substring", [argument, ..]) => match static_type(argument, scope)? {
            SqlType::Bytea => Some(SqlType::Bytea),
            sql_type if sql_type.is_character() => Some(SqlType::Text),
            _ => None,
        },
        ("json_array_length" | "jsonb_array_length", [_]) => Some(SqlType::Integer),
        ("round" | "trunc" | "ceil" | "ceiling" | "floor", [argument, ..]) => {
            match static_type(argument, scope)? {
                SqlType::Numeric(_) => Some(SqlType::Numeric(None)),
                sql_type if sql_type.is_numeric() && arguments.len() == 1 => Some(SqlType::Double),
                sql_type if is_integer(sql_type) => Some(SqlType::Numeric(None)),
                _ => None,
            }
        }
        ("nullif", [left, _]) => static_type(left, scope),
        ("coalesce" | "greatest" | "least" | "mysql_greatest" | "mysql_least", [_, ..]) => {
            let mut common: Option<SqlType> = None;
            for argument in arguments {
                if is_untyped_literal(argument) {
                    continue;
                }
                let found = static_type(argument, scope)?;
                common = Some(match common {
                    None => found,
                    Some(known) => crate::set_operations::common_type(known, found)?,
                });
            }
            common
        }
        _ => None,
    }
}

fn bytes_concatenation(left: &Expr, right: &Expr, scope: Scope<'_>) -> bool {
    let bytes = |expr: &Expr| static_type(expr, scope) == Some(SqlType::Bytea);
    (bytes(left) && (bytes(right) || is_untyped_literal(right)))
        || (bytes(right) && is_untyped_literal(left))
}

pub(crate) fn static_type(expr: &Expr, scope: Scope<'_>) -> Option<SqlType> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer(number)) => Some(if i32::try_from(*number).is_ok() {
            SqlType::Integer
        } else {
            SqlType::BigInt
        }),
        ExprKind::Literal(Literal::Boolean(_)) => Some(SqlType::Boolean),
        ExprKind::Literal(Literal::String(_)) => Some(SqlType::Text),
        ExprKind::Literal(Literal::Decimal(_)) => Some(SqlType::Numeric(None)),
        ExprKind::Literal(Literal::Null) => None,
        ExprKind::Column { qualifier, name } => scope
            .column(qualifier.as_deref(), name, expr.position)
            .ok()
            .and_then(|offset| scope.column_type(offset)),
        ExprKind::Slot { sql_type, .. }
        | ExprKind::Constant { sql_type, .. }
        | ExprKind::OuterColumn { sql_type, .. } => *sql_type,
        ExprKind::Subquery(subquery) => crate::subquery::result_type(subquery),
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
            static_type(operand, scope).filter(|sql_type| {
                sql_type.is_numeric() || matches!(sql_type, SqlType::Interval { .. })
            })
        }
        ExprKind::Binary(operator, left, right) => match operator {
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual
            | BinaryOperator::RegexMatch
            | BinaryOperator::RegexMatchFolded
            | BinaryOperator::RegexNoMatch
            | BinaryOperator::RegexNoMatchFolded => Some(SqlType::Boolean),
            BinaryOperator::Concat if crate::arrays::concat_type(left, right, scope).is_some() => {
                crate::arrays::concat_type(left, right, scope)
            }
            BinaryOperator::Concat if bytes_concatenation(left, right, scope) => {
                Some(SqlType::Bytea)
            }
            BinaryOperator::Concat
            | BinaryOperator::JsonFieldText
            | BinaryOperator::JsonPathText => Some(SqlType::Text),
            BinaryOperator::JsonContains | BinaryOperator::JsonContainedIn => {
                Some(SqlType::Boolean)
            }
            BinaryOperator::JsonField | BinaryOperator::JsonPath => {
                static_type(left, scope).filter(|sql_type| sql_type.is_json())
            }
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
        ExprKind::SqliteFunction { .. } | ExprKind::OracleFunction { .. } => None,
        ExprKind::SqlServerFunction {
            function,
            arguments,
        } => {
            let first = match function {
                crate::sqlserver_functions::Function::DateAdd(_) => arguments.get(1),
                _ => arguments.first(),
            };
            function.result_type(first.and_then(|first| static_type(first, scope)))
        }
        ExprKind::Aggregate { .. } => None,
    }
}
