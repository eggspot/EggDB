use super::polymorphic::{self, is_polymorphic, is_polymorphic_array};
use super::{
    ParameterMode, ReturnType, RoutineDefinition, RoutineKind, RoutineLanguage, RoutineParameter,
    RoutineReturns, Volatility, resolve,
};
use crate::arrays::ArrayValue;
use crate::ast::{
    CallRoutine, Delete, Expr, ExprKind, FromClause, GeneratedTable, Insert, InsertSource, Literal,
    Name, ORACLE_COLLECTION_TABLE, Query, Select, SelectItem, SetExpr, Statement, TableFunction,
    TableReference, Update, ValuesList,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, children, children_mut};
use crate::execute::TableCache;
use crate::lexer::tokenize_in;
use crate::parser::Parser;
use crate::plpgsql::{Call, ReadOnlyWrite, Returned, TransactionControl};
use crate::records::{RecordValue, field_names};
use crate::schemas::{base_name, display};
use crate::session::{OutputColumn, QueryResult};
use crate::settings::Settings;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::{Conversion, DefinedValue, SqlType, Value, convert};
use std::sync::Arc;

type BestMatch<'a> = Option<(usize, &'a Arc<RoutineDefinition>, MatchedSlots)>;

struct NamedArguments<'a> {
    names: &'a [Option<String>],
    values: Vec<Value>,
    untyped: &'a [bool],
}

struct MatchedSlots {
    values: Vec<Option<Value>>,
    untyped: Vec<bool>,
}

pub(crate) const ROUTINE_CALL: &str = "*routine_call*";
pub(crate) const DEFAULT_ARGUMENT: &str = "*default*";
const NAME_SEPARATOR: &str = "\0";
const ORDINALITY_COLUMN: &str = "ordinality";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prefetch {
    Everything,
    NonVolatile,
}

struct Chosen {
    definition: Arc<RoutineDefinition>,
    arguments: Vec<Value>,
}

struct UserCall {
    name: String,
    names: Vec<Option<String>>,
    marked: bool,
    volatile: bool,
    row_independent: bool,
}

struct MarkedCall<'e> {
    name: String,
    names: Vec<Option<String>>,
    arguments: &'e [Expr],
}

struct SetItem {
    label: String,
    values: Vec<Value>,
    sql_type: Option<SqlType>,
    position: usize,
}

pub(crate) fn marked_call(
    name: String,
    names: Vec<Option<String>>,
    arguments: Vec<Expr>,
    position: usize,
) -> Expr {
    let mut all = Vec::with_capacity(arguments.len() + 2);
    all.push(text_literal(name, position));
    all.push(text_literal(joined_names(&names), position));
    all.extend(arguments);
    Expr {
        kind: ExprKind::Function {
            name: ROUTINE_CALL.to_owned(),
            arguments: all,
        },
        position,
    }
}

pub(crate) fn default_argument(position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: DEFAULT_ARGUMENT.to_owned(),
            arguments: Vec::new(),
        },
        position,
    }
}

pub(crate) fn is_default_argument(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, arguments } if name == DEFAULT_ARGUMENT && arguments.is_empty())
}

pub(crate) fn select_calls_marked_routine(select: &Select) -> bool {
    select
        .from
        .as_ref()
        .is_some_and(|from| is_user_table_function(&from.first))
        || crate::subquery::select_expressions(select)
            .into_iter()
            .any(mentions_routine_call)
}

pub(crate) fn query_calls_marked_routine(query: &Query) -> bool {
    body_calls_marked_routine(&query.body)
}

pub(crate) fn insert_calls_marked_routine(insert: &mut Insert) -> bool {
    let in_query =
        matches!(&insert.source, InsertSource::Query(query) if query_calls_marked_routine(query));
    in_query || exprs_call_marked_routine(crate::execute::insert_exprs(insert))
}

pub(crate) fn exprs_call_marked_routine(exprs: Vec<&mut Expr>) -> bool {
    exprs.into_iter().any(|expr| mentions_routine_call(expr))
}

fn body_calls_marked_routine(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select_calls_marked_routine(select),
        SetExpr::Query(query) => query_calls_marked_routine(query),
        SetExpr::Operation(operation) => {
            body_calls_marked_routine(&operation.left)
                || body_calls_marked_routine(&operation.right)
        }
    }
}

pub(crate) fn called_name(arguments: &[Expr]) -> String {
    match arguments.first().map(|first| &first.kind) {
        Some(ExprKind::Literal(Literal::String(name))) => base_name(name).to_owned(),
        _ => "?column?".to_owned(),
    }
}

pub(crate) fn mentions_routine_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == ROUTINE_CALL)
        || children(expr).into_iter().any(mentions_routine_call)
}

pub(crate) fn evaluate(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let call = marked_parts(arguments)?;
    let mut values = Vec::with_capacity(call.arguments.len());
    for argument in call.arguments {
        values.push(evaluator.eval(argument, None)?);
    }
    let untyped = untyped_arguments(call.arguments);
    match evaluator.correlation {
        Some(correlation) => correlation.call_routine(&call.name, &call.names, values, &untyped),
        None => Err(not_callable_here(&call.name)),
    }
}

/// Whether each argument expression is an untyped literal (a quoted string with no cast), whose
/// type a call settles from the parameter it fills instead of from the value.
pub(crate) fn untyped_arguments(arguments: &[Expr]) -> Vec<bool> {
    arguments
        .iter()
        .map(|argument| matches!(argument.kind, ExprKind::Literal(Literal::String(_))))
        .collect()
}

pub(crate) fn call_in_row(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
    names: &[Option<String>],
    values: Vec<Value>,
    untyped: &[bool],
) -> SqlResult<Value> {
    let settings = crate::pg_catalog::statement_settings();
    let mut writer = ReadOnlyWrite { inner: reader };
    let call = NamedArguments {
        names,
        values,
        untyped,
    };
    match call_function(&mut writer, tables, &settings, name, call)? {
        (Returned::Value(value), _) => Ok(value),
        (Returned::Set(_), _) => Err(set_not_allowed()),
    }
}

