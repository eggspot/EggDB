use crate::ast::{
    CommonTable, Expr, ExprKind, FromClause, GeneratedTable, InsertSource, InsertValue, Name,
    Query, Select, SelectItem, SetExpr, Statement, TableReference, WithStatement, WithTable,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children_mut;
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::table_variables::TableVariable;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
struct Materialized {
    name: String,
    variable: Option<String>,
}

type Frame = HashMap<String, TableVariable>;

fn variable_name(number: usize, name: &str) -> String {
    format!(
        "{}with {number} {name}",
        crate::temp_tables::HIDDEN_COLUMN_MARKER
    )
}

fn without_returning(name: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!(
        "WITH query \"{name}\" does not have a RETURNING clause"
    ))
    .with_hint("Add RETURNING to the INSERT, UPDATE or DELETE that the query names.")
    .at(position)
}

fn visible_in(query: &Query, materialized: &[Materialized]) -> Vec<Materialized> {
    materialized
        .iter()
        .filter(|found| !query.with.iter().any(|table| table.name.text == found.name))
        .cloned()
        .collect()
}

fn replace_reference(
    reference: &mut TableReference,
    materialized: &[Materialized],
) -> SqlResult<()> {
    if let Some(derived) = &mut reference.derived {
        return replace_in_query(&mut derived.query, materialized);
    }
    if let Some(generated) = &mut reference.generated {
        for expr in crate::set_returning::expressions_mut(generated) {
            replace_in_expr(expr, materialized)?;
        }
        return Ok(());
    }
    let Some(found) = materialized
        .iter()
        .rev()
        .find(|found| found.name == reference.name.text)
    else {
        return Ok(());
    };
    let Some(variable) = &found.variable else {
        return Err(without_returning(&found.name, reference.name.position));
    };
    let label = reference
        .alias
        .clone()
        .unwrap_or_else(|| found.name.clone());
    *reference = TableReference {
        name: Name {
            text: variable.clone(),
            position: reference.name.position,
        },
        alias: Some(label),
        derived: None,
        generated: Some(Box::new(GeneratedTable::Variable(variable.clone()))),
    };
    Ok(())
}

fn replace_in_expr(expr: &mut Expr, materialized: &[Materialized]) -> SqlResult<()> {
    if let ExprKind::Subquery(subquery) = &mut expr.kind {
        replace_in_query(&mut subquery.query, materialized)?;
    }
    for child in children_mut(expr) {
        replace_in_expr(child, materialized)?;
    }
    Ok(())
}

fn replace_in_select(select: &mut Select, materialized: &[Materialized]) -> SqlResult<()> {
    if let Some(from) = &mut select.from {
        replace_reference(&mut from.first, materialized)?;
        for join in &mut from.joins {
            replace_reference(&mut join.table, materialized)?;
            if let crate::ast::JoinCondition::On(on) = &mut join.condition {
                replace_in_expr(on, materialized)?;
            }
        }
    }
    for item in &mut select.items {
        if let SelectItem::Expr { expr, .. } = item {
            replace_in_expr(expr, materialized)?;
        }
    }
    let rest = select
        .filter
        .iter_mut()
        .chain(select.group_by.iter_mut())
        .chain(select.having.iter_mut())
        .chain(select.order_by.iter_mut().map(|item| &mut item.expr))
        .chain(select.limit.iter_mut())
        .chain(select.offset.iter_mut());
    for expr in rest {
        replace_in_expr(expr, materialized)?;
    }
    Ok(())
}

fn replace_in_body(body: &mut SetExpr, materialized: &[Materialized]) -> SqlResult<()> {
    match body {
        SetExpr::Select(select) => replace_in_select(select, materialized),
        SetExpr::Operation(operation) => {
            replace_in_body(&mut operation.left, materialized)?;
            replace_in_body(&mut operation.right, materialized)
        }
        SetExpr::Query(query) => replace_in_query(query, materialized),
    }
}

fn replace_in_query(query: &mut Query, materialized: &[Materialized]) -> SqlResult<()> {
    let visible = visible_in(query, materialized);
    for table in &mut query.with {
        replace_in_query(&mut table.query, &visible)?;
    }
    replace_in_body(&mut query.body, &visible)?;
    let rest = query
        .order_by
        .iter_mut()
        .map(|item| &mut item.expr)
        .chain(query.limit.iter_mut())
        .chain(query.offset.iter_mut());
    for expr in rest {
        replace_in_expr(expr, &visible)?;
    }
    Ok(())
}

