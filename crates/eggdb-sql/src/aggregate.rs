use crate::ast::{AggregateFunction, Expr, ExprKind, Literal, Select, SelectItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, check_columns, first_aggregate, output_name};
use crate::hash_index::SeededIntegerHash;
use crate::memory::WorkMemory;
use crate::numeric::Numeric;
use crate::types::{SqlType, Value, compare_values, equality_key, float_overflow};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub(crate) type RowFeed<'f> =
    dyn FnMut(&mut dyn FnMut(&[Value]) -> SqlResult<bool>) -> SqlResult<()> + 'f;

enum GroupColumn {
    Carried(usize),
    Aggregate(usize),
}

struct Call {
    function: AggregateFunction,
    argument: Option<Expr>,
    distinct: bool,
    position: usize,
    shape: Expr,
}

pub(crate) struct Grouping {
    keys: Vec<Expr>,
    group_columns: Vec<GroupColumn>,
    calls: Vec<Call>,
    pub(crate) items: Vec<SelectItem>,
    pub(crate) having: Option<Expr>,
    pub(crate) order: Vec<Option<Expr>>,
}

pub(crate) struct OutputItem {
    pub(crate) expr: Expr,
    pub(crate) name: String,
}

fn grouping_error(message: impl Into<String>, position: usize) -> SqlError {
    SqlError::new(SqlState::GROUPING_ERROR, message).at(position)
}

pub(crate) fn rebuild(
    expr: &Expr,
    child: &mut dyn FnMut(&Expr) -> SqlResult<Expr>,
) -> SqlResult<Expr> {
    let kind = match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => expr.kind.clone(),
        ExprKind::Subquery(subquery) => {
            let mut rebuilt = subquery.clone();
            if let Some(operand) = rebuilt.test.operand_mut() {
                *operand = child(operand)?;
            }
            ExprKind::Subquery(rebuilt)
        }
        ExprKind::Unary(operator, operand) => ExprKind::Unary(*operator, Box::new(child(operand)?)),
        ExprKind::And(operands) => {
            ExprKind::And(operands.iter().map(&mut *child).collect::<SqlResult<_>>()?)
        }
        ExprKind::Or(operands) => {
            ExprKind::Or(operands.iter().map(&mut *child).collect::<SqlResult<_>>()?)
        }
        ExprKind::Binary(operator, left, right) => {
            ExprKind::Binary(*operator, Box::new(child(left)?), Box::new(child(right)?))
        }
        ExprKind::IsNull { expr, negated } => ExprKind::IsNull {
            expr: Box::new(child(expr)?),
            negated: *negated,
        },
        ExprKind::IsTruth {
            expr,
            test,
            negated,
        } => ExprKind::IsTruth {
            expr: Box::new(child(expr)?),
            test: *test,
            negated: *negated,
        },
        ExprKind::IsDistinctFrom {
            left,
            right,
            negated,
        } => ExprKind::IsDistinctFrom {
            left: Box::new(child(left)?),
            right: Box::new(child(right)?),
            negated: *negated,
        },
        ExprKind::Between {
            expr,
            low,
            high,
            negated,
        } => ExprKind::Between {
            expr: Box::new(child(expr)?),
            low: Box::new(child(low)?),
            high: Box::new(child(high)?),
            negated: *negated,
        },
        ExprKind::InList {
            expr,
            list,
            negated,
        } => ExprKind::InList {
            expr: Box::new(child(expr)?),
            list: list.iter().map(&mut *child).collect::<SqlResult<_>>()?,
            negated: *negated,
        },
        ExprKind::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => ExprKind::Like {
            expr: Box::new(child(expr)?),
            pattern: Box::new(child(pattern)?),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        ExprKind::Cast { expr, sql_type } => ExprKind::Cast {
            expr: Box::new(child(expr)?),
            sql_type: *sql_type,
        },
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => ExprKind::Case {
            operand: operand
                .as_deref()
                .map(|inner| child(inner).map(Box::new))
                .transpose()?,
            branches: branches
                .iter()
                .map(|(condition, result)| Ok((child(condition)?, child(result)?)))
                .collect::<SqlResult<_>>()?,
            otherwise: otherwise
                .as_deref()
                .map(|inner| child(inner).map(Box::new))
                .transpose()?,
        },
        ExprKind::Function { name, arguments } => ExprKind::Function {
            name: name.clone(),
            arguments: arguments
                .iter()
                .map(&mut *child)
                .collect::<SqlResult<_>>()?,
        },
        ExprKind::SqliteFunction { name, arguments } => ExprKind::SqliteFunction {
            name,
            arguments: arguments
                .iter()
                .map(&mut *child)
                .collect::<SqlResult<_>>()?,
        },
        ExprKind::OracleFunction { name, arguments } => ExprKind::OracleFunction {
            name,
            arguments: arguments
                .iter()
                .map(&mut *child)
                .collect::<SqlResult<_>>()?,
        },
        ExprKind::Aggregate {
            function,
            argument,
            distinct,
        } => ExprKind::Aggregate {
            function: *function,
            argument: argument
                .as_deref()
                .map(|inner| child(inner).map(Box::new))
                .transpose()?,
            distinct: *distinct,
        },
    };
    Ok(Expr {
        kind,
        position: expr.position,
    })
}

