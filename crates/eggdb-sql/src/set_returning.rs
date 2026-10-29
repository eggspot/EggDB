use crate::ast::{
    ExpandedItem, Expr, ExprKind, FromClause, GeneratedTable, JoinCondition, Literal, Name, Query,
    Select, SelectItem, SelectListExpansion, TableFunction, TableReference, ValuesList,
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
    fields: bool,
}

const FUNCTIONS: [SetReturningFunction; 16] = [
    SetReturningFunction {
        name: "generate_series",
        result_type: series_type,
        rows: series_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "unnest",
        result_type: unnest_type,
        rows: unnest_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "generate_subscripts",
        result_type: subscripts_type,
        rows: subscripts_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "pg_partition_ancestors",
        result_type: crate::pg_catalog::partition_ancestors_type,
        rows: crate::pg_catalog::partition_ancestors_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "regexp_matches",
        result_type: regexp_matches_type,
        rows: regexp_matches_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "regexp_split_to_table",
        result_type: regexp_split_type,
        rows: regexp_split_rows,
        fields: false,
    },
    json_records("json_each", json_record_type, json_each_rows),
    json_records("jsonb_each", jsonb_record_type, jsonb_each_rows),
    json_records("json_each_text", json_record_type, json_each_text_rows),
    json_records("jsonb_each_text", jsonb_record_type, jsonb_each_text_rows),
    json_records("json_array_elements", json_type, json_array_elements_rows),
    json_records(
        "jsonb_array_elements",
        jsonb_type,
        jsonb_array_elements_rows,
    ),
    json_records(
        "json_array_elements_text",
        json_text_type,
        json_array_elements_text_rows,
    ),
    json_records(
        "jsonb_array_elements_text",
        jsonb_text_type,
        jsonb_array_elements_text_rows,
    ),
    SetReturningFunction {
        name: "json_object_keys",
        result_type: json_text_type,
        rows: json_object_keys_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "jsonb_object_keys",
        result_type: jsonb_text_type,
        rows: jsonb_object_keys_rows,
        fields: false,
    },
];

const fn json_records(
    name: &'static str,
    result_type: fn(&[Option<SqlType>]) -> SqlResult<SqlType>,
    rows: fn(&[Value], SqlType, &mut RowSink<'_>) -> SqlResult<()>,
) -> SetReturningFunction {
    SetReturningFunction {
        name,
        result_type,
        rows,
        fields: true,
    }
}

fn one_document_argument(
    name: &str,
    types: &[Option<SqlType>],
    accepted: SqlType,
) -> SqlResult<()> {
    match types {
        [None] => Ok(()),
        [Some(sql_type)] if *sql_type == accepted || sql_type.is_character() => Ok(()),
        _ => Err(does_not_exist(name, types)),
    }
}

fn json_record_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("json_each", types, SqlType::Json).map(|()| SqlType::Text)
}

fn jsonb_record_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("jsonb_each", types, SqlType::Jsonb).map(|()| SqlType::Text)
}

fn json_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("json_array_elements", types, SqlType::Json).map(|()| SqlType::Json)
}

fn jsonb_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("jsonb_array_elements", types, SqlType::Jsonb).map(|()| SqlType::Jsonb)
}

fn json_text_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("json_object_keys", types, SqlType::Json).map(|()| SqlType::Text)
}

fn jsonb_text_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    one_document_argument("jsonb_object_keys", types, SqlType::Jsonb).map(|()| SqlType::Text)
}

fn json_rows_of(name: &str, values: &[Value], sink: &mut RowSink<'_>) -> SqlResult<()> {
    crate::json_functions::record_rows(name, values, &mut |record| {
        sink(crate::json_functions::record_text(&record))
    })
}

fn json_each_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("json_each", values, sink)
}

fn jsonb_each_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("jsonb_each", values, sink)
}

fn json_each_text_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("json_each_text", values, sink)
}

fn jsonb_each_text_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("jsonb_each_text", values, sink)
}

fn json_array_elements_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("json_array_elements", values, sink)
}

