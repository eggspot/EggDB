use crate::ast::{
    Expr, ExprKind, FromClause, GeneratedTable, JoinCondition, Literal, Name, Query, Select,
    SelectItem, SelectListExpansion, TableFunction, TableReference, ValuesList,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, children, first_aggregate};
use crate::memory::{RowBytes, WorkMemory};
use crate::session::{OutputColumn, QueryResult};
use crate::types::{Conversion, SqlType, Value, convert};

type RowSink<'s> = dyn FnMut(Value) -> SqlResult<()> + 's;

pub(crate) struct SetReturningFunction {
    pub(crate) name: &'static str,
    result_type: fn(&[Option<SqlType>]) -> SqlResult<SqlType>,
    rows: fn(&[Value], SqlType, &mut RowSink<'_>) -> SqlResult<()>,
}

const FUNCTIONS: [SetReturningFunction; 2] = [
    SetReturningFunction {
        name: "generate_series",
        result_type: series_type,
        rows: series_rows,
    },
    SetReturningFunction {
        name: "unnest",
        result_type: unnest_type,
        rows: unnest_rows,
    },
];

pub(crate) const VALUES_LABEL: &str = "*VALUES*";

pub(crate) fn named(name: &str) -> Option<&'static SetReturningFunction> {
    FUNCTIONS.iter().find(|function| function.name == name)
}

fn type_list(types: &[Option<SqlType>]) -> String {
    types
        .iter()
        .map(|sql_type| sql_type.map_or_else(|| "unknown".to_owned(), SqlType::name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn does_not_exist(name: &str, types: &[Option<SqlType>]) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}({}) does not exist", type_list(types)),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
}

fn not_unique(name: &str, types: &[Option<SqlType>]) -> SqlError {
    SqlError::new(
        SqlState::AMBIGUOUS_FUNCTION,
        format!("function {name}({}) is not unique", type_list(types)),
    )
    .with_hint(
        "Could not choose a best candidate function. You might need to add explicit type casts.",
    )
}

fn series_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    if !(2..=3).contains(&types.len()) {
        return Err(does_not_exist("generate_series", types));
    }
    let mut widest: Option<SqlType> = None;
    for sql_type in types.iter().flatten() {
        match sql_type {
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
                widest = Some(match (widest, sql_type) {
                    (Some(SqlType::BigInt), _) | (_, SqlType::BigInt) => SqlType::BigInt,
                    (Some(SqlType::Integer), _) | (_, SqlType::Integer) => SqlType::Integer,
                    _ => SqlType::SmallInt,
                });
            }
            SqlType::Numeric(_) | SqlType::Timestamp(_) | SqlType::TimestampTz(_) => {
                return Err(SqlError::not_supported(format!(
                    "generate_series over {} is not supported yet",
                    sql_type.name()
                )));
            }
            _ => return Err(does_not_exist("generate_series", types)),
        }
    }
    match widest {
        Some(SqlType::SmallInt) | None => Err(not_unique("generate_series", types)),
        Some(found) => Ok(found),
    }
}

fn series_rows(values: &[Value], sql_type: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    let number = |value: &Value| value.as_integer().map(i128::from);
    let (start, stop, step) = match values {
        [start, stop] => (number(start), number(stop), Some(1)),
        [start, stop, step] => (number(start), number(stop), number(step)),
        _ => return Ok(()),
    };
    let (Some(start), Some(stop), Some(step)) = (start, stop, step) else {
        return Ok(());
    };
    if step == 0 {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero",
        ));
    }
    let mut current = start;
    while (step > 0 && current <= stop) || (step < 0 && current >= stop) {
        let value = i64::try_from(current).map_err(|_| crate::types::out_of_range(sql_type))?;
        sink(crate::types::integer_value(value, sql_type)?)?;
        current += step;
    }
    Ok(())
}

fn unnest_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [Some(SqlType::Array(element))] => Ok(element.sql_type()),
        [None] => Err(not_unique("unnest", types)),
        _ => Err(does_not_exist("unnest", types)),
    }
}

fn unnest_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    if let [Value::Array(array)] = values {
        for item in &array.items {
            sink(item.clone())?;
        }
    }
    Ok(())
}

fn is_unknown_literal(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Null)
    )
}

fn argument_types(arguments: &[Expr], scope: Scope<'_>) -> Vec<Option<SqlType>> {
    arguments
        .iter()
        .map(|argument| {
            if is_unknown_literal(argument) {
                None
            } else {
                crate::typing::static_type(argument, scope)
            }
        })
        .collect()
}