fn replace_in_returning(
    returning: &mut Option<Vec<SelectItem>>,
    materialized: &[Materialized],
) -> SqlResult<()> {
    for item in returning.iter_mut().flatten() {
        if let SelectItem::Expr { expr, .. } = item {
            replace_in_expr(expr, materialized)?;
        }
    }
    Ok(())
}

fn replace_in_statement(statement: &mut Statement, materialized: &[Materialized]) -> SqlResult<()> {
    match statement {
        Statement::Select(select) => replace_in_select(select, materialized),
        Statement::Query(query) => replace_in_query(query, materialized),
        Statement::Insert(insert) => {
            match &mut insert.source {
                InsertSource::Query(query) => replace_in_query(query, materialized)?,
                InsertSource::Values(rows) => {
                    for value in rows.iter_mut().flatten() {
                        if let InsertValue::Expr(expr) = value {
                            replace_in_expr(expr, materialized)?;
                        }
                    }
                }
                InsertSource::DefaultValues => {}
            }
            replace_in_returning(&mut insert.returning, materialized)
        }
        Statement::Update(update) => {
            for (_, value) in &mut update.assignments {
                if let InsertValue::Expr(expr) = value {
                    replace_in_expr(expr, materialized)?;
                }
            }
            if let Some(filter) = &mut update.filter {
                replace_in_expr(filter, materialized)?;
            }
            replace_in_returning(&mut update.returning, materialized)
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &mut delete.filter {
                replace_in_expr(filter, materialized)?;
            }
            replace_in_returning(&mut delete.returning, materialized)
        }
        _ => Ok(()),
    }
}

fn table_query(table: &CommonTable, recursive: bool) -> Query {
    let position = table.name.position;
    let select = Select {
        distinct: false,
        items: vec![SelectItem::Wildcard {
            qualifier: None,
            position,
        }],
        from: Some(FromClause {
            first: TableReference::named(table.name.clone(), None),
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    };
    Query {
        with: vec![table.clone()],
        recursive,
        body: SetExpr::Select(Box::new(select)),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        position,
    }
}

fn renamed(columns: Vec<OutputColumn>, names: &[Name], table: &Name) -> SqlResult<Vec<OutputColumn>> {
    if names.len() > columns.len() {
        let position = names.get(columns.len()).map_or(table.position, |name| name.position);
        return Err(SqlError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            format!(
                "WITH query \"{}\" has {} columns available but {} columns specified",
                table.text,
                columns.len(),
                names.len()
            ),
        )
        .at(position));
    }
    Ok(columns
        .into_iter()
        .enumerate()
        .map(|(index, column)| match names.get(index) {
            Some(name) => OutputColumn {
                name: name.text.clone(),
                sql_type: column.sql_type,
            },
            None => column,
        })
        .collect())
}

fn entered(frame: &Frame) -> crate::table_variables::EnteredVariables {
    crate::table_variables::enter(&Arc::new(frame.clone()))
}

fn bind(
    binding: &Binding<'_>,
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    statement: &mut Statement,
) -> SqlResult<()> {
    match statement {
        Statement::Select(select) => binding.select(reader, tables, select),
        Statement::Query(query) => binding.query(reader, tables, query),
        Statement::Insert(insert) => binding.insert(reader, tables, insert),
        Statement::Update(update) => binding.update(reader, tables, update),
        Statement::Delete(delete) => binding.delete(reader, tables, delete),
        _ => Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a WITH statement holds a statement it cannot run",
        )),
    }
}

fn run_statement(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    statement: &Statement,
) -> SqlResult<QueryResult> {
    match statement {
        Statement::Select(select) => execute::select(writer, tables, select, false),
        Statement::Query(query) => execute::query(writer, tables, query, false),
        Statement::Insert(insert) => execute::insert(writer, tables, insert),
        Statement::Update(update) => execute::update(writer, tables, update),
        Statement::Delete(delete) => execute::delete(writer, tables, delete),
        _ => Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a WITH statement holds a statement it cannot run",
        )),
    }
}

