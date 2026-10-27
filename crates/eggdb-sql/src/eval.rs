use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, TruthTest, UnaryOperator};
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{
    Conversion, SqlType, Value, comparable, compare_values, convert, float_overflow, integer_value,
    out_of_range, parse_input,
};
use std::cmp::Ordering;

#[derive(Clone, Copy)]
pub(crate) struct Source<'a> {
    pub(crate) table: &'a TableDefinition,
    pub(crate) label: &'a str,
    pub(crate) offset: usize,
}

pub(crate) struct Sources<'a> {
    pub(crate) list: Vec<Source<'a>>,
    pub(crate) merged: Vec<usize>,
    pub(crate) out_of_reach: Vec<&'a str>,
    pub(crate) star: Vec<usize>,
}

#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    Empty,
    One(Source<'a>),
    Many(&'a Sources<'a>),
}

fn missing_column(qualifier: Option<&str>, name: &str, position: usize) -> SqlError {
    let shown = match qualifier {
        Some(qualifier) => format!("{qualifier}.{name}"),
        None => name.to_owned(),
    };
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{shown}\" does not exist"),
    )
    .at(position)
}

fn missing_table(qualifier: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("missing FROM-clause entry for table \"{qualifier}\""),
    )
    .at(position)
}

impl<'a> Scope<'a> {
    pub(crate) fn empty() -> Scope<'static> {
        Scope::Empty
    }

    pub(crate) fn sources(&self) -> &[Source<'a>] {
        match self {
            Scope::Empty => &[],
            Scope::One(source) => std::slice::from_ref(source),
            Scope::Many(sources) => &sources.list,
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.sources()
            .last()
            .map_or(0, |source| source.offset + source.table.columns.len())
    }

    pub(crate) fn star(&self) -> Vec<usize> {
        match self {
            Scope::Many(sources) => sources.star.clone(),
            _ => (0..self.width()).collect(),
        }
    }

    pub(crate) fn source_of(&self, offset: usize) -> Option<(&Source<'a>, usize)> {
        self.sources().iter().find_map(|source| {
            offset
                .checked_sub(source.offset)
                .filter(|&index| index < source.table.columns.len())
                .map(|index| (source, index))
        })
    }

    pub(crate) fn column_type(&self, offset: usize) -> Option<SqlType> {
        self.source_of(offset)
            .map(|(source, index)| source.table.columns[index].sql_type)
    }

    pub(crate) fn column(
        &self,
        qualifier: Option<&str>,
        name: &str,
        position: usize,
    ) -> SqlResult<usize> {
        match self {
            Scope::Empty => match qualifier {
                Some(qualifier) => Err(missing_table(qualifier, position)),
                None => Err(missing_column(None, name, position)),
            },
            Scope::One(source) => {
                if let Some(qualifier) = qualifier
                    && qualifier != source.label
                {
                    return Err(missing_table(qualifier, position));
                }
                source
                    .table
                    .column_index(name)
                    .map(|index| source.offset + index)
                    .ok_or_else(|| missing_column(qualifier, name, position))
            }
            Scope::Many(sources) => sources.column(qualifier, name, position),
        }
    }
}

impl Sources<'_> {
    fn column(&self, qualifier: Option<&str>, name: &str, position: usize) -> SqlResult<usize> {
        if let Some(qualifier) = qualifier {
            let Some(source) = self.list.iter().find(|source| source.label == qualifier) else {
                if self.out_of_reach.contains(&qualifier) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_TABLE,
                        format!("invalid reference to FROM-clause entry for table \"{qualifier}\""),
                    )
                    .with_hint(format!(
                        "There is an entry for table \"{qualifier}\", but it cannot be referenced from this part of the query."
                    ))
                    .at(position));
                }
                return Err(missing_table(qualifier, position));
            };
            return source
                .table
                .column_index(name)
                .map(|index| source.offset + index)
                .ok_or_else(|| missing_column(Some(qualifier), name, position));
        }
        let mut found = None;
        for source in &self.list {
            let Some(index) = source.table.column_index(name) else {
                continue;
            };
            let offset = source.offset + index;
            if self.merged.contains(&offset) {
                continue;
            }
            if found.is_some() {
                return Err(SqlError::new(
                    SqlState::AMBIGUOUS_COLUMN,
                    format!("column reference \"{name}\" is ambiguous"),
                )
                .at(position));
            }
            found = Some(offset);
        }
        found.ok_or_else(|| missing_column(None, name, position))
    }
}

