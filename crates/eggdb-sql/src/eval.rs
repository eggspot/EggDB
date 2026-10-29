use crate::arrays::ArrayElement;
use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, TruthTest, UnaryOperator};
use crate::datetime::{self, DateTimeKind, LocalTime};
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::json;
use crate::numeric::Numeric;
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
                let index = source
                    .table
                    .column_index(name)
                    .ok_or_else(|| missing_column(qualifier, name, position))?;
                if crate::temp_tables::is_repeated(source.table, name) {
                    return Err(crate::temp_tables::ambiguous(name, position));
                }
                Ok(source.offset + index)
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
            let index = source
                .table
                .column_index(name)
                .ok_or_else(|| missing_column(Some(qualifier), name, position))?;
            if crate::temp_tables::is_repeated(source.table, name) {
                return Err(crate::temp_tables::ambiguous(name, position));
            }
            return Ok(source.offset + index);
        }
        let mut found = None;
        for source in &self.list {
            let Some(index) = source.table.column_index(name) else {
                continue;
            };
            if crate::temp_tables::is_repeated(source.table, name) {
                return Err(crate::temp_tables::ambiguous(name, position));
            }
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
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => Vec::new(),
        ExprKind::Subquery(subquery) => subquery.test.operand().into_iter().collect(),
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
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => arguments.iter().collect(),
        ExprKind::Aggregate { argument, .. } => argument.iter().map(AsRef::as_ref).collect(),
    }
}

pub(crate) fn children_mut(expr: &mut Expr) -> Vec<&mut Expr> {
    match &mut expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => Vec::new(),
        ExprKind::Subquery(subquery) => subquery.test.operand_mut().into_iter().collect(),
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
            let mut all = vec![expr.as_mut()];
            all.extend(list.iter_mut());
            all
        }
        ExprKind::And(operands) | ExprKind::Or(operands) => operands.iter_mut().collect(),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => {
            let mut all: Vec<&mut Expr> = operand.iter_mut().map(AsMut::as_mut).collect();
            for (condition, result) in branches {
                all.push(condition);
                all.push(result);
            }
            all.extend(otherwise.iter_mut().map(AsMut::as_mut));
            all
        }
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => arguments.iter_mut().collect(),
        ExprKind::Aggregate { argument, .. } => argument.iter_mut().map(AsMut::as_mut).collect(),
    }
}

pub(crate) fn first_aggregate(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::Aggregate { .. } => Some(expr),
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => None,
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => first_aggregate(operand),
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => first_aggregate(left).or_else(|| first_aggregate(right)),
        _ => children(expr).into_iter().find_map(first_aggregate),
    }
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
        ExprKind::Function { name, .. } if name == crate::json_functions::WHOLE_ROW => Ok(()),
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => arguments
            .iter()
            .try_for_each(|argument| check_columns(argument, scope)),
        ExprKind::Aggregate { argument, .. } => argument
            .as_deref()
            .map_or(Ok(()), |argument| check_columns(argument, scope)),
        ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => Ok(()),
        ExprKind::Subquery(subquery) => subquery
            .test
            .operand()
            .map_or(Ok(()), |operand| check_columns(operand, scope)),
    }
}

pub(crate) fn output_name(expr: &Expr) -> String {
    let (name, _) = name_and_whether_it_names_a_value(expr);
    name
}

