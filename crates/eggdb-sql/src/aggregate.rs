use crate::ast::{AggregateFunction, BinaryOperator, Expr, ExprKind, Literal, Select, SelectItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, check_columns, first_aggregate, output_name};
use crate::memory::WorkMemory;
use crate::types::{SqlType, Value, compare_values, equality_key, float_overflow};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub(crate) type RowFeed<'f> =
    dyn FnMut(&mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()> + 'f;

enum Extra {
    Carried(usize),
    Aggregate(usize),
}

struct Call {
    function: AggregateFunction,
    argument: Option<Expr>,
    distinct: bool,
    exact_bigint_allowed: bool,
    position: usize,
    shape: Expr,
}

pub(crate) struct Grouping {
    keys: Vec<Expr>,
    extras: Vec<Extra>,
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
        ExprKind::Literal(_) | ExprKind::Column { .. } | ExprKind::Slot { .. } => expr.kind.clone(),
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
        ExprKind::Column { qualifier, name } => scope
            .column(qualifier.as_deref(), name, expr.position)
            .ok()
            .and_then(|offset| scope.column_type(offset)),
        ExprKind::Cast { sql_type, .. } => Some(*sql_type),
        _ => None,
    }
}

fn is_integer(sql_type: SqlType) -> bool {
    matches!(
        sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    )
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

fn numeric_result(function: AggregateFunction, type_name: &str) -> SqlError {
    let hint = match function {
        AggregateFunction::Avg => format!(
            "Write avg(expr::double precision) for a double precision average of {type_name} values."
        ),
        _ => "Use sum(expr) on its own as a select item or an ORDER BY item, or as one side of a comparison, or write sum(expr::double precision).".to_owned(),
    };
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!(
            "{}({type_name}) here returns type numeric, which is not supported yet",
            function.name()
        ),
    )
    .with_hint(hint)
}

fn check_argument_type(
    function: AggregateFunction,
    sql_type: SqlType,
    exact_bigint_allowed: bool,
) -> SqlResult<()> {
    let name = || sql_type.name();
    match function {
        AggregateFunction::Count => Ok(()),
        AggregateFunction::Min | AggregateFunction::Max => {
            if sql_type == SqlType::Boolean {
                Err(undefined_aggregate(function, &name()))
            } else {
                Ok(())
            }
        }
        AggregateFunction::Sum | AggregateFunction::Avg if !sql_type.is_numeric() => {
            Err(undefined_aggregate(function, &name()))
        }
        AggregateFunction::Avg if is_integer(sql_type) => Err(numeric_result(function, &name())),
        AggregateFunction::Sum if sql_type == SqlType::BigInt && !exact_bigint_allowed => {
            Err(numeric_result(function, &name()))
        }
        _ => Ok(()),
    }
}

fn keeps_exact_bigint(kind: &ExprKind) -> bool {
    matches!(
        kind,
        ExprKind::Binary(
            BinaryOperator::Equal
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessOrEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterOrEqual,
            ..
        ) | ExprKind::IsNull { .. }
            | ExprKind::IsDistinctFrom { .. }
            | ExprKind::Between { .. }
            | ExprKind::InList { .. }
    )
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
    extras: Vec<Extra>,
    calls: Vec<Call>,
}