pub(crate) fn correlated_table_function_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    function: &TableFunction,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    let Some(found) = resolve(reader, tables, &function.name)? else {
        return Ok(None);
    };
    let mut candidates = found
        .routines
        .iter()
        .filter(|routine| routine.kind == RoutineKind::Function);
    let (Some(definition), None) = (candidates.next(), candidates.next()) else {
        return Ok(None);
    };
    let definition = Arc::clone(definition);
    let mut columns = if is_inline_tsql_table(&definition) {
        let settings = crate::pg_catalog::statement_settings();
        let mut writer = ReadOnlyWrite { inner: reader };
        crate::tsql::inline_table_columns(&mut writer, tables, &settings, &definition)?
    } else if let RoutineReturns::Table(declared) = &definition.returns {
        declared
            .iter()
            .map(|(name, sql_type)| OutputColumn {
                name: name.clone(),
                sql_type: Some(*sql_type),
            })
            .collect()
    } else {
        return Ok(None);
    };
    if function.ordinality {
        columns.push(OutputColumn {
            name: ORDINALITY_COLUMN.to_owned(),
            sql_type: Some(SqlType::BigInt),
        });
    }
    Ok(Some(columns))
}

pub(crate) fn correlated_table_function_rows(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    function: &TableFunction,
    evaluator: &Evaluator<'_>,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut values = Vec::with_capacity(function.arguments.len());
    let mut names = Vec::with_capacity(function.arguments.len());
    for argument in &function.arguments {
        if is_default_argument(argument) {
            values.push(Value::Null);
            names.push(Some(DEFAULT_ARGUMENT.to_owned()));
        } else {
            values.push(evaluator.eval(argument, None)?);
            names.push(None);
        }
    }
    let settings = crate::pg_catalog::statement_settings();
    let mut writer = ReadOnlyWrite { inner: reader };
    let untyped = untyped_arguments(&function.arguments);
    let (returned, _) = call_function(
        &mut writer,
        tables,
        &settings,
        &function.name,
        NamedArguments {
            names: &names,
            values,
            untyped: &untyped,
        },
    )
    .map_err(|error| error.at_if_unplaced(function.position))?;
    let elements = match returned {
        Returned::Set(elements) => elements,
        Returned::Value(value) => vec![value],
    };
    let mut rows: Vec<Vec<Value>> = elements.into_iter().map(row_of).collect();
    if function.ordinality {
        for (number, row) in rows.iter_mut().enumerate() {
            row.push(Value::BigInt(i64::try_from(number + 1).unwrap_or(i64::MAX)));
        }
    }
    Ok(rows)
}

pub(crate) fn call_procedure(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: &CallRoutine,
) -> SqlResult<QueryResult> {
    call_procedure_outputs(writer, tables, settings, call).map(|(result, _)| result)
}

pub(crate) fn call_procedure_outputs(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: &CallRoutine,
) -> SqlResult<(QueryResult, Vec<Option<usize>>)> {
    let chosen = chosen_procedure(writer, tables, settings, call)?;
    let returned = crate::plpgsql::invoke(
        writer,
        tables,
        settings,
        Call {
            definition: &chosen.definition,
            arguments: chosen.arguments,
            trigger: None,
        },
    )?;
    let sources = output_sources(&chosen.definition, &call.argument_names);
    Ok((procedure_result(&chosen.definition, returned), sources))
}

pub(crate) fn call_procedure_nonatomic(
    control: &mut dyn TransactionControl,
    tables: &mut TableCache,
    settings: &Settings,
    call: &CallRoutine,
) -> SqlResult<(QueryResult, Vec<Option<usize>>)> {
    let chosen = chosen_procedure(&mut *control, tables, settings, call)?;
    let returned = crate::plpgsql::invoke_nonatomic(
        control,
        tables,
        settings,
        Call {
            definition: &chosen.definition,
            arguments: chosen.arguments,
            trigger: None,
        },
    )?;
    let sources = output_sources(&chosen.definition, &call.argument_names);
    Ok((procedure_result(&chosen.definition, returned), sources))
}

fn chosen_procedure(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    call: &CallRoutine,
) -> SqlResult<Chosen> {
    let mut arguments = call.arguments.clone();
    Preparing {
        writer: &mut *writer,
        tables: &mut *tables,
        settings,
        prefetch: Prefetch::Everything,
    }
    .exprs(arguments.iter_mut().collect())?;
    let evaluator = constant_evaluator();
    let mut values = Vec::with_capacity(arguments.len());
    for argument in &arguments {
        values.push(evaluator.eval(argument, None)?);
    }
    let untyped = untyped_arguments(&arguments);
    choose(
        &*writer,
        tables,
        RoutineKind::Procedure,
        &call.name.text,
        NamedArguments {
            names: &call.argument_names,
            values,
            untyped: &untyped,
        },
    )
    .map_err(|error| error.at_if_unplaced(call.name.position))
}

fn output_sources(definition: &RoutineDefinition, names: &[Option<String>]) -> Vec<Option<usize>> {
    let positional = names.iter().take_while(|name| name.is_none()).count();
    call_parameters(definition)
        .into_iter()
        .enumerate()
        .filter(|(_, parameter)| parameter.mode.is_output())
        .map(|(at, parameter)| {
            if at < positional {
                return Some(at);
            }
            names
                .iter()
                .position(|name| name.is_some() && name.as_deref() == parameter.name.as_deref())
        })
        .collect()
}

pub(crate) fn run_do(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    code: &str,
) -> SqlResult<QueryResult> {
    crate::plpgsql::run_inline(writer, tables, settings, code)?;
    Ok(QueryResult::command("DO"))
}

pub(crate) fn run_do_nonatomic(
    control: &mut dyn TransactionControl,
    tables: &mut TableCache,
    settings: &Settings,
    code: &str,
) -> SqlResult<QueryResult> {
    crate::plpgsql::run_inline_nonatomic(control, tables, settings, code)?;
    Ok(QueryResult::command("DO"))
}