fn name_and_whether_it_names_a_value(expr: &Expr) -> (String, bool) {
    match &expr.kind {
        ExprKind::Function { name, arguments } if name == "regclass_cast" => {
            match arguments.first().map(name_and_whether_it_names_a_value) {
                Some((name, true)) => (name, true),
                _ => ("regclass".to_owned(), false),
            }
        }
        ExprKind::Function { name, .. } if name == "array_constructor" => ("array".into(), true),
        ExprKind::Function { name, arguments }
            if name == "array_subscript" || name == "collate" =>
        {
            match arguments.first().map(name_and_whether_it_names_a_value) {
                Some((name, true)) => (name, true),
                _ => ("?column?".into(), false),
            }
        }
        ExprKind::Function { name, arguments } if name == "array_element_cast" => {
            arguments.first().map_or_else(
                || ("?column?".into(), false),
                name_and_whether_it_names_a_value,
            )
        }
        ExprKind::Function { name, .. } if name == "array_any" || name == "array_all" => {
            ("?column?".into(), false)
        }
        ExprKind::Column { name, .. } | ExprKind::Function { name, .. } => (name.clone(), true),
        ExprKind::SessionValue { name, .. }
        | ExprKind::SqliteFunction { name, .. }
        | ExprKind::OracleFunction { name, .. } => ((*name).to_owned(), true),
        ExprKind::SqlServerFunction {
            function: crate::sqlserver_functions::Function::Convert { .. },
            arguments,
        } => match arguments.first().map(name_and_whether_it_names_a_value) {
            Some((name, true)) => (name, true),
            _ => ("convert".to_owned(), false),
        },
        ExprKind::SqlServerFunction { function, .. } => (function.name().to_owned(), true),
        ExprKind::Aggregate { function, .. } => (function.name().to_owned(), true),
        ExprKind::Cast { expr, sql_type } => match name_and_whether_it_names_a_value(expr) {
            (name, true) => (name, true),
            (_, false) => (sql_type.internal_name().to_owned(), false),
        },
        ExprKind::Case { .. } => ("case".into(), false),
        ExprKind::Subquery(subquery) => match crate::subquery::output_column(expr) {
            Some(column)
                if !matches!(
                    subquery.test,
                    crate::ast::SubqueryTest::Scalar | crate::ast::SubqueryTest::Array
                ) =>
            {
                (column.name, false)
            }
            Some(column) => (column.name, true),
            None => ("?column?".into(), false),
        },
        ExprKind::Literal(Literal::Boolean(_)) => ("bool".into(), false),
        _ => ("?column?".into(), false),
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
    pub(crate) correlation: Option<&'a crate::subquery::Correlation<'a>>,
}

impl Evaluator<'_> {
    pub(crate) fn eval(&self, expr: &Expr, hint: Option<SqlType>) -> SqlResult<Value> {
        self.eval_unplaced(expr, hint)
            .map_err(|error| error.at_if_unplaced(expr.position))
    }

    fn eval_unplaced(&self, expr: &Expr, hint: Option<SqlType>) -> SqlResult<Value> {
        match &expr.kind {
            ExprKind::Literal(literal) => literal_value(literal, hint),
            ExprKind::SessionValue { name, value }
                if value.is_empty() && *name == "current_schema" =>
            {
                Ok(Value::Null)
            }
            ExprKind::SessionValue { value, .. } => Ok(Value::Text(value.clone())),
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
                let value = as_text_if_character(self.eval(expr, Some(SqlType::Text))?);
                let pattern = as_text_if_character(self.eval(pattern, Some(SqlType::Text))?);
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
            ExprKind::Cast {
                expr: inner,
                sql_type: SqlType::Double,
            } if matches!(hint, Some(SqlType::Numeric(_)))
                && matches!(inner.kind, ExprKind::Literal(Literal::Decimal(_))) =>
            {
                let ExprKind::Literal(Literal::Decimal(text)) = &inner.kind else {
                    return self.cast(inner, SqlType::Double);
                };
                decimal_constant(text, hint)
            }
            ExprKind::Cast {
                expr: inner,
                sql_type: target @ SqlType::Array(element),
            } if matches!(&inner.kind, ExprKind::Function { name, .. } if name == "array_constructor") => {
                crate::arrays::cast_constructor(self, inner, *element)
                    .unwrap_or_else(|| self.cast(inner, *target))
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
            ExprKind::SqliteFunction { name, arguments } => {
                let values = self.arguments(arguments)?;
                crate::sqlite_functions::call(name, &values)
            }
            ExprKind::OracleFunction { name, arguments } if *name == "oracle_decode" => {
                self.decode(arguments, hint)
            }
            ExprKind::OracleFunction { name, arguments } => {
                let values = self.arguments(arguments)?;
                let declared: Vec<Option<SqlType>> =
                    if matches!(*name, "oracle_concat" | "oracle_to_char")
                        && values
                            .iter()
                            .any(|value| value.sql_type().is_some_and(SqlType::is_datetime))
                    {
                        arguments
                            .iter()
                            .map(|argument| self.oracle_declared_type(argument))
                            .collect()
                    } else {
                        Vec::new()
                    };
                crate::oracle_functions::call(name, &values, &declared)
            }
            ExprKind::SqlServerFunction {
                function,
                arguments,
            } => self.sqlserver_function(*function, arguments, hint),
            ExprKind::Slot { index, .. } => {
                Ok(self.row.get(*index).cloned().unwrap_or(Value::Null))
            }
            ExprKind::Aggregate { .. } => Err(SqlError::new(
                SqlState::GROUPING_ERROR,
                "aggregate functions are not allowed here",
            )),
            ExprKind::Subquery(subquery) => crate::subquery::evaluate(self, subquery),
            ExprKind::Constant { value, .. } => Ok(value.clone()),
            ExprKind::OuterColumn { .. } => Err(SqlError::not_supported(
                "a subquery that refers to the outer query here is not supported yet",
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

    pub(crate) fn declared_type(&self, expr: &Expr) -> Option<SqlType> {
        match &expr.kind {
            ExprKind::Column { qualifier, name } => {
                let index = self
                    .scope
                    .column(qualifier.as_deref(), name, expr.position)
                    .ok()?;
                self.scope.column_type(index)
            }
            ExprKind::Slot { sql_type, .. }
            | ExprKind::Constant { sql_type, .. }
            | ExprKind::OuterColumn { sql_type, .. } => *sql_type,
            ExprKind::Subquery(subquery) => crate::subquery::result_type(subquery),
            ExprKind::Cast { sql_type, .. } => Some(*sql_type),
            ExprKind::SqlServerFunction {
                function: crate::sqlserver_functions::Function::Convert { target, .. },
                ..
            } => Some(target.engine_type()),
            ExprKind::SessionValue { .. } => Some(SqlType::Text),
            ExprKind::Unary(UnaryOperator::Minus | UnaryOperator::Plus, operand) => {
                self.declared_type(operand)
            }
            _ => None,
        }
    }

    fn oracle_declared_type(&self, expr: &Expr) -> Option<SqlType> {
        match &expr.kind {
            ExprKind::Function { name, .. } => match name.as_str() {
                "mysql_now" => Some(SqlType::Timestamp(Some(0))),
                "localtimestamp" => Some(SqlType::Timestamp(Some(6))),
                "now" => Some(SqlType::TimestampTz(Some(6))),
                _ => None,
            },
            ExprKind::OracleFunction { .. } => crate::typing::static_type(expr, self.scope),
            _ => self.declared_type(expr),
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
            Value::Numeric(number) => Ok(Value::Numeric(number.negated())),
            Value::Interval(interval) => interval.negated().map(Value::Interval),
            other => Err(no_prefix_operator(symbol, &other)),
        }
    }

    fn binary(&self, operator: BinaryOperator, left: &Expr, right: &Expr) -> SqlResult<Value> {
        match operator {
            BinaryOperator::Concat => self
                .concatenation(left, right)
                .map(|concatenated| concatenated.value),
            BinaryOperator::Subtract if !is_unknown_literal(left) => {
                let left_value = self.eval(left, None)?;
                let left_type = self.type_of(&left_value, left);
                if left_type == Some(SqlType::Jsonb) {
                    let right_value = self.eval(right, None)?;
                    return crate::json_functions::subtract(&left_value, &right_value);
                }
                let right_value = self.eval(right, left_type)?;
                arithmetic(operator, &left_value, &right_value)
            }
            BinaryOperator::JsonHasKey
            | BinaryOperator::JsonHasAnyKey
            | BinaryOperator::JsonHasAllKeys => {
                let document = self.eval(left, Some(SqlType::Jsonb))?;
                let keys = self.eval(right, Some(SqlType::Text))?;
                crate::json_functions::has_key(operator.symbol(), &document, &keys)
            }
            BinaryOperator::JsonDeletePath => {
                let document = self.eval(left, Some(SqlType::Jsonb))?;
                let path = self.eval(right, Some(SqlType::Text))?;
                crate::json_functions::delete_path(&document, &path)
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
            BinaryOperator::JsonContains | BinaryOperator::JsonContainedIn => {
                let (left, right) = self.pair(left, right)?;
                let (container, contained) = if operator == BinaryOperator::JsonContains {
                    (&left, &right)
                } else {
                    (&right, &left)
                };
                match (container, contained) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::Jsonb(have), Value::Jsonb(want)) => {
                        let have = json::parse(have, true)?;
                        let want = json::parse(want, true)?;
                        Ok(Value::Boolean(json::contains(&have.value, &want.value)))
                    }
                    _ => Err(no_operator(operator.symbol(), &left, &right)),
                }
            }
            BinaryOperator::RegexMatch
            | BinaryOperator::RegexMatchFolded
            | BinaryOperator::RegexNoMatch
            | BinaryOperator::RegexNoMatchFolded => {
                let text = self.eval(left, Some(SqlType::Text))?;
                let pattern = self.eval(right, Some(SqlType::Text))?;
                crate::regex::operator(operator, &text, &pattern)
            }
            BinaryOperator::JsonField
            | BinaryOperator::JsonFieldText
            | BinaryOperator::JsonPath
            | BinaryOperator::JsonPathText => {
                let document = self.eval(left, None)?;
                let selector = self.eval(right, Some(SqlType::Text))?;
                json_select(operator, &document, &selector)
            }
        }
    }

    fn concatenation(&self, left: &Expr, right: &Expr) -> SqlResult<Concatenated> {
        let (left_operand, right_operand) = self.concatenated(left, right)?;
        let array_operand = left_operand.is_array_typed(left, self.scope)
            || right_operand.is_array_typed(right, self.scope);
        if array_operand {
            return crate::arrays::concatenation(
                self,
                (left, left_operand),
                (right, right_operand),
            );
        }
        let jsonb_pair = match (&left_operand.value, &right_operand.value) {
            (Value::Jsonb(_), Value::Jsonb(_) | Value::Null)
            | (Value::Null, Value::Jsonb(_)) => true,
            (Value::Jsonb(_), Value::Text(_)) => is_unknown_literal(right),
            (Value::Text(_), Value::Jsonb(_)) => is_unknown_literal(left),
            _ => false,
        };
        if jsonb_pair {
            return crate::json_functions::concatenated(&left_operand.value, &right_operand.value)
                .map(Concatenated::scalar);
        }
        let (left, right) = (left_operand.value, right_operand.value);
        if let Some(joined) = bytes_concatenated(&left, &right) {
            return joined.map(Concatenated::scalar);
        }
        let either_text = matches!(left, Value::Text(_)) || matches!(right, Value::Text(_));
        let either_null = left.is_null() || right.is_null();
        if !either_text && !either_null {
            return Err(no_operator("||", &left, &right));
        }
        if let (Value::Text(left), Value::Text(right)) = (&left, &right) {
            crate::memory::check_value_bytes(left.len().saturating_add(right.len()))?;
        }
        Ok(Concatenated::scalar(
            match (left.to_text(), right.to_text()) {
                (Some(mut left), Some(right)) => {
                    crate::memory::check_value_bytes(left.len().saturating_add(right.len()))?;
                    left.push_str(&right);
                    Value::Text(left)
                }
                _ => Value::Null,
            },
        ))
    }

    fn concatenation_operand(&self, expr: &Expr, hint: Option<SqlType>) -> SqlResult<Concatenated> {
        match &expr.kind {
            ExprKind::Binary(BinaryOperator::Concat, left, right) => self
                .concatenation(left, right)
                .map_err(|error| error.at_if_unplaced(expr.position)),
            _ => self.eval(expr, hint).map(Concatenated::scalar),
        }
    }

    fn concatenated(&self, left: &Expr, right: &Expr) -> SqlResult<(Concatenated, Concatenated)> {
        let bytes_hint = |operand: &Concatenated| {
            if matches!(operand.value, Value::Bytea(_)) {
                Some(SqlType::Bytea)
            } else {
                Some(SqlType::Text)
            }
        };
        let (left, right) = if is_unknown_literal(left) && !is_unknown_literal(right) {
            let right = self.concatenation_operand(right, Some(SqlType::Text))?;
            (self.concatenation_operand(left, bytes_hint(&right))?, right)
        } else {
            let left = self.concatenation_operand(left, Some(SqlType::Text))?;
            let right = self.concatenation_operand(right, bytes_hint(&left))?;
            (left, right)
        };
        Ok((
            left.with_character_as_text(),
            right.with_character_as_text(),
        ))
    }

    fn datetime_function(&self, name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
        let now = datetime::statement_now();
        Some(match (name, values) {
            ("now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp", []) => {
                Ok(Value::TimestampTz(now))
            }
            ("clock_timestamp", []) => Ok(Value::TimestampTz(now)),
            ("mysql_now", []) => datetime::utc_to_local(now).map(|local| {
                Value::Timestamp(local - local.rem_euclid(datetime::MICROS_PER_SECOND))
            }),
            ("mysql_curtime", []) => datetime::utc_to_local(now).map(|local| {
                let of_day = datetime::time_of_timestamp(local);
                Value::Time(of_day - of_day.rem_euclid(datetime::MICROS_PER_SECOND))
            }),
            ("mysql_utc_now", []) => Ok(Value::Timestamp(
                now - now.rem_euclid(datetime::MICROS_PER_SECOND),
            )),
            ("localtimestamp", []) => datetime::utc_to_local(now).map(Value::Timestamp),
            ("current_date", []) => datetime::utc_to_local(now)
                .map(|local| Value::Date(datetime::timestamp_to_date(local))),
            (
                "extract" | "date_part" | "date_trunc" | "mysql_extract",
                [_, Value::Null] | [Value::Null, _],
            ) => Ok(Value::Null),
            ("mysql_extract", [Value::Text(unit), value]) => {
                let (_, local, _) = match local_instant(value) {
                    Ok(Some(found)) => found,
                    Ok(None) => return None,
                    Err(error) => return Some(Err(error)),
                };
                datetime::mysql_extract(unit, local)
                    .map(Value::BigInt)
                    .ok_or_else(|| {
                        SqlError::syntax(format!("syntax error at or near \"{unit}\""), 0)
                    })
            }
            ("extract" | "date_part", [Value::Text(field), value]) => {
                let (kind, local, offset) = match local_instant(value) {
                    Ok(Some(found)) => found,
                    Ok(None) => return None,
                    Err(error) => return Some(Err(error)),
                };
                let extracted = datetime::extract(field, kind, local, offset);
                if name == "extract" {
                    extracted.map(Value::Numeric)
                } else {
                    extracted.map(|number| Value::Double(number.to_f64()))
                }
            }
            ("date_trunc", [Value::Text(field), value]) => {
                let (kind, local, _) = match local_instant(value) {
                    Ok(Some(found)) => found,
                    Ok(None) => return None,
                    Err(error) => return Some(Err(error)),
                };
                let kind = if kind == DateTimeKind::Date {
                    DateTimeKind::TimestampTz
                } else {
                    kind
                };
                datetime::truncate(field, kind, local).and_then(|truncated| {
                    if kind == DateTimeKind::Timestamp {
                        Ok(Value::Timestamp(truncated))
                    } else {
                        datetime::local_to_utc(truncated).map(Value::TimestampTz)
                    }
                })
            }
            _ => return None,
        })
    }

    fn cast(&self, expr: &Expr, target: SqlType) -> SqlResult<Value> {
        match &expr.kind {
            ExprKind::Literal(Literal::String(text))
                if target.is_character()
                    && target != SqlType::Catalog(crate::pg_catalog::CatalogType::NodeTree) =>
            {
                convert(Value::Text(text.clone()), target, Conversion::Explicit)
            }
            ExprKind::Literal(Literal::String(text)) => match oracle_datetime_of(text, target) {
                Some(value) => value,
                None => parse_input(text, target),
            },
            ExprKind::Literal(Literal::Decimal(text)) => decimal_literal(text, target, true),
            _ => {
                let value = self.eval(expr, None)?;
                if let Value::Text(text) = &value
                    && let Some(converted) = oracle_datetime_of(text, target)
                {
                    return converted;
                }
                convert(value, target, Conversion::Explicit)
            }
        }
    }

    fn declared_scale(&self, expr: &Expr) -> Option<u8> {
        match crate::typing::static_type(expr, self.scope)? {
            SqlType::Timestamp(scale) | SqlType::TimestampTz(scale) | SqlType::Time(scale) => scale,
            _ => None,
        }
    }

    fn sqlserver_function(
        &self,
        function: crate::sqlserver_functions::Function,
        arguments: &[Expr],
        hint: Option<SqlType>,
    ) -> SqlResult<Value> {
        use crate::sqlserver_functions::Function;
        match (function, arguments) {
            (Function::Convert { target, style }, [argument]) => {
                let value = self.eval(argument, None)?;
                let scale = self.declared_scale(argument);
                crate::sqlserver_conversion::convert(value, target, style, scale)
            }
            (Function::IsNull, [first, second]) => {
                let declared = crate::typing::static_type(first, self.scope);
                let value = self.eval(first, hint)?;
                if !value.is_null() {
                    return Ok(value);
                }
                let fallback = self.eval(second, declared.or(hint))?;
                match declared {
                    Some(sql_type) => isnull_fallback(fallback, sql_type),
                    None => Ok(fallback),
                }
            }
            _ => {
                let values = self.arguments(arguments)?;
                crate::sqlserver_functions::call(function, &values)
            }
        }
    }

    fn decode(&self, arguments: &[Expr], hint: Option<SqlType>) -> SqlResult<Value> {
        let Some((subject, rest)) = arguments.split_first() else {
            return Ok(Value::Null);
        };
        let subject = self.eval(subject, None)?;
        let (pairs, otherwise) = rest.as_chunks::<2>();
        for [search, result] in pairs {
            let candidate = self.eval(search, subject.sql_type())?;
            let matched = match (subject.is_null(), candidate.is_null()) {
                (true, true) => true,
                (false, false) => compare_truth(&subject, &candidate, "=")? == Some(true),
                _ => false,
            };
            if matched {
                return self.eval(result, hint);
            }
        }
        match otherwise.first() {
            Some(otherwise) => self.eval(otherwise, hint),
            None => Ok(Value::Null),
        }
    }

    fn oracle_extract(&self, arguments: &[Expr]) -> SqlResult<Value> {
        let [field, source] = arguments else {
            return Err(invalid_extract_field());
        };
        let ExprKind::Literal(Literal::String(field)) = &field.kind else {
            return Err(invalid_extract_field());
        };
        let declared_date = matches!(
            self.oracle_declared_type(source),
            Some(SqlType::Timestamp(Some(0)))
        ) || matches!(&source.kind, ExprKind::Function { name, .. } if name == "mysql_now");
        let value = self.eval(source, None)?;
        let Some((kind, local, offset)) = local_instant(&value)? else {
            return if value.is_null() {
                Ok(Value::Null)
            } else {
                Err(invalid_extract_field())
            };
        };
        let time_of_day = matches!(field.as_str(), "hour" | "minute" | "second");
        let zone = field.starts_with("timezone_");
        if (time_of_day && (declared_date || kind == DateTimeKind::Date))
            || (zone && kind != DateTimeKind::TimestampTz)
        {
            return Err(invalid_extract_field());
        }
        datetime::oracle_extract(field, local, offset)
            .map(Value::Numeric)
            .ok_or_else(invalid_extract_field)
    }

    fn arguments(&self, arguments: &[Expr]) -> SqlResult<Vec<Value>> {
        let mut held = crate::memory::RowBytes::default();
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let value = self.eval(argument, None)?;
            held.add(&value)?;
            values.push(value);
        }
        Ok(values)
    }

    fn function(&self, name: &str, arguments: &[Expr]) -> SqlResult<Value> {
        match name {
            "oracle_extract" => return self.oracle_extract(arguments),
            "array_constructor" => return crate::arrays::construct(self, arguments, None),
            "array_element_cast" => return crate::arrays::element_cast(self, arguments),
            "array_any" => return crate::arrays::quantified(self, arguments, false),
            "array_all" => return crate::arrays::quantified(self, arguments, true),
            "collate" => return crate::string_functions::collated(self, arguments),
            crate::json_functions::WHOLE_ROW => {
                return crate::json_functions::whole_row(self, arguments);
            }
            crate::json_functions::ROW_OBJECT => {
                return crate::json_functions::row_object(self, arguments);
            }
            "substring" => {
                if let Some(result) = crate::regexp_functions::substring(self, arguments) {
                    return result;
                }
            }
            _ => {}
        }
        if name == "pg_get_expr" {
            crate::pg_catalog::check_expression_tree_argument(name, arguments, self.scope)?;
        }
        let mut values = Vec::with_capacity(arguments.len());
        let mut hint = None;
        let declared =
            declared_argument_types(name).or_else(|| crate::pg_catalog::argument_types(name));
        let mut held = crate::memory::RowBytes::default();
        for (position, argument) in arguments.iter().enumerate() {
            let argument = crate::sequences::name_argument(name, position, argument);
            let value = match declared {
                Some(types) => self.eval(argument, types.get(position).copied().flatten())?,
                None => {
                    let value = self.eval(argument, hint)?;
                    hint = hint.or_else(|| self.type_of(&value, argument));
                    value
                }
            };
            held.add(&value)?;
            values.push(value);
        }
        let unknown = || {
            if crate::set_returning::named(name).is_some() {
                return SqlError::not_supported("set-returning functions are not allowed here")
                    .with_hint("Call it alone as a select-list item of a SELECT, or in FROM.");
            }
            let types: Vec<String> = values.iter().map(Value::type_name).collect();
            SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {name}({}) does not exist", types.join(", ")),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            )
        };
        if let Some(result) = crate::sequences::function(name, &values) {
            return result;
        }
        if let Some(result) = crate::pg_catalog::function(name, &values) {
            return result;
        }
        if let Some(result) = self.datetime_function(name, &values) {
            return result;
        }
        if let Some(result) = numeric_function(name, &values) {
            return result;
        }
        if let Some(result) = json_function(name, &values) {
            return result;
        }
        if let Some(result) = crate::json_functions::function(name, &values) {
            return result;
        }
        if let Some(result) = crate::regexp_functions::function(name, &values) {
            return result;
        }
        let formatted = match name {
            "to_char" => crate::pg_format::to_char(&values),
            "to_date" => crate::pg_format::to_date(&values),
            "to_timestamp" => crate::pg_format::to_timestamp(&values),
            "to_number" => crate::pg_format::to_number(&values),
            _ => None,
        };
        if let Some(result) = formatted {
            return result;
        }
        if let Some(result) = crate::temporal::function(name, &values) {
            return result;
        }
        if let Some(result) = crate::bytea::function(name, &values) {
            return result;
        }
        if let Some(result) = crate::arrays::function(name, &values) {
            return result;
        }
        if let Some(result) = crate::string_functions::function(name, &values) {
            return result;
        }
        match (name, values.as_slice()) {
            ("coalesce", [_, ..]) => {
                let found = values.iter().find(|value| !value.is_null());
                found.map_or(Ok(Value::Null), |value| in_common_type(value, &values))
            }
            ("greatest" | "least" | "mysql_greatest" | "mysql_least", [_, ..]) => {
                extreme(name, &values)
            }
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
                Value::Numeric(number) => Ok(Value::Numeric(number.absolute())),
                _ => Err(unknown()),
            },
            ("concat_or_null", _)
                if values.iter().any(|value| matches!(value, Value::Bytea(_))) =>
            {
                mysql_binary_concat(&values)
            }
            ("concat_or_null", _) => {
                let mut joined = String::new();
                for value in &values {
                    match crate::dialect::Dialect::MySql.value_text(value) {
                        Some(text) => {
                            crate::memory::check_value_bytes(
                                joined.len().saturating_add(text.len()),
                            )?;
                            joined.push_str(&text);
                        }
                        None => return Ok(Value::Null),
                    }
                }
                Ok(Value::Text(joined))
            }
            ("octet_length", [Value::Null]) => Ok(Value::Null),
            ("octet_length", [Value::Text(text)]) => {
                integer_value(text.len() as i64, SqlType::Integer)
            }
            ("length" | "char_length" | "character_length", [Value::Text(text)]) => {
                integer_value(text.chars().count() as i64, SqlType::Integer)
            }
            ("lower", [Value::Text(text)]) => Ok(Value::Text(text.to_lowercase())),
            ("upper", [Value::Text(text)]) => Ok(Value::Text(text.to_uppercase())),
            ("pg_sleep", [Value::Null]) => Ok(Value::Null),
            ("pg_sleep", [seconds]) => match seconds.as_float() {
                Some(seconds) => crate::cancel::sleep(seconds).map(|()| Value::Text(String::new())),
                None => Err(unknown()),
            },
            _ => Err(unknown()),
        }
    }
}

fn declared_argument_types(name: &str) -> Option<&'static [Option<SqlType>]> {
    const TIMESTAMP: Option<SqlType> = Some(SqlType::Timestamp(None));
    const INTERVAL: Option<SqlType> = Some(SqlType::INTERVAL);
    Some(match name {
        "encode" => &[Some(SqlType::Bytea), Some(SqlType::Text)],
        "decode" => &[Some(SqlType::Text), Some(SqlType::Text)],
        "get_byte" => &[Some(SqlType::Bytea), Some(SqlType::Integer)],
        "substring" => &[None, Some(SqlType::Integer), Some(SqlType::Integer)],
        "age" => &[TIMESTAMP, TIMESTAMP],
        "justify_days" | "justify_hours" | "justify_interval" => &[INTERVAL],
        "nextval" | "currval" | "regclass_cast" | "oracle_nextval" | "oracle_currval" => {
            &[Some(SqlType::Text)]
        }
        "setval" => &[
            Some(SqlType::Text),
            Some(SqlType::BigInt),
            Some(SqlType::Boolean),
        ],
        "array_to_string" => &[None, Some(SqlType::Text), Some(SqlType::Text)],
        "array_length" | "array_upper" | "array_lower" | "array_subscript" => {
            &[None, Some(SqlType::Integer)]
        }
        "array_ndims" | "cardinality" => &[None],
        "btrim" | "ltrim" | "rtrim" => &[Some(SqlType::Text), Some(SqlType::Text)],
        "quote_ident" => &[Some(SqlType::Text)],
        _ => return function_argument_types(name),
    })
}

fn function_argument_types(name: &str) -> Option<&'static [Option<SqlType>]> {
    const TEXT: Option<SqlType> = Some(SqlType::Text);
    const INTEGER: Option<SqlType> = Some(SqlType::Integer);
    const DOUBLE: Option<SqlType> = Some(SqlType::Double);
    const BOOLEAN: Option<SqlType> = Some(SqlType::Boolean);
    const JSONB: Option<SqlType> = Some(SqlType::Jsonb);
    const JSON: Option<SqlType> = Some(SqlType::Json);
    const INTERVAL: Option<SqlType> = Some(SqlType::INTERVAL);
    Some(match name {
        "split_part" => &[TEXT, TEXT, INTEGER],
        "replace" | "translate" => &[TEXT, TEXT, TEXT],
        "strpos" | "position" | "starts_with" => &[TEXT, TEXT],
        "left" | "right" | "repeat" => &[TEXT, INTEGER],
        "lpad" | "rpad" => &[TEXT, INTEGER, TEXT],
        "initcap" | "reverse" | "ascii" | "md5" | "quote_literal" => &[TEXT],
        "chr" => &[INTEGER],
        "pg_sleep" => &[DOUBLE],
        "overlay" => &[TEXT, TEXT, INTEGER, INTEGER],
        "substr" => &[None, INTEGER, INTEGER],
        "format" => &[TEXT],
        "regexp_like" | "regexp_match" | "regexp_split_to_array" | "similar_to_escape"
        | "similar_substring_pattern" => &[TEXT, TEXT, TEXT],
        "regexp_replace" => &[TEXT, TEXT, TEXT],
        "regexp_count" => &[TEXT, TEXT, INTEGER, TEXT],
        "regexp_instr" => &[TEXT, TEXT, INTEGER, INTEGER, INTEGER, TEXT, INTEGER],
        "regexp_substr" => &[TEXT, TEXT, INTEGER, INTEGER, TEXT, INTEGER],
        "to_char" => &[None, TEXT],
        "to_date" | "to_number" => &[TEXT, TEXT],
        "to_timestamp" => &[None, TEXT],
        "make_date" => &[INTEGER, INTEGER, INTEGER],
        "make_time" => &[INTEGER, INTEGER, DOUBLE],
        "make_timestamp" => &[INTEGER, INTEGER, INTEGER, INTEGER, INTEGER, DOUBLE],
        "make_timestamptz" => &[INTEGER, INTEGER, INTEGER, INTEGER, INTEGER, DOUBLE, TEXT],
        "make_interval" => &[INTEGER, INTEGER, INTEGER, INTEGER, INTEGER, INTEGER, DOUBLE],
        "date_bin" => &[INTERVAL, None, None],
        "timezone" => &[None, Some(SqlType::TimestampTz(None))],
        "jsonb_set" | "jsonb_insert" => &[JSONB, None, JSONB, BOOLEAN],
        "jsonb_strip_nulls" | "jsonb_pretty" => &[JSONB],
        "json_strip_nulls" => &[JSON],
        "json_extract_path" | "json_extract_path_text" => &[JSON],
        "jsonb_extract_path" | "jsonb_extract_path_text" => &[JSONB],
        "row_to_json" => &[None, BOOLEAN],
        "concat" | "concat_ws" | "json_build_object" | "jsonb_build_object"
        | "json_build_array" | "jsonb_build_array" | "to_json" | "to_jsonb"
        | "quote_nullable" | "isfinite" => &[],
        _ => return None,
    })
}