pub(crate) fn bind(expr: &Expr, scope: Scope<'_>) -> SqlResult<Expr> {
    match &expr.kind {
        ExprKind::Column { qualifier, name } => {
            let offset = scope.column(qualifier.as_deref(), name, expr.position)?;
            Ok(Expr {
                kind: ExprKind::Slot {
                    index: offset,
                    sql_type: scope.column_type(offset),
                },
                position: expr.position,
            })
        }
        _ => rebuild(expr, &mut |child| bind(child, scope)),
    }
}

pub(crate) fn shape_of(expr: &Expr) -> Expr {
    let mut stripped = rebuild(expr, &mut |child| Ok(shape_of(child))).unwrap_or(Expr {
        kind: ExprKind::Literal(Literal::Null),
        position: 0,
    });
    stripped.position = 0;
    stripped
}

fn static_type(expr: &Expr, scope: Scope<'_>) -> Option<SqlType> {
    match &expr.kind {
        ExprKind::Slot { sql_type, .. } => *sql_type,
        ExprKind::Column { qualifier, name } => scope
            .column(qualifier.as_deref(), name, expr.position)
            .ok()
            .and_then(|offset| scope.column_type(offset)),
        ExprKind::Cast { sql_type, .. } => Some(*sql_type),
        _ => None,
    }
}

fn result_type(function: AggregateFunction, argument: Option<SqlType>) -> Option<SqlType> {
    match function {
        AggregateFunction::Count => Some(SqlType::BigInt),
        AggregateFunction::Avg => match argument? {
            SqlType::Real | SqlType::Double => Some(SqlType::Double),
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt | SqlType::Numeric(_) => {
                Some(SqlType::Numeric(None))
            }
            _ => None,
        },
        AggregateFunction::Sum => match argument? {
            SqlType::SmallInt | SqlType::Integer => Some(SqlType::BigInt),
            SqlType::BigInt | SqlType::Numeric(_) => Some(SqlType::Numeric(None)),
            SqlType::Real => Some(SqlType::Real),
            SqlType::Double => Some(SqlType::Double),
            _ => None,
        },
        AggregateFunction::Min | AggregateFunction::Max => match argument? {
            SqlType::Varchar(_) => Some(SqlType::Text),
            other => Some(other.without_modifier()),
        },
    }
}

fn undefined_aggregate(function: AggregateFunction, type_name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {}({type_name}) does not exist", function.name()),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
}

