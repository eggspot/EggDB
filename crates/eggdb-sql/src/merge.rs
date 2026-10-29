use crate::ast::{
    Expr, ExprKind, Insert, InsertSource, InsertValue, MergeStatement, Name, OnDuplicate,
};
use crate::constraints::PreparedChecks;
use crate::encoding::{Column, TableDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, Sources, check_columns, reject_aggregates};
use crate::execute::{self, TableCache};
use crate::memory::WorkMemory;
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::types::{SqlType, Value};

fn unstable_rows() -> SqlError {
    SqlError::new(
        SqlState::CARDINALITY_VIOLATION,
        "unable to get a stable set of rows in the source tables",
    )
    .with_oracle_code(30926)
}

fn on_column_updated(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_COLUMN_REFERENCE,
        format!(
            "Columns referenced in the ON Clause cannot be updated: \"{}\"",
            crate::lexer::swap_uniform_case(name)
        ),
    )
    .with_oracle_code(38104)
    .at(position)
}

fn subquery_refused(position: usize) -> SqlError {
    SqlError::not_supported("subqueries in the ON, SET, VALUES and WHERE clauses of MERGE")
        .at(position)
}

fn source_definition(columns: &[crate::session::OutputColumn], label: &str) -> TableDefinition {
    TableDefinition {
        name: label.to_owned(),
        columns: columns
            .iter()
            .map(|column| Column {
                name: column.name.clone(),
                sql_type: column.sql_type.unwrap_or(SqlType::Text),
                not_null: false,
                default: None,
                auto_increment: false,
            })
            .collect(),
        primary_key: Vec::new(),
        ..TableDefinition::default()
    }
}

fn target_columns_in(expr: &Expr, scope: Scope<'_>, width: usize, found: &mut Vec<usize>) {
    if let ExprKind::Column { qualifier, name } = &expr.kind
        && let Ok(offset) = scope.column(qualifier.as_deref(), name, expr.position)
        && offset < width
    {
        found.push(offset);
    }
    for child in crate::eval::children(expr) {
        target_columns_in(child, scope, width, found);
    }
}

fn clause_exprs(merge: &MergeStatement) -> Vec<&Expr> {
    let mut exprs = vec![&merge.on];
    if let Some(update) = &merge.update {
        exprs.extend(update.assignments.iter().map(|(_, expr)| expr));
        exprs.extend(update.condition.iter());
        exprs.extend(update.delete_condition.iter());
    }
    if let Some(insert) = &merge.insert {
        exprs.extend(insert.values.iter());
        exprs.extend(insert.condition.iter());
    }
    exprs
}

fn target_index(table: &TableDefinition, name: &Name) -> SqlResult<usize> {
    table.column_index(&name.text).ok_or_else(|| {
        SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{}\" does not exist", name.text),
        )
        .at(name.position)
    })
}

struct Joined<'a> {
    scope: Scope<'a>,
    target_width: usize,
}

impl Joined<'_> {
    fn row(&self, target: &[Value], source: &[Value]) -> Vec<Value> {
        let mut row = Vec::with_capacity(target.len() + source.len());
        if target.is_empty() {
            row.resize(self.target_width, Value::Null);
        } else {
            row.extend_from_slice(target);
        }
        row.extend_from_slice(source);
        row
    }

    fn holds(&self, condition: Option<&Expr>, row: &[Value], clause: &str) -> SqlResult<bool> {
        let Some(condition) = condition else {
            return Ok(true);
        };
        let evaluator = Evaluator {
            scope: self.scope,
            row,
            correlation: None,
        };
        Ok(evaluator.boolean(condition, clause)? == Some(true))
    }

    fn value(&self, expr: &Expr, row: &[Value], hint: Option<SqlType>) -> SqlResult<Value> {
        let evaluator = Evaluator {
            scope: self.scope,
            row,
            correlation: None,
        };
        evaluator.eval(expr, hint)
    }
}