fn value_types(values: &[Value], declared: &[Option<SqlType>]) -> Vec<Option<SqlType>> {
    values
        .iter()
        .zip(declared)
        .map(|(value, declared)| declared.or_else(|| value.sql_type()))
        .collect()
}

pub(crate) fn mentions_set_returning(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if named(name).is_some())
        || children(expr).into_iter().any(mentions_set_returning)
}

fn first_set_returning(expr: &Expr) -> Option<&Expr> {
    if matches!(&expr.kind, ExprKind::Function { name, .. } if named(name).is_some()) {
        return Some(expr);
    }
    children(expr).into_iter().find_map(first_set_returning)
}

fn is_top_level_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if named(name).is_some())
}

fn refuse_in(expr: &Expr, clause: &str) -> SqlResult<()> {
    match first_set_returning(expr) {
        Some(found) => Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("set-returning functions are not allowed in {clause}"),
        )
        .at(found.position)),
        None => Ok(()),
    }
}

fn refuse_nested(expr: &Expr) -> SqlResult<()> {
    if let Some(aggregate) = first_aggregate(expr)
        && let Some(found) = first_set_returning(aggregate)
    {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "aggregate function calls cannot contain set-returning function calls",
        )
        .with_hint("You might be able to move the set-returning function into a LATERAL FROM item.")
        .at(found.position));
    }
    match first_set_returning(expr) {
        Some(found) => Err(SqlError::not_supported(
            "a set-returning function inside an expression is not supported yet",
        )
        .with_hint("Call it alone as a select-list item, or in FROM.")
        .at(found.position)),
        None => Ok(()),
    }
}

pub(crate) fn select_mentions_set_returning(select: &Select) -> bool {
    select.items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => mentions_set_returning(expr),
        SelectItem::Wildcard { .. } => false,
    }) || select.filter.iter().any(mentions_set_returning)
        || select.group_by.iter().any(mentions_set_returning)
        || select.having.iter().any(mentions_set_returning)
        || select
            .order_by
            .iter()
            .any(|item| mentions_set_returning(&item.expr))
        || select.from.as_ref().is_some_and(|from| {
            from.joins.iter().any(|join| {
                matches!(&join.condition, JoinCondition::On(on) if mentions_set_returning(on))
            })
        })
}

pub(crate) fn check_placement(select: &Select) -> SqlResult<()> {
    let mut calls = 0;
    for item in &select.items {
        let SelectItem::Expr { expr, .. } = item else {
            continue;
        };
        if is_top_level_call(expr) {
            calls += 1;
            if calls > 1 {
                return Err(SqlError::not_supported(
                    "more than one set-returning function in a select list is not supported yet",
                )
                .at(expr.position));
            }
            for argument in children(expr) {
                refuse_nested(argument)?;
            }
        } else {
            refuse_nested(expr)?;
        }
    }
    if let Some(filter) = &select.filter {
        refuse_in(filter, "WHERE")?;
    }
    for key in &select.group_by {
        refuse_in(key, "GROUP BY")?;
    }
    if let Some(having) = &select.having {
        refuse_in(having, "HAVING")?;
    }
    for item in &select.order_by {
        refuse_in(&item.expr, "ORDER BY")?;
        if calls > 0
            && let Some(aggregate) = first_aggregate(&item.expr)
        {
            return Err(SqlError::not_supported(
                "an aggregate in the ORDER BY of a select list with a set-returning function is not supported yet",
            )
            .with_hint("Name the aggregate in the select list and order by its output column.")
            .at(aggregate.position));
        }
    }
    for expr in select.limit.iter().chain(select.offset.iter()) {
        refuse_in(expr, "LIMIT")?;
    }
    if let Some(from) = &select.from {
        for join in &from.joins {
            if let JoinCondition::On(on) = &join.condition {
                refuse_in(on, "JOIN conditions")?;
            }
        }
    }
    Ok(())
}

