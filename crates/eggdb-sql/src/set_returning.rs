use crate::ast::{
    ExpandedItem, ExpansionKey, Expr, ExprKind, FromClause, GeneratedTable, JoinCondition,
    Literal, Name, OrderItem, Query, Select, SelectItem, SelectListExpansion, TableFunction,
    TableReference, ValuesList,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, children, first_aggregate};
use crate::memory::{RowBytes, WorkMemory};
use crate::session::{OutputColumn, QueryResult};
use crate::types::{Conversion, SqlType, Value, convert};

pub(crate) type RowSink<'s> = dyn FnMut(Value) -> SqlResult<()> + 's;

pub(crate) struct SetReturningFunction {
    pub(crate) name: &'static str,
    result_type: fn(&[Option<SqlType>]) -> SqlResult<SqlType>,
    rows: fn(&[Value], SqlType, &mut RowSink<'_>) -> SqlResult<()>,
    fields: bool,
}

const FUNCTIONS: [SetReturningFunction; 20] = [
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
    SetReturningFunction {
        name: OPENJSON_FUNCTION,
        result_type: openjson_type,
        rows: openjson_rows,
        fields: false,
    },
    SetReturningFunction {
        name: STRING_SPLIT_FUNCTION,
        result_type: string_split_type,
        rows: string_split_rows,
        fields: false,
    },
    SetReturningFunction {
        name: "eggdb_foreign",
        result_type: crate::foreign::foreign_type,
        rows: crate::foreign::foreign_rows,
        fields: false,
    },
    SetReturningFunction {
        name: crate::records::EXPAND_ARRAY_FUNCTION,
        result_type: crate::records::expanded_array_type,
        rows: crate::records::expanded_array_rows,
        fields: false,
    },
];

pub(crate) const OPENJSON_FUNCTION: &str = "*openjson*";
pub(crate) const STRING_SPLIT_FUNCTION: &str = "*string_split*";

fn string_split_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [_, _] => Ok(SqlType::Text),
        _ => Err(does_not_exist("STRING_SPLIT", types)),
    }
}

fn string_split_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    crate::sqlserver_strings::split_rows(values, sink)
}

fn openjson_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [None] => Ok(SqlType::Json),
        [Some(sql_type)] if sql_type.is_character() || *sql_type == SqlType::Json => {
            Ok(SqlType::Json)
        }
        _ => Err(does_not_exist("OPENJSON", types)),
    }
}

fn openjson_rows(values: &[Value], _: SqlType, sink: &mut RowSink<'_>) -> SqlResult<()> {
    crate::json_functions::openjson_rows(values, &mut |record| {
        sink(crate::json_functions::record_text(&record))
    })
}

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

fn selected_call(expr: &Expr) -> Option<(&Expr, Option<&str>)> {
    if is_top_level_call(expr) {
        return Some((expr, None));
    }
    if let ExprKind::Function { name, arguments } = &expr.kind
        && name == crate::records::FIELD_FUNCTION
        && let [call, field] = arguments.as_slice()
        && is_top_level_call(call)
        && let ExprKind::Literal(Literal::String(field)) = &field.kind
    {
        return Some((call, Some(field)));
    }
    None
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
        .with_hint("Compute it in a derived table and use its column here.")
        .at(found.position)),
        None => Ok(()),
    }
}

fn forbid_window(expr: &Expr, clause: &str) -> SqlResult<()> {
    match crate::window::first_call(expr) {
        Some(found) => Err(SqlError::new(
            SqlState::WINDOWING_ERROR,
            format!("window functions are not allowed in {clause}"),
        )
        .at(found.position)),
        None => Ok(()),
    }
}

fn check_windows(expr: &Expr) -> SqlResult<()> {
    if crate::window::is_call(expr) {
        crate::window::check_call(expr)?;
        return children(expr).into_iter().try_for_each(refuse_nested);
    }
    if is_top_level_call(expr) {
        return refuse_nested(expr);
    }
    if let ExprKind::Aggregate {
        argument: Some(argument),
        ..
    } = &expr.kind
        && let Some(found) = crate::window::first_call(argument)
    {
        return Err(SqlError::new(
            SqlState::GROUPING_ERROR,
            "aggregate function calls cannot contain window function calls",
        )
        .at(found.position));
    }
    children(expr).into_iter().try_for_each(check_windows)
}