fn check_argument_type(function: AggregateFunction, sql_type: SqlType) -> SqlResult<()> {
    let name = || sql_type.name();
    match function {
        AggregateFunction::Count => Ok(()),
        AggregateFunction::Min | AggregateFunction::Max => {
            if sql_type == SqlType::Boolean || sql_type == SqlType::Json {
                Err(undefined_aggregate(function, &name()))
            } else {
                Ok(())
            }
        }
        AggregateFunction::Sum | AggregateFunction::Avg if !sql_type.is_numeric() => {
            Err(undefined_aggregate(function, &name()))
        }
        _ => Ok(()),
    }
}

pub(crate) fn is_aggregate_query(select: &Select) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select.items.iter().any(|item| match item {
            SelectItem::Expr { expr, .. } => first_aggregate(expr).is_some(),
            SelectItem::Wildcard { .. } => false,
        })
        || select
            .order_by
            .iter()
            .any(|item| first_aggregate(&item.expr).is_some())
}

pub(crate) fn output_items(items: &[SelectItem], scope: Scope<'_>) -> SqlResult<Vec<OutputItem>> {
    let mut outputs = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::Wildcard {
                qualifier,
                position,
            } => {
                if matches!(scope, Scope::Empty) {
                    return Err(SqlError::syntax(
                        "SELECT * with no tables specified is not valid",
                        *position,
                    ));
                }
                let offsets: Vec<usize> = match qualifier {
                    None => scope.star(),
                    Some(qualifier) => {
                        let source = scope
                            .sources()
                            .iter()
                            .find(|source| source.label == qualifier)
                            .ok_or_else(|| {
                                SqlError::new(
                                    SqlState::UNDEFINED_TABLE,
                                    format!("missing FROM-clause entry for table \"{qualifier}\""),
                                )
                                .at(*position)
                            })?;
                        (source.offset..source.offset + source.table.columns.len()).collect()
                    }
                };
                for offset in offsets {
                    if let Some((source, index)) = scope.source_of(offset) {
                        let name = source.table.columns[index].name.clone();
                        outputs.push(OutputItem {
                            expr: Expr {
                                kind: ExprKind::Column {
                                    qualifier: Some(source.label.to_owned()),
                                    name: name.clone(),
                                },
                                position: *position,
                            },
                            name,
                        });
                    }
                }
            }
            SelectItem::Expr { expr, alias } => outputs.push(OutputItem {
                expr: expr.clone(),
                name: alias.clone().unwrap_or_else(|| output_name(expr)),
            }),
        }
    }
    Ok(outputs)
}

struct Analyzer<'a> {
    scope: Scope<'a>,
    key_shapes: Vec<Expr>,
    key_offsets: Vec<Option<usize>>,
    key_types: Vec<Option<SqlType>>,
    dependent: Vec<std::ops::Range<usize>>,
    group_columns: Vec<GroupColumn>,
    calls: Vec<Call>,
}