fn jsonb_array_elements_rows(
    values: &[Value],
    _: SqlType,
    sink: &mut RowSink<'_>,
) -> SqlResult<()> {
    json_rows_of("jsonb_array_elements", values, sink)
}

fn json_array_elements_text_rows(
    values: &[Value],
    _: SqlType,
    sink: &mut RowSink<'_>,
) -> SqlResult<()> {
    json_rows_of("json_array_elements_text", values, sink)
}

fn jsonb_array_elements_text_rows(
    values: &[Value],
    _: SqlType,
    sink: &mut RowSink<'_>,
) -> SqlResult<()> {
    json_rows_of("jsonb_array_elements_text", values, sink)
}

fn json_object_keys_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("json_object_keys", values, sink)
}

fn jsonb_object_keys_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    json_rows_of("jsonb_object_keys", values, sink)
}

fn text_arguments(name: &str, types: &[Option<SqlType>]) -> SqlResult<()> {
    let texts = types
        .iter()
        .all(|sql_type| sql_type.is_none_or(SqlType::is_character));
    if (2..=3).contains(&types.len()) && texts {
        Ok(())
    } else {
        Err(does_not_exist(name, types))
    }
}

fn regexp_matches_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    text_arguments("regexp_matches", types)
        .map(|()| SqlType::Array(crate::arrays::ArrayElement::Text))
}

fn regexp_split_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    text_arguments("regexp_split_to_table", types).map(|()| SqlType::Text)
}

fn regexp_matches_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    crate::regexp_functions::matches_rows(values, sink)
}

fn regexp_split_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    crate::regexp_functions::split_rows(values, sink)
}

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

fn subscripts_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [Some(SqlType::Array(_)), _] | [Some(SqlType::Array(_)), _, _] => Ok(SqlType::Integer),
        [None, _] | [None, _, _] => Err(not_unique("generate_subscripts", types)),
        _ => Err(does_not_exist("generate_subscripts", types)),
    }
}

fn subscripts_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    let (Some(Value::Array(array)), Some(dimension)) = (values.first(), values.get(1)) else {
        return Ok(());
    };
    if dimension.as_integer() != Some(1) {
        return Ok(());
    }
    let reverse = matches!(values.get(2), Some(Value::Boolean(true)));
    let lower = i64::from(array.lower_bound);
    let count = array.items.len() as i64;
    let mut emit = |offset: i64| match i32::try_from(lower + offset) {
        Ok(subscript) => sink(Value::Integer(subscript)),
        Err(_) => Ok(()),
    };
    if reverse {
        for offset in (0..count).rev() {
            emit(offset)?;
        }
    } else {
        for offset in 0..count {
            emit(offset)?;
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

pub(crate) fn select_mentions_window(select: &Select) -> bool {
    let mentions = |expr: &Expr| crate::window::first_call(expr).is_some();
    select.items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => mentions(expr),
        SelectItem::Wildcard { .. } => false,
    }) || select.filter.iter().any(mentions)
        || select.group_by.iter().any(mentions)
        || select.having.iter().any(mentions)
        || select.order_by.iter().any(|item| mentions(&item.expr))
}

pub(crate) fn select_needs_expansion(select: &Select) -> bool {
    select_mentions_set_returning(select) || select_mentions_window(select)
}

fn refuse_window(expr: &Expr, place: &str) -> SqlResult<()> {
    match crate::window::first_call(expr) {
        Some(found) => Err(SqlError::not_supported(format!(
            "a window function {place} is not supported yet"
        ))
        .with_hint("Call it alone as a select-list item.")
        .at(found.position)),
        None => Ok(()),
    }
}