fn mysql_binary_concat(values: &[Value]) -> SqlResult<Value> {
    let mut joined = Vec::new();
    for value in values {
        let text;
        let bytes = match value {
            Value::Null => return Ok(Value::Null),
            Value::Bytea(bytes) => bytes.as_slice(),
            other => {
                text = crate::dialect::Dialect::MySql
                    .value_text(other)
                    .unwrap_or_default();
                text.as_bytes()
            }
        };
        crate::bytea::check_size(joined.len().saturating_add(bytes.len()))?;
        joined.extend_from_slice(bytes);
    }
    Ok(Value::Bytea(joined))
}

pub(crate) struct Concatenated {
    pub(crate) value: Value,
    pub(crate) array_element: Option<ArrayElement>,
}

impl Concatenated {
    fn scalar(value: Value) -> Self {
        Self {
            value,
            array_element: None,
        }
    }

    fn with_character_as_text(self) -> Self {
        Self {
            value: as_text_if_character(self.value),
            ..self
        }
    }

    fn is_array_typed(&self, expr: &Expr, scope: Scope<'_>) -> bool {
        match self.value {
            Value::Array(_) => true,
            Value::Null => {
                self.array_element.is_some()
                    || (!is_unknown_literal(expr)
                        && !matches!(expr.kind, ExprKind::Binary(BinaryOperator::Concat, ..))
                        && matches!(
                            crate::typing::static_type(expr, scope),
                            Some(SqlType::Array(_))
                        ))
            }
            _ => false,
        }
    }
}