pub(crate) fn select_may_write(select: &Select) -> bool {
    match &select.from {
        None => crate::subquery::select_expressions(select)
            .into_iter()
            .any(calls_any_function),
        Some(from) => is_user_table_function(&from.first),
    }
}

pub(crate) fn select_calls_volatile(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
) -> SqlResult<bool> {
    let mut names: Vec<&str> = Vec::new();
    for expr in crate::subquery::select_expressions(select) {
        function_names(expr, &mut names);
    }
    if let Some(from) = &select.from
        && let Some(generated) = &from.first.generated
        && let GeneratedTable::Function(function) = generated.as_ref()
    {
        names.push(&function.name);
    }
    for name in names {
        if let Some(found) = resolve(reader, tables, name)?
            && found.routines.iter().any(|routine| {
                routine.kind == RoutineKind::Function && routine.volatility == Volatility::Volatile
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn prepare_oracle_table_function(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    select: &mut Select,
) -> SqlResult<()> {
    let Some(from) = &mut select.from else {
        return Ok(());
    };
    if !is_user_table_function(&from.first) {
        return Ok(());
    }
    let mut preparing = Preparing {
        writer,
        tables,
        settings,
        prefetch: Prefetch::NonVolatile,
    };
    if preparing.table_function(&mut from.first)? {
        select.filter = Some(and_false(select.filter.take(), from.first.name.position));
    }
    Ok(())
}

pub(crate) fn prepare_select(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    select: &mut Select,
    prefetch: Prefetch,
) -> SqlResult<()> {
    let from_less = select.from.is_none();
    let in_from = select
        .from
        .as_ref()
        .is_some_and(|from| is_user_table_function(&from.first));
    if !in_from
        && !crate::subquery::select_expressions(select)
            .into_iter()
            .any(calls_any_function)
    {
        return Ok(());
    }
    let mut preparing = Preparing {
        writer,
        tables,
        settings,
        prefetch: if from_less {
            Prefetch::Everything
        } else {
            prefetch
        },
    };
    if let Some(from) = &mut select.from
        && preparing.table_function(&mut from.first)?
    {
        select.filter = Some(and_false(select.filter.take(), from.first.name.position));
    }
    let mut set_item = None;
    for item in &mut select.items {
        if let SelectItem::Expr { expr, alias } = item {
            preparing.item(expr, alias, from_less, &mut set_item)?;
        }
    }
    preparing.exprs(crate::subquery::select_expressions_mut(select))?;
    if let Some(set) = set_item {
        let empty = set.values.is_empty();
        let rows = set.values.into_iter().map(|value| vec![value]).collect();
        select.from = Some(FromClause {
            first: values_table(
                set.label.clone(),
                vec![set.label],
                rows,
                &[set.sql_type],
                set.position,
            ),
            joins: Vec::new(),
        });
        if empty {
            select.filter = Some(and_false(select.filter.take(), set.position));
        }
    }
    Ok(())
}

pub(crate) fn prepare_query(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    query: &mut Query,
    prefetch: Prefetch,
) -> SqlResult<()> {
    prepare_body(writer, tables, settings, &mut query.body, prefetch)
}

fn prepare_body(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    body: &mut SetExpr,
    prefetch: Prefetch,
) -> SqlResult<()> {
    match body {
        SetExpr::Select(select) => prepare_select(writer, tables, settings, select, prefetch),
        SetExpr::Query(query) => prepare_query(writer, tables, settings, query, prefetch),
        SetExpr::Operation(operation) => {
            prepare_body(writer, tables, settings, &mut operation.left, prefetch)?;
            prepare_body(writer, tables, settings, &mut operation.right, prefetch)
        }
    }
}

pub(crate) fn prepare_insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    insert: &mut Insert,
) -> SqlResult<()> {
    if let InsertSource::Query(query) = &mut insert.source {
        prepare_query(writer, tables, settings, query, Prefetch::NonVolatile)?;
    }
    prepare_exprs(
        writer,
        tables,
        settings,
        crate::execute::insert_exprs(insert),
        Prefetch::Everything,
    )
}

pub(crate) fn prepare_update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    update: &mut Update,
) -> SqlResult<()> {
    prepare_exprs(
        writer,
        tables,
        settings,
        crate::execute::update_exprs(update),
        Prefetch::NonVolatile,
    )
}

pub(crate) fn prepare_delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    delete: &mut Delete,
) -> SqlResult<()> {
    prepare_exprs(
        writer,
        tables,
        settings,
        crate::execute::delete_exprs(delete),
        Prefetch::NonVolatile,
    )
}

fn prepare_exprs(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    exprs: Vec<&mut Expr>,
    prefetch: Prefetch,
) -> SqlResult<()> {
    if !exprs.iter().any(|expr| calls_any_function(expr)) {
        return Ok(());
    }
    Preparing {
        writer,
        tables,
        settings,
        prefetch,
    }
    .exprs(exprs)
}

struct Preparing<'p> {
    writer: &'p mut dyn KeyValueWrite,
    tables: &'p mut TableCache,
    settings: &'p Settings,
    prefetch: Prefetch,
}