pub(crate) fn check_placement(select: &Select) -> SqlResult<()> {
    let mut calls = 0;
    let mut windows = 0;
    for item in &select.items {
        let SelectItem::Expr { expr, .. } = item else {
            continue;
        };
        if is_top_level_call(expr) {
            calls += 1;
            for argument in children(expr) {
                refuse_nested(argument)?;
                refuse_window(argument, "inside an argument of a set-returning function")?;
            }
        } else if crate::window::is_call(expr) {
            windows += 1;
            for key in children(expr) {
                refuse_nested(key)?;
                refuse_window(key, "inside a window definition")?;
            }
        } else {
            refuse_nested(expr)?;
            refuse_window(expr, "inside an expression")?;
        }
        if calls > 0 && windows > 0 {
            return Err(SqlError::not_supported(
                "a window function beside a set-returning function in one select list is not supported yet",
            )
            .at(expr.position));
        }
    }
    if let Some(filter) = &select.filter {
        refuse_in(filter, "WHERE")?;
        refuse_window(filter, "in WHERE")?;
    }
    for key in &select.group_by {
        refuse_in(key, "GROUP BY")?;
        refuse_window(key, "in GROUP BY")?;
    }
    if let Some(having) = &select.having {
        refuse_in(having, "HAVING")?;
        refuse_window(having, "in HAVING")?;
    }
    for item in &select.order_by {
        refuse_in(&item.expr, "ORDER BY")?;
        refuse_window(&item.expr, "in ORDER BY")?;
        if calls + windows > 0
            && let Some(aggregate) = first_aggregate(&item.expr)
        {
            return Err(SqlError::not_supported(
                "an aggregate in the ORDER BY of a select list with a set-returning or window function is not supported yet",
            )
            .with_hint("Name the aggregate in the select list and order by its output column.")
            .at(aggregate.position));
        }
    }
    for expr in select.limit.iter().chain(select.offset.iter()) {
        refuse_in(expr, "LIMIT")?;
        refuse_window(expr, "in LIMIT")?;
    }
    if let Some(from) = &select.from {
        for join in &from.joins {
            if let JoinCondition::On(on) = &join.condition {
                refuse_in(on, "JOIN conditions")?;
                refuse_window(on, "in JOIN conditions")?;
            }
        }
    }
    Ok(())
}

fn computed_item(item: &SelectItem) -> Option<usize> {
    match item {
        SelectItem::Expr { expr, .. }
            if is_top_level_call(expr) || crate::window::is_call(expr) =>
        {
            Some(expr.position)
        }
        _ => None,
    }
}