impl Analyzer<'_> {
    fn group_column_slot(&mut self, column: GroupColumn) -> usize {
        let found = self
            .group_columns
            .iter()
            .position(|existing| match (existing, &column) {
                (GroupColumn::Carried(a), GroupColumn::Carried(b))
                | (GroupColumn::Aggregate(a), GroupColumn::Aggregate(b)) => a == b,
                _ => false,
            });
        let index = found.unwrap_or_else(|| {
            self.group_columns.push(column);
            self.group_columns.len() - 1
        });
        self.key_shapes.len() + index
    }

    fn rewrite(&mut self, expr: &Expr) -> SqlResult<Expr> {
        let shape = shape_of(expr);
        let bound_shape = bind(expr, self.scope).ok().map(|bound| shape_of(&bound));
        if let Some(index) = self
            .key_shapes
            .iter()
            .position(|key| Some(key) == bound_shape.as_ref())
        {
            return Ok(Expr {
                kind: ExprKind::Slot {
                    index,
                    sql_type: self.key_types[index],
                },
                position: expr.position,
            });
        }
        match &expr.kind {
            ExprKind::Aggregate {
                function,
                argument,
                distinct,
            } => {
                if let Some(argument) = argument {
                    if let Some(inner) = first_aggregate(argument) {
                        return Err(grouping_error(
                            "aggregate function calls cannot be nested",
                            inner.position,
                        ));
                    }
                    check_columns(argument, self.scope)?;
                    if let Some(sql_type) = static_type(argument, self.scope) {
                        check_argument_type(*function, sql_type)
                            .map_err(|error| error.at(expr.position))?;
                    }
                }
                let call = match self.calls.iter().position(|call| call.shape == shape) {
                    Some(index) => index,
                    None => {
                        self.calls.push(Call {
                            function: *function,
                            argument: argument
                                .as_deref()
                                .map(|argument| bind(argument, self.scope))
                                .transpose()?,
                            distinct: *distinct,
                            position: expr.position,
                            shape,
                        });
                        self.calls.len() - 1
                    }
                };
                let argument_type = argument
                    .as_deref()
                    .and_then(|argument| static_type(argument, self.scope));
                Ok(Expr {
                    kind: ExprKind::Slot {
                        index: self.group_column_slot(GroupColumn::Aggregate(call)),
                        sql_type: result_type(*function, argument_type),
                    },
                    position: expr.position,
                })
            }
            ExprKind::Column { qualifier, name } => {
                let offset = self
                    .scope
                    .column(qualifier.as_deref(), name, expr.position)?;
                if let Some(index) = self.key_offsets.iter().position(|key| *key == Some(offset)) {
                    return Ok(Expr {
                        kind: ExprKind::Slot {
                            index,
                            sql_type: self.key_types[index],
                        },
                        position: expr.position,
                    });
                }
                if self.dependent.iter().any(|range| range.contains(&offset)) {
                    return Ok(Expr {
                        kind: ExprKind::Slot {
                            index: self.group_column_slot(GroupColumn::Carried(offset)),
                            sql_type: self.scope.column_type(offset),
                        },
                        position: expr.position,
                    });
                }
                let shown = match self.scope.source_of(offset) {
                    Some((source, index)) => {
                        format!("{}.{}", source.label, source.table.columns[index].name)
                    }
                    None => name.clone(),
                };
                Err(grouping_error(
                    format!(
                        "column \"{shown}\" must appear in the GROUP BY clause or be used in an aggregate function"
                    ),
                    expr.position,
                ))
            }
            _ => rebuild(expr, &mut |child| self.rewrite(child)),
        }
    }
}

fn is_ordinal(expr: &Expr) -> Option<i64> {
    match expr.kind {
        ExprKind::Literal(Literal::Integer(position)) => Some(position),
        _ => None,
    }
}

fn group_key(key: &Expr, outputs: &[OutputItem], scope: Scope<'_>) -> SqlResult<Expr> {
    let no_aggregates = |expr: &Expr| match first_aggregate(expr) {
        Some(found) => Err(grouping_error(
            "aggregate functions are not allowed in GROUP BY",
            found.position,
        )),
        None => Ok(()),
    };
    if let Some(position) = is_ordinal(key) {
        let output = usize::try_from(position)
            .ok()
            .filter(|&index| index >= 1 && index <= outputs.len())
            .map(|index| &outputs[index - 1])
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!("GROUP BY position {position} is not in select list"),
                )
                .at(key.position)
            })?;
        no_aggregates(&output.expr)?;
        return Ok(output.expr.clone());
    }
    no_aggregates(key)?;
    if let ExprKind::Column {
        qualifier: None,
        name,
    } = &key.kind
        && let Err(error) = scope.column(None, name, key.position)
        && error.state() == SqlState::UNDEFINED_COLUMN
    {
        let mut named = outputs.iter().filter(|output| &output.name == name);
        if let Some(output) = named.next() {
            if named.any(|other| shape_of(&other.expr) != shape_of(&output.expr)) {
                return Err(SqlError::new(
                    SqlState::AMBIGUOUS_COLUMN,
                    format!("GROUP BY \"{name}\" is ambiguous"),
                )
                .at(key.position));
            }
            no_aggregates(&output.expr)?;
            return Ok(output.expr.clone());
        }
    }
    check_columns(key, scope)?;
    Ok(key.clone())
}

