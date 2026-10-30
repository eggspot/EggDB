use super::control::{Declaration, Program};
use super::variables::BatchContext;
use crate::ast::{
    BinaryOperator, Expr, ExprKind, FromClause, GeneratedTable, InsertValue, Name, Select,
    SelectItem, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::table_variables::TableVariable;
use crate::types::{SqlType, Value};
use std::collections::HashMap;

pub(crate) type MatchingRow = (usize, Vec<Value>);
pub(crate) type RowChange = (usize, Vec<(usize, Value)>);

pub(crate) fn undeclared(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_PARAMETER,
        format!("Must declare the scalar variable \"{name}\"."),
    )
    .with_sqlserver_code(137)
}

pub(crate) fn columns_mismatch(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "Column name or number of supplied values does not match table definition.",
    )
    .with_sqlserver_code(213)
    .at(position)
}

pub(crate) fn constant(value: Value) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    }
}

pub(crate) fn combined(operator: BinaryOperator, left: Value, right: Value) -> SqlResult<Value> {
    let expr = Expr {
        kind: ExprKind::Binary(operator, Box::new(constant(left)), Box::new(constant(right))),
        position: 0,
    };
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    evaluator.eval(&expr, None)
}

pub(crate) fn scalar_select(items: Vec<Expr>, from: Option<TableReference>) -> Select {
    Select {
        distinct: false,
        items: items
            .into_iter()
            .map(|expr| SelectItem::Expr { expr, alias: None })
            .collect(),
        from: from.map(|first| FromClause {
            first,
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

pub(crate) fn variable_table(variable: &Name) -> TableReference {
    TableReference {
        name: variable.clone(),
        alias: Some(variable.text.clone()),
        derived: None,
        generated: Some(Box::new(GeneratedTable::Variable(variable.text.clone()))),
    }
}

pub(crate) fn declare(
    context: &mut BatchContext,
    tables: &mut HashMap<String, TableVariable>,
    program: &Program,
) {
    for declaration in &program.declarations {
        match declaration {
            Declaration::Scalar(variable) => {
                if context.variable(&variable.name).is_none() {
                    context.variables.push(variable.clone());
                }
            }
            Declaration::Table { name, columns } => {
                tables
                    .entry(name.clone())
                    .or_insert_with(|| TableVariable {
                        columns: columns.clone(),
                        rows: Vec::new(),
                    });
            }
        }
    }
}

pub(crate) fn variable_column(
    variables: &HashMap<String, TableVariable>,
    variable: &Name,
    column: &Name,
) -> SqlResult<(usize, SqlType)> {
    variables
        .get(&variable.text)
        .and_then(|declared| {
            let at = declared
                .columns
                .iter()
                .position(|found| found.name == column.text)?;
            let sql_type = declared
                .columns
                .get(at)
                .and_then(|found| found.sql_type)
                .unwrap_or(SqlType::Text);
            Some((at, sql_type))
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

pub(crate) fn variable_columns(
    variables: &HashMap<String, TableVariable>,
    variable: &Name,
    columns: Option<&[Name]>,
) -> SqlResult<(Vec<(usize, SqlType)>, usize)> {
    let Some(declared) = variables.get(&variable.text) else {
        return Err(crate::table_variables::undeclared(&variable.text).at(variable.position));
    };
    let targets = match columns {
        None => declared
            .columns
            .iter()
            .enumerate()
            .map(|(at, column)| (at, column.sql_type.unwrap_or(SqlType::Text)))
            .collect(),
        Some(names) => names
            .iter()
            .map(|name| variable_column(variables, variable, name))
            .collect::<SqlResult<_>>()?,
    };
    Ok((targets, declared.columns.len()))
}

pub(crate) fn value_items(row: &[InsertValue]) -> Vec<Expr> {
    row.iter()
        .map(|value| match value {
            InsertValue::Expr(expr) => expr.clone(),
            InsertValue::Default => constant(Value::Null),
        })
        .collect()
}

pub(crate) fn filled_rows(
    targets: &[(usize, SqlType)],
    width: usize,
    rows: Vec<Vec<Value>>,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut filled: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut full = vec![Value::Null; width];
        for ((at, sql_type), value) in targets.iter().zip(row) {
            let converted = crate::sqlserver_conversion::implicit(value, *sql_type)?;
            if let Some(slot) = full.get_mut(*at) {
                *slot = converted;
            }
        }
        filled.push(full);
    }
    Ok(filled)
}

pub(crate) fn matching_select(variable: &Name, filter: Option<&Expr>, values: &[Expr]) -> Option<Select> {
    let mut items: Vec<Expr> = filter.into_iter().cloned().collect();
    items.extend(values.iter().cloned());
    if items.is_empty() {
        return None;
    }
    Some(scalar_select(items, Some(variable_table(variable))))
}

pub(crate) fn every_row(count: usize) -> Vec<MatchingRow> {
    (0..count).map(|index| (index, Vec::new())).collect()
}

pub(crate) fn matching_of(rows: Vec<Vec<Value>>, filtered: bool) -> Vec<MatchingRow> {
    rows.into_iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let mut row = row.into_iter();
            if filtered && !matches!(row.next(), Some(Value::Boolean(true))) {
                return None;
            }
            Some((index, row.collect()))
        })
        .collect()
}

pub(crate) fn changed_rows(
    columns: &[(usize, SqlType)],
    matching: Vec<MatchingRow>,
) -> SqlResult<Vec<RowChange>> {
    let mut changes: Vec<RowChange> = Vec::with_capacity(matching.len());
    for (index, row) in matching {
        let mut changed = Vec::with_capacity(columns.len());
        for ((at, sql_type), value) in columns.iter().zip(row) {
            changed.push((*at, crate::sqlserver_conversion::implicit(value, *sql_type)?));
        }
        changes.push((index, changed));
    }
    Ok(changes)
}

pub(crate) fn apply_changes(rows: &mut [Vec<Value>], changes: Vec<RowChange>) {
    for (index, changed) in changes {
        if let Some(row) = rows.get_mut(index) {
            for (at, value) in changed {
                if let Some(slot) = row.get_mut(at) {
                    *slot = value;
                }
            }
        }
    }
}

pub(crate) fn remove_rows(rows: &mut Vec<Vec<Value>>, matching: Vec<MatchingRow>) {
    let mut doomed = matching.into_iter().map(|(index, _)| index).peekable();
    let mut index = 0usize;
    rows.retain(|_| {
        let keep = doomed.peek() != Some(&index);
        if !keep {
            doomed.next();
        }
        index += 1;
        keep
    });
}