pub(crate) fn check_placement(select: &Select) -> SqlResult<()> {
    let windows = select_mentions_window(select);
    let mut calls = 0;
    for item in &select.items {
        let SelectItem::Expr { expr, .. } = item else {
            continue;
        };
        if let Some((call, _)) = selected_call(expr) {
            calls += 1;
            for argument in children(call) {
                refuse_nested(argument)?;
                refuse_window(argument, "inside an argument of a set-returning function")?;
            }
        } else if crate::window::first_call(expr).is_some() {
            check_windows(expr)?;
        } else {
            refuse_nested(expr)?;
        }
    }
    if let Some(filter) = &select.filter {
        refuse_in(filter, "WHERE")?;
        forbid_window(filter, "WHERE")?;
    }
    for key in &select.group_by {
        refuse_in(key, "GROUP BY")?;
        forbid_window(key, "GROUP BY")?;
    }
    if let Some(having) = &select.having {
        refuse_in(having, "HAVING")?;
        forbid_window(having, "HAVING")?;
    }
    for item in &select.order_by {
        refuse_in(&item.expr, "ORDER BY")?;
        if windows {
            check_windows(&item.expr)?;
        } else if calls > 0
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
        forbid_window(expr, "LIMIT")?;
    }
    if let Some(from) = &select.from {
        for join in &from.joins {
            if let JoinCondition::On(on) = &join.condition {
                refuse_in(on, "JOIN conditions")?;
                forbid_window(on, "JOIN conditions")?;
            }
        }
    }
    Ok(())
}

fn computed_item(item: &SelectItem) -> Option<usize> {
    match item {
        SelectItem::Expr { expr, .. } if selected_call(expr).is_some() => Some(expr.position),
        SelectItem::Expr { expr, .. } => crate::window::first_call(expr).map(|call| call.position),
        SelectItem::Wildcard { .. } => None,
    }
}

const WINDOW_SLOTS: usize = usize::MAX / 2;

fn lost_window(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "a window call lost its definition",
    )
    .at(position)
}

#[derive(Default)]
struct Decomposition {
    inputs: Vec<Expr>,
    windows: Vec<(crate::window::WindowSpec, Vec<Expr>)>,
}

fn decompose(expr: &Expr, parts: &mut Decomposition) -> SqlResult<Expr> {
    if crate::window::is_call(expr) {
        let at = expr.position;
        let (spec, keys) = crate::window::split(expr.clone()).ok_or_else(|| lost_window(at))?;
        parts.windows.push((spec, keys));
        return Ok(Expr {
            kind: ExprKind::Slot {
                index: WINDOW_SLOTS + parts.windows.len() - 1,
                sql_type: None,
            },
            position: at,
        });
    }
    if crate::window::first_call(expr).is_none() {
        if matches!(
            expr.kind,
            ExprKind::Literal(_) | ExprKind::Constant { .. } | ExprKind::SessionValue { .. }
        ) {
            return Ok(expr.clone());
        }
        parts.inputs.push(expr.clone());
        return Ok(Expr {
            kind: ExprKind::Slot {
                index: parts.inputs.len() - 1,
                sql_type: None,
            },
            position: expr.position,
        });
    }
    crate::aggregate::rebuild(expr, &mut |child| decompose(child, parts))
}

fn renumbered(expr: &Expr, inputs: usize) -> SqlResult<Expr> {
    if let ExprKind::Slot { index, sql_type } = &expr.kind
        && *index >= WINDOW_SLOTS
    {
        return Ok(Expr {
            kind: ExprKind::Slot {
                index: inputs + (*index - WINDOW_SLOTS),
                sql_type: *sql_type,
            },
            position: expr.position,
        });
    }
    crate::aggregate::rebuild(expr, &mut |child| renumbered(child, inputs))
}

fn typed_slots(expr: &Expr, types: &[Option<SqlType>]) -> SqlResult<Expr> {
    if let ExprKind::Slot { index, .. } = &expr.kind {
        return Ok(Expr {
            kind: ExprKind::Slot {
                index: *index,
                sql_type: types.get(*index).copied().flatten(),
            },
            position: expr.position,
        });
    }
    crate::aggregate::rebuild(expr, &mut |child| typed_slots(child, types))
}

#[derive(Default)]
struct ExpansionBuilder {
    inner_items: Vec<SelectItem>,
    items: Vec<ExpandedItem>,
    label: Option<String>,
    run: Option<Option<usize>>,
    unknown_runs: usize,
    names: Vec<(String, usize, usize)>,
}