impl Preparing<'_> {
    fn exprs(&mut self, exprs: Vec<&mut Expr>) -> SqlResult<()> {
        for expr in exprs {
            self.expr(expr)?;
        }
        Ok(())
    }

    fn expr(&mut self, expr: &mut Expr) -> SqlResult<()> {
        for child in children_mut(expr) {
            self.expr(child)?;
        }
        let Some(call) = self.user_call(expr)? else {
            return Ok(());
        };
        if !self.runs_now(&call) {
            mark(expr, call);
            return Ok(());
        }
        let (returned, definition) = self.call_now(expr, &call)?;
        match returned {
            Returned::Value(value) => {
                *expr = constant(value, declared_type(&definition), expr.position);
                Ok(())
            }
            Returned::Set(_) => Err(set_not_allowed().at(expr.position)),
        }
    }

    fn item(
        &mut self,
        expr: &mut Expr,
        alias: &mut Option<String>,
        from_less: bool,
        set_item: &mut Option<SetItem>,
    ) -> SqlResult<()> {
        for child in children_mut(expr) {
            self.expr(child)?;
        }
        let Some(call) = self.user_call(expr)? else {
            return Ok(());
        };
        if !self.runs_now(&call) {
            mark(expr, call);
            return Ok(());
        }
        let label = alias
            .clone()
            .unwrap_or_else(|| base_name(&call.name).to_owned());
        let position = expr.position;
        let (returned, definition) = self.call_now(expr, &call)?;
        match returned {
            Returned::Value(value) => {
                *expr = constant(value, declared_type(&definition), position);
            }
            Returned::Set(values) if from_less && set_item.is_none() => {
                *expr = Expr {
                    kind: ExprKind::Column {
                        qualifier: None,
                        name: label.clone(),
                    },
                    position,
                };
                *set_item = Some(SetItem {
                    label: label.clone(),
                    values,
                    sql_type: declared_type(&definition),
                    position,
                });
            }
            Returned::Set(_) => return Err(set_not_allowed().at(position)),
        }
        *alias = Some(label);
        Ok(())
    }

    fn runs_now(&self, call: &UserCall) -> bool {
        call.row_independent && (self.prefetch == Prefetch::Everything || !call.volatile)
    }

    fn user_call(&mut self, expr: &Expr) -> SqlResult<Option<UserCall>> {
        let ExprKind::Function { name, arguments } = &expr.kind else {
            return Ok(None);
        };
        let (name, names, marked) = if name == ROUTINE_CALL {
            let call = marked_parts(arguments)?;
            (call.name, call.names, true)
        } else if could_be_routine(name) {
            (name.clone(), vec![None; arguments.len()], false)
        } else {
            return Ok(None);
        };
        let Some(found) = resolve(&*self.writer, self.tables, &name)? else {
            return Ok(None);
        };
        let volatile = found.routines.iter().any(|routine| {
            routine.kind == RoutineKind::Function && routine.volatility == Volatility::Volatile
        });
        let row_independent = call_arguments(expr).iter().all(is_row_independent);
        Ok(Some(UserCall {
            name,
            names,
            marked,
            volatile,
            row_independent,
        }))
    }

    fn call_now(
        &mut self,
        expr: &Expr,
        call: &UserCall,
    ) -> SqlResult<(Returned, Arc<RoutineDefinition>)> {
        let evaluator = constant_evaluator();
        let mut values = Vec::new();
        for argument in call_arguments(expr) {
            values.push(evaluator.eval(argument, None)?);
        }
        let untyped = untyped_arguments(call_arguments(expr));
        call_function(
            &mut *self.writer,
            self.tables,
            self.settings,
            &call.name,
            NamedArguments {
                names: &call.names,
                values,
                untyped: &untyped,
            },
        )
        .map_err(|error| error.at_if_unplaced(expr.position))
    }

    fn table_function(&mut self, table: &mut TableReference) -> SqlResult<bool> {
        if !is_user_table_function(table) {
            return Ok(false);
        }
        if is_oracle_collection_table(table) {
            return self.collection_table(table);
        }
        let Some(generated) = &mut table.generated else {
            return Ok(false);
        };
        let GeneratedTable::Function(function) = generated.as_mut() else {
            return Ok(false);
        };
        let Some(found) = resolve(&*self.writer, self.tables, &function.name)? else {
            return Ok(false);
        };
        if !found
            .routines
            .iter()
            .any(|routine| routine.kind == RoutineKind::Function)
        {
            return Ok(false);
        }
        let prefetch = std::mem::replace(&mut self.prefetch, Prefetch::Everything);
        let prepared = function
            .arguments
            .iter_mut()
            .try_for_each(|argument| self.expr(argument));
        self.prefetch = prefetch;
        prepared?;
        if !function.arguments.iter().all(is_row_independent) {
            return Ok(false);
        }
        let evaluator = constant_evaluator();
        let mut values = Vec::with_capacity(function.arguments.len());
        let mut names = Vec::with_capacity(function.arguments.len());
        for argument in &function.arguments {
            if is_default_argument(argument) {
                values.push(Value::Null);
                names.push(Some(DEFAULT_ARGUMENT.to_owned()));
            } else {
                values.push(evaluator.eval(argument, None)?);
                names.push(None);
            }
        }
        let position = function.position;
        let untyped = untyped_arguments(&function.arguments);
        let (returned, definition) = call_function(
            &mut *self.writer,
            self.tables,
            self.settings,
            &function.name,
            NamedArguments {
                names: &names,
                values,
                untyped: &untyped,
            },
        )
        .map_err(|error| error.at_if_unplaced(position))?;
        let elements = match returned {
            Returned::Set(elements) => elements,
            Returned::Value(value) if definition.dialect == Dialect::Oracle => {
                oracle_collection_elements(value)
            }
            Returned::Value(value) => vec![value],
        };
        let label = base_name(&function.name).to_owned();
        let (mut columns, mut types) = if elements.is_empty() && is_inline_tsql_table(&definition) {
            let found = crate::tsql::inline_table_columns(
                &mut *self.writer,
                self.tables,
                self.settings,
                &definition,
            )?;
            (
                found.iter().map(|column| column.name.clone()).collect(),
                found.iter().map(|column| column.sql_type).collect(),
            )
        } else {
            output_columns(&definition, &elements, &label)
        };
        let fields = record_columns(&elements).filter(|fields| *fields == columns);
        let mut rows = element_rows(elements, fields.as_deref())
            .map_err(|error| error.at_if_unplaced(position))?;
        if function.ordinality {
            columns.push(ORDINALITY_COLUMN.to_owned());
            types.push(Some(SqlType::BigInt));
            for (number, row) in rows.iter_mut().enumerate() {
                row.push(Value::BigInt(i64::try_from(number + 1).unwrap_or(i64::MAX)));
            }
        }
        let mut renamed: Vec<String> = function
            .column_names
            .iter()
            .map(|name| name.text.clone())
            .collect();
        if renamed.len() < columns.len() {
            renamed.extend(columns.into_iter().skip(renamed.len()));
        }
        let empty = rows.is_empty();
        let replacement = values_table(table.name.text.clone(), renamed, rows, &types, position);
        table.generated = replacement.generated;
        Ok(empty)
    }

    fn collection_table(&mut self, table: &mut TableReference) -> SqlResult<bool> {
        let Some(GeneratedTable::Function(function)) = table.generated.as_deref_mut() else {
            return Ok(false);
        };
        let prefetch = std::mem::replace(&mut self.prefetch, Prefetch::Everything);
        let prepared = function
            .arguments
            .iter_mut()
            .try_for_each(|argument| self.expr(argument));
        self.prefetch = prefetch;
        prepared?;
        let position = function.position;
        let [collection] = function.arguments.as_slice() else {
            return Err(SqlError::syntax(
                "TABLE() takes one collection expression",
                position,
            ));
        };
        if !is_row_independent(collection) {
            return Err(SqlError::not_supported(
                "a TABLE() collection expression that reads columns of other FROM items is not supported yet",
            )
            .at(position));
        }
        let value = constant_evaluator().eval(collection, None)?;
        let elements = oracle_collection_elements(value);
        let fields = record_columns(&elements);
        let columns = fields
            .clone()
            .unwrap_or_else(|| vec![ORACLE_COLUMN_VALUE.to_owned()]);
        let types = vec![None; columns.len()];
        let rows = element_rows(elements, fields.as_deref()).map_err(|error| error.at(position))?;
        let empty = rows.is_empty();
        let replacement = values_table(table.name.text.clone(), columns, rows, &types, position);
        table.generated = replacement.generated;
        Ok(empty)
    }
}

