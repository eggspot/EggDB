use crate::ast::{
    Delete, Expr, ExprKind, FromClause, InsertValue, Join, JoinCondition, JoinKind, JoinedTables,
    Name, Select, SelectItem, TableReference, Update,
};
use crate::encoding::{TableDefinition, encode_row};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, Sources, check_columns, reject_aggregates};
use crate::execute::{self, TableCache};
use crate::memory::WorkMemory;
use crate::merge::{PlannedUpdate, source_definition, store_updates};
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::triggers::{Firing, RowOutcome, TriggerEvent};
use crate::types::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

struct Item {
    label: String,
    definition: Arc<TableDefinition>,
    stored: bool,
    offset: usize,
}

impl Item {
    fn columns(&self) -> std::ops::Range<usize> {
        self.offset..self.offset + self.definition.columns.len()
    }
}

struct JoinedSource {
    from: FromClause,
    items: Vec<Item>,
}

struct Matched {
    key: Vec<u8>,
    joined_row: usize,
}

fn subquery_refused(clause: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!(
        "subqueries in the {clause} of an UPDATE or DELETE that joins other tables"
    ))
    .at(position)
}

fn not_writable(verb: &str, item: &Item, position: usize) -> SqlError {
    SqlError::not_supported(format!(
        "{verb} \"{}\" joined with other tables when it is a view, a subquery or a function",
        item.label
    ))
    .at(position)
}

fn width_mismatch() -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "the joined rows of an UPDATE or DELETE do not have the joined tables' columns",
    )
}

pub(crate) fn full_from(target: &TableReference, joined: &JoinedTables) -> FromClause {
    if joined.target_at.is_some() {
        return joined.tables.clone();
    }
    let mut joins = Vec::with_capacity(joined.tables.joins.len() + 1);
    joins.push(Join {
        kind: JoinKind::Comma,
        table: joined.tables.first.clone(),
        condition: JoinCondition::Always,
    });
    joins.extend(joined.tables.joins.iter().cloned());
    FromClause {
        first: target.clone(),
        joins,
    }
}

pub(crate) fn from_position(joined: &JoinedTables, listed_at: usize) -> usize {
    match joined.target_at {
        Some(_) => listed_at,
        None => listed_at + 1,
    }
}

fn target_position(joined: &JoinedTables) -> usize {
    joined.target_at.unwrap_or(0)
}

fn references(from: &FromClause) -> impl Iterator<Item = &TableReference> {
    std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table))
}

pub(crate) fn stored_table(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    reference: &TableReference,
) -> SqlResult<Option<Arc<TableDefinition>>> {
    if reference.derived.is_some()
        || reference.generated.is_some()
        || crate::updatable_views::is_view(reader, tables, reference)?
    {
        return Ok(None);
    }
    tables.table(reader, &reference.name.text)
}

fn every_column_select(from: FromClause, items: Vec<SelectItem>, filter: Option<Expr>) -> Select {
    Select {
        distinct: false,
        items,
        from: Some(from),
        filter,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
        grouping_sets: None,
        distinct_on: Vec::new(),
    }
}

fn listed_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    from: &FromClause,
    label: &str,
    position: usize,
) -> SqlResult<TableDefinition> {
    let select = every_column_select(
        from.clone(),
        vec![SelectItem::Wildcard {
            qualifier: Some(label.to_owned()),
            position,
        }],
        None,
    );
    let described = execute::select(reader, tables, &select, true)?;
    Ok(source_definition(
        &described.columns.unwrap_or_default(),
        label,
    ))
}

fn joined_source(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    from: FromClause,
) -> SqlResult<JoinedSource> {
    let mut items = Vec::new();
    let mut offset = 0;
    for reference in references(&from) {
        let item = match stored_table(reader, tables, reference)? {
            Some(definition) => Item {
                label: reference
                    .alias
                    .clone()
                    .unwrap_or_else(|| definition.name.clone()),
                definition,
                stored: true,
                offset,
            },
            None => {
                let label = reference
                    .alias
                    .clone()
                    .unwrap_or_else(|| reference.name.text.clone());
                let definition = Arc::new(listed_columns(
                    reader,
                    tables,
                    &from,
                    &label,
                    reference.name.position,
                )?);
                Item {
                    label,
                    definition,
                    stored: false,
                    offset,
                }
            }
        };
        offset += item.definition.columns.len();
        items.push(item);
    }
    Ok(JoinedSource { from, items })
}

impl JoinedSource {
    fn width(&self) -> usize {
        self.items.last().map_or(0, |item| item.columns().end)
    }