impl ExpansionBuilder {
    fn close_run(&mut self) {
        if let Some(width) = self.run.take() {
            if width.is_none() {
                self.unknown_runs += 1;
            }
            self.items.push(ExpandedItem::Columns(width));
        }
    }

    fn name_output(&mut self, name: &str) {
        self.label.get_or_insert_with(|| name.to_owned());
        self.names.push((name.to_owned(), self.items.len(), 0));
    }

    fn push_window(&mut self, expr: Expr, output_name: String) -> SqlResult<()> {
        let at = expr.position;
        let (spec, keys) = crate::window::split(expr).ok_or_else(|| lost_window(at))?;
        self.items.push(ExpandedItem::Window {
            keys: keys.len(),
            spec,
            output_name,
        });
        self.inner_items
            .extend(keys.into_iter().map(|key| SelectItem::Expr {
                expr: key,
                alias: None,
            }));
        Ok(())
    }

    fn push_computed(&mut self, expr: &Expr, output_name: String) -> SqlResult<()> {
        let mut parts = Decomposition::default();
        let decomposed = decompose(expr, &mut parts)?;
        let inputs = parts.inputs.len();
        let expr = renumbered(&decomposed, inputs)?;
        let mut windows = Vec::with_capacity(parts.windows.len());
        let mut keys_of_windows = Vec::new();
        for (spec, keys) in parts.windows {
            windows.push((spec, keys.len()));
            keys_of_windows.extend(keys);
        }
        self.inner_items.extend(
            parts
                .inputs
                .into_iter()
                .chain(keys_of_windows)
                .map(|expr| SelectItem::Expr { expr, alias: None }),
        );
        self.items.push(ExpandedItem::Computed {
            expr,
            inputs,
            windows,
            output_name,
        });
        Ok(())
    }

    fn add(&mut self, item: SelectItem) -> SqlResult<()> {
        match item {
            SelectItem::Expr { expr, alias } if selected_call(&expr).is_some() => {
                self.close_run();
                let at = expr.position;
                let field = selected_call(&expr).and_then(|(_, field)| field.map(str::to_owned));
                let call = match (expr.kind, &field) {
                    (ExprKind::Function { mut arguments, .. }, Some(_)) if !arguments.is_empty() => {
                        arguments.swap_remove(0)
                    }
                    (kind, _) => Expr {
                        kind,
                        position: at,
                    },
                };
                let ExprKind::Function { name, arguments } = call.kind else {
                    return Ok(());
                };
                let output_name = alias
                    .or_else(|| field.clone())
                    .unwrap_or_else(|| name.clone());
                self.name_output(&output_name);
                self.items.push(ExpandedItem::SetReturning {
                    function: name,
                    arguments: arguments.len(),
                    output_name,
                    position: at,
                    field,
                });
                self.inner_items
                    .extend(arguments.into_iter().map(|argument| SelectItem::Expr {
                        expr: argument,
                        alias: None,
                    }));
            }
            SelectItem::Expr { expr, alias } if crate::window::is_call(&expr) => {
                self.close_run();
                let output_name = alias.unwrap_or_else(|| crate::eval::output_name(&expr));
                self.name_output(&output_name);
                self.push_window(expr, output_name)?;
            }
            SelectItem::Expr { expr, alias } if crate::window::first_call(&expr).is_some() => {
                self.close_run();
                let output_name = alias.unwrap_or_else(|| crate::eval::output_name(&expr));
                self.name_output(&output_name);
                self.push_computed(&expr, output_name)?;
            }
            other => {
                let width = match &other {
                    SelectItem::Wildcard { .. } => None,
                    SelectItem::Expr { expr, alias } => {
                        if let Some(Some(offset)) = self.run.or(Some(Some(0))) {
                            let name = alias
                                .clone()
                                .unwrap_or_else(|| crate::eval::output_name(expr));
                            self.names.push((name, self.items.len(), offset));
                        }
                        Some(1)
                    }
                };
                self.run = Some(match (self.run.take(), width) {
                    (None, width) => width,
                    (Some(Some(known)), Some(more)) => Some(known + more),
                    _ => None,
                });
                self.inner_items.push(other);
            }
        }
        Ok(())
    }

