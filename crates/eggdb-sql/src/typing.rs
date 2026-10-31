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
    if operator == BinaryOperator::Subtract
        && left_type.is_datetime()
        && right_type.is_datetime()
        && crate::constraints::current_dialect() == crate::Dialect::Oracle
    {
        let date_like =
            |sql_type: SqlType| matches!(sql_type, SqlType::Timestamp(Some(0)) | SqlType::Date);
        return Some(if date_like(left_type) && date_like(right_type) {
            SqlType::Numeric(None)
        } else {
            crate::oracle_intervals::IntervalKind::DayToSecond
                .sql_type(crate::oracle_intervals::DEFAULT_FRACTION_DIGITS)
        });
    }
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
        if operator == BinaryOperator::Divide
            && crate::constraints::current_dialect() == crate::Dialect::SqlServer
            && let Some(quotient) = crate::sqlserver_numbers::quotient_sql_type(left, right, scope)
        {
            return Some(quotient);
        }
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

fn added_function_type(name: &str, arguments: &[Expr], scope: Scope<'_>) -> Option<Option<SqlType>> {
    if let Some(sql_type) = crate::advisory_locks::result_type(name) {
        return Some(Some(sql_type));
    }
    let argument_type = |at: usize| arguments.get(at).and_then(|argument| static_type(argument, scope));
    Some(match name {
        "split_part" | "concat" | "concat_ws" | "replace" | "left" | "right" | "lpad" | "rpad"
        | "repeat" | "initcap" | "translate" | "reverse" | "chr" | "md5" | "format"
        | "overlay" | "quote_literal" | "quote_nullable" | "regexp_replace" | "regexp_substr"
        | "similar_to_escape" | "similar_substring_pattern" | "to_char" | "jsonb_pretty"
        | "json_extract_path_text" | "jsonb_extract_path_text" | "pg_sleep" | "pg_notify"
        | "to_tsvector" | "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery"
        | "websearch_to_tsquery" => Some(SqlType::Text),
        "ts_rank" | "ts_rank_cd" => Some(SqlType::Real),
        "l2_distance" | "cosine_distance" | "inner_product" | "l1_distance" | "vector_norm" => {
            Some(SqlType::Double)
        }
        "vector_dims" => Some(SqlType::Integer),
        "eggdb_embed" => Some(SqlType::Text),
        "rrf_score" => Some(SqlType::Double),
        name if crate::geo_functions::FUNCTIONS.contains(&name) => crate::geo_functions::result_type(name),
        "l2_normalize" => Some(SqlType::Text),
        "strpos" | "position" | "ascii" | "regexp_count" | "regexp_instr" => {
            Some(SqlType::Integer)
        }
        "starts_with" | "regexp_like" | "isfinite" => Some(SqlType::Boolean),
        "regexp_match" | "regexp_split_to_array" => {
            Some(SqlType::Array(crate::arrays::ArrayElement::Text))
        }
        "substr" => match argument_type(0) {
            Some(SqlType::Bytea) => Some(SqlType::Bytea),
            _ => Some(SqlType::Text),
        },
        "to_date" | "make_date" => Some(SqlType::Date),
        "to_timestamp" | "make_timestamptz" => Some(SqlType::TimestampTz(None)),
        "to_number" => Some(SqlType::Numeric(None)),
        "make_time" => Some(SqlType::Time(None)),
        "make_timestamp" => Some(SqlType::Timestamp(None)),
        "make_interval" => Some(SqlType::INTERVAL),
        "time_bucket" => match argument_type(1) {
            Some(SqlType::Date) => Some(SqlType::Date),
            Some(SqlType::TimestampTz(_)) => Some(SqlType::TimestampTz(None)),
            Some(SqlType::SmallInt) => Some(SqlType::SmallInt),
            Some(SqlType::Integer) => Some(SqlType::Integer),
            Some(SqlType::BigInt) => Some(SqlType::BigInt),
            Some(_) => Some(SqlType::Timestamp(None)),
            None => None,
        },
        "date_bin" => match argument_type(1) {
            Some(SqlType::TimestampTz(_)) => Some(SqlType::TimestampTz(None)),
            Some(_) => Some(SqlType::Timestamp(None)),
            None => None,
        },
        "timezone" => match argument_type(1) {
            Some(SqlType::Timestamp(_) | SqlType::Date) => Some(SqlType::TimestampTz(None)),
            Some(SqlType::Time(_) | SqlType::TimeTz(_)) => Some(SqlType::TimeTz(None)),
            Some(_) => Some(SqlType::Timestamp(None)),
            None => None,
        },
        "to_json" | "row_to_json" | "json_build_object" | "json_build_array"
        | "json_strip_nulls" | "json_extract_path" | crate::json_functions::WHOLE_ROW
        | crate::json_functions::ROW_OBJECT => Some(SqlType::Json),
        "to_jsonb" | "jsonb_build_object" | "jsonb_build_array" | "jsonb_set" | "jsonb_insert"
        | "jsonb_strip_nulls" | "jsonb_extract_path" => Some(SqlType::Jsonb),
        _ => return None,
    })
}