fn as_text_if_character(value: Value) -> Value {
    match value {
        Value::Bpchar(mut text) => {
            text.truncate(crate::types::without_trailing_spaces(&text).len());
            Value::Text(text)
        }
        other => other,
    }
}

fn bytes_concatenated(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let joined = |a: &[u8], b: &[u8]| -> SqlResult<Value> {
        crate::bytea::check_size(a.len().saturating_add(b.len()))?;
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        Ok(Value::Bytea(out))
    };
    Some(match (left, right) {
        (Value::Bytea(a), Value::Bytea(b)) => joined(a, b),
        (Value::Bytea(_), Value::Null) | (Value::Null, Value::Bytea(_)) => Ok(Value::Null),
        _ => return None,
    })
}

fn extreme(name: &str, values: &[Value]) -> SqlResult<Value> {
    let mysql = name.starts_with("mysql_");
    if mysql && values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let symbol = if name.ends_with("greatest") { ">" } else { "<" };
    let mut best: Option<&Value> = None;
    for value in values.iter().filter(|value| !value.is_null()) {
        best = match best {
            Some(current) if compare_truth(value, current, symbol)? != Some(true) => Some(current),
            _ => Some(value),
        };
    }
    best.map_or(Ok(Value::Null), |value| in_common_type(value, values))
}