    fn hidden_key(&mut self, expr: Expr) -> SqlResult<ExpansionKey> {
        self.close_run();
        let item = self.items.len();
        if crate::window::is_call(&expr) {
            self.push_window(expr, String::new())?;
        } else if crate::window::first_call(&expr).is_some() {
            self.push_computed(&expr, String::new())?;
        } else {
            self.items.push(ExpandedItem::Columns(Some(1)));
            self.inner_items.push(SelectItem::Expr { expr, alias: None });
        }
        Ok(ExpansionKey::Output { item, offset: 0 })
    }

    fn add_order(
        &mut self,
        order: Vec<OrderItem>,
    ) -> SqlResult<Vec<(ExpansionKey, crate::types::SortDirection)>> {
        let mut keys = Vec::with_capacity(order.len());
        for item in order {
            let direction = crate::types::SortDirection::of(&item);
            let named = match &item.expr.kind {
                ExprKind::Column {
                    qualifier: None,
                    name,
                } => self
                    .names
                    .iter()
                    .find(|(output, ..)| output == name)
                    .map(|&(_, item, offset)| ExpansionKey::Output { item, offset }),
                _ => None,
            };
            let key = match (&item.expr.kind, named) {
                (ExprKind::Literal(Literal::Integer(number)), _) => ExpansionKey::Ordinal {
                    number: *number,
                    position: item.expr.position,
                },
                (_, Some(named)) => named,
                _ => self.hidden_key(item.expr)?,
            };
            keys.push((key, direction));
        }
        Ok(keys)
    }
}