pub(crate) fn analyze(select: &Select, scope: Scope<'_>) -> SqlResult<Grouping> {
    let outputs = output_items(&select.items, scope)?;
    let keys: Vec<Expr> = select
        .group_by
        .iter()
        .map(|key| group_key(key, &outputs, scope))
        .collect::<SqlResult<_>>()?;
    let key_offsets: Vec<Option<usize>> = keys
        .iter()
        .map(|key| match &key.kind {
            ExprKind::Column { qualifier, name } => {
                scope.column(qualifier.as_deref(), name, key.position).ok()
            }
            _ => None,
        })
        .collect();
    let dependent = scope
        .sources()
        .iter()
        .filter(|source| {
            !source.table.primary_key.is_empty()
                && source
                    .table
                    .primary_key
                    .iter()
                    .all(|&index| key_offsets.contains(&Some(source.offset + index)))
        })
        .map(|source| source.offset..source.offset + source.table.columns.len())
        .collect();
    let mut analyzer = Analyzer {
        scope,
        key_shapes: keys
            .iter()
            .map(|key| bind(key, scope).map(|bound| shape_of(&bound)))
            .collect::<SqlResult<_>>()?,
        key_types: key_offsets
            .iter()
            .map(|offset| offset.and_then(|offset| scope.column_type(offset)))
            .collect(),
        key_offsets,
        dependent,
        group_columns: Vec::new(),
        calls: Vec::new(),
    };
    let mut items = Vec::with_capacity(outputs.len());
    for output in &outputs {
        items.push(SelectItem::Expr {
            expr: analyzer.rewrite(&output.expr)?,
            alias: Some(output.name.clone()),
        });
    }
    let having = match &select.having {
        Some(having) => Some(analyzer.rewrite(having)?),
        None => None,
    };
    let mut order = Vec::with_capacity(select.order_by.len());
    for item in &select.order_by {
        let refers_to_output = is_ordinal(&item.expr).is_some()
            || matches!(&item.expr.kind, ExprKind::Column { qualifier: None, name }
                if outputs.iter().any(|output| &output.name == name));
        order.push(if refers_to_output {
            None
        } else {
            Some(analyzer.rewrite(&item.expr)?)
        });
    }
    for call in &analyzer.calls {
        if let Some(argument) = &call.argument
            && let Some(sql_type) = static_type(argument, scope)
        {
            check_argument_type(call.function, sql_type)
                .map_err(|error| error.at(call.position))?;
        }
    }
    let keys = keys
        .iter()
        .map(|key| bind(key, scope))
        .collect::<SqlResult<_>>()?;
    Ok(Grouping {
        keys,
        group_columns: analyzer.group_columns,
        calls: analyzer.calls,
        items,
        having,
        order,
    })
}

enum Sum {
    Empty,
    Integer { total: i128, wide: bool },
    Real(f32),
    Double(f64),
    Numeric(Numeric),
}

enum Accumulator {
    Count(i64),
    Sum(Sum),
    Average { sum: Sum, count: i64 },
    Extreme(Value),
}

struct Group {
    keys: Vec<Value>,
    carried: Vec<Value>,
    accumulators: Vec<Accumulator>,
    seen: Vec<HashSet<Vec<u8>>>,
    held: crate::memory::RowBytes,
}

impl Accumulator {
    fn held_bytes(&self) -> usize {
        match self {
            Self::Extreme(value) => crate::memory::held_bytes(value),
            _ => 0,
        }
    }
}