fn in_common_type(chosen: &Value, values: &[Value]) -> SqlResult<Value> {
    let mut common: Option<SqlType> = None;
    for sql_type in values.iter().filter_map(Value::sql_type) {
        common = match common {
            None => Some(sql_type),
            Some(known) => match crate::set_operations::common_type(known, sql_type) {
                Some(found) => Some(found),
                None => return Ok(chosen.clone()),
            },
        };
    }
    match common {
        Some(sql_type) if chosen.sql_type() != Some(sql_type) && !sql_type.is_character() => {
            convert(chosen.clone(), sql_type, Conversion::Explicit)
        }
        _ => Ok(chosen.clone()),
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

fn read_by_dialect(sql_type: SqlType) -> Option<crate::Dialect> {
    if !matches!(
        sql_type,
        SqlType::Date
            | SqlType::Time(_)
            | SqlType::Timestamp(_)
            | SqlType::TimestampTz(_)
            | SqlType::Boolean
            | SqlType::Uuid
    ) {
        return None;
    }
    match crate::constraints::current_dialect() {
        crate::Dialect::SqlServer => Some(crate::Dialect::SqlServer),
        crate::Dialect::Oracle if sql_type.is_datetime() => Some(crate::Dialect::Oracle),
        _ => None,
    }
}

fn oracle_datetime_of(text: &str, target: SqlType) -> Option<SqlResult<Value>> {
    if !target.is_datetime() || crate::constraints::current_dialect() != crate::Dialect::Oracle {
        return None;
    }
    crate::oracle_conversion::datetime_of(text, target)
}

pub(crate) fn string_constant(text: &str, hint: Option<SqlType>) -> SqlResult<Value> {
    if let Some(sql_type) = hint
        && let Some(dialect) = read_by_dialect(sql_type)
    {
        let read = match dialect {
            crate::Dialect::Oracle => crate::oracle_conversion::literal_of(text, sql_type),
            _ => crate::sqlserver_conversion::literal_of(text, sql_type),
        };
        if let Some(value) = read {
            return value;
        }
    }
    match hint {
        Some(sql_type) if !sql_type.is_string() => {
            let parsed = parse_input(text, sql_type.without_modifier());
            if parsed.is_err()
                && sql_type.is_numeric()
                && crate::constraints::current_dialect() == crate::Dialect::Oracle
                && let Some(number) = crate::oracle_conversion::literal_of(text, sql_type)
            {
                return number;
            }
            parsed
        }
        _ => Ok(Value::Text(text.to_owned())),
    }
}

pub(crate) fn decimal_constant(text: &str, hint: Option<SqlType>) -> SqlResult<Value> {
    match hint {
        Some(sql_type) if sql_type.is_float() => parse_input(text, SqlType::Double),
        _ => Numeric::parse(text).map(Value::Numeric),
    }
}

fn decimal_literal(text: &str, target: SqlType, explicit: bool) -> SqlResult<Value> {
    if target.is_float() {
        return parse_input(text, target);
    }
    let conversion = if explicit {
        Conversion::Explicit
    } else {
        Conversion::Assignment
    };
    convert(Value::Numeric(Numeric::parse(text)?), target, conversion)
}

fn local_instant(value: &Value) -> SqlResult<Option<(DateTimeKind, LocalTime, i32)>> {
    Ok(Some(match value {
        Value::Date(days) => (DateTimeKind::Date, LocalTime::of_date(*days), 0),
        Value::Timestamp(micros) => (DateTimeKind::Timestamp, LocalTime::of_micros(*micros), 0),
        Value::TimestampTz(micros) => (
            DateTimeKind::TimestampTz,
            LocalTime::of_micros(datetime::utc_to_local(*micros)?),
            datetime::session_offset(),
        ),
        _ => return Ok(None),
    }))
}

fn invalid_extract_field() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "invalid extract field for extract source",
    )
    .with_oracle_code(30076)
}