pub(crate) fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Literal(_) | ExprKind::Column { .. } | ExprKind::Slot { .. } => Vec::new(),
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => vec![operand],
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => vec![left, right],
        ExprKind::Between {
            expr, low, high, ..
        } => vec![expr, low, high],
        ExprKind::InList { expr, list, .. } => {
            let mut all = vec![expr.as_ref()];
            all.extend(list);
            all
        }
        ExprKind::And(operands) | ExprKind::Or(operands) => operands.iter().collect(),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => {
            let mut all: Vec<&Expr> = operand.iter().map(AsRef::as_ref).collect();
            for (condition, result) in branches {
                all.push(condition);
                all.push(result);
            }
            all.extend(otherwise.iter().map(AsRef::as_ref));
            all
        }
        ExprKind::Function { arguments, .. } => arguments.iter().collect(),
        ExprKind::Aggregate { argument, .. } => argument.iter().map(AsRef::as_ref).collect(),
    }
}

pub(crate) fn first_aggregate(expr: &Expr) -> Option<&Expr> {
    if matches!(expr.kind, ExprKind::Aggregate { .. }) {
        return Some(expr);
    }
    children(expr).into_iter().find_map(first_aggregate)
}

pub(crate) fn reject_aggregates(expr: &Expr, clause: &str) -> SqlResult<()> {
    match first_aggregate(expr) {
        Some(found) => Err(SqlError::new(
            SqlState::GROUPING_ERROR,
            format!("aggregate functions are not allowed in {clause}"),
        )
        .at(found.position)),
        None => Ok(()),
    }
}

pub(crate) fn check_columns(expr: &Expr, scope: Scope<'_>) -> SqlResult<()> {
    match &expr.kind {
        ExprKind::Literal(_) => Ok(()),
        ExprKind::Column { qualifier, name } => scope
            .column(qualifier.as_deref(), name, expr.position)
            .map(|_| ()),
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => check_columns(operand, scope),
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => {
            check_columns(left, scope)?;
            check_columns(right, scope)
        }
        ExprKind::Between {
            expr, low, high, ..
        } => {
            check_columns(expr, scope)?;
            check_columns(low, scope)?;
            check_columns(high, scope)
        }
        ExprKind::InList { expr, list, .. } => {
            check_columns(expr, scope)?;
            list.iter().try_for_each(|item| check_columns(item, scope))
        }
        ExprKind::And(operands) | ExprKind::Or(operands) => operands
            .iter()
            .try_for_each(|operand| check_columns(operand, scope)),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => {
            if let Some(operand) = operand {
                check_columns(operand, scope)?;
            }
            for (condition, result) in branches {
                check_columns(condition, scope)?;
                check_columns(result, scope)?;
            }
            otherwise
                .as_deref()
                .map_or(Ok(()), |otherwise| check_columns(otherwise, scope))
        }
        ExprKind::Function { arguments, .. } => arguments
            .iter()
            .try_for_each(|argument| check_columns(argument, scope)),
        ExprKind::Aggregate { argument, .. } => argument
            .as_deref()
            .map_or(Ok(()), |argument| check_columns(argument, scope)),
        ExprKind::Slot { .. } => Ok(()),
    }
}

pub(crate) fn output_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Column { name, .. } => name.clone(),
        ExprKind::Function { name, .. } => name.clone(),
        ExprKind::Aggregate { function, .. } => function.name().to_owned(),
        ExprKind::Cast { expr, sql_type } => match &expr.kind {
            ExprKind::Column { .. } | ExprKind::Function { .. } | ExprKind::Cast { .. } => {
                output_name(expr)
            }
            _ => sql_type.internal_name().to_owned(),
        },
        ExprKind::Case { .. } => "case".into(),
        ExprKind::Literal(Literal::Boolean(_)) => "bool".into(),
        _ => "?column?".into(),
    }
}

fn is_unknown_literal(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Decimal(_) | Literal::Null)
    )
}

pub(crate) struct Evaluator<'a> {
    pub(crate) scope: Scope<'a>,
    pub(crate) row: &'a [Value],
}