fn add_float(total: f64, value: f64) -> SqlResult<f64> {
    let sum = total + value;
    if sum.is_infinite() && total.is_finite() && value.is_finite() {
        return Err(float_overflow());
    }
    Ok(sum)
}

impl Call {
    fn start(&self) -> Accumulator {
        match self.function {
            AggregateFunction::Count => Accumulator::Count(0),
            AggregateFunction::Sum => Accumulator::Sum(Sum::Empty),
            AggregateFunction::Avg => Accumulator::Average {
                sum: Sum::Empty,
                count: 0,
            },
            AggregateFunction::Min | AggregateFunction::Max => Accumulator::Extreme(Value::Null),
        }
    }

    fn add(&self, accumulator: &mut Accumulator, value: Value) -> SqlResult<()> {
        let always_valid = matches!(
            (self.function, &value),
            (AggregateFunction::Count, _)
                | (
                    AggregateFunction::Sum | AggregateFunction::Avg,
                    Value::SmallInt(_)
                        | Value::Integer(_)
                        | Value::BigInt(_)
                        | Value::Real(_)
                        | Value::Double(_)
                        | Value::Numeric(_)
                )
                | (
                    AggregateFunction::Min | AggregateFunction::Max,
                    Value::SmallInt(_)
                        | Value::Integer(_)
                        | Value::BigInt(_)
                        | Value::Real(_)
                        | Value::Double(_)
                        | Value::Text(_)
                )
        );
        if !always_valid && let Some(sql_type) = value.sql_type() {
            check_argument_type(self.function, sql_type)?;
        }
        match accumulator {
            Accumulator::Count(count) => *count += 1,
            Accumulator::Sum(sum) => add_to_sum(sum, &value)?,
            Accumulator::Average { sum, count } => {
                add_to_sum(sum, &value)?;
                *count += 1;
            }
            Accumulator::Extreme(best) => {
                let replace = best.is_null() || {
                    let ordering = compare_values(&value, best);
                    match self.function {
                        AggregateFunction::Min => ordering == Ordering::Less,
                        _ => ordering == Ordering::Greater,
                    }
                };
                if replace {
                    *best = value;
                }
            }
        }
        Ok(())
    }
}

fn integer_group_key(value: &Value) -> Option<i64> {
    match value {
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value.as_integer(),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN);
            (number.fract() == 0.0 && number.abs() < 9.0e18).then_some(number as i64)
        }
        _ => None,
    }
}

fn add_to_sum(sum: &mut Sum, value: &Value) -> SqlResult<()> {
    if let Value::Numeric(number) = value {
        *sum = match &*sum {
            Sum::Empty => Sum::Numeric(number.clone()),
            Sum::Numeric(total) => Sum::Numeric(total.add(number)?),
            Sum::Integer { total, .. } => Sum::Numeric(Numeric::from_i128(*total).add(number)?),
            Sum::Real(total) => Sum::Double(add_float(f64::from(*total), number.to_f64())?),
            Sum::Double(total) => Sum::Double(add_float(*total, number.to_f64())?),
        };
        return Ok(());
    }
    if let Sum::Numeric(total) = &*sum {
        *sum = match value.as_integer() {
            Some(whole) => Sum::Numeric(total.add(&Numeric::from_i64(whole))?),
            None => Sum::Double(add_float(
                total.to_f64(),
                value.as_float().unwrap_or(f64::NAN),
            )?),
        };
        return Ok(());
    }
    let wide_input = matches!(value, Value::BigInt(_));
    let float = || value.as_float().unwrap_or(f64::NAN);
    *sum = match (&*sum, value.as_integer()) {
        (Sum::Empty, Some(number)) => Sum::Integer {
            total: i128::from(number),
            wide: wide_input,
        },
        (Sum::Integer { total, wide }, Some(number)) => Sum::Integer {
            total: total.saturating_add(i128::from(number)),
            wide: *wide || wide_input,
        },
        (Sum::Empty, None) => match value {
            Value::Real(number) => Sum::Real(*number),
            _ => Sum::Double(float()),
        },
        (Sum::Real(total), None) if matches!(value, Value::Real(_)) => {
            let next = total + value.as_float().unwrap_or(f64::NAN) as f32;
            if next.is_infinite() && total.is_finite() && float().is_finite() {
                return Err(float_overflow());
            }
            Sum::Real(next)
        }
        (Sum::Real(total), _) => Sum::Double(add_float(f64::from(*total), float())?),
        (Sum::Integer { total, .. }, None) => Sum::Double(add_float(*total as f64, float())?),
        (Sum::Double(total), _) => Sum::Double(add_float(*total, float())?),
        (Sum::Numeric(total), _) => Sum::Numeric(total.clone()),
    };
    Ok(())
}