pub(crate) fn expand_select_list(select: &mut Select) -> SqlResult<()> {
    let Some(at) = select
        .items
        .iter()
        .position(|item| matches!(item, SelectItem::Expr { expr, .. } if is_top_level_call(expr)))
    else {
        return Ok(());
    };
    let SelectItem::Expr { expr, alias } = select.items.remove(at) else {
        return Ok(());
    };
    let position = expr.position;
    let ExprKind::Function { name, arguments } = expr.kind else {
        return Ok(());
    };
    let wildcard = |items: &[SelectItem]| {
        items
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard { .. }))
    };
    let (before, after) = select.items.split_at(at);
    let before_columns = (!wildcard(before)).then_some(before.len());
    let after_columns = (!wildcard(after)).then_some(after.len());
    if before_columns.is_none() && after_columns.is_none() {
        return Err(SqlError::not_supported(
            "a set-returning function between two * items in a select list is not supported yet",
        )
        .at(position));
    }
    let argument_count = arguments.len();
    let mut items: Vec<SelectItem> = before.to_vec();
    items.extend(arguments.into_iter().map(|argument| SelectItem::Expr {
        expr: argument,
        alias: None,
    }));
    items.extend(after.iter().cloned());
    let inner = Select {
        distinct: false,
        items,
        from: select.from.take(),
        filter: select.filter.take(),
        group_by: std::mem::take(&mut select.group_by),
        having: select.having.take(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: select.source_row_limit.take(),
    };
    let output_name = alias.unwrap_or_else(|| name.clone());
    select.items = vec![SelectItem::Wildcard {
        qualifier: None,
        position,
    }];
    select.from = Some(FromClause {
        first: TableReference {
            name: Name {
                text: output_name.clone(),
                position,
            },
            alias: None,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Expansion(Box::new(
                SelectListExpansion {
                    query: Query::of_select(inner, position),
                    function: name,
                    arguments: argument_count,
                    before_columns,
                    after_columns,
                    output_name,
                    position,
                },
            )))),
        },
        joins: Vec::new(),
    });
    Ok(())
}

fn conformed(value: Value, sql_type: SqlType) -> SqlResult<Value> {
    if value.is_null() || value.sql_type() == Some(sql_type) {
        return Ok(value);
    }
    if sql_type.is_string() && matches!(value, Value::Text(_)) {
        return Ok(value);
    }
    convert(value, sql_type, Conversion::Explicit)
}

pub(crate) struct GeneratedRows {
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) rows: Vec<Vec<Value>>,
}

struct RowCollector {
    rows: Vec<Vec<Value>>,
    memory: WorkMemory,
}

impl RowCollector {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            memory: WorkMemory::new(),
        }
    }

    fn push(&mut self, row: Vec<Value>) -> SqlResult<()> {
        let mut held = RowBytes::default();
        for value in &row {
            held.add(value)?;
        }
        self.memory.charge_row(&row)?;
        self.rows.push(row);
        Ok(())
    }

    fn finish(self) -> Vec<Vec<Value>> {
        self.rows
    }
}

fn function_of(function: &TableFunction) -> SqlResult<&'static SetReturningFunction> {
    named(&function.name).ok_or_else(|| {
        SqlError::not_supported(format!(
            "the function {}() in FROM is not supported yet",
            function.name
        ))
        .at(function.position)
    })
}

fn function_columns(
    function: &TableFunction,
    label: &str,
    alias_given: bool,
    sql_type: SqlType,
) -> Vec<OutputColumn> {
    let mut columns = vec![OutputColumn {
        name: if alias_given {
            label.to_owned()
        } else {
            function.name.clone()
        },
        sql_type: Some(sql_type),
    }];
    if function.ordinality {
        columns.push(OutputColumn {
            name: "ordinality".to_owned(),
            sql_type: Some(SqlType::BigInt),
        });
    }
    columns
}

pub(crate) fn table_function_columns(
    function: &TableFunction,
    label: &str,
    alias_given: bool,
    scope: Scope<'_>,
) -> SqlResult<Vec<OutputColumn>> {
    let found = function_of(function)?;
    let types = argument_types(&function.arguments, scope);
    let sql_type =
        (found.result_type)(&types).map_err(|error| error.at_if_unplaced(function.position))?;
    Ok(function_columns(function, label, alias_given, sql_type))
}