const ORACLE_COLUMN_VALUE: &str = "column_value";

fn is_oracle_collection_table(table: &TableReference) -> bool {
    matches!(
        table.generated.as_deref(),
        Some(GeneratedTable::Function(function)) if function.name == ORACLE_COLLECTION_TABLE
    )
}

fn oracle_collection_elements(value: Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Defined(defined) => match *defined {
            DefinedValue::Record(record)
                if record.names.len() == record.fields.len()
                    && record.names.iter().all(|name| name.parse::<u32>().is_ok()) =>
            {
                record.fields
            }
            other => vec![Value::Defined(Box::new(other))],
        },
        other => vec![other],
    }
}

fn call_function(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    name: &str,
    call: NamedArguments<'_>,
) -> SqlResult<(Returned, Arc<RoutineDefinition>)> {
    let chosen = choose(&*writer, tables, RoutineKind::Function, name, call)?;
    let returned = crate::plpgsql::invoke(
        writer,
        tables,
        settings,
        Call {
            definition: &chosen.definition,
            arguments: chosen.arguments,
            trigger: None,
        },
    )?;
    Ok((returned, chosen.definition))
}

fn choose(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    kind: RoutineKind,
    name: &str,
    call: NamedArguments<'_>,
) -> SqlResult<Chosen> {
    let NamedArguments {
        names,
        values,
        untyped,
    } = call;
    let found = resolve(reader, tables, name)?;
    let routines: &[Arc<RoutineDefinition>] = found
        .as_deref()
        .map_or(&[], |named| named.routines.as_slice());
    if let Some(routine) = routines
        .iter()
        .find(|routine| routine.language == RoutineLanguage::Tsql)
    {
        let definition = Arc::clone(routine);
        let arguments =
            crate::tsql::function_arguments(reader, tables, &definition, names, values)?;
        return Ok(Chosen {
            definition,
            arguments,
        });
    }
    let mut best: BestMatch<'_> = None;
    let mut tied = false;
    for routine in routines.iter().filter(|routine| routine.kind == kind) {
        let Some((score, slots)) = matched(routine, names, &values, untyped) else {
            continue;
        };
        match &best {
            Some((known, _, _)) if *known > score => {}
            Some((known, _, _)) if *known == score => tied = true,
            _ => {
                best = Some((score, routine, slots));
                tied = false;
            }
        }
    }
    let plsql = routines
        .iter()
        .any(|routine| routine.language == RoutineLanguage::PlSql);
    let Some((_, definition, slots)) = best else {
        if plsql {
            return Err(super::oracle::wrong_arguments(kind, name, routines));
        }
        return Err(no_routine(kind, name, names, &values, untyped, routines));
    };
    if tied && plsql {
        return Err(super::oracle::too_many_matches(name));
    }
    if tied {
        return Err(SqlError::new(
            SqlState::AMBIGUOUS_FUNCTION,
            format!(
                "{} {}({}) is not unique",
                kind.noun(),
                display(name),
                type_list(&values)
            ),
        )
        .with_hint(format!(
            "Could not choose a best candidate {}. You might need to add explicit type casts.",
            kind.noun()
        )));
    }
    let definition = Arc::clone(definition);
    let (input_untyped, input_given): (Vec<bool>, Vec<bool>) = call_parameters(&definition)
        .into_iter()
        .zip(slots.untyped.iter().zip(&slots.values))
        .filter(|(parameter, _)| parameter.mode.is_input())
        .map(|(_, (untyped, value))| (*untyped, value.is_some()))
        .unzip();
    let arguments = input_arguments(reader, tables, &definition, slots.values)?;
    let (definition, arguments) = polymorphic::specialize(
        tables,
        definition,
        arguments,
        polymorphic::ArgumentKinds {
            untyped: &input_untyped,
            given: &input_given,
        },
    )?;
    Ok(Chosen {
        definition,
        arguments,
    })
}

fn call_parameters(routine: &RoutineDefinition) -> Vec<&RoutineParameter> {
    routine
        .parameters
        .iter()
        .filter(|parameter| routine.kind == RoutineKind::Procedure || parameter.mode.is_input())
        .collect()
}