fn numeric_function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let places = |value: &Value| value.as_integer();
    Some(match (name, values) {
        ("round" | "trunc" | "ceil" | "ceiling" | "floor", [Value::Null, ..])
        | ("round" | "trunc", [_, Value::Null]) => Ok(Value::Null),
        ("round", [Value::Numeric(number)]) => number.rounded(0).map(Value::Numeric),
        ("trunc", [Value::Numeric(number)]) => number.truncated(0).map(Value::Numeric),
        ("round", [Value::Numeric(number), count]) => {
            number.rounded(places(count)?).map(Value::Numeric)
        }
        ("trunc", [Value::Numeric(number), count]) => {
            number.truncated(places(count)?).map(Value::Numeric)
        }
        ("round" | "trunc", [whole, count])
            if whole.as_integer().is_some() && places(count).is_some() =>
        {
            let number = Value::Numeric(Numeric::from_i64(whole.as_integer().unwrap_or(0)));
            return numeric_function(name, &[number, count.clone()]);
        }
        ("ceil" | "ceiling", [Value::Numeric(number)]) => number.ceiling().map(Value::Numeric),
        ("floor", [Value::Numeric(number)]) => number.floor().map(Value::Numeric),
        ("round" | "trunc" | "ceil" | "ceiling" | "floor", [value]) => {
            let number = value.as_float()?;
            if matches!(value, Value::Boolean(_)) {
                return None;
            }
            Ok(Value::Double(match name {
                "round" => number.round_ties_even(),
                "trunc" => number.trunc(),
                "floor" => number.floor(),
                _ => number.ceil(),
            }))
        }
        _ => return None,
    })
}