impl Evaluator<'_> {
    pub(crate) fn eval(&self, expr: &Expr, hint: Option<SqlType>) -> SqlResult<Value> {
        self.eval_unplaced(expr, hint)
            .map_err(|error| error.at_if_unplaced(expr.position))
    }

    fn eval_unplaced(&self, expr: &Expr, hint: Option<SqlType>) -> SqlResult<Value> {
        match &expr.kind {
            ExprKind::Literal(literal) => literal_value(literal, hint),
            ExprKind::Column { qualifier, name } => {
                let index = self
                    .scope
                    .column(qualifier.as_deref(), name, expr.position)?;
                Ok(self.row.get(index).cloned().unwrap_or(Value::Null))
            }
            ExprKind::Unary(operator, operand) => self.unary(*operator, operand, hint),
            ExprKind::And(operands) => {
                let mut result = Some(true);
                for operand in operands {
                    result = and(result, self.boolean(operand, "AND")?);
                    if result == Some(false) {
                        break;
                    }
                }
                Ok(truth_value(result))
            }
            ExprKind::Or(operands) => {
                let mut result = Some(false);
                for operand in operands {
                    result = or(result, self.boolean(operand, "OR")?);
                    if result == Some(true) {
                        break;
                    }
                }
                Ok(truth_value(result))
            }
            ExprKind::Binary(operator, left, right) => self.binary(*operator, left, right),
            ExprKind::IsNull { expr, negated } => {
                Ok(Value::Boolean(self.eval(expr, None)?.is_null() != *negated))
            }
            ExprKind::IsTruth {
                expr,
                test,
                negated,
            } => {
                let value = self.boolean(expr, "IS")?;
                let matched = match test {
                    TruthTest::True => value == Some(true),
                    TruthTest::False => value == Some(false),
                    TruthTest::Unknown => value.is_none(),
                };
                Ok(Value::Boolean(matched != *negated))
            }
            ExprKind::IsDistinctFrom {
                left,
                right,
                negated,
            } => {
                let (left, right) = self.pair(left, right)?;
                let distinct = match (&left, &right) {
                    (Value::Null, Value::Null) => false,
                    (Value::Null, _) | (_, Value::Null) => true,
                    _ => compare_checked(&left, &right, "=")? != Ordering::Equal,
                };
                Ok(Value::Boolean(distinct != *negated))
            }
            ExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let (value, low) = self.pair(expr, low)?;
                let high = self.eval(high, self.type_of(&value, expr))?;
                let above = compare_truth(&value, &low, ">=")?;
                let below = compare_truth(&value, &high, "<=")?;
                Ok(truth_value(not_if(and(above, below), *negated)))
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let value = self.eval(expr, None)?;
                let hint = self.type_of(&value, expr);
                let mut result = Some(false);
                for item in list {
                    let item = self.eval(item, hint)?;
                    result = or(result, compare_truth(&value, &item, "=")?);
                    if result == Some(true) {
                        break;
                    }
                }
                Ok(truth_value(not_if(result, *negated)))
            }
            ExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let value = self.eval(expr, Some(SqlType::Text))?;
                let pattern = self.eval(pattern, Some(SqlType::Text))?;
                let operator = if *case_insensitive { "~~*" } else { "~~" };
                match (&value, &pattern) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::Text(text), Value::Text(pattern)) => {
                        let matched = if *case_insensitive {
                            like(&text.to_lowercase(), &pattern.to_lowercase())?
                        } else {
                            like(text, pattern)?
                        };
                        Ok(Value::Boolean(matched != *negated))
                    }
                    _ => Err(no_operator(operator, &value, &pattern)),
                }
            }
            ExprKind::Cast { expr, sql_type } => self.cast(expr, *sql_type),
            ExprKind::Case {
                operand,
                branches,
                otherwise,
            } => {
                let operand = match operand {
                    Some(operand) => Some(self.eval(operand, None)?),
                    None => None,
                };
                for (condition, result) in branches {
                    let matched = match &operand {
                        Some(operand) => {
                            let candidate = self.eval(condition, operand.sql_type())?;
                            compare_truth(operand, &candidate, "=")? == Some(true)
                        }
                        None => self.boolean(condition, "CASE/WHEN")? == Some(true),
                    };
                    if matched {
                        return self.eval(result, hint);
                    }
                }
                match otherwise {
                    Some(otherwise) => self.eval(otherwise, hint),
                    None => Ok(Value::Null),
                }
            }
            ExprKind::Function { name, arguments } => self.function(name, arguments),
            ExprKind::Slot { index, .. } => {
                Ok(self.row.get(*index).cloned().unwrap_or(Value::Null))
            }
            ExprKind::Aggregate { .. } => Err(SqlError::new(
                SqlState::GROUPING_ERROR,
                "aggregate functions are not allowed here",
            )),
        }
    }

    pub(crate) fn boolean(&self, expr: &Expr, context: &str) -> SqlResult<Option<bool>> {
        match self.eval(expr, Some(SqlType::Boolean))? {
            Value::Null => Ok(None),
            Value::Boolean(flag) => Ok(Some(flag)),
            other => Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "argument of {context} must be type boolean, not type {}",
                    other.type_name()
                ),
            )
            .at(expr.position)),
        }
    }

    fn pair(&self, left: &Expr, right: &Expr) -> SqlResult<(Value, Value)> {
        if is_unknown_literal(left) && !is_unknown_literal(right) {
            let right_value = self.eval(right, None)?;
            let left = self.eval(left, self.type_of(&right_value, right))?;
            Ok((left, right_value))
        } else {
            let left_value = self.eval(left, None)?;
            let right = self.eval(right, self.type_of(&left_value, left))?;
            Ok((left_value, right))
        }
    }

    fn type_of(&self, value: &Value, expr: &Expr) -> Option<SqlType> {
        value.sql_type().or_else(|| self.declared_type(expr))
    }

    fn declared_type(&self, expr: &Expr) -> Option<SqlType> {
        match &expr.kind {
            ExprKind::Column { qualifier, name } => {
                let index = self
                    .scope
                    .column(qualifier.as_deref(), name, expr.position)
                    .ok()?;
                self.scope.column_type(index)
            }
            ExprKind::Slot { sql_type, .. } => *sql_type,
            ExprKind::Cast { sql_type, .. } => Some(*sql_type),
            ExprKind::Unary(UnaryOperator::Minus | UnaryOperator::Plus, operand) => {
                self.declared_type(operand)
            }
            _ => None,
        }
    }

    fn unary(
        &self,
        operator: UnaryOperator,
        operand: &Expr,
        hint: Option<SqlType>,
    ) -> SqlResult<Value> {
        if operator == UnaryOperator::Not {
            return Ok(truth_value(self.boolean(operand, "NOT")?.map(|flag| !flag)));
        }
        let value = self.eval(operand, hint)?;
        let symbol = if operator == UnaryOperator::Minus {
            "-"
        } else {
            "+"
        };
        if operator == UnaryOperator::Plus {
            return match value.sql_type() {
                None => Ok(Value::Null),
                Some(sql_type) if sql_type.is_numeric() => Ok(value),
                Some(_) => Err(no_prefix_operator(symbol, &value)),
            };
        }
        match value {
            Value::Null => Ok(Value::Null),
            Value::SmallInt(number) => number
                .checked_neg()
                .map(Value::SmallInt)
                .ok_or_else(|| out_of_range(SqlType::SmallInt)),
            Value::Integer(number) => number
                .checked_neg()
                .map(Value::Integer)
                .ok_or_else(|| out_of_range(SqlType::Integer)),
            Value::BigInt(number) => number
                .checked_neg()
                .map(Value::BigInt)
                .ok_or_else(|| out_of_range(SqlType::BigInt)),
            Value::Real(number) => Ok(Value::Real(-number)),
            Value::Double(number) => Ok(Value::Double(-number)),
            other => Err(no_prefix_operator(symbol, &other)),
        }
    }

    fn binary(&self, operator: BinaryOperator, left: &Expr, right: &Expr) -> SqlResult<Value> {
        match operator {
            BinaryOperator::Concat => {
                let left = self.eval(left, Some(SqlType::Text))?;
                let right = self.eval(right, Some(SqlType::Text))?;
                let either_text = matches!(left, Value::Text(_)) || matches!(right, Value::Text(_));
                let either_null = left.is_null() || right.is_null();
                if !either_text && !either_null {
                    return Err(no_operator("||", &left, &right));
                }
                match (left.to_text(), right.to_text()) {
                    (Some(mut left), Some(right)) => {
                        left.push_str(&right);
                        Ok(Value::Text(left))
                    }
                    _ => Ok(Value::Null),
                }
            }
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual => {
                let (left, right) = self.pair(left, right)?;
                Ok(truth_value(compare_truth(
                    &left,
                    &right,
                    operator.symbol(),
                )?))
            }
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::Power => {
                let (left, right) = self.pair(left, right)?;
                arithmetic(operator, &left, &right)
            }
        }
    }

    fn cast(&self, expr: &Expr, target: SqlType) -> SqlResult<Value> {
        match &expr.kind {
            ExprKind::Literal(Literal::String(text)) if target.is_string() => {
                convert(Value::Text(text.clone()), target, Conversion::Explicit)
            }
            ExprKind::Literal(Literal::String(text)) => parse_input(text, target),
            ExprKind::Literal(Literal::Decimal(text)) => decimal_literal(text, target, true),
            _ => convert(self.eval(expr, None)?, target, Conversion::Explicit),
        }
    }

    fn function(&self, name: &str, arguments: &[Expr]) -> SqlResult<Value> {
        let mut values = Vec::with_capacity(arguments.len());
        let mut hint = None;
        for argument in arguments {
            let value = self.eval(argument, hint)?;
            hint = hint.or_else(|| self.type_of(&value, argument));
            values.push(value);
        }
        let unknown = || {
            let types: Vec<String> = values.iter().map(Value::type_name).collect();
            SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {name}({}) does not exist", types.join(", ")),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            )
        };
        match (name, values.as_slice()) {
            ("coalesce", [_, ..]) => Ok(values
                .iter()
                .find(|value| !value.is_null())
                .cloned()
                .unwrap_or(Value::Null)),
            ("nullif", [left, right]) => {
                if compare_truth(left, right, "=")? == Some(true) {
                    Ok(Value::Null)
                } else {
                    Ok(left.clone())
                }
            }
            (_, [Value::Null])
                if ["abs", "length", "char_length", "lower", "upper"].contains(&name) =>
            {
                Ok(Value::Null)
            }
            ("abs", [value]) => match value {
                Value::SmallInt(number) => number
                    .checked_abs()
                    .map(Value::SmallInt)
                    .ok_or_else(|| out_of_range(SqlType::SmallInt)),
                Value::Integer(number) => number
                    .checked_abs()
                    .map(Value::Integer)
                    .ok_or_else(|| out_of_range(SqlType::Integer)),
                Value::BigInt(number) => number
                    .checked_abs()
                    .map(Value::BigInt)
                    .ok_or_else(|| out_of_range(SqlType::BigInt)),
                Value::Real(number) => Ok(Value::Real(number.abs())),
                Value::Double(number) => Ok(Value::Double(number.abs())),
                _ => Err(unknown()),
            },
            ("length" | "char_length" | "character_length", [Value::Text(text)]) => {
                integer_value(text.chars().count() as i64, SqlType::Integer)
            }
            ("lower", [Value::Text(text)]) => Ok(Value::Text(text.to_lowercase())),
            ("upper", [Value::Text(text)]) => Ok(Value::Text(text.to_uppercase())),
            _ => Err(unknown()),
        }
    }
}

