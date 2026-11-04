use crate::ast::{
    Delete, Expr, ExprKind, Insert, InsertSource, InsertValue, Name, OnDuplicate, Overriding,
    TableReference, Update,
};
use crate::constraints::{PreparedChecks, RowChange};
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult};
use crate::eval::{Evaluator, Scope};
use crate::execute::{self, RowUpdate, TableCache};
use crate::partitions::Partitioned;
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::types::{Conversion, Value, convert};
use std::sync::Arc;

pub(crate) struct MovedRow {
    from: Arc<TableDefinition>,
    row: Vec<Value>,
}

fn constant(value: Value) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position: 0,
    }
}

fn affected(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

fn retagged(tag: &str, total: u64) -> String {
    match tag.rsplit_once(' ') {
        Some((verb, _)) => format!("{verb} {total}"),
        None => tag.to_owned(),
    }
}

#[derive(Default)]
struct Merged {
    result: Option<QueryResult>,
    total: u64,
}

impl Merged {
    fn add(&mut self, part: QueryResult) {
        self.total += affected(&part.tag);
        match &mut self.result {
            None => self.result = Some(part),
            Some(result) => {
                result.rows.extend(part.rows);
                result.notices.extend(part.notices);
                result.warnings.extend(part.warnings);
                result.info_messages.extend(part.info_messages);
                result.last_insert_id = result.last_insert_id.or(part.last_insert_id);
                if part.last_insert_rowid.is_some() {
                    result.last_insert_rowid = part.last_insert_rowid;
                }
                if part.last_identity_value.is_some() {
                    result.last_identity_value = part.last_identity_value;
                }
            }
        }
    }

    fn finish(self, empty_tag: &str) -> QueryResult {
        match self.result {
            Some(mut result) => {
                result.tag = retagged(&result.tag, self.total);
                result
            }
            None => QueryResult::command(empty_tag.to_owned()),
        }
    }
}

fn reference_to(child: &str, written: &TableReference) -> TableReference {
    let alias = written
        .alias
        .clone()
        .unwrap_or_else(|| crate::schemas::base_name(&written.name.text).to_owned());
    TableReference::named(
        Name {
            text: child.to_owned(),
            position: written.name.position,
        },
        Some(alias),
    )
}

fn qualifiers(written: &TableReference) -> Vec<&str> {
    let mut found = vec![crate::schemas::base_name(&written.name.text)];
    if let Some(alias) = &written.alias {
        found.push(alias);
    }
    found
}

fn evaluated(expr: &Expr, table: &TableDefinition, column: usize) -> SqlResult<Value> {
    let sql_type = table.columns[column].sql_type;
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let value = evaluator.eval(expr, Some(sql_type))?;
    convert(value, sql_type, Conversion::Assignment)
}

pub(crate) fn insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
    table: &TableDefinition,
    partitioned: &Partitioned,
) -> SqlResult<QueryResult> {
    let targets = match &insert.columns {
        Some(names) => execute::target_columns(table, names)?,
        None => (0..table.columns.len()).collect(),
    };
    let all_defaults: Vec<Vec<InsertValue>> = vec![Vec::new()];
    let (rows, targets) = match &insert.source {
        InsertSource::Values(rows) => (rows, targets),
        InsertSource::DefaultValues => (&all_defaults, Vec::new()),
        InsertSource::Query(_) => {
            return Err(SqlError::not_supported(
                "INSERT ... SELECT into a partitioned table before its rows are selected",
            ));
        }
    };
    let mut groups: Vec<(String, Vec<usize>, Vec<Vec<InsertValue>>)> = Vec::new();
    let mut key_row = vec![Value::Null; table.columns.len()];
    let key_columns = partitioned.columns_read();
    for row in rows {
        let mut values: Vec<InsertValue> = row.clone();
        let mut columns = targets.clone();
        for &key in &key_columns {
            let value = match targets.iter().position(|&target| target == key) {
                Some(at) => {
                    let value = match row.get(at) {
                        Some(InsertValue::Expr(expr)) => evaluated(expr, table, key)?,
                        Some(InsertValue::Default) | None => execute::default_of(table, key)?,
                    };
                    if let Some(slot) = values.get_mut(at) {
                        *slot = InsertValue::Expr(constant(value.clone()));
                    }
                    value
                }
                None => {
                    let value = execute::default_of(table, key)?;
                    columns.push(key);
                    values.push(InsertValue::Expr(constant(value.clone())));
                    value
                }
            };
            if let Some(slot) = key_row.get_mut(key) {
                *slot = value;
            }
        }
        let Some(child) = partitioned.route(&key_row)? else {
            return Err(partitioned.no_partition(&key_row));
        };
        match groups
            .iter_mut()
            .find(|(name, group_columns, _)| name == child && *group_columns == columns)
        {
            Some((_, _, group)) => group.push(values),
            None => groups.push((child.to_owned(), columns, vec![values])),
        }
    }
    let mut merged = Merged::default();
    for (child, columns, rows) in groups {
        let names = columns
            .iter()
            .map(|&column| Name {
                text: table.columns[column].name.clone(),
                position: insert.table.name.position,
            })
            .collect();
        let routed = Insert {
            table: reference_to(&child, &insert.table),
            columns: Some(names),
            source: InsertSource::Values(rows),
            returning: insert.returning.clone(),
            on_duplicate: insert.on_duplicate.clone(),
            conflict_target: insert.conflict_target.clone(),
            rollback_on_conflict: insert.rollback_on_conflict,
            keeps_changes_on_conflict: insert.keeps_changes_on_conflict,
            overriding: insert.overriding,
        };
        merged.add(execute::insert(writer, tables, &routed)?);
    }
    Ok(merged.finish("INSERT 0 0"))
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
    partitioned: &Partitioned,
    moved: Option<&mut Vec<MovedRow>>,
) -> SqlResult<QueryResult> {
    let children: Vec<String> = partitioned
        .surviving(update.filter.as_ref(), &qualifiers(&update.table))
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut own_moves = Vec::new();
    let routes_moves = moved.is_none();
    let moved = moved.unwrap_or(&mut own_moves);
    let mut merged = Merged::default();
    for child in children {
        let routed = Update {
            table: reference_to(&child, &update.table),
            ..update.clone()
        };
        merged.add(execute::update_with_row_movement(
            writer,
            tables,
            &routed,
            Some(&mut *moved),
        )?);
    }
    if routes_moves {
        insert_moved_rows(writer, tables, &partitioned.table, own_moves)?;
    }
    Ok(merged.finish("UPDATE 0"))
}