    fn sources(&self) -> Sources<'_> {
        let list: Vec<Source<'_>> = self
            .items
            .iter()
            .map(|item| Source {
                table: &item.definition,
                label: &item.label,
                offset: item.offset,
            })
            .collect();
        let star = self
            .items
            .iter()
            .flat_map(|item| {
                item.definition
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| !crate::temp_tables::is_hidden_column(&column.name))
                    .map(move |(index, _)| item.offset + index)
            })
            .collect();
        Sources {
            list,
            merged: Vec::new(),
            out_of_reach: Vec::new(),
            star,
        }
    }

    fn selection(&self, filter: Option<&Expr>, position: usize) -> Select {
        let mut items = Vec::with_capacity(self.width());
        for item in &self.items {
            if !item.stored {
                items.push(SelectItem::Wildcard {
                    qualifier: Some(item.label.clone()),
                    position,
                });
                continue;
            }
            for column in &item.definition.columns {
                items.push(SelectItem::Expr {
                    expr: Expr {
                        kind: ExprKind::Column {
                            qualifier: Some(item.label.clone()),
                            name: column.name.clone(),
                        },
                        position,
                    },
                    alias: None,
                });
            }
        }
        every_column_select(self.from.clone(), items, filter.cloned())
    }

    fn joined_rows(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        filter: Option<&Expr>,
        position: usize,
        memory: &mut WorkMemory,
    ) -> SqlResult<Vec<Vec<Value>>> {
        let selected = execute::select(reader, tables, &self.selection(filter, position), false)?;
        let width = self.width();
        for row in &selected.rows {
            if row.len() != width {
                return Err(width_mismatch());
            }
            memory.charge_row(row)?;
        }
        Ok(selected.rows)
    }

    fn writable(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        at: usize,
        verb: &str,
        position: usize,
    ) -> SqlResult<&Item> {
        let Some(item) = self.items.get(at) else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "an UPDATE or DELETE writes a table it does not join",
            ));
        };
        if !item.stored {
            return Err(not_writable(verb, item, position));
        }
        if crate::partitions::partitioned(reader, tables, &item.definition)?.is_some() {
            return Err(SqlError::not_supported(format!(
                "{verb} a partitioned table joined with other tables"
            ))
            .at(position));
        }
        Ok(item)
    }
}

fn matched_rows(
    reader: &dyn KeyValueRead,
    item: &Item,
    joined_rows: &[Vec<Value>],
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Matched>> {
    let table = &item.definition;
    let by_content = if table.primary_key.is_empty() {
        let mut keys: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
        for (key, row) in execute::every_row(reader, table)? {
            let content = encode_row(&row);
            memory.charge(content.len() + key.len())?;
            keys.entry(content).or_default().push(key);
        }
        Some(keys)
    } else {
        None
    };
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut matched = Vec::new();
    for (joined_row, row) in joined_rows.iter().enumerate() {
        let part = &row[item.columns()];
        let keys = match &by_content {
            Some(by_content) => by_content
                .get(&encode_row(part))
                .cloned()
                .unwrap_or_default(),
            None if table.primary_key.iter().any(|&index| part[index].is_null()) => Vec::new(),
            None => vec![execute::row_key(table, part)],
        };
        for key in keys {
            if seen.contains(&key) {
                continue;
            }
            memory.charge(key.len() * 2)?;
            seen.insert(key.clone());
            matched.push(Matched { key, joined_row });
        }
    }
    Ok(matched)
}

fn refuse_subqueries<'e>(exprs: impl IntoIterator<Item = &'e Expr>, clause: &str) -> SqlResult<()> {
    for expr in exprs {
        if crate::subquery::mentions_subquery(expr) {
            return Err(subquery_refused(clause, expr.position));
        }
    }
    Ok(())
}

fn returning_exprs(returning: &Option<Vec<SelectItem>>) -> impl Iterator<Item = &Expr> {
    returning.iter().flatten().filter_map(|item| match item {
        SelectItem::Expr { expr, .. } => Some(expr),
        SelectItem::Wildcard { .. } => None,
    })
}

fn ambiguous_column(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::AMBIGUOUS_COLUMN,
        format!("Column '{}' in field list is ambiguous", name.text),
    )
    .with_vendor_code(1052)
    .at(name.position)
}

fn unknown_column(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("Unknown column '{}' in 'field list'", name.text),
    )
    .with_vendor_code(1054)
    .at(name.position)
}

pub(crate) fn column_owner(
    references: &[(usize, Arc<TableDefinition>)],
    name: &Name,
) -> SqlResult<usize> {
    let owners: Vec<usize> = references
        .iter()
        .filter(|(_, table)| table.column_index(&name.text).is_some())
        .map(|(at, _)| *at)
        .collect();
    match owners.as_slice() {
        [owner] => Ok(*owner),
        [] => Err(unknown_column(name)),
        _ => Err(ambiguous_column(name)),
    }
}