pub(crate) fn literal_value(literal: &Literal, hint: Option<SqlType>) -> SqlResult<Value> {
    match literal {
        Literal::Null => Ok(Value::Null),
        Literal::Boolean(flag) => Ok(Value::Boolean(*flag)),
        Literal::Integer(number) => Ok(match i32::try_from(*number) {
            Ok(small) => Value::Integer(small),
            Err(_) => Value::BigInt(*number),
        }),
        Literal::String(text) => string_constant(text, hint),
        Literal::Decimal(text) => decimal_constant(text, hint),
    }
}

pub(crate) fn string_constant(text: &str, hint: Option<SqlType>) -> SqlResult<Value> {
    match hint {
        Some(sql_type) if !sql_type.is_string() => parse_input(text, sql_type),
        _ => Ok(Value::Text(text.to_owned())),
    }
}

pub(crate) fn decimal_constant(text: &str, hint: Option<SqlType>) -> SqlResult<Value> {
    match hint {
        Some(sql_type) if sql_type.is_float() => decimal_literal(text, SqlType::Double, false),
        _ => Err(numeric_not_supported(text)),
    }
}

fn numeric_not_supported(text: &str) -> SqlError {
    SqlError::not_supported(format!(
        "the numeric constant {text} needs type numeric, which is not supported yet"
    ))
    .with_hint(format!(
        "Write {text}::double precision, or compare it with a real or double precision column."
    ))
}