pub(crate) fn table_function(
    function: &TableFunction,
    label: &str,
    alias_given: bool,
    evaluator: &Evaluator<'_>,
) -> SqlResult<GeneratedRows> {
    let found = function_of(function)?;
    let declared = argument_types(&function.arguments, evaluator.scope);
    let series_type = (found.name == "generate_series")
        .then(|| (found.result_type)(&declared).ok())
        .flatten();
    let mut values = Vec::with_capacity(function.arguments.len());
    let mut held = RowBytes::default();
    for (index, argument) in function.arguments.iter().enumerate() {
        let hint = series_type.or_else(|| declared.get(index).copied().flatten());
        let value = evaluator.eval(argument, hint)?;
        held.add(&value)?;
        values.push(value);
    }
    let types: Vec<Option<SqlType>> = function
        .arguments
        .iter()
        .zip(&values)
        .zip(&declared)
        .map(|((argument, value), declared)| {
            if is_unknown_literal(argument) {
                None
            } else {
                declared.or_else(|| value.sql_type())
            }
        })
        .collect();
    let sql_type =
        (found.result_type)(&types).map_err(|error| error.at_if_unplaced(function.position))?;
    let mut collector = RowCollector::new();
    let mut ordinal: i64 = 0;
    (found.rows)(&values, sql_type, &mut |value| {
        ordinal += 1;
        let value = conformed(value, sql_type)?;
        let row = if function.ordinality {
            vec![value, Value::BigInt(ordinal)]
        } else {
            vec![value]
        };
        collector.push(row)
    })
    .map_err(|error| error.at_if_unplaced(function.position))?;
    Ok(GeneratedRows {
        columns: function_columns(function, label, alias_given, sql_type),
        rows: collector.finish(),
    })
}

fn values_mismatch(left: SqlType, right: SqlType, position: usize) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "VALUES types {} and {} cannot be matched",
            left.name(),
            right.name()
        ),
    )
    .at(position)
}

fn values_column_types(list: &ValuesList, scope: Scope<'_>) -> SqlResult<Vec<Option<SqlType>>> {
    let width = list.rows.first().map_or(0, Vec::len);
    let mut types = Vec::with_capacity(width);
    for column in 0..width {
        let mut common: Option<SqlType> = None;
        let mut undetermined = false;
        for row in &list.rows {
            let Some(expr) = row.get(column) else {
                continue;
            };
            if is_unknown_literal(expr) {
                continue;
            }
            match crate::typing::static_type(expr, scope) {
                Some(found) => {
                    common = Some(match common {
                        None => found,
                        Some(known) => crate::set_operations::common_type(known, found)
                            .ok_or_else(|| values_mismatch(known, found, expr.position))?,
                    });
                }
                None => undetermined = true,
            }
        }
        types.push(if undetermined {
            None
        } else {
            Some(common.unwrap_or(SqlType::Text))
        });
    }
    Ok(types)
}

pub(crate) fn values_columns(list: &ValuesList, scope: Scope<'_>) -> SqlResult<Vec<OutputColumn>> {
    Ok(values_column_types(list, scope)?
        .into_iter()
        .enumerate()
        .map(|(index, sql_type)| OutputColumn {
            name: format!("column{}", index + 1),
            sql_type: Some(sql_type.unwrap_or(SqlType::Text)),
        })
        .collect())
}

pub(crate) fn values_rows(
    list: &ValuesList,
    evaluator: &Evaluator<'_>,
) -> SqlResult<GeneratedRows> {
    let types = values_column_types(list, evaluator.scope)?;
    let mut collector = RowCollector::new();
    for row in &list.rows {
        let mut values = Vec::with_capacity(row.len());
        for (expr, sql_type) in row.iter().zip(&types) {
            values.push(evaluator.eval(expr, *sql_type)?);
        }
        collector.push(values)?;
    }
    let mut rows = collector.finish();
    let mut columns = Vec::with_capacity(types.len());
    for (index, declared) in types.iter().enumerate() {
        let sql_type = match declared {
            Some(sql_type) => *sql_type,
            None => {
                let mut common: Option<SqlType> = None;
                for row in &rows {
                    if let Some(found) = row.get(index).and_then(Value::sql_type) {
                        common = Some(match common {
                            None => found,
                            Some(known) => crate::set_operations::common_type(known, found)
                                .ok_or_else(|| values_mismatch(known, found, list.position))?,
                        });
                    }
                }
                common.unwrap_or(SqlType::Text)
            }
        };
        columns.push(OutputColumn {
            name: format!("column{}", index + 1),
            sql_type: Some(sql_type),
        });
    }
    for row in &mut rows {
        for (value, column) in row.iter_mut().zip(&columns) {
            if let Some(sql_type) = column.sql_type {
                let original = std::mem::replace(value, Value::Null);
                *value = conformed(original, sql_type)?;
            }
        }
    }
    Ok(GeneratedRows { columns, rows })
}