fn matched(
    routine: &RoutineDefinition,
    names: &[Option<String>],
    values: &[Value],
    untyped: &[bool],
) -> Option<(usize, MatchedSlots)> {
    let parameters = call_parameters(routine);
    let variadic = parameters
        .last()
        .filter(|parameter| parameter.mode == ParameterMode::Variadic)
        .map(|parameter| parameter.sql_type);
    let fixed = parameters.len() - usize::from(variadic.is_some());
    let mut slots: Vec<Option<Value>> = vec![None; parameters.len()];
    let mut slot_untyped = vec![true; parameters.len()];
    let mut spread = Vec::new();
    let is_untyped = |at: usize| untyped.get(at).copied().unwrap_or(true);
    let mut score = 0usize;
    for (at, value) in values.iter().enumerate() {
        let index = match names.get(at).cloned().flatten() {
            None if at < fixed => at,
            None if variadic.is_some() => {
                spread.push((value.clone(), is_untyped(at)));
                continue;
            }
            None => return None,
            Some(name) => parameters
                .iter()
                .position(|parameter| parameter.name.as_deref() == Some(name.as_str()))?,
        };
        let parameter = parameters.get(index)?;
        let slot = slots.get_mut(index)?;
        if slot.is_some() {
            return None;
        }
        score += fit(value, parameter.sql_type)?;
        *slot = Some(literal_argument(value, parameter.sql_type));
        *slot_untyped.get_mut(index)? = is_untyped(at);
    }
    if let Some(SqlType::Array(element)) = variadic
        && !spread.is_empty()
    {
        let mut items = Vec::with_capacity(spread.len());
        for (item, _) in spread {
            score += fit(&item, element.sql_type())?;
            items.push(convert(item, element.sql_type(), Conversion::Assignment).ok()?);
        }
        *slots.get_mut(fixed)? = Some(Value::Array(Box::new(ArrayValue::new(element, items))));
        *slot_untyped.get_mut(fixed)? = false;
    } else if let Some(declared) = variadic
        && is_polymorphic_array(declared)
        && !spread.is_empty()
    {
        score += spread.len();
        *slots.get_mut(fixed)? = Some(polymorphic::variadic_array(declared, spread)?);
        *slot_untyped.get_mut(fixed)? = false;
    } else if !spread.is_empty() {
        return None;
    }
    if parameters
        .iter()
        .any(|parameter| is_polymorphic(parameter.sql_type))
    {
        polymorphic::resolve(
            parameters
                .iter()
                .zip(slots.iter().zip(&slot_untyped))
                .filter(|(parameter, _)| parameter.mode.is_input())
                .map(|(parameter, (slot, untyped))| (parameter.sql_type, slot.as_ref(), *untyped)),
        )?;
    }
    let complete = slots.iter().zip(&parameters).all(|(slot, parameter)| {
        slot.is_some()
            || parameter.default.is_some()
            || (routine.kind == RoutineKind::Procedure && parameter.mode == ParameterMode::Out)
    });
    complete.then_some((
        score,
        MatchedSlots {
            values: slots,
            untyped: slot_untyped,
        },
    ))
}

fn literal_argument(value: &Value, target: SqlType) -> Value {
    let text =
        matches!(value, Value::Text(_)) && target != SqlType::Dynamic && !is_polymorphic(target);
    if text && convert(value.clone(), target, Conversion::Assignment).is_err() {
        return convert(value.clone(), target, Conversion::Explicit)
            .unwrap_or_else(|_| value.clone());
    }
    value.clone()
}

fn fit(value: &Value, target: SqlType) -> Option<usize> {
    if target == SqlType::Dynamic || is_polymorphic(target) {
        return Some(1);
    }
    match value.sql_type() {
        None => Some(1),
        Some(source) if source.without_modifier() == target.without_modifier() => Some(3),
        Some(source)
            if (source.is_numeric() && target.is_numeric())
                || (source.is_character() && target.is_character()) =>
        {
            convert(value.clone(), target, Conversion::Assignment)
                .ok()
                .map(|_| 2)
        }
        Some(_) => convert(value.clone(), target, Conversion::Assignment)
            .ok()
            .or_else(|| {
                matches!(value, Value::Text(_))
                    .then(|| convert(value.clone(), target, Conversion::Explicit).ok())
                    .flatten()
            })
            .map(|_| 0),
    }
}

fn input_arguments(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    definition: &RoutineDefinition,
    slots: Vec<Option<Value>>,
) -> SqlResult<Vec<Value>> {
    let mut given: Vec<(&RoutineParameter, Option<Value>)> = call_parameters(definition)
        .into_iter()
        .zip(slots)
        .filter(|(parameter, _)| parameter.mode.is_input())
        .collect();
    let resolves_with_defaults = polymorphic::declares_polymorphism(definition);
    while !resolves_with_defaults && given.last().is_some_and(|(_, value)| value.is_none()) {
        given.pop();
    }
    let mut arguments = Vec::with_capacity(given.len());
    for (parameter, value) in given {
        arguments.push(match (value, &parameter.default) {
            (Some(value), _) => value,
            (None, Some(default)) => {
                default_value(reader, tables, default, default_dialect(definition))?
            }
            (None, None) => Value::Null,
        });
    }
    Ok(arguments)
}

fn default_dialect(definition: &RoutineDefinition) -> Dialect {
    match definition.dialect {
        Dialect::SqlServer => Dialect::SqlServer,
        Dialect::Oracle => Dialect::Oracle,
        _ => Dialect::PostgreSql,
    }
}