fn json_function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let (text, binary) = match values {
        [Value::Json(text)] => (text, false),
        [Value::Jsonb(text)] => (text, true),
        [Value::Text(text)] => (text, name.starts_with("jsonb")),
        [Value::Null]
            if [
                "json_typeof",
                "jsonb_typeof",
                "json_array_length",
                "jsonb_array_length",
            ]
            .contains(&name) =>
        {
            return Some(Ok(Value::Null));
        }
        _ => return None,
    };
    let wanted_binary = name.starts_with("jsonb");
    if wanted_binary != binary {
        return None;
    }
    Some(match name {
        "json_typeof" | "jsonb_typeof" => {
            json::parse(text, binary).map(|node| Value::Text(json::type_of(&node.value).to_owned()))
        }
        "json_array_length" | "jsonb_array_length" => {
            json::parse(text, binary).and_then(|node| match node.value {
                json::Json::Array(items) => integer_value(items.len() as i64, SqlType::Integer),
                _ => Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    if binary {
                        "cannot get array length of a non-array"
                    } else {
                        "cannot get array length of a scalar"
                    },
                )),
            })
        }
        _ => return None,
    })
}

fn json_select(operator: BinaryOperator, document: &Value, selector: &Value) -> SqlResult<Value> {
    let (text, binary) = match document {
        Value::Null => return Ok(Value::Null),
        Value::Json(text) => (text, false),
        Value::Jsonb(text) => (text, true),
        _ => return Err(no_operator(operator.symbol(), document, selector)),
    };
    let steps: Vec<json::Step> = match (operator, selector) {
        (_, Value::Null) => return Ok(Value::Null),
        (BinaryOperator::JsonField | BinaryOperator::JsonFieldText, Value::Text(key)) => {
            vec![json::Step::Key(key.clone())]
        }
        (BinaryOperator::JsonField | BinaryOperator::JsonFieldText, index)
            if index.as_integer().is_some() =>
        {
            vec![json::Step::Index(index.as_integer().unwrap_or(0))]
        }
        (BinaryOperator::JsonPath | BinaryOperator::JsonPathText, Value::Text(path)) => {
            json::parse_path(path)?
                .into_iter()
                .map(json::Step::Key)
                .collect()
        }
        _ => return Err(no_operator(operator.symbol(), document, selector)),
    };
    let root = json::parse(text, binary)?;
    let field_on_array = matches!(
        operator,
        BinaryOperator::JsonField | BinaryOperator::JsonFieldText
    ) && matches!(root.value, json::Json::Array(_))
        && matches!(steps.first(), Some(json::Step::Key(_)));
    if field_on_array {
        return Ok(Value::Null);
    }
    let mut node = &root;
    for step in &steps {
        match json::child(node, step, binary) {
            Some(next) => node = next,
            None => return Ok(Value::Null),
        }
    }
    let as_text = matches!(
        operator,
        BinaryOperator::JsonFieldText | BinaryOperator::JsonPathText
    );
    if as_text {
        return Ok(json::render_text(text, node, binary)?.map_or(Value::Null, Value::Text));
    }
    let rendered = json::render(text, node, binary)?;
    Ok(if binary {
        Value::Jsonb(rendered)
    } else {
        Value::Json(rendered)
    })
}

fn datetime_arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> SqlResult<Value> {
    let shifted = |days: i32, by: i64| -> SqlResult<Value> {
        let moved = i64::from(days)
            .checked_add(by)
            .ok_or_else(datetime::date_out_of_range)?;
        datetime::checked_date(moved).map(Value::Date)
    };
    match (operator, left, right) {
        (BinaryOperator::Add, Value::Date(days), other)
        | (BinaryOperator::Add, other, Value::Date(days))
            if other.as_integer().is_some() =>
        {
            shifted(*days, other.as_integer().unwrap_or(0))
        }
        (BinaryOperator::Subtract, Value::Date(days), other) if other.as_integer().is_some() => {
            shifted(
                *days,
                other
                    .as_integer()
                    .unwrap_or(0)
                    .checked_neg()
                    .unwrap_or(i64::MAX),
            )
        }
        (BinaryOperator::Subtract, Value::Date(a), Value::Date(b)) => {
            integer_value(i64::from(*a) - i64::from(*b), SqlType::Integer)
        }
        _ => Err(no_operator(operator.symbol(), left, right)),
    }
}

pub(crate) fn truth_value(truth: Option<bool>) -> Value {
    truth.map_or(Value::Null, Value::Boolean)
}

pub(crate) fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(crate) fn or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
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
    engine_compare_truth(left, right, symbol).or_else(|error| {
        if error.state() != SqlState::UNDEFINED_FUNCTION {
            return Err(error);
        }
        let dialect = crate::constraints::current_dialect();
        if dialect != crate::Dialect::SqlServer && dialect != crate::Dialect::Oracle {
            return Err(error);
        }
        if left.is_null() || right.is_null() {
            return Ok(None);
        }
        let converted = if dialect == crate::Dialect::Oracle {
            crate::oracle_conversion::converted_for_comparison(left, right)
        } else {
            crate::sqlserver_conversion::by_precedence(left, right)
        };
        match converted {
            Some(converted) => {
                let (left, right) = converted?;
                engine_compare_truth(&left, &right, symbol)
            }
            None => Err(error),
        }
    })
}