impl Call {
    fn finish(&self, accumulator: Accumulator) -> SqlResult<Value> {
        Ok(match accumulator {
            Accumulator::Count(count) => Value::BigInt(count),
            Accumulator::Sum(Sum::Empty) => Value::Null,
            Accumulator::Sum(Sum::Integer { total, wide: true }) => {
                Value::Numeric(Numeric::from_i128(total))
            }
            Accumulator::Sum(Sum::Integer { total, wide: false }) => match i64::try_from(total) {
                Ok(total) => Value::BigInt(total),
                Err(_) => {
                    return Err(SqlError::new(
                        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                        "bigint out of range",
                    ));
                }
            },
            Accumulator::Sum(Sum::Real(total)) => Value::Real(total),
            Accumulator::Sum(Sum::Double(total)) => Value::Double(total),
            Accumulator::Sum(Sum::Numeric(total)) => Value::Numeric(total),
            Accumulator::Average { count: 0, .. } => Value::Null,
            Accumulator::Average { sum, count } => {
                let count_value = Numeric::from_i64(count);
                match sum {
                    Sum::Empty => Value::Null,
                    Sum::Integer { total, .. } => {
                        Value::Numeric(Numeric::from_i128(total).divide(&count_value)?)
                    }
                    Sum::Numeric(total) => Value::Numeric(total.divide(&count_value)?),
                    Sum::Real(total) => Value::Double(f64::from(total) / count as f64),
                    Sum::Double(total) => Value::Double(total / count as f64),
                }
            }
            Accumulator::Extreme(best) => best,
        })
    }
}