pub(crate) fn default_value(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    text: &str,
    dialect: Dialect,
) -> SqlResult<Value> {
    let query = match dialect {
        Dialect::Oracle => format!("SELECT {text} FROM dual"),
        _ => format!("SELECT {text}"),
    };
    let tokens = tokenize_in(&query, dialect)?;
    let Statement::Select(select) = Parser::new(&query, &tokens)
        .in_dialect(dialect)
        .statement()?
    else {
        return Err(SqlError::new(
            SqlState::INVALID_FUNCTION_DEFINITION,
            format!("invalid default for a parameter: {text}"),
        ));
    };
    let result = crate::execute::select(reader, tables, &select, false)?;
    Ok(result
        .rows
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or(Value::Null))
}

fn no_routine(
    kind: RoutineKind,
    name: &str,
    names: &[Option<String>],
    values: &[Value],
    untyped: &[bool],
    routines: &[Arc<RoutineDefinition>],
) -> SqlError {
    let shown = format!("{}({})", display(name), type_list(values));
    let other_kind = routines
        .iter()
        .any(|routine| routine.kind != kind && matched(routine, names, values, untyped).is_some());
    match (other_kind, kind) {
        (true, RoutineKind::Function) => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("{shown} is a procedure"),
        )
        .with_hint("To call a procedure, use CALL."),
        (true, RoutineKind::Procedure) => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("{shown} is not a procedure"),
        )
        .with_hint("To call a function, use SELECT."),
        (false, _) => SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("{} {shown} does not exist", kind.noun()),
        )
        .with_hint(format!(
            "No {} matches the given name and argument types. You might need to add explicit type casts.",
            kind.noun()
        )),
    }
}

fn type_list(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| match value {
            Value::Null => "unknown".to_owned(),
            other => other.type_name(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn procedure_result(definition: &RoutineDefinition, returned: Returned) -> QueryResult {
    let Returned::Value(value) = returned else {
        return QueryResult::command("CALL");
    };
    let Some(record) = record_of(&value) else {
        return QueryResult::command("CALL");
    };
    let outputs: Vec<&RoutineParameter> = definition
        .parameters
        .iter()
        .filter(|parameter| parameter.mode.is_output())
        .collect();
    let names = field_names(record);
    let columns = record
        .fields
        .iter()
        .enumerate()
        .map(|(at, field)| OutputColumn {
            name: outputs
                .get(at)
                .and_then(|parameter| parameter.name.clone())
                .or_else(|| names.get(at).cloned())
                .unwrap_or_else(|| format!("column{}", at + 1)),
            sql_type: outputs
                .get(at)
                .map(|parameter| parameter.sql_type)
                .or_else(|| field.sql_type()),
        })
        .collect();
    QueryResult::rows("CALL".to_owned(), columns, vec![record.fields.clone()])
}

fn output_columns(
    definition: &RoutineDefinition,
    elements: &[Value],
    label: &str,
) -> (Vec<String>, Vec<Option<SqlType>>) {
    if let RoutineReturns::Table(columns) = &definition.returns {
        return (
            columns.iter().map(|(name, _)| name.clone()).collect(),
            columns
                .iter()
                .map(|(_, sql_type)| Some(*sql_type))
                .collect(),
        );
    }
    let outputs: Vec<&RoutineParameter> = definition
        .parameters
        .iter()
        .filter(|parameter| parameter.mode.is_output())
        .collect();
    if outputs.len() > 1 {
        return (
            outputs
                .iter()
                .enumerate()
                .map(|(at, parameter)| {
                    parameter
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("column{}", at + 1))
                })
                .collect(),
            outputs
                .iter()
                .map(|parameter| Some(parameter.sql_type))
                .collect(),
        );
    }
    if let Some(fields) = record_columns(elements) {
        let types = vec![None; fields.len()];
        return (fields, types);
    }
    let name = outputs
        .first()
        .and_then(|parameter| parameter.name.clone())
        .unwrap_or_else(|| {
            if definition.dialect == Dialect::Oracle {
                ORACLE_COLUMN_VALUE.to_owned()
            } else {
                label.to_owned()
            }
        });
    (vec![name], vec![declared_type(definition)])
}

fn is_inline_tsql_table(definition: &RoutineDefinition) -> bool {
    matches!(
        definition.tsql.as_deref(),
        Some(crate::tsql::TsqlRoutine {
            returns: crate::tsql::TsqlReturns::InlineTable,
        })
    )
}

fn row_of(element: Value) -> Vec<Value> {
    match element {
        Value::Defined(defined) => match *defined {
            DefinedValue::Record(record) => record.fields,
            other => vec![Value::Defined(Box::new(other))],
        },
        other => vec![other],
    }
}

fn record_columns(elements: &[Value]) -> Option<Vec<String>> {
    let mut columns: Option<Vec<String>> = None;
    for record in elements.iter().filter_map(record_of) {
        let known = columns.get_or_insert_with(Vec::new);
        for name in field_names(record) {
            if !known.contains(&name) {
                known.push(name);
            }
        }
    }
    columns
}

fn element_rows(elements: Vec<Value>, fields: Option<&[String]>) -> SqlResult<Vec<Vec<Value>>> {
    let Some(fields) = fields else {
        return Ok(elements.into_iter().map(row_of).collect());
    };
    elements
        .into_iter()
        .map(|element| match element {
            Value::Null => Ok(vec![Value::Null; fields.len()]),
            Value::Defined(defined) => match *defined {
                DefinedValue::Record(record) => Ok(fields_in_order(record, fields)),
                _ => Err(mixed_collection_elements()),
            },
            _ => Err(mixed_collection_elements()),
        })
        .collect()
}

fn fields_in_order(record: RecordValue, columns: &[String]) -> Vec<Value> {
    let names = field_names(&record);
    if names == columns {
        return record.fields;
    }
    let mut fields: Vec<Option<Value>> = record.fields.into_iter().map(Some).collect();
    columns
        .iter()
        .map(|column| {
            names
                .iter()
                .position(|name| name == column)
                .and_then(|at| fields.get_mut(at))
                .and_then(Option::take)
                .unwrap_or(Value::Null)
        })
        .collect()
}

fn mixed_collection_elements() -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        "inconsistent datatypes: a collection in TABLE() mixes records with scalar elements",
    )
    .with_oracle_code(932)
    .with_hint("Make every element of the collection a record of the same type, or NULL.")
}