pub(crate) fn move_out(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    checks: &PreparedChecks,
    changes: &mut Vec<RowUpdate>,
    old_rows: &mut Vec<Vec<Value>>,
    moved: &mut Vec<MovedRow>,
) -> SqlResult<usize> {
    if changes
        .iter()
        .all(|(_, _, new_row)| checks.fits_partition(new_row))
    {
        return Ok(0);
    }
    let kept_old_rows = !old_rows.is_empty();
    let mut old_iter = std::mem::take(old_rows).into_iter();
    let mut leaving: Vec<(Vec<u8>, Option<Vec<Value>>, Vec<Value>)> = Vec::new();
    for (old_key, new_key, new_row) in std::mem::take(changes) {
        let old_row = if kept_old_rows { old_iter.next() } else { None };
        if checks.fits_partition(&new_row) {
            changes.push((old_key, new_key, new_row));
            old_rows.extend(old_row);
        } else {
            leaving.push((old_key, old_row, new_row));
        }
    }
    let count = leaving.len();
    let mut propagated = Vec::new();
    for (old_key, old_row, new_row) in leaving {
        if let Some(old_row) = &old_row
            && !table.indexes.is_empty()
        {
            crate::indexes::delete_entries(writer, table, old_row, &old_key)?;
        }
        writer.delete(&old_key)?;
        if let Some(old_row) = old_row
            && crate::constraints::is_referenced(table)
        {
            propagated.push(RowChange {
                table: Arc::clone(table),
                old: old_row,
                new: Some(new_row.clone()),
            });
        }
        moved.push(MovedRow {
            from: Arc::clone(table),
            row: new_row,
        });
    }
    if !propagated.is_empty() {
        crate::constraints::propagate(writer, tables, propagated)?;
    }
    Ok(count)
}

fn insert_moved_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    target: &str,
    moved: Vec<MovedRow>,
) -> SqlResult<()> {
    let mut groups: Vec<(Arc<TableDefinition>, Vec<Vec<InsertValue>>)> = Vec::new();
    for MovedRow { from, row } in moved {
        let values = row
            .into_iter()
            .map(|value| InsertValue::Expr(constant(value)))
            .collect();
        match groups
            .iter_mut()
            .find(|(table, _)| Arc::ptr_eq(table, &from) || table.name == from.name)
        {
            Some((_, rows)) => rows.push(values),
            None => groups.push((from, vec![values])),
        }
    }
    for (from, rows) in groups {
        let columns = from
            .columns
            .iter()
            .map(|column| Name {
                text: column.name.clone(),
                position: 0,
            })
            .collect();
        let insert = Insert {
            table: TableReference::named(
                Name {
                    text: target.to_owned(),
                    position: 0,
                },
                None,
            ),
            columns: Some(columns),
            source: InsertSource::Values(rows),
            returning: None,
            on_duplicate: OnDuplicate::Fail,
            conflict_target: None,
            rollback_on_conflict: false,
            keeps_changes_on_conflict: false,
            overriding: Some(Overriding::SystemValue),
        };
        execute::insert(writer, tables, &insert)?;
    }
    Ok(())
}

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
    partitioned: &Partitioned,
) -> SqlResult<QueryResult> {
    let children: Vec<String> = partitioned
        .surviving(delete.filter.as_ref(), &qualifiers(&delete.table))
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut merged = Merged::default();
    for child in children {
        let routed = Delete {
            table: reference_to(&child, &delete.table),
            ..delete.clone()
        };
        merged.add(execute::delete(writer, tables, &routed)?);
    }
    Ok(merged.finish("DELETE 0"))
}