fn described(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    statement: &Statement,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    match statement {
        Statement::Select(select) => Ok(execute::select(reader, tables, select, true)?.columns),
        Statement::Query(query) => Ok(execute::query(reader, tables, query, true)?.columns),
        Statement::Insert(insert) => {
            execute::returning_columns(reader, tables, &insert.table, &insert.returning)
        }
        Statement::Update(update) => {
            execute::returning_columns(reader, tables, &update.table, &update.returning)
        }
        Statement::Delete(delete) => {
            execute::returning_columns(reader, tables, &delete.table, &delete.returning)
        }
        _ => Ok(None),
    }
}

fn record(
    frame: &mut Frame,
    materialized: &mut Vec<Materialized>,
    number: usize,
    name: &Name,
    column_names: &[Name],
    result: Option<QueryResult>,
) -> SqlResult<()> {
    let variable = match result.and_then(|result| result.columns.map(|columns| (columns, result.rows)))
    {
        Some((columns, rows)) => {
            let variable = variable_name(number, &name.text);
            frame.insert(
                variable.clone(),
                TableVariable {
                    columns: renamed(columns, column_names, name)?,
                    rows,
                },
            );
            Some(variable)
        }
        None => None,
    };
    materialized.push(Materialized {
        name: name.text.clone(),
        variable,
    });
    Ok(())
}

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    with: &WithStatement,
    binding: &Binding<'_>,
) -> SqlResult<QueryResult> {
    let mut frame: Frame = (*crate::table_variables::current()).clone();
    let mut materialized: Vec<Materialized> = Vec::with_capacity(with.tables.len());
    for (number, table) in with.tables.iter().enumerate() {
        match table {
            WithTable::Query(common) => {
                let mut statement = Statement::Query(Box::new(table_query(common, with.recursive)));
                replace_in_statement(&mut statement, &materialized)?;
                let result = {
                    let _entered = entered(&frame);
                    bind(binding, writer, tables, &mut statement)?;
                    run_statement(writer, tables, &statement)?
                };
                record(
                    &mut frame,
                    &mut materialized,
                    number,
                    &common.name,
                    &common.column_names,
                    Some(result),
                )?;
            }
            WithTable::Modifying {
                name,
                column_names,
                statement,
            } => {
                let mut statement = statement.clone();
                replace_in_statement(&mut statement, &materialized)?;
                let result = {
                    let _entered = entered(&frame);
                    bind(binding, writer, tables, &mut statement)?;
                    run_statement(writer, tables, &statement)?
                };
                record(
                    &mut frame,
                    &mut materialized,
                    number,
                    name,
                    column_names,
                    Some(result),
                )?;
            }
        }
    }
    let mut body = with.body.clone();
    replace_in_statement(&mut body, &materialized)?;
    let _entered = entered(&frame);
    bind(binding, writer, tables, &mut body)?;
    run_statement(writer, tables, &body)
}

pub(crate) fn describe(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    with: &WithStatement,
    binding: &Binding<'_>,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    let mut frame: Frame = (*crate::table_variables::current()).clone();
    let mut materialized: Vec<Materialized> = Vec::with_capacity(with.tables.len());
    for (number, table) in with.tables.iter().enumerate() {
        let (name, column_names, mut statement) = match table {
            WithTable::Query(common) => (
                &common.name,
                &common.column_names,
                Statement::Query(Box::new(table_query(common, with.recursive))),
            ),
            WithTable::Modifying {
                name,
                column_names,
                statement,
            } => (name, column_names, statement.clone()),
        };
        replace_in_statement(&mut statement, &materialized)?;
        let columns = {
            let _entered = entered(&frame);
            bind(binding, reader, tables, &mut statement)?;
            described(reader, tables, &statement)?
        };
        let result = columns.map(|columns| QueryResult::rows("SELECT 0".to_owned(), columns, Vec::new()));
        record(&mut frame, &mut materialized, number, name, column_names, result)?;
    }
    let mut body = with.body.clone();
    replace_in_statement(&mut body, &materialized)?;
    let _entered = entered(&frame);
    bind(binding, reader, tables, &mut body)?;
    described(reader, tables, &body)
}

pub(crate) fn classify(with: &WithStatement) -> Option<crate::destructive::DestructiveStatement> {
    crate::destructive::classify(&with.body).or_else(|| {
        with.tables.iter().find_map(|table| match table {
            WithTable::Modifying { statement, .. } => crate::destructive::classify(statement),
            WithTable::Query(_) => None,
        })
    })
}