fn record_of(value: &Value) -> Option<&RecordValue> {
    match value {
        Value::Defined(defined) => match &**defined {
            DefinedValue::Record(record) => Some(record),
            _ => None,
        },
        _ => None,
    }
}

fn values_table(
    label: String,
    column_names: Vec<String>,
    rows: Vec<Vec<Value>>,
    types: &[Option<SqlType>],
    position: usize,
) -> TableReference {
    let rows: Vec<Vec<Expr>> = if rows.is_empty() {
        vec![
            types
                .iter()
                .map(|sql_type| constant(Value::Null, *sql_type, position))
                .collect(),
        ]
    } else {
        rows.into_iter()
            .map(|row| {
                row.into_iter()
                    .enumerate()
                    .map(|(at, value)| {
                        let sql_type = types.get(at).copied().flatten();
                        constant(value, sql_type, position)
                    })
                    .collect()
            })
            .collect()
    };
    TableReference {
        name: Name {
            text: label.clone(),
            position,
        },
        alias: Some(label),
        derived: None,
        generated: Some(Box::new(GeneratedTable::Values(ValuesList {
            rows,
            column_names: column_names
                .into_iter()
                .map(|text| Name { text, position })
                .collect(),
            position,
        }))),
    }
}

fn and_false(filter: Option<Expr>, position: usize) -> Expr {
    let never = Expr {
        kind: ExprKind::Literal(Literal::Boolean(false)),
        position,
    };
    match filter {
        Some(filter) => Expr {
            kind: ExprKind::And(vec![never, filter]),
            position,
        },
        None => never,
    }
}

fn constant(value: Value, sql_type: Option<SqlType>, position: usize) -> Expr {
    let sql_type = sql_type.or_else(|| value.sql_type());
    Expr {
        kind: ExprKind::Constant { value, sql_type },
        position,
    }
}

fn declared_type(definition: &RoutineDefinition) -> Option<SqlType> {
    match &definition.returns {
        RoutineReturns::One(ReturnType::Type(sql_type))
        | RoutineReturns::Set(ReturnType::Type(sql_type)) => Some(*sql_type),
        RoutineReturns::One(ReturnType::Void) => Some(SqlType::VOID),
        _ => None,
    }
}

fn mark(expr: &mut Expr, call: UserCall) {
    if call.marked {
        return;
    }
    let position = expr.position;
    let ExprKind::Function { arguments, .. } = &mut expr.kind else {
        return;
    };
    let arguments = std::mem::take(arguments);
    *expr = marked_call(call.name, call.names, arguments, position);
}

fn call_arguments(expr: &Expr) -> &[Expr] {
    match &expr.kind {
        ExprKind::Function { name, arguments } if name == ROUTINE_CALL => {
            arguments.get(2..).unwrap_or(&[])
        }
        ExprKind::Function { arguments, .. } => arguments,
        _ => &[],
    }
}

fn is_row_independent(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Subquery(_)
        | ExprKind::Aggregate { .. } => false,
        ExprKind::Function { name, .. }
            if name == ROUTINE_CALL
                || name == crate::window::WINDOW_CALL
                || name == crate::json_functions::WHOLE_ROW
                || name == crate::json_functions::ROW_OBJECT =>
        {
            false
        }
        _ => children(expr).into_iter().all(is_row_independent),
    }
}

fn could_be_routine(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('*')
}

fn calls_any_function(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == ROUTINE_CALL || could_be_routine(name))
        || children(expr).into_iter().any(calls_any_function)
}

fn function_names<'e>(expr: &'e Expr, names: &mut Vec<&'e str>) {
    if let ExprKind::Function { name, arguments } = &expr.kind {
        if name == ROUTINE_CALL {
            if let Some(Expr {
                kind: ExprKind::Literal(Literal::String(called)),
                ..
            }) = arguments.first()
            {
                names.push(called);
            }
        } else if could_be_routine(name) {
            names.push(name);
        }
    }
    for child in children(expr) {
        function_names(child, names);
    }
}

fn is_user_table_function(table: &TableReference) -> bool {
    matches!(
        table.generated.as_deref(),
        Some(GeneratedTable::Function(function))
            if crate::set_returning::named(&function.name).is_none()
    )
}

fn constant_evaluator() -> Evaluator<'static> {
    Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    }
}

fn text_literal(text: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::String(text)),
        position,
    }
}

fn joined_names(names: &[Option<String>]) -> String {
    if names.iter().all(Option::is_none) {
        return String::new();
    }
    names
        .iter()
        .map(|name| name.as_deref().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(NAME_SEPARATOR)
}

fn marked_parts(arguments: &[Expr]) -> SqlResult<MarkedCall<'_>> {
    let [name, names, rest @ ..] = arguments else {
        return Err(lost_call());
    };
    let (ExprKind::Literal(Literal::String(name)), ExprKind::Literal(Literal::String(names))) =
        (&name.kind, &names.kind)
    else {
        return Err(lost_call());
    };
    let names: Vec<Option<String>> = if names.is_empty() {
        vec![None; rest.len()]
    } else {
        names
            .split(NAME_SEPARATOR)
            .map(|name| (!name.is_empty()).then(|| name.to_owned()))
            .collect()
    };
    if names.len() != rest.len() {
        return Err(lost_call());
    }
    Ok(MarkedCall {
        name: name.clone(),
        names,
        arguments: rest,
    })
}

fn lost_call() -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "a call of a user-defined function lost its name",
    )
}

fn not_callable_here(name: &str) -> SqlError {
    SqlError::not_supported(format!(
        "calling the function {}() in this part of a statement",
        display(name)
    ))
    .with_hint("Call it in the select list or WHERE clause of a query, or in a SELECT of its own.")
}

fn set_not_allowed() -> SqlError {
    SqlError::not_supported("set-returning functions are not allowed here")
        .with_hint("Call it alone as a select-list item of a SELECT, or in FROM.")
}