fn decimal_literal(text: &str, target: SqlType, explicit: bool) -> SqlResult<Value> {
    if target.is_float() {
        return parse_input(text, target);
    }
    if explicit
        && matches!(
            target,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        )
    {
        let parsed: f64 = text.parse().map_err(|_| numeric_not_supported(text))?;
        return convert(Value::Double(parsed.round()), target, Conversion::Explicit);
    }
    Err(numeric_not_supported(text))
}

pub(crate) fn truth_value(truth: Option<bool>) -> Value {
    truth.map_or(Value::Null, Value::Boolean)
}

fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn not_if(value: Option<bool>, negated: bool) -> Option<bool> {
    value.map(|flag| flag != negated)
}

fn no_operator(symbol: &str, left: &Value, right: &Value) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {symbol} {}",
            left.type_name(),
            right.type_name()
        ),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
}

fn no_prefix_operator(symbol: &str, value: &Value) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {symbol} {}", value.type_name()),
    )
}

fn compare_checked(left: &Value, right: &Value, symbol: &str) -> SqlResult<Ordering> {
    match (left.sql_type(), right.sql_type()) {
        (Some(a), Some(b)) if comparable(a, b) => Ok(compare_values(left, right)),
        _ => Err(no_operator(symbol, left, right)),
    }
}