pub(crate) fn expand_select_list(select: &mut Select) -> SqlResult<()> {
    let windows = select_mentions_window(select);
    let position = select.items.iter().find_map(computed_item).or_else(|| {
        select
            .order_by
            .iter()
            .find_map(|item| crate::window::first_call(&item.expr).map(|call| call.position))
    });
    let Some(position) = position else {
        return Ok(());
    };
    let mut builder = ExpansionBuilder::default();
    for item in std::mem::take(&mut select.items) {
        builder.add(item)?;
    }
    builder.close_run();
    let visible = builder.items.len();
    let order = if windows {
        builder.add_order(std::mem::take(&mut select.order_by))?
    } else {
        Vec::new()
    };
    let hidden = builder.items.len() - visible;
    if builder.unknown_runs > 1 {
        return Err(SqlError::not_supported(
            "a set-returning or window function between two * items in a select list is not supported yet",
        )
        .at(position));
    }
    let inner = Select {
        distinct: false,
        items: builder.inner_items,
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
                text: builder.label.unwrap_or_default(),
                position,
            },
            alias: None,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Expansion(Box::new(
                SelectListExpansion {
                    query: Query::of_select(inner, position),
                    items: builder.items,
                    order,
                    hidden,
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

fn input_width(item: &ExpandedItem) -> Option<usize> {
    match item {
        ExpandedItem::Columns(width) => *width,
        ExpandedItem::SetReturning { arguments, .. } => Some(*arguments),
        ExpandedItem::Window { keys, .. } => Some(*keys),
        ExpandedItem::Computed {
            inputs, windows, ..
        } => Some(inputs + windows.iter().map(|(_, keys)| keys).sum::<usize>()),
    }
}

fn item_spans(items: &[ExpandedItem], total: usize) -> Vec<std::ops::Range<usize>> {
    let known: usize = items.iter().filter_map(input_width).sum();
    let unknown = total.saturating_sub(known);
    let mut spans = Vec::with_capacity(items.len());
    let mut at = 0;
    for item in items {
        let width = input_width(item).unwrap_or(unknown);
        let end = at.saturating_add(width).min(total);
        spans.push(at..end);
        at = end;
    }
    spans
}

enum PreparedItem {
    Columns,
    SetReturning(ResolvedFunction, Option<String>),
    Window(crate::window::WindowResult),
    Computed {
        expr: Expr,
        inputs: usize,
        results: Vec<crate::window::WindowResult>,
        sql_type: Option<SqlType>,
    },
}

fn span_rows(rows: &[Vec<Value>], span: &std::ops::Range<usize>) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| row.get(span.clone()).unwrap_or(&[]).to_vec())
        .collect()
}

fn span_types(columns: &[OutputColumn], span: &std::ops::Range<usize>) -> Vec<Option<SqlType>> {
    columns
        .get(span.clone())
        .unwrap_or(&[])
        .iter()
        .map(|column| column.sql_type)
        .collect()
}

fn prepare_item(
    item: &ExpandedItem,
    span: &std::ops::Range<usize>,
    columns: &[OutputColumn],
    rows: &[Vec<Value>],
) -> SqlResult<PreparedItem> {
    Ok(match item {
        ExpandedItem::Columns(_) => PreparedItem::Columns,
        ExpandedItem::SetReturning {
            function,
            position,
            field,
            ..
        } => {
            let found = named(function).ok_or_else(|| {
                SqlError::not_supported(format!(
                    "the set-returning function {function}() is not supported yet"
                ))
            })?;
            let declared = span_types(columns, span);
            let sql_type = if declared.iter().all(Option::is_some) {
                Some((found.result_type)(&declared).map_err(|error| error.at_if_unplaced(*position))?)
            } else {
                None
            };
            PreparedItem::SetReturning(
                ResolvedFunction {
                    found,
                    declared,
                    sql_type,
                    position: *position,
                },
                field.clone(),
            )
        }
        ExpandedItem::Window { spec, .. } => PreparedItem::Window(crate::window::evaluate(
            spec,
            &span_rows(rows, span),
            &span_types(columns, span),
        )?),
        ExpandedItem::Computed {
            expr,
            inputs,
            windows,
            ..
        } => {
            let mut types = span_types(columns, &(span.start..span.start + inputs));
            let mut results = Vec::with_capacity(windows.len());
            let mut at = span.start + inputs;
            for (spec, keys) in windows {
                let window_span = at..at + keys;
                let result = crate::window::evaluate(
                    spec,
                    &span_rows(rows, &window_span),
                    &span_types(columns, &window_span),
                )?;
                types.push(result.sql_type);
                results.push(result);
                at += keys;
            }
            let typed = typed_slots(expr, &types)?;
            let sql_type = crate::typing::static_type(&typed, Scope::empty());
            PreparedItem::Computed {
                expr: typed,
                inputs: *inputs,
                results,
                sql_type,
            }
        }
    })
}

fn output_starts(items: &[ExpandedItem], spans: &[std::ops::Range<usize>]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(items.len());
    let mut at = 0;
    for (item, span) in items.iter().zip(spans) {
        starts.push(at);
        at += match item {
            ExpandedItem::Columns(_) => span.len(),
            _ => 1,
        };
    }
    starts
}

fn sorted_by_keys(
    expansion: &SelectListExpansion,
    spans: &[std::ops::Range<usize>],
    visible: usize,
    rows: Vec<Vec<Value>>,
) -> SqlResult<Vec<Vec<Value>>> {
    let starts = output_starts(&expansion.items, spans);
    let mut columns = Vec::with_capacity(expansion.order.len());
    let mut directions = Vec::with_capacity(expansion.order.len());
    for (key, direction) in &expansion.order {
        let column = match *key {
            ExpansionKey::Ordinal { number, position } => usize::try_from(number)
                .ok()
                .filter(|&number| number >= 1 && number <= visible)
                .map(|number| number - 1)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!("ORDER BY position {number} is not in select list"),
                    )
                    .at(position)
                })?,
            ExpansionKey::Output { item, offset } => starts
                .get(item)
                .map(|start| start + offset)
                .ok_or_else(|| lost_window(expansion.position))?,
        };
        columns.push(column);
        directions.push(*direction);
    }
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = rows
        .into_iter()
        .map(|row| {
            let keys = columns
                .iter()
                .map(|&column| row.get(column).cloned().unwrap_or(Value::Null))
                .collect();
            (keys, row)
        })
        .collect();
    keyed.sort_by(|left, right| {
        crate::types::compare_in_directions(&directions, &left.0, &right.0)
    });
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}