pub(crate) fn expanded(
    expansion: &SelectListExpansion,
    inner: QueryResult,
) -> SqlResult<GeneratedRows> {
    let inner_columns = inner.columns.unwrap_or_default();
    let total = inner_columns.len();
    let count = expansion.arguments;
    let before = match (expansion.before_columns, expansion.after_columns) {
        (Some(before), _) => before,
        (None, Some(after)) => total.saturating_sub(count).saturating_sub(after),
        (None, None) => 0,
    };
    let after_start = before.saturating_add(count).min(total);
    let found = named(&expansion.function).ok_or_else(|| {
        SqlError::not_supported(format!(
            "the set-returning function {}() is not supported yet",
            expansion.function
        ))
    })?;
    let declared: Vec<Option<SqlType>> = inner_columns
        .get(before..after_start)
        .unwrap_or(&[])
        .iter()
        .map(|column| column.sql_type)
        .collect();
    let resolved = |types: &[Option<SqlType>]| {
        (found.result_type)(types).map_err(|error| error.at_if_unplaced(expansion.position))
    };
    let mut sql_type = if declared.iter().all(Option::is_some) {
        Some(resolved(&declared)?)
    } else {
        None
    };
    let mut collector = RowCollector::new();
    for row in inner.rows {
        let arguments = row.get(before..after_start).unwrap_or(&[]);
        let output_type = match sql_type {
            Some(found) => found,
            None => {
                let found = resolved(&value_types(arguments, &declared))?;
                sql_type = Some(found);
                found
            }
        };
        let arguments: Vec<Value> = arguments
            .iter()
            .map(|value| {
                if found.name == "generate_series" {
                    conformed(value.clone(), output_type)
                } else {
                    Ok(value.clone())
                }
            })
            .collect::<SqlResult<_>>()?;
        let head = row.get(..before).unwrap_or(&[]);
        let tail = row.get(after_start..).unwrap_or(&[]);
        (found.rows)(&arguments, output_type, &mut |value| {
            let mut expanded = Vec::with_capacity(head.len() + 1 + tail.len());
            expanded.extend_from_slice(head);
            expanded.push(conformed(value, output_type)?);
            expanded.extend_from_slice(tail);
            collector.push(expanded)
        })
        .map_err(|error| error.at_if_unplaced(expansion.position))?;
    }
    let mut columns: Vec<OutputColumn> = inner_columns.get(..before).unwrap_or(&[]).to_vec();
    columns.push(OutputColumn {
        name: expansion.output_name.clone(),
        sql_type: Some(sql_type.unwrap_or(SqlType::Integer)),
    });
    columns.extend(
        inner_columns
            .get(after_start..)
            .unwrap_or(&[])
            .iter()
            .cloned(),
    );
    Ok(GeneratedRows {
        columns,
        rows: collector.finish(),
    })
}

pub(crate) fn expressions(generated: &GeneratedTable) -> Vec<&Expr> {
    match generated {
        GeneratedTable::Values(list) => list.rows.iter().flatten().collect(),
        GeneratedTable::Function(function) => function.arguments.iter().collect(),
        GeneratedTable::Expansion(_) => Vec::new(),
    }
}

pub(crate) fn expressions_mut(generated: &mut GeneratedTable) -> Vec<&mut Expr> {
    match generated {
        GeneratedTable::Values(list) => list.rows.iter_mut().flatten().collect(),
        GeneratedTable::Function(function) => function.arguments.iter_mut().collect(),
        GeneratedTable::Expansion(_) => Vec::new(),
    }
}

pub(crate) fn refuse_nested_in_from(generated: &GeneratedTable) -> SqlResult<()> {
    for expr in expressions(generated) {
        if let Some(found) = first_set_returning(expr) {
            return Err(SqlError::not_supported(
                "a set-returning function in the arguments of a FROM item is not supported yet",
            )
            .at(found.position));
        }
    }
    Ok(())
}

pub(crate) fn rows_of(
    generated: &GeneratedTable,
    evaluator: &Evaluator<'_>,
) -> SqlResult<Vec<Vec<Value>>> {
    match generated {
        GeneratedTable::Values(list) => values_rows(list, evaluator).map(|built| built.rows),
        GeneratedTable::Function(function) => {
            table_function(function, &function.name, false, evaluator).map(|built| built.rows)
        }
        GeneratedTable::Expansion(_) => Ok(Vec::new()),
    }
}