pub(crate) fn compare_truth(left: &Value, right: &Value, symbol: &str) -> SqlResult<Option<bool>> {
    if left.is_null() || right.is_null() {
        if let (Some(a), Some(b)) = (left.sql_type(), right.sql_type())
            && !comparable(a, b)
        {
            return Err(no_operator(symbol, left, right));
        }
        return Ok(None);
    }
    let ordering = compare_checked(left, right, symbol)?;
    Ok(Some(match symbol {
        "=" => ordering == Ordering::Equal,
        "<>" => ordering != Ordering::Equal,
        "<" => ordering == Ordering::Less,
        "<=" => ordering != Ordering::Greater,
        ">" => ordering == Ordering::Greater,
        _ => ordering != Ordering::Less,
    }))
}

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

fn division_by_zero() -> SqlError {
    SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}

fn arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> SqlResult<Value> {
    let symbol = operator.symbol();
    let (Some(left_type), Some(right_type)) = (left.sql_type(), right.sql_type()) else {
        let other = if left.is_null() { right } else { left };
        if let Some(sql_type) = other.sql_type()
            && !sql_type.is_numeric()
        {
            return Err(no_operator(symbol, left, right));
        }
        return Ok(Value::Null);
    };
    if !left_type.is_numeric() || !right_type.is_numeric() {
        return Err(no_operator(symbol, left, right));
    }
    if operator == BinaryOperator::Power {
        return power(
            left.as_float().unwrap_or(f64::NAN),
            right.as_float().unwrap_or(f64::NAN),
        );
    }
    if let (Some(a), Some(b)) = (left.as_integer(), right.as_integer()) {
        let result_type = wider_integer(left_type, right_type);
        let result = match operator {
            BinaryOperator::Add => a.checked_add(b),
            BinaryOperator::Subtract => a.checked_sub(b),
            BinaryOperator::Multiply => a.checked_mul(b),
            BinaryOperator::Divide if b == 0 => return Err(division_by_zero()),
            BinaryOperator::Divide => a.checked_div(b),
            BinaryOperator::Modulo if b == 0 => return Err(division_by_zero()),
            _ => Some(a.checked_rem(b).unwrap_or(0)),
        };
        return integer_value(
            result.ok_or_else(|| out_of_range(result_type))?,
            result_type,
        );
    }
    if operator == BinaryOperator::Modulo {
        return Err(no_operator(symbol, left, right));
    }
    let single = left_type == SqlType::Real && right_type == SqlType::Real;
    let (a, b) = (
        left.as_float().unwrap_or(f64::NAN),
        right.as_float().unwrap_or(f64::NAN),
    );
    if operator == BinaryOperator::Divide && b == 0.0 {
        return Err(division_by_zero());
    }
    let result = if single {
        let (a, b) = (a as f32, b as f32);
        f64::from(match operator {
            BinaryOperator::Add => a + b,
            BinaryOperator::Subtract => a - b,
            BinaryOperator::Multiply => a * b,
            _ => a / b,
        })
    } else {
        match operator {
            BinaryOperator::Add => a + b,
            BinaryOperator::Subtract => a - b,
            BinaryOperator::Multiply => a * b,
            _ => a / b,
        }
    };
    if result.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(float_overflow());
    }
    let underflowed = match operator {
        BinaryOperator::Multiply => a != 0.0 && b != 0.0,
        BinaryOperator::Divide => a != 0.0 && b.is_finite(),
        _ => false,
    };
    if underflowed && result == 0.0 {
        return Err(SqlError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow",
        ));
    }
    Ok(if single {
        Value::Real(result as f32)
    } else {
        Value::Double(result)
    })
}