pub(crate) fn expand_select_list(select: &mut Select) -> SqlResult<()> {
    let Some(position) = select.items.iter().find_map(computed_item) else {
        return Ok(());
    };
    let mut inner_items: Vec<SelectItem> = Vec::with_capacity(select.items.len());
    let mut items: Vec<ExpandedItem> = Vec::new();
    let mut label: Option<String> = None;
    let mut run: Option<Option<usize>> = None;
    let mut unknown_runs = 0;
    let mut close_run = |run: &mut Option<Option<usize>>, items: &mut Vec<ExpandedItem>| {
        if let Some(width) = run.take() {
            if width.is_none() {
                unknown_runs += 1;
            }
            items.push(ExpandedItem::Columns(width));
        }
    };
    for item in std::mem::take(&mut select.items) {
        match item {
            SelectItem::Expr { expr, alias } if is_top_level_call(&expr) => {
                close_run(&mut run, &mut items);
                let at = expr.position;
                let ExprKind::Function { name, arguments } = expr.kind else {
                    continue;
                };
                let output_name = alias.unwrap_or_else(|| name.clone());
                label.get_or_insert_with(|| output_name.clone());
                items.push(ExpandedItem::SetReturning {
                    function: name,
                    arguments: arguments.len(),
                    output_name,
                    position: at,
                });
                inner_items.extend(arguments.into_iter().map(|argument| SelectItem::Expr {
                    expr: argument,
                    alias: None,
                }));
            }
            SelectItem::Expr { expr, alias } if crate::window::is_call(&expr) => {
                close_run(&mut run, &mut items);
                let at = expr.position;
                let Some((spec, keys)) = crate::window::split(expr) else {
                    return Err(SqlError::new(
                        SqlState::INTERNAL_ERROR,
                        "a window call lost its definition",
                    )
                    .at(at));
                };
                let output_name = alias.unwrap_or_else(|| spec.function.name().to_owned());
                label.get_or_insert_with(|| output_name.clone());
                items.push(ExpandedItem::Window {
                    keys: keys.len(),
                    spec,
                    output_name,
                });
                inner_items.extend(keys.into_iter().map(|key| SelectItem::Expr {
                    expr: key,
                    alias: None,
                }));
            }
            other => {
                let width = match &other {
                    SelectItem::Wildcard { .. } => None,
                    SelectItem::Expr { .. } => Some(1),
                };
                run = Some(match (run.take(), width) {
                    (None, width) => width,
                    (Some(Some(known)), Some(more)) => Some(known + more),
                    _ => None,
                });
                inner_items.push(other);
            }
        }
    }
    close_run(&mut run, &mut items);
    if unknown_runs > 1 {
        return Err(SqlError::not_supported(
            "a set-returning or window function between two * items in a select list is not supported yet",
        )
        .at(position));
    }
    let inner = Select {
        distinct: false,
        items: inner_items,
        from: select.from.take(),
        filter: select.filter.take(),
        group_by: std::mem::take(&mut select.group_by),
        having: select.having.take(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: select.source_row_limit.take(),
    };
    select.items = vec![SelectItem::Wildcard {
        qualifier: None,
        position,
    }];
    select.from = Some(FromClause {
        first: TableReference {
            name: Name {
                text: label.unwrap_or_default(),
                position,
            },
            alias: None,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Expansion(Box::new(
                SelectListExpansion {
                    query: Query::of_select(inner, position),
                    items,
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
    pub(crate) charge: WorkMemory,
}

impl GeneratedRows {
    pub(crate) fn empty(columns: Vec<OutputColumn>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            charge: WorkMemory::new(),
        }
    }
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

    fn finish(self) -> (Vec<Vec<Value>>, WorkMemory) {
        (self.rows, self.memory)
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
    found: &SetReturningFunction,
    label: &str,
    alias_given: bool,
    sql_type: SqlType,
) -> Vec<OutputColumn> {
    let mut columns: Vec<OutputColumn> = if found.fields {
        crate::json_functions::record_names(found.name)
            .iter()
            .zip(crate::json_functions::record_types(found.name))
            .map(|(name, sql_type)| OutputColumn {
                name: (*name).to_owned(),
                sql_type: Some(sql_type),
            })
            .collect()
    } else {
        vec![OutputColumn {
            name: if alias_given {
                label.to_owned()
            } else {
                function.name.clone()
            },
            sql_type: Some(sql_type),
        }]
    };
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
    Ok(function_columns(
        function,
        found,
        label,
        alias_given,
        sql_type,
    ))
}

pub(crate) struct BoundFunction {
    found: &'static SetReturningFunction,
    values: Vec<Value>,
    sql_type: SqlType,
    ordinality: bool,
    position: usize,
}

pub(crate) fn bind_function(
    function: &TableFunction,
    evaluator: &Evaluator<'_>,
) -> SqlResult<BoundFunction> {
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
    Ok(BoundFunction {
        found,
        values,
        sql_type,
        ordinality: function.ordinality,
        position: function.position,
    })
}

impl BoundFunction {
    pub(crate) fn columns(
        &self,
        function: &TableFunction,
        label: &str,
        alias_given: bool,
    ) -> Vec<OutputColumn> {
        function_columns(function, self.found, label, alias_given, self.sql_type)
    }

    pub(crate) fn generate(
        &self,
        store: &mut dyn FnMut(Vec<Value>) -> SqlResult<()>,
    ) -> SqlResult<()> {
        let mut ordinal: i64 = 0;
        if self.found.fields {
            return crate::json_functions::record_rows(
                self.found.name,
                &self.values,
                &mut |mut record| {
                    ordinal += 1;
                    if self.ordinality {
                        record.push(Value::BigInt(ordinal));
                    }
                    store(record)
                },
            )
            .map_err(|error| error.at_if_unplaced(self.position));
        }
        (self.found.rows)(&self.values, self.sql_type, &mut |value| {
            ordinal += 1;
            let value = conformed(value, self.sql_type)?;
            store(if self.ordinality {
                vec![value, Value::BigInt(ordinal)]
            } else {
                vec![value]
            })
        })
        .map_err(|error| error.at_if_unplaced(self.position))
    }
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
    let (mut rows, charge) = collector.finish();
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
    Ok(GeneratedRows {
        columns,
        rows,
        charge,
    })
}

struct ResolvedFunction {
    found: &'static SetReturningFunction,
    declared: Vec<Option<SqlType>>,
    sql_type: Option<SqlType>,
    position: usize,
}

impl ResolvedFunction {
    fn output_type(&mut self, arguments: &[Value]) -> SqlResult<SqlType> {
        if let Some(found) = self.sql_type {
            return Ok(found);
        }
        let resolved = (self.found.result_type)(&value_types(arguments, &self.declared))
            .map_err(|error| error.at_if_unplaced(self.position))?;
        self.sql_type = Some(resolved);
        Ok(resolved)
    }

    fn values(&mut self, arguments: &[Value], pending: &mut WorkMemory) -> SqlResult<Vec<Value>> {
        let output_type = self.output_type(arguments)?;
        let arguments: Vec<Value> = arguments
            .iter()
            .map(|value| {
                if self.found.name == "generate_series" {
                    conformed(value.clone(), output_type)
                } else {
                    Ok(value.clone())
                }
            })
            .collect::<SqlResult<_>>()?;
        let mut values = Vec::new();
        (self.found.rows)(&arguments, output_type, &mut |value| {
            let value = conformed(value, output_type)?;
            pending.charge(crate::memory::held_bytes(&value).saturating_add(16))?;
            values.push(value);
            Ok(())
        })
        .map_err(|error| error.at_if_unplaced(self.position))?;
        Ok(values)
    }
}

fn item_spans(items: &[ExpandedItem], total: usize) -> Vec<std::ops::Range<usize>> {
    let known: usize = items
        .iter()
        .map(|item| match item {
            ExpandedItem::Columns(width) => width.unwrap_or(0),
            ExpandedItem::SetReturning { arguments, .. } => *arguments,
            ExpandedItem::Window { keys, .. } => *keys,
        })
        .sum();
    let unknown = total.saturating_sub(known);
    let mut spans = Vec::with_capacity(items.len());
    let mut at = 0;
    for item in items {
        let width = match item {
            ExpandedItem::Columns(width) => width.unwrap_or(unknown),
            ExpandedItem::SetReturning { arguments, .. } => *arguments,
            ExpandedItem::Window { keys, .. } => *keys,
        };
        let end = at.saturating_add(width).min(total);
        spans.push(at..end);
        at = end;
    }
    spans
}

pub(crate) fn expanded(
    expansion: &SelectListExpansion,
    inner: QueryResult,
) -> SqlResult<GeneratedRows> {
    let inner_columns = inner.columns.unwrap_or_default();
    let spans = item_spans(&expansion.items, inner_columns.len());
    let mut functions: Vec<Option<ResolvedFunction>> = Vec::with_capacity(expansion.items.len());
    let mut windows: Vec<Option<Vec<i64>>> = Vec::with_capacity(expansion.items.len());
    for (item, span) in expansion.items.iter().zip(&spans) {
        match item {
            ExpandedItem::SetReturning {
                function, position, ..
            } => {
                let found = named(function).ok_or_else(|| {
                    SqlError::not_supported(format!(
                        "the set-returning function {function}() is not supported yet"
                    ))
                })?;
                let declared: Vec<Option<SqlType>> = inner_columns
                    .get(span.clone())
                    .unwrap_or(&[])
                    .iter()
                    .map(|column| column.sql_type)
                    .collect();
                let sql_type = if declared.iter().all(Option::is_some) {
                    Some(
                        (found.result_type)(&declared)
                            .map_err(|error| error.at_if_unplaced(*position))?,
                    )
                } else {
                    None
                };
                functions.push(Some(ResolvedFunction {
                    found,
                    declared,
                    sql_type,
                    position: *position,
                }));
                windows.push(None);
            }
            ExpandedItem::Window { spec, .. } => {
                let keys: Vec<Vec<Value>> = inner
                    .rows
                    .iter()
                    .map(|row| row.get(span.clone()).unwrap_or(&[]).to_vec())
                    .collect();
                functions.push(None);
                windows.push(Some(crate::window::numbers(spec, &keys)));
            }
            ExpandedItem::Columns(_) => {
                functions.push(None);
                windows.push(None);
            }
        }
    }
    let sets_rows = functions.iter().any(Option::is_some);
    let mut collector = RowCollector::new();
    for (row_at, row) in inner.rows.iter().enumerate() {
        if sets_rows {
            let mut pending = WorkMemory::new();
            let mut produced: Vec<Option<Vec<Value>>> = Vec::with_capacity(functions.len());
            let mut longest = 0;
            for (function, span) in functions.iter_mut().zip(&spans) {
                let Some(function) = function else {
                    produced.push(None);
                    continue;
                };
                let values = function.values(row.get(span.clone()).unwrap_or(&[]), &mut pending)?;
                longest = longest.max(values.len());
                produced.push(Some(values));
            }
            for index in 0..longest {
                let mut expanded = Vec::with_capacity(row.len());
                for (values, span) in produced.iter().zip(&spans) {
                    match values {
                        Some(values) => {
                            expanded.push(values.get(index).cloned().unwrap_or(Value::Null));
                        }
                        None => expanded.extend_from_slice(row.get(span.clone()).unwrap_or(&[])),
                    }
                }
                collector.push(expanded)?;
            }
        } else {
            let mut expanded = Vec::with_capacity(row.len());
            for (numbers, span) in windows.iter().zip(&spans) {
                match numbers {
                    Some(numbers) => expanded.push(Value::BigInt(
                        numbers.get(row_at).copied().unwrap_or_default(),
                    )),
                    None => expanded.extend_from_slice(row.get(span.clone()).unwrap_or(&[])),
                }
            }
            collector.push(expanded)?;
        }
    }
    let mut columns: Vec<OutputColumn> = Vec::with_capacity(inner_columns.len());
    for ((item, span), function) in expansion.items.iter().zip(&spans).zip(&functions) {
        match item {
            ExpandedItem::Columns(_) => columns.extend(
                inner_columns
                    .get(span.clone())
                    .unwrap_or(&[])
                    .iter()
                    .cloned(),
            ),
            ExpandedItem::SetReturning { output_name, .. } => columns.push(OutputColumn {
                name: output_name.clone(),
                sql_type: Some(
                    function
                        .as_ref()
                        .and_then(|function| function.sql_type)
                        .unwrap_or(SqlType::Integer),
                ),
            }),
            ExpandedItem::Window { output_name, .. } => columns.push(OutputColumn {
                name: output_name.clone(),
                sql_type: Some(SqlType::BigInt),
            }),
        }
    }
    let (rows, charge) = collector.finish();
    Ok(GeneratedRows {
        columns,
        rows,
        charge,
    })
}

pub(crate) fn expressions(generated: &GeneratedTable) -> Vec<&Expr> {
    match generated {
        GeneratedTable::Values(list) => list.rows.iter().flatten().collect(),
        GeneratedTable::Function(function) => function.arguments.iter().collect(),
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) => Vec::new(),
    }
}

pub(crate) fn expressions_mut(generated: &mut GeneratedTable) -> Vec<&mut Expr> {
    match generated {
        GeneratedTable::Values(list) => list.rows.iter_mut().flatten().collect(),
        GeneratedTable::Function(function) => function.arguments.iter_mut().collect(),
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) => Vec::new(),
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

pub(crate) fn generate_rows(
    generated: &GeneratedTable,
    evaluator: &Evaluator<'_>,
    store: &mut dyn FnMut(Vec<Value>) -> SqlResult<()>,
) -> SqlResult<()> {
    match generated {
        GeneratedTable::Values(list) => values_rows(list, evaluator)?
            .rows
            .into_iter()
            .try_for_each(store),
        GeneratedTable::Function(function) => bind_function(function, evaluator)?.generate(store),
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) => Ok(()),
    }
}