fn engine_compare_truth(left: &Value, right: &Value, symbol: &str) -> SqlResult<Option<bool>> {
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

pub(crate) fn arithmetic(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
) -> SqlResult<Value> {
    if operator == BinaryOperator::Subtract
        && matches!(left, Value::Timestamp(_) | Value::TimestampTz(_))
        && matches!(right, Value::Timestamp(_) | Value::TimestampTz(_))
        && crate::constraints::current_dialect() == crate::Dialect::Oracle
        && let Some(days) = crate::oracle_conversion::arithmetic(operator, left, right)
    {
        return days;
    }
    engine_arithmetic(operator, left, right).or_else(|error| {
        if error.state() != SqlState::UNDEFINED_FUNCTION {
            return Err(error);
        }
        match crate::constraints::current_dialect() {
            crate::Dialect::SqlServer => sqlserver_arithmetic(operator, left, right),
            crate::Dialect::Oracle => {
                crate::oracle_conversion::arithmetic(operator, left, right).unwrap_or(Err(error))
            }
            _ => Err(error),
        }
    })
}

fn isnull_fallback(fallback: Value, sql_type: SqlType) -> SqlResult<Value> {
    let converted = crate::sqlserver_conversion::implicit(fallback, sql_type)?;
    let (Value::Text(text), SqlType::Varchar(Some(length)) | SqlType::Bpchar(Some(length))) =
        (&converted, sql_type)
    else {
        return Ok(converted);
    };
    Ok(match text.char_indices().nth(length as usize) {
        Some((cut, _)) => Value::Text(text[..cut].to_owned()),
        None => converted,
    })
}

fn operator_name(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Add => "add",
        BinaryOperator::Subtract => "subtract",
        BinaryOperator::Multiply => "multiply",
        BinaryOperator::Divide => "divide",
        BinaryOperator::Modulo => "modulo",
        _ => operator.symbol(),
    }
}

fn sqlserver_concatenated(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    match (left, right) {
        (Value::Text(left) | Value::Bpchar(left), Value::Text(right) | Value::Bpchar(right)) => {
            let joined = (|| {
                crate::sqlserver_functions::within_limit(left.len().saturating_add(right.len()))?;
                let mut joined = String::with_capacity(left.len() + right.len());
                joined.push_str(left);
                joined.push_str(right);
                Ok(Value::Text(joined))
            })();
            Some(joined)
        }
        (Value::Bytea(left), Value::Bytea(right)) => Some((|| {
            crate::sqlserver_functions::within_limit(left.len().saturating_add(right.len()))?;
            let mut joined = Vec::with_capacity(left.len() + right.len());
            joined.extend_from_slice(left);
            joined.extend_from_slice(right);
            Ok(Value::Bytea(joined))
        })()),
        _ => None,
    }
}

fn days_added(operator: BinaryOperator, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (stamp, days, sign) = match (operator, left, right) {
        (BinaryOperator::Add, Value::Timestamp(_) | Value::TimestampTz(_), number)
            if number.sql_type().is_some_and(SqlType::is_numeric) =>
        {
            (left, number, 1.0)
        }
        (BinaryOperator::Subtract, Value::Timestamp(_) | Value::TimestampTz(_), number)
            if number.sql_type().is_some_and(SqlType::is_numeric) =>
        {
            (left, number, -1.0)
        }
        (BinaryOperator::Add, number, Value::Timestamp(_) | Value::TimestampTz(_))
            if number.sql_type().is_some_and(SqlType::is_numeric) =>
        {
            (right, number, 1.0)
        }
        _ => return None,
    };
    let shift = days.as_float().unwrap_or(f64::NAN) * sign * datetime::MICROS_PER_DAY as f64;
    if !shift.is_finite() || shift.abs() > 4e17 {
        return Some(Err(crate::sqlserver_functions::date_add_overflow(
            "datetime",
        )));
    }
    let shifted = |micros: i64| {
        datetime::checked_timestamp(i128::from(micros) + shift.round() as i128)
            .map_err(|_| crate::sqlserver_functions::date_add_overflow("datetime"))
    };
    Some(match stamp {
        Value::Timestamp(micros) => shifted(*micros).map(Value::Timestamp),
        Value::TimestampTz(micros) => shifted(*micros).map(Value::TimestampTz),
        _ => return None,
    })
}

fn sqlserver_arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    if operator == BinaryOperator::Add
        && let Some(joined) = sqlserver_concatenated(left, right)
    {
        return joined;
    }
    if let Some(shifted) = days_added(operator, left, right) {
        return shifted;
    }
    let incompatible =
        || crate::sqlserver_types::incompatible_in_operator(left, right, operator_name(operator));
    match crate::sqlserver_conversion::by_precedence(left, right) {
        Some(converted) => {
            let (left, right) = converted?;
            if operator == BinaryOperator::Add
                && let Some(joined) = sqlserver_concatenated(&left, &right)
            {
                return joined;
            }
            engine_arithmetic(operator, &left, &right).map_err(|error| {
                if error.state() == SqlState::UNDEFINED_FUNCTION {
                    incompatible()
                } else {
                    error
                }
            })
        }
        None => Err(incompatible()),
    }
}

fn engine_arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> SqlResult<Value> {
    let symbol = operator.symbol();
    let (Some(left_type), Some(right_type)) = (left.sql_type(), right.sql_type()) else {
        let other = if left.is_null() { right } else { left };
        if let Some(sql_type) = other.sql_type()
            && !sql_type.is_numeric()
            && !crate::temporal::is_time_or_interval(sql_type)
        {
            return Err(no_operator(symbol, left, right));
        }
        return Ok(Value::Null);
    };
    let temporal =
        |sql_type| crate::temporal::is_time_or_interval(sql_type) || SqlType::is_datetime(sql_type);
    if temporal(left_type) || temporal(right_type) {
        if let Some(result) = crate::temporal::arithmetic(operator, left, right) {
            return result;
        }
        if !left_type.is_datetime() && !right_type.is_datetime() {
            return Err(no_operator(symbol, left, right));
        }
        return datetime_arithmetic(operator, left, right);
    }
    if !left_type.is_numeric() || !right_type.is_numeric() {
        return Err(no_operator(symbol, left, right));
    }
    let exact = |value: &Value| matches!(value, Value::Numeric(_));
    if (exact(left) || exact(right)) && !left_type.is_float() && !right_type.is_float() {
        let nan = Numeric::nan;
        let a = left.as_numeric().unwrap_or_else(nan);
        let b = right.as_numeric().unwrap_or_else(nan);
        return match operator {
            BinaryOperator::Add => a.add(&b),
            BinaryOperator::Subtract => a.subtract(&b),
            BinaryOperator::Multiply => a.multiply(&b),
            BinaryOperator::Divide => a.divide(&b),
            BinaryOperator::Modulo => a.remainder(&b),
            _ => a.power(&b),
        }
        .map(Value::Numeric);
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

pub(crate) fn like(text: &str, pattern: &str) -> SqlResult<bool> {
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