impl Grouping {
    pub(crate) fn run(
        &self,
        scope: Scope<'_>,
        feed: &mut RowFeed<'_>,
        memory: &mut WorkMemory,
    ) -> SqlResult<Vec<Vec<Value>>> {
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut integer_index: HashMap<i64, usize, SeededIntegerHash> = HashMap::default();
        let mut groups: Vec<Group> = Vec::new();
        let mut key = Vec::new();
        let mut key_values = Vec::with_capacity(self.keys.len());
        let mut distinct_key = Vec::new();
        let carried: Vec<usize> = self
            .group_columns
            .iter()
            .filter_map(|column| match column {
                GroupColumn::Carried(offset) => Some(*offset),
                GroupColumn::Aggregate(_) => None,
            })
            .collect();
        let new_group = |keys: Vec<Value>, row: &[Value]| Group {
            keys,
            carried: carried
                .iter()
                .map(|&offset| row.get(offset).cloned().unwrap_or(Value::Null))
                .collect(),
            accumulators: self.calls.iter().map(Call::start).collect(),
            seen: self.calls.iter().map(|_| HashSet::new()).collect(),
            held: crate::memory::RowBytes::default(),
        };
        feed(&mut |row| {
            let evaluator = Evaluator {
                scope,
                row,
                correlation: None,
            };
            key.clear();
            key_values.clear();
            let mut integer_key = None;
            let mut key_bytes = crate::memory::RowBytes::default();
            if let [expr] = self.keys.as_slice() {
                let value = evaluator.eval(expr, None)?;
                key_bytes.add(&value)?;
                integer_key = integer_group_key(&value);
                if integer_key.is_none() {
                    equality_key(&mut key, &value);
                    key_bytes.add_bytes(key.len())?;
                }
                key_values.push(value);
            } else {
                for expr in &self.keys {
                    let value = evaluator.eval(expr, None)?;
                    key_bytes.add(&value)?;
                    let before = key.len();
                    equality_key(&mut key, &value);
                    key_bytes.add_bytes(key.len() - before)?;
                    key_values.push(value);
                }
            }
            let found = if self.keys.is_empty() && !groups.is_empty() {
                Some(&0)
            } else if let Some(integer_key) = integer_key {
                integer_index.get(&integer_key)
            } else {
                index.get(key.as_slice())
            };
            let at = match found {
                Some(&at) => at,
                None => {
                    memory.charge_row(&key_values)?;
                    memory.charge(key.len() + self.calls.len() * 64)?;
                    let mut group = new_group(std::mem::take(&mut key_values), row);
                    for value in &group.carried {
                        key_bytes.add(value)?;
                    }
                    memory.charge_row(&group.carried)?;
                    group.held = key_bytes;
                    groups.push(group);
                    match integer_key {
                        Some(integer_key) => integer_index.insert(integer_key, groups.len() - 1),
                        None => index.insert(key.clone(), groups.len() - 1),
                    };
                    groups.len() - 1
                }
            };
            let group = &mut groups[at];
            for (number, call) in self.calls.iter().enumerate() {
                let Some(argument) = &call.argument else {
                    if let Accumulator::Count(count) = &mut group.accumulators[number] {
                        *count += 1;
                    }
                    continue;
                };
                let value = evaluator.eval(argument, None)?;
                if value.is_null() {
                    continue;
                }
                if call.distinct {
                    distinct_key.clear();
                    equality_key(&mut distinct_key, &value);
                    if group.seen[number].contains(&distinct_key) {
                        continue;
                    }
                    memory.charge(distinct_key.len() + 32)?;
                    group.seen[number].insert(distinct_key.clone());
                }
                let before = group.accumulators[number].held_bytes();
                call.add(&mut group.accumulators[number], value)
                    .map_err(|error| error.at_if_unplaced(call.position))?;
                let grown = group.accumulators[number]
                    .held_bytes()
                    .saturating_sub(before);
                if grown > 0 {
                    group.held.add_bytes(grown)?;
                    memory.charge(grown)?;
                }
            }
            Ok(true)
        })?;
        if groups.is_empty() && self.keys.is_empty() {
            groups.push(new_group(Vec::new(), &[]));
        }
        let mut rows = Vec::with_capacity(groups.len());
        for group in groups {
            let mut accumulators: Vec<Option<Accumulator>> =
                group.accumulators.into_iter().map(Some).collect();
            let mut carried = group.carried.into_iter();
            let mut row = group.keys;
            for column in &self.group_columns {
                row.push(match column {
                    GroupColumn::Carried(_) => carried.next().unwrap_or(Value::Null),
                    GroupColumn::Aggregate(call) => match accumulators[*call].take() {
                        Some(accumulator) => self.calls[*call]
                            .finish(accumulator)
                            .map_err(|error| error.at_if_unplaced(self.calls[*call].position))?,
                        None => Value::Null,
                    },
                });
            }
            if let Some(having) = &self.having {
                let evaluator = Evaluator {
                    scope: Scope::empty(),
                    row: &row,
                    correlation: None,
                };
                if evaluator.boolean(having, "HAVING")? != Some(true) {
                    continue;
                }
            }
            rows.push(row);
        }
        Ok(rows)
    }
}