impl Analyzer<'_> {
    fn extra_slot(&mut self, extra: Extra) -> usize {
        let found = self
            .extras
            .iter()
            .position(|existing| match (existing, &extra) {
                (Extra::Carried(a), Extra::Carried(b))
                | (Extra::Aggregate(a), Extra::Aggregate(b)) => a == b,
                _ => false,
            });
        let index = found.unwrap_or_else(|| {
            self.extras.push(extra);
            self.extras.len() - 1
        });
        self.key_shapes.len() + index
    }

    fn rewrite(&mut self, expr: &Expr, exact_bigint_allowed: bool) -> SqlResult<Expr> {
        let shape = shape_of(expr);
        if let Some(index) = self.key_shapes.iter().position(|key| *key == shape) {
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
                        check_argument_type(*function, sql_type, true)
                            .map_err(|error| error.at(expr.position))?;
                    }
                }
                let call = match self.calls.iter().position(|call| call.shape == shape) {
                    Some(index) => {
                        self.calls[index].exact_bigint_allowed &= exact_bigint_allowed;
                        index
                    }
                    None => {
                        self.calls.push(Call {
                            function: *function,
                            argument: argument.as_deref().cloned(),
                            distinct: *distinct,
                            exact_bigint_allowed,
                            position: expr.position,
                            shape,
                        });
                        self.calls.len() - 1
                    }
                };
                Ok(Expr {
                    kind: ExprKind::Slot {
                        index: self.extra_slot(Extra::Aggregate(call)),
                        sql_type: None,
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
                            index: self.extra_slot(Extra::Carried(offset)),
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
            kind => {
                let exact = keeps_exact_bigint(kind);
                rebuild(expr, &mut |child| self.rewrite(child, exact))
            }
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
        key_shapes: keys.iter().map(shape_of).collect(),
        key_types: key_offsets
            .iter()
            .map(|offset| offset.and_then(|offset| scope.column_type(offset)))
            .collect(),
        key_offsets,
        dependent,
        extras: Vec::new(),
        calls: Vec::new(),
    };
    let mut items = Vec::with_capacity(outputs.len());
    for output in &outputs {
        items.push(SelectItem::Expr {
            expr: analyzer.rewrite(&output.expr, true)?,
            alias: Some(output.name.clone()),
        });
    }
    let having = match &select.having {
        Some(having) => Some(analyzer.rewrite(having, false)?),
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
            Some(analyzer.rewrite(&item.expr, true)?)
        });
    }
    for call in &analyzer.calls {
        if let Some(argument) = &call.argument
            && let Some(sql_type) = static_type(argument, scope)
        {
            check_argument_type(call.function, sql_type, call.exact_bigint_allowed)
                .map_err(|error| error.at(call.position))?;
        }
    }
    Ok(Grouping {
        keys,
        extras: analyzer.extras,
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
}

enum Accumulator {
    Count(i64),
    Sum(Sum),
    Average { total: f64, count: i64 },
    Extreme(Value),
}

struct Group {
    keys: Vec<Value>,
    carried: Vec<Value>,
    accumulators: Vec<Accumulator>,
    seen: Vec<HashSet<Vec<u8>>>,
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
                total: 0.0,
                count: 0,
            },
            AggregateFunction::Min | AggregateFunction::Max => Accumulator::Extreme(Value::Null),
        }
    }

    fn add(&self, accumulator: &mut Accumulator, value: Value) -> SqlResult<()> {
        if let Some(sql_type) = value.sql_type() {
            check_argument_type(self.function, sql_type, self.exact_bigint_allowed)?;
        }
        match accumulator {
            Accumulator::Count(count) => *count += 1,
            Accumulator::Sum(sum) => add_to_sum(sum, &value)?,
            Accumulator::Average { total, count } => {
                *total = add_float(*total, value.as_float().unwrap_or(f64::NAN))?;
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

fn add_to_sum(sum: &mut Sum, value: &Value) -> SqlResult<()> {
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
    };
    Ok(())
}

impl Call {
    fn finish(&self, accumulator: Accumulator) -> SqlResult<Value> {
        Ok(match accumulator {
            Accumulator::Count(count) => Value::BigInt(count),
            Accumulator::Sum(Sum::Empty) => Value::Null,
            Accumulator::Sum(Sum::Integer { total, wide }) => {
                if wide && !self.exact_bigint_allowed {
                    return Err(numeric_result(self.function, "bigint"));
                }
                match i64::try_from(total) {
                    Ok(total) => Value::BigInt(total),
                    Err(_) if wide => {
                        return Err(numeric_result(self.function, "bigint")
                            .with_detail("The sum does not fit in a bigint."));
                    }
                    Err(_) => {
                        return Err(SqlError::new(
                            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                            "bigint out of range",
                        ));
                    }
                }
            }
            Accumulator::Sum(Sum::Real(total)) => Value::Real(total),
            Accumulator::Sum(Sum::Double(total)) => Value::Double(total),
            Accumulator::Average { count: 0, .. } => Value::Null,
            Accumulator::Average { total, count } => Value::Double(total / count as f64),
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
        let mut groups: Vec<Group> = Vec::new();
        let mut key = Vec::new();
        let mut key_values = Vec::with_capacity(self.keys.len());
        let mut distinct_key = Vec::new();
        let carried: Vec<usize> = self
            .extras
            .iter()
            .filter_map(|extra| match extra {
                Extra::Carried(offset) => Some(*offset),
                Extra::Aggregate(_) => None,
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
        };
        feed(&mut |row| {
            let evaluator = Evaluator { scope, row: &row };
            key.clear();
            key_values.clear();
            for expr in &self.keys {
                let value = evaluator.eval(expr, None)?;
                equality_key(&mut key, &value);
                key_values.push(value);
            }
            let at = match index.get(key.as_slice()) {
                Some(&at) => at,
                None => {
                    memory.charge_row(&key_values)?;
                    memory.charge(key.len() + self.calls.len() * 64)?;
                    groups.push(new_group(key_values.clone(), &row));
                    index.insert(key.clone(), groups.len() - 1);
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
                call.add(&mut group.accumulators[number], value)
                    .map_err(|error| error.at_if_unplaced(call.position))?;
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
            for extra in &self.extras {
                row.push(match extra {
                    Extra::Carried(_) => carried.next().unwrap_or(Value::Null),
                    Extra::Aggregate(call) => match accumulators[*call].take() {
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