/// Runs `MERGE`: the source is read once, every target row is joined with the source rows
/// its `ON` condition matches, matched rows are updated (more than one source row for a target
/// row is ORA-30926) and then deleted where `DELETE WHERE` holds, and source rows that match no
/// target row are inserted where the insert's `WHERE` holds. The result is `MERGE n`, the rows
/// updated plus the rows inserted.
pub(crate) fn merge(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    merge: &MergeStatement,
) -> SqlResult<QueryResult> {
    for expr in clause_exprs(merge) {
        if crate::subquery::mentions_subquery(expr) {
            return Err(subquery_refused(expr.position));
        }
        reject_aggregates(expr, "MERGE")?;
    }
    let source = execute::select(writer, tables, &merge.source, false)?;
    let source_columns = source.columns.clone().unwrap_or_default();
    let mut memory = WorkMemory::new();
    for row in &source.rows {
        memory.charge_row(row)?;
    }
    let table = tables.require_target(writer, &merge.target.name, "merge into")?;
    let label = merge
        .target
        .alias
        .clone()
        .unwrap_or_else(|| table.name.clone());
    let source_label = merge.source_label.clone().unwrap_or_default();
    let source_table = source_definition(&source_columns, &source_label);
    let target_width = table.columns.len();
    let sources = Sources {
        list: vec![
            Source {
                table: &table,
                label: &label,
                offset: 0,
            },
            Source {
                table: &source_table,
                label: &source_label,
                offset: target_width,
            },
        ],
        merged: Vec::new(),
        out_of_reach: Vec::new(),
        star: (0..target_width + source_table.columns.len()).collect(),
    };
    let joined = Joined {
        scope: Scope::Many(&sources),
        target_width,
    };
    for expr in clause_exprs(merge) {
        check_columns(expr, joined.scope)?;
    }
    let mut updated_columns = Vec::new();
    if let Some(update) = &merge.update {
        let mut on_columns = Vec::new();
        target_columns_in(&merge.on, joined.scope, target_width, &mut on_columns);
        for (name, _) in &update.assignments {
            let index = target_index(&table, name)?;
            if on_columns.contains(&index) {
                return Err(on_column_updated(&name.text, name.position));
            }
            if updated_columns.contains(&index) {
                return Err(
                    SqlError::new(SqlState::DUPLICATE_COLUMN, "duplicate column name")
                        .with_oracle_code(957)
                        .at(name.position),
                );
            }
            updated_columns.push(index);
        }
    }
    let target_rows = execute::every_row(writer, &table)?;
    let mut matched_sources = vec![false; source.rows.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (target_index, (_, target_row)) in target_rows.iter().enumerate() {
        let mut chosen = None;
        for (source_index, source_row) in source.rows.iter().enumerate() {
            let row = joined.row(target_row, source_row);
            if !joined.holds(Some(&merge.on), &row, "ON")? {
                continue;
            }
            matched_sources[source_index] = true;
            let Some(update) = &merge.update else {
                continue;
            };
            if !joined.holds(update.condition.as_ref(), &row, "WHERE")? {
                continue;
            }
            if chosen.is_some() {
                return Err(unstable_rows());
            }
            chosen = Some(source_index);
        }
        if let Some(source_index) = chosen {
            memory.charge(std::mem::size_of::<(usize, usize)>())?;
            pairs.push((target_index, source_index));
        }
    }
    let mut affected = 0usize;
    if let Some(update) = &merge.update {
        let checks = PreparedChecks::of(&table)?;
        let rule = execute::assignment_rule(crate::constraints::current_dialect());
        let mut changes: Vec<execute::RowUpdate> = Vec::with_capacity(pairs.len());
        let mut old_rows = Vec::with_capacity(pairs.len());
        for &(target_index, source_index) in &pairs {
            let (old_key, old_row) = &target_rows[target_index];
            let row = joined.row(old_row, &source.rows[source_index]);
            let mut new_row = old_row.clone();
            for ((_, expr), &index) in update.assignments.iter().zip(&updated_columns) {
                let column = &table.columns[index];
                let value = joined.value(expr, &row, Some(column.sql_type))?;
                new_row[index] =
                    rule(value, column).map_err(|error| error.at_if_unplaced(expr.position))?;
            }
            execute::check_not_null_of(&table, &new_row)?;
            if !checks.is_empty() {
                checks.check(&table, &new_row)?;
            }
            let new_key = if table.primary_key.is_empty() {
                old_key.clone()
            } else {
                execute::primary_key_of(&table, &new_row)
            };
            memory.charge_row(&new_row)?;
            old_rows.push(old_row.clone());
            changes.push((old_key.clone(), new_key, new_row));
        }
        execute::write_updated_rows(writer, tables, &table, &changes, &old_rows)?;
        affected += changes.len();
        if let Some(condition) = &update.delete_condition {
            let mut deleted = Vec::new();
            for ((_, new_key, new_row), &(_, source_index)) in changes.into_iter().zip(&pairs) {
                let row = joined.row(&new_row, &source.rows[source_index]);
                if joined.holds(Some(condition), &row, "DELETE WHERE")? {
                    deleted.push((new_key, new_row));
                }
            }
            execute::delete_rows(writer, tables, &table, deleted)?;
        }
    }
    if let Some(insert) = &merge.insert {
        let targets: Vec<usize> = match &insert.columns {
            Some(names) => names
                .iter()
                .map(|name| target_index(&table, name))
                .collect::<SqlResult<_>>()?,
            None => (0..target_width).collect(),
        };
        if targets.len() != insert.values.len() {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                if targets.len() > insert.values.len() {
                    "not enough values"
                } else {
                    "too many values"
                },
            )
            .with_oracle_code(if targets.len() > insert.values.len() {
                947
            } else {
                913
            })
            .at(merge.target.name.position));
        }
        let mut rows = Vec::new();
        for (source_index, source_row) in source.rows.iter().enumerate() {
            if matched_sources[source_index] {
                continue;
            }
            let row = joined.row(&[], source_row);
            if !joined.holds(insert.condition.as_ref(), &row, "WHERE")? {
                continue;
            }
            let mut values = Vec::with_capacity(targets.len());
            for (expr, &index) in insert.values.iter().zip(&targets) {
                let sql_type = table.columns[index].sql_type;
                let value = joined.value(expr, &row, Some(sql_type))?;
                values.push(InsertValue::Expr(Expr {
                    kind: ExprKind::Constant {
                        sql_type: value.sql_type(),
                        value,
                    },
                    position: expr.position,
                }));
            }
            rows.push(values);
        }
        if !rows.is_empty() {
            let columns = targets
                .iter()
                .map(|&index| Name {
                    text: table.columns[index].name.clone(),
                    position: merge.target.name.position,
                })
                .collect();
            let count = rows.len();
            execute::insert(
                writer,
                tables,
                &Insert {
                    table: merge.target.clone(),
                    columns: Some(columns),
                    source: InsertSource::Values(rows),
                    returning: None,
                    on_duplicate: OnDuplicate::Fail,
                    rollback_on_conflict: false,
                },
            )?;
            affected += count;
        }
    }
    Ok(QueryResult::command(format!("MERGE {affected}")))
}