pub(crate) fn expanded(
    expansion: &SelectListExpansion,
    inner: QueryResult,
) -> SqlResult<GeneratedRows> {
    let inner_columns = inner.columns.unwrap_or_default();
    let spans = item_spans(&expansion.items, inner_columns.len());
    let mut prepared: Vec<PreparedItem> = Vec::with_capacity(expansion.items.len());
    for (item, span) in expansion.items.iter().zip(&spans) {
        prepared.push(prepare_item(item, span, &inner_columns, &inner.rows)?);
    }
    let sets_rows = prepared
        .iter()
        .any(|item| matches!(item, PreparedItem::SetReturning(..)));
    let mut collector = RowCollector::new();
    for (row_at, row) in inner.rows.iter().enumerate() {
        let mut pending = WorkMemory::new();
        let mut fixed: Vec<Vec<Value>> = Vec::with_capacity(prepared.len());
        let mut produced: Vec<Option<Vec<Value>>> = Vec::with_capacity(prepared.len());
        let mut longest = usize::from(!sets_rows);
        for (item, span) in prepared.iter_mut().zip(&spans) {
            let values = row.get(span.clone()).unwrap_or(&[]);
            match item {
                PreparedItem::Columns => {
                    fixed.push(values.to_vec());
                    produced.push(None);
                }
                PreparedItem::SetReturning(function, field) => {
                    let mut generated = function.values(values, &mut pending)?;
                    if let Some(field) = field {
                        for value in &mut generated {
                            let record = std::mem::replace(value, Value::Null);
                            *value = crate::records::field(&record, field)?;
                        }
                    }
                    longest = longest.max(generated.len());
                    fixed.push(Vec::new());
                    produced.push(Some(generated));
                }
                PreparedItem::Window(result) => {
                    fixed.push(vec![
                        result.values.get(row_at).cloned().unwrap_or(Value::Null),
                    ]);
                    produced.push(None);
                }
                PreparedItem::Computed {
                    expr,
                    inputs,
                    results,
                    ..
                } => {
                    let mut local: Vec<Value> = values.get(..*inputs).unwrap_or(&[]).to_vec();
                    local.extend(
                        results
                            .iter()
                            .map(|result| result.values.get(row_at).cloned().unwrap_or(Value::Null)),
                    );
                    let evaluator = Evaluator {
                        scope: Scope::empty(),
                        row: &local,
                        correlation: None,
                    };
                    fixed.push(vec![evaluator.eval(expr, None)?]);
                    produced.push(None);
                }
            }
        }
        for index in 0..longest {
            let mut expanded = Vec::with_capacity(row.len());
            for (values, fixed) in produced.iter().zip(&fixed) {
                match values {
                    Some(values) => {
                        expanded.push(values.get(index).cloned().unwrap_or(Value::Null));
                    }
                    None => expanded.extend_from_slice(fixed),
                }
            }
            collector.push(expanded)?;
        }
    }
    let mut columns: Vec<OutputColumn> = Vec::with_capacity(inner_columns.len());
    for ((item, span), prepared) in expansion.items.iter().zip(&spans).zip(&prepared) {
        match (item, prepared) {
            (ExpandedItem::Columns(_), _) => columns.extend(
                inner_columns
                    .get(span.clone())
                    .unwrap_or(&[])
                    .iter()
                    .cloned(),
            ),
            (ExpandedItem::SetReturning { output_name, .. }, prepared) => {
                columns.push(OutputColumn {
                    name: output_name.clone(),
                    sql_type: match prepared {
                        PreparedItem::SetReturning(function, Some(field)) => {
                            crate::records::expanded_field_type(
                                function.declared.first().copied().flatten(),
                                field,
                            )
                        }
                        PreparedItem::SetReturning(function, None) => {
                            Some(function.sql_type.unwrap_or(SqlType::Integer))
                        }
                        _ => Some(SqlType::Integer),
                    },
                });
            }
            (ExpandedItem::Window { output_name, .. }, prepared) => columns.push(OutputColumn {
                name: output_name.clone(),
                sql_type: match prepared {
                    PreparedItem::Window(result) => result.sql_type,
                    _ => None,
                },
            }),
            (ExpandedItem::Computed { output_name, .. }, prepared) => {
                columns.push(OutputColumn {
                    name: output_name.clone(),
                    sql_type: match prepared {
                        PreparedItem::Computed { sql_type, .. } => *sql_type,
                        _ => None,
                    },
                });
            }
        }
    }
    let (mut rows, charge) = collector.finish();
    let visible = columns.len().saturating_sub(expansion.hidden);
    if !expansion.order.is_empty() {
        rows = sorted_by_keys(expansion, &spans, visible, rows)?;
    }
    if expansion.hidden > 0 {
        for row in &mut rows {
            row.truncate(visible);
        }
        columns.truncate(visible);
    }
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
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) | GeneratedTable::Variable(_) => {
            Vec::new()
        }
    }
}

pub(crate) fn expressions_mut(generated: &mut GeneratedTable) -> Vec<&mut Expr> {
    match generated {
        GeneratedTable::Values(list) => list.rows.iter_mut().flatten().collect(),
        GeneratedTable::Function(function) => function.arguments.iter_mut().collect(),
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) | GeneratedTable::Variable(_) => {
            Vec::new()
        }
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
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) | GeneratedTable::Variable(_) => {
            Ok(())
        }
    }
}
