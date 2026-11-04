use crate::ast::{
    Insert, InsertSource, InsertValue, Name, OnDuplicate, OutputInto, OutputTarget, Statement,
    TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::table_variables::TableVariable;
use crate::types::{SqlType, Value};

pub(crate) const END_OF_OUTPUT_INTO: &str = "*end of output into*";

struct SplitOutput {
    width: usize,
    kept: Vec<Vec<Value>>,
    returned: QueryResult,
}

pub(crate) fn command(statement: &Statement) -> &'static str {
    match statement {
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        _ => "MERGE",
    }
}

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    output: &mut OutputInto,
    variable: Option<&mut TableVariable>,
) -> SqlResult<QueryResult> {
    let changed = changed_rows(writer, tables, binding, &mut output.statement)?;
    let split = split_at_end(changed)?;
    let columns = output.destination.columns.as_deref();
    if columns.is_some_and(|columns| columns.len() != split.width) {
        return Err(crate::tsql::columns_mismatch(
            columns
                .and_then(<[Name]>::first)
                .map_or(0, |name| name.position),
        ));
    }
    match (&output.destination.target, variable) {
        (OutputTarget::Variable(name), Some(variable)) => {
            append_to_variable(variable, name, columns, split.width, split.kept)?;
        }
        (OutputTarget::Variable(name), None) => {
            return Err(crate::table_variables::undeclared(&name.text).at(name.position));
        }
        (OutputTarget::Table(table), _) => {
            insert_into_table(writer, tables, binding, table, columns, split.kept)?;
        }
    }
    Ok(split.returned)
}

fn changed_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    statement: &mut Statement,
) -> SqlResult<QueryResult> {
    let binding = Binding {
        settings: binding.settings,
        walk_expressions: binding.walk_expressions,
        target_privilege: Some(crate::roles::target_privilege(statement)),
    };
    let settings = binding.settings;
    match statement {
        Statement::Insert(insert) => {
            binding.insert(&*writer, tables, insert)?;
            if crate::routines::insert_calls_marked_routine(insert) {
                crate::routines::prepare_insert(writer, tables, settings, insert)?;
            }
            execute::insert(writer, tables, insert)
        }
        Statement::Update(update) => {
            binding.update(&*writer, tables, update)?;
            if crate::routines::exprs_call_marked_routine(execute::update_exprs(update)) {
                crate::routines::prepare_update(writer, tables, settings, update)?;
            }
            execute::update(writer, tables, update)
        }
        Statement::Delete(delete) => {
            binding.delete(&*writer, tables, delete)?;
            if crate::routines::exprs_call_marked_routine(execute::delete_exprs(delete)) {
                crate::routines::prepare_delete(writer, tables, settings, delete)?;
            }
            execute::delete(writer, tables, delete)
        }
        Statement::Merge(merge) => {
            binding.merge(&*writer, tables, merge)?;
            crate::merge::merge(writer, tables, merge)
        }
        _ => Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "OUTPUT ... INTO follows a statement that changes no rows",
        )),
    }
}

fn split_at_end(mut result: QueryResult) -> SqlResult<SplitOutput> {
    let mut columns = result.columns.take().unwrap_or_default();
    let Some(width) = columns
        .iter()
        .position(|column| column.name == END_OF_OUTPUT_INTO)
    else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "the rows of OUTPUT ... INTO lost their end",
        ));
    };
    let returned_columns = columns.split_off(width + 1);
    let rows = std::mem::take(&mut result.rows);
    let mut kept = Vec::with_capacity(rows.len());
    let mut returned = Vec::new();
    for mut row in rows {
        let rest = row.split_off(width.min(row.len()));
        kept.push(row);
        if !returned_columns.is_empty() {
            returned.push(rest.into_iter().skip(1).collect());
        }
    }
    if !returned_columns.is_empty() {
        result.columns = Some(returned_columns);
        result.rows = returned;
    }
    Ok(SplitOutput {
        width,
        kept,
        returned: result,
    })
}

fn variable_column(variable: &TableVariable, column: &Name) -> SqlResult<(usize, SqlType)> {
    variable
        .columns
        .iter()
        .position(|found| found.name == column.text)
        .map(|at| {
            let sql_type = variable.columns[at].sql_type.unwrap_or(SqlType::Text);
            (at, sql_type)
        })
        .ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!("Invalid column name '{}'.", column.text),
            )
            .with_sqlserver_code(207)
            .at(column.position)
        })
}

fn append_to_variable(
    variable: &mut TableVariable,
    name: &Name,
    columns: Option<&[Name]>,
    width: usize,
    rows: Vec<Vec<Value>>,
) -> SqlResult<()> {
    let targets: Vec<(usize, SqlType)> = match columns {
        Some(names) => names
            .iter()
            .map(|column| variable_column(variable, column))
            .collect::<SqlResult<_>>()?,
        None => variable
            .columns
            .iter()
            .enumerate()
            .map(|(at, column)| (at, column.sql_type.unwrap_or(SqlType::Text)))
            .collect(),
    };
    if targets.len() != width {
        return Err(crate::tsql::columns_mismatch(name.position));
    }
    let appended = crate::tsql::filled_rows(&targets, variable.columns.len(), rows)?;
    variable.change_rows(&name.text, |rows| rows.extend(appended))
}

fn insert_into_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    table: &TableReference,
    columns: Option<&[Name]>,
    rows: Vec<Vec<Value>>,
) -> SqlResult<()> {
    let filled = !rows.is_empty();
    let mut insert = Insert {
        table: table.clone(),
        columns: columns.map(<[Name]>::to_vec),
        source: InsertSource::Values(
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| InsertValue::Expr(crate::tsql::constant(value)))
                        .collect()
                })
                .collect(),
        ),
        returning: None,
        on_duplicate: OnDuplicate::Fail,
        conflict_target: None,
        rollback_on_conflict: false,
        keeps_changes_on_conflict: false,
        overriding: None,
    };
    let binding = Binding {
        settings: binding.settings,
        walk_expressions: binding.walk_expressions,
        target_privilege: Some(crate::roles::Privilege::Insert),
    };
    binding.insert(&*writer, tables, &mut insert)?;
    if filled {
        execute::insert(writer, tables, &insert)?;
    } else {
        tables.require_target(&*writer, &insert.table.name, "insert into")?;
    }
    Ok(())
}