fn assigned_items(source: &JoinedSource, update: &Update, joined: &JoinedTables) -> Vec<usize> {
    let main = target_position(joined);
    (0..update.assignments.len())
        .map(|at| match joined.assigned_at.get(at).copied().flatten() {
            Some(listed_at) => from_position(joined, listed_at).min(source.items.len()),
            None => main,
        })
        .collect()
}

fn with_target_row(joined_row: &[Value], item: &Item, target_row: &[Value]) -> Vec<Value> {
    let mut row = joined_row.to_vec();
    for (slot, value) in row.iter_mut().skip(item.offset).zip(target_row) {
        *slot = value.clone();
    }
    row
}

fn current_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    matched: Vec<Matched>,
) -> SqlResult<(Vec<(Vec<u8>, Vec<Value>)>, Vec<usize>)> {
    let mut rows = Vec::with_capacity(matched.len());
    let mut joined = Vec::with_capacity(matched.len());
    for Matched { key, joined_row } in matched {
        if let Some(row) = execute::stored_row(reader, table, &key)? {
            rows.push((key, row));
            joined.push(joined_row);
        }
    }
    Ok((rows, joined))
}

fn returned(
    returning: &Option<Vec<SelectItem>>,
    scope: Scope<'_>,
    rows: Vec<Vec<Value>>,
    tag: String,
) -> SqlResult<QueryResult> {
    Ok(match execute::returned_rows(returning, scope, &rows)? {
        Some((columns, rows)) => QueryResult::rows(tag, columns, rows),
        None => QueryResult::command(tag),
    })
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
    joined: &JoinedTables,
) -> SqlResult<QueryResult> {
    let position = update.table.name.position;
    let source = joined_source(&*writer, tables, full_from(&update.table, joined))?;
    let main = target_position(joined);
    let assigned = assigned_items(&source, update, joined);
    let mut targets: Vec<usize> = Vec::new();
    for &at in &assigned {
        if !targets.contains(&at) {
            targets.push(at);
        }
    }
    for &at in &targets {
        source.writable(&*writer, tables, at, "UPDATE", position)?;
    }
    let assignment_exprs = update
        .assignments
        .iter()
        .filter_map(|(_, value)| match value {
            InsertValue::Expr(expr) => Some(expr),
            InsertValue::Default => None,
        });
    refuse_subqueries(assignment_exprs.clone(), "SET clause")?;
    refuse_subqueries(returning_exprs(&update.returning), "RETURNING clause")?;
    if let Some(filter) = &update.filter {
        reject_aggregates(filter, "WHERE")?;
    }
    let sources = source.sources();
    let scope = Scope::Many(&sources);
    for expr in assignment_exprs {
        reject_aggregates(expr, "UPDATE")?;
        check_columns(expr, scope)?;
    }
    execute::returned_columns(&update.returning, scope)?;
    let mut memory = WorkMemory::new();
    let joined_rows = source.joined_rows(
        &*writer,
        tables,
        update.filter.as_ref(),
        position,
        &mut memory,
    )?;
    let mut matches = Vec::with_capacity(targets.len());
    for &at in &targets {
        matches.push(matched_rows(
            &*writer,
            &source.items[at],
            &joined_rows,
            &mut memory,
        )?);
    }
    let rule = execute::assignment_rule(crate::constraints::current_dialect());
    let mut affected = 0usize;
    let mut returned_rows = Vec::new();
    for (&at, matched) in targets.iter().zip(matches) {
        let item = &source.items[at];
        let table = &item.definition;
        let written: Vec<usize> = (0..assigned.len())
            .filter(|&assignment| assigned[assignment] == at)
            .collect();
        let assignments: Vec<(Name, InsertValue)> = written
            .iter()
            .map(|&assignment| update.assignments[assignment].clone())
            .collect();
        let names: Vec<Name> = assignments.iter().map(|(name, _)| name.clone()).collect();
        let columns = execute::target_columns(table, &names)?;
        execute::check_updated_to_default(table, &columns, &assignments)?;
        let (target_rows, joined_at) = current_rows(&*writer, table, matched)?;
        let mut planned = Vec::with_capacity(target_rows.len());
        for (target_index, ((_, old_row), &joined_row)) in
            target_rows.iter().zip(&joined_at).enumerate()
        {
            let row = with_target_row(&joined_rows[joined_row], item, old_row);
            let evaluator = Evaluator {
                scope,
                row: &row,
                correlation: None,
            };
            let mut new_row = old_row.clone();
            for ((_, value), &column) in assignments.iter().zip(&columns) {
                new_row[column] = execute::assigned_value(&evaluator, value, table, column, rule)?;
            }
            memory.charge_row(&new_row)?;
            planned.push(PlannedUpdate {
                target_index,
                new_row,
                source_index: Some(joined_row),
            });
        }
        let stored = store_updates(
            writer,
            tables,
            table,
            &target_rows,
            &columns,
            planned,
            &mut memory,
        )?;
        affected += stored.changes.len();
        if at == main && update.returning.is_some() {
            for ((_, _, new_row), joined_row) in stored.changes.iter().zip(&stored.sources) {
                let Some(joined_row) = joined_row.and_then(|at| joined_rows.get(at)) else {
                    continue;
                };
                returned_rows.push(with_target_row(joined_row, item, new_row));
            }
        }
    }
    returned(
        &update.returning,
        scope,
        returned_rows,
        format!("UPDATE {affected}"),
    )
}