fn defined_function_type(
    name: &str,
    arguments: &[Expr],
    scope: Scope<'_>,
) -> Option<Option<SqlType>> {
    let first = || {
        arguments
            .first()
            .and_then(|argument| static_type(argument, scope))
    };
    match name {
        crate::records::ROW_FUNCTION | crate::records::EXPAND_ARRAY_FUNCTION => {
            return Some(Some(SqlType::Defined(crate::types::DefinedType::RECORD)));
        }
        crate::records::FIELD_FUNCTION => {
            let Some(ExprKind::Literal(Literal::String(field))) =
                arguments.get(1).map(|field| &field.kind)
            else {
                return Some(None);
            };
            let record = arguments.first()?;
            if let ExprKind::Function {
                name,
                arguments: inner,
            } = &record.kind
                && name == crate::records::EXPAND_ARRAY_FUNCTION
            {
                let array = inner.first().and_then(|array| static_type(array, scope));
                return Some(crate::records::expanded_field_type(array, field));
            }
            return Some(crate::records::field_type(static_type(record, scope), field));
        }
        _ => {}
    }
    if crate::enums::FUNCTIONS.contains(&name) {
        return Some(crate::enums::result_type(name, first()));
    }
    crate::ranges::function_type(name, first())
}

fn function_type(name: &str, arguments: &[Expr], scope: Scope<'_>) -> Option<SqlType> {
    if let Some(sql_type) = crate::sequences::result_type(name) {
        return Some(sql_type);
    }
    if let Some(sql_type) = defined_function_type(name, arguments, scope) {
        return sql_type;
    }
    if let Some(sql_type) = added_function_type(name, arguments, scope) {
        return sql_type;
    }
    if let Some(sql_type) = crate::pg_catalog::result_type(name) {
        return Some(sql_type);
    }
    if let Some(sql_type) = crate::pg_catalog::monitoring_result_type(name) {
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

fn concatenation_type(left: &Expr, right: &Expr, scope: Scope<'_>) -> SqlType {
    let operand_type = |expr: &Expr| {
        if is_untyped_literal(expr) {
            None
        } else {
            static_type(expr, scope)
        }
    };
    let (left_type, right_type) = (operand_type(left), operand_type(right));
    let jsonb = |sql_type: Option<SqlType>| sql_type == Some(SqlType::Jsonb);
    if (jsonb(left_type) && (jsonb(right_type) || is_untyped_literal(right)))
        || (jsonb(right_type) && is_untyped_literal(left))
    {
        return SqlType::Jsonb;
    }
    if let Some(array) = crate::arrays::concat_type(left_type, right_type) {
        return array;
    }
    let bytes = |sql_type: Option<SqlType>| sql_type == Some(SqlType::Bytea);
    let bytes_concatenation = (bytes(left_type)
        && (bytes(right_type) || is_untyped_literal(right)))
        || (bytes(right_type) && is_untyped_literal(left));
    if bytes_concatenation {
        SqlType::Bytea
    } else {
        SqlType::Text
    }
}

fn oracle_function_type(name: &str, arguments: &[Expr]) -> Option<SqlType> {
    match name {
        "oracle_to_date" | "oracle_date_literal" | "oracle_add_months" | "oracle_last_day" => {
            Some(SqlType::Timestamp(Some(0)))
        }
        "oracle_to_timestamp" | "oracle_sys_extract_utc" => Some(SqlType::Timestamp(Some(6))),
        "oracle_next_day" => Some(SqlType::Timestamp(Some(0))),
        "oracle_to_timestamp_tz" | "oracle_from_tz" => Some(SqlType::TimestampTz(Some(6))),
        "oracle_months_between" => Some(SqlType::Numeric(None)),
        "oracle_tz_offset" | "oracle_listagg" => Some(SqlType::Text),
        "oracle_numtodsinterval" | "oracle_to_dsinterval" => Some(
            crate::oracle_intervals::IntervalKind::DayToSecond
                .sql_type(crate::oracle_intervals::DEFAULT_FRACTION_DIGITS),
        ),
        "oracle_numtoyminterval" | "oracle_to_yminterval" => Some(
            crate::oracle_intervals::IntervalKind::YearToMonth
                .sql_type(crate::oracle_intervals::DEFAULT_FRACTION_DIGITS),
        ),
        "oracle_timestamp_literal" => match arguments.first().map(|argument| &argument.kind) {
            Some(ExprKind::Literal(Literal::String(text)))
                if crate::oracle_datetime::literal_has_offset(text) =>
            {
                Some(SqlType::TimestampTz(Some(6)))
            }
            _ => Some(SqlType::Timestamp(Some(6))),
        },
        _ => None,
    }
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
            .and_then(|offset| scope.column_type(offset))
            .filter(|sql_type| *sql_type != SqlType::Dynamic),
        ExprKind::Slot { sql_type, .. }
        | ExprKind::Constant { sql_type, .. }
        | ExprKind::OuterColumn { sql_type, .. } => *sql_type,
        ExprKind::Subquery(subquery) => crate::subquery::result_type(subquery),
        ExprKind::Cast { sql_type, .. } => Some(crate::user_types::storable_base(*sql_type)),
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
            | BinaryOperator::RegexNoMatchFolded
            | BinaryOperator::JsonHasKey
            | BinaryOperator::JsonHasAnyKey
            | BinaryOperator::JsonHasAllKeys => Some(SqlType::Boolean),
            BinaryOperator::JsonDeletePath => Some(SqlType::Jsonb),
            BinaryOperator::TextSearchMatch => Some(SqlType::Boolean),
            BinaryOperator::VectorL2Distance
            | BinaryOperator::VectorCosineDistance
            | BinaryOperator::VectorNegativeInnerProduct
            | BinaryOperator::VectorL1Distance => Some(SqlType::Double),
            BinaryOperator::Subtract if static_type(left, scope) == Some(SqlType::Jsonb) => {
                Some(SqlType::Jsonb)
            }
            BinaryOperator::Concat => Some(concatenation_type(left, right, scope)),
            BinaryOperator::JsonFieldText | BinaryOperator::JsonPathText => Some(SqlType::Text),
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
        ExprKind::OracleFunction { name, arguments } => oracle_function_type(name, arguments),
        ExprKind::SqliteFunction { .. } => None,
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