fn power(base: f64, exponent: f64) -> SqlResult<Value> {
    let invalid = |message: &str| SqlError::new(SqlState::INVALID_ARGUMENT_FOR_POWER, message);
    if base == 0.0 && exponent < 0.0 {
        return Err(invalid("zero raised to a negative power is undefined"));
    }
    if base < 0.0 && exponent.is_finite() && exponent.fract() != 0.0 {
        return Err(invalid(
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    let result = base.powf(exponent);
    if result.is_infinite() && base.is_finite() && exponent.is_finite() {
        return Err(float_overflow());
    }
    Ok(Value::Double(result))
}

fn like(text: &str, pattern: &str) -> SqlResult<bool> {
    let text: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(character) = chars.next() {
        tokens.push(match character {
            '%' => LikeToken::Any,
            '_' => LikeToken::One,
            '\\' => match chars.next() {
                Some(escaped) => LikeToken::Literal(escaped),
                None => {
                    return Err(SqlError::new(
                        SqlState::INVALID_ESCAPE_SEQUENCE,
                        "LIKE pattern must not end with escape character",
                    ));
                }
            },
            other => LikeToken::Literal(other),
        });
    }
    let mut text_at = 0;
    let mut token_at = 0;
    let mut backtrack: Option<(usize, usize)> = None;
    while text_at < text.len() {
        match tokens.get(token_at) {
            Some(LikeToken::Any) => {
                backtrack = Some((token_at, text_at));
                token_at += 1;
                continue;
            }
            Some(LikeToken::One) => {
                text_at += 1;
                token_at += 1;
                continue;
            }
            Some(LikeToken::Literal(expected)) if *expected == text[text_at] => {
                text_at += 1;
                token_at += 1;
                continue;
            }
            _ => {}
        }
        match backtrack {
            Some((star, matched)) => {
                token_at = star + 1;
                text_at = matched + 1;
                backtrack = Some((star, matched + 1));
            }
            None => return Ok(false),
        }
    }
    Ok(tokens[token_at..]
        .iter()
        .all(|token| matches!(token, LikeToken::Any)))
}

#[derive(Clone, Copy)]
enum LikeToken {
    Any,
    One,
    Literal(char),
}

#[cfg(test)]
mod tests {
    use super::like;

    #[test]
    fn like_matches_percent_underscore_and_escapes() {
        let cases = [
            ("abc", "abc", true),
            ("abc", "a%", true),
            ("abc", "%c", true),
            ("abc", "a_c", true),
            ("abc", "a_", false),
            ("a%c", "a\\%c", true),
            ("abbbc", "a%b%c", true),
            ("", "%", true),
            ("", "_", false),
            ("mississippi", "%iss%ppi", true),
            ("mississippi", "%iss%ppx", false),
        ];
        for (text, pattern, expected) in cases {
            assert_eq!(like(text, pattern), Ok(expected), "{text} LIKE {pattern}");
        }
        assert!(like("a", "a\\").is_err());
    }
}