fn delete_matched(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    matched: Vec<Matched>,
) -> SqlResult<Vec<usize>> {
    let (rows, joined_at) = current_rows(&*writer, table, matched)?;
    let mut firing = Firing::for_table(&mut *writer, tables, table, TriggerEvent::Delete, &[])?;
    let mut kept = Vec::with_capacity(rows.len());
    let mut kept_joined = Vec::with_capacity(rows.len());
    for ((key, row), joined_row) in rows.into_iter().zip(joined_at) {
        if let Some(firing) = firing.as_mut()
            && firing.has_before()
            && firing.before_row(writer, tables, Some(row.as_slice()), None)? == RowOutcome::Skip
        {
            continue;
        }
        kept.push((key, row));
        kept_joined.push(joined_row);
    }
    let deleted: Vec<Vec<Value>> = match &firing {
        Some(_) => kept.iter().map(|(_, row)| row.clone()).collect(),
        None => Vec::new(),
    };
    execute::delete_rows(writer, tables, table, kept, &mut |_| Ok(()))?;
    if let Some(mut firing) = firing {
        for row in &deleted {
            firing.after_row(writer, tables, Some(row.as_slice()), None)?;
        }
        firing.finish();
    }
    Ok(kept_joined)
}

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
    joined: &JoinedTables,
) -> SqlResult<QueryResult> {
    let position = delete.table.name.position;
    let source = joined_source(&*writer, tables, full_from(&delete.table, joined))?;
    let main = target_position(joined);
    let mut targets = vec![main];
    for &listed_at in &joined.more_targets_at {
        let at = from_position(joined, listed_at);
        if !targets.contains(&at) {
            targets.push(at);
        }
    }
    for &at in &targets {
        source.writable(&*writer, tables, at, "DELETE FROM", position)?;
    }
    refuse_subqueries(returning_exprs(&delete.returning), "RETURNING clause")?;
    if let Some(filter) = &delete.filter {
        reject_aggregates(filter, "WHERE")?;
    }
    let sources = source.sources();
    let scope = Scope::Many(&sources);
    execute::returned_columns(&delete.returning, scope)?;
    let mut memory = WorkMemory::new();
    let joined_rows = source.joined_rows(
        &*writer,
        tables,
        delete.filter.as_ref(),
        position,
        &mut memory,
    )?;
    let mut matches = Vec::with_capacity(targets.len());
    for &at in &targets {
        matches.push(matched_rows(
            &*writer,
            &source.items[at],
            &joined_rows,
            &mut memory,
        )?);
    }
    let mut affected = 0usize;
    let mut returned_rows = Vec::new();
    for (&at, matched) in targets.iter().zip(matches) {
        let deleted = delete_matched(writer, tables, &source.items[at].definition, matched)?;
        affected += deleted.len();
        if at == main && delete.returning.is_some() {
            returned_rows.extend(
                deleted
                    .into_iter()
                    .filter_map(|joined_row| joined_rows.get(joined_row).cloned()),
            );
        }
    }
    returned(
        &delete.returning,
        scope,
        returned_rows,
        format!("DELETE {affected}"),
    )
}

pub(crate) fn update_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    update: &Update,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    match &update.from {
        None => execute::returning_columns(reader, tables, &update.table, &update.returning),
        Some(joined) => joined_columns(reader, tables, &update.table, joined, &update.returning),
    }
}

pub(crate) fn delete_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    match &delete.using {
        None => execute::returning_columns(reader, tables, &delete.table, &delete.returning),
        Some(joined) => joined_columns(reader, tables, &delete.table, joined, &delete.returning),
    }
}

fn joined_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    target: &TableReference,
    joined: &JoinedTables,
    returning: &Option<Vec<SelectItem>>,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    if returning.is_none() {
        return Ok(None);
    }
    let source = joined_source(reader, tables, full_from(target, joined))?;
    let sources = source.sources();
    execute::returned_columns(returning, Scope::Many(&sources))
}
