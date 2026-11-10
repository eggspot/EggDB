use crate::ast::{
    Delete, Expr, ExprKind, Insert, InsertSource, InsertValue, Name, OnDuplicate, Overriding,
    TableReference, Update,
};
use crate::constraints::{PreparedChecks, RowChange};
use crate::dialect::Dialect;
use crate::encoding::{
    Column, IdentitySeed, IndexMethod, TableDefinition, decode_key_values, decode_row,
    index_prefix, prefix_end, rows_prefix,
};
use crate::error::{SqlError, SqlResult};
use crate::eval::{Evaluator, Scope};
use crate::execute::{self, RowUpdate, TableCache};
use crate::partition_selection::SelectedPartitions;
use crate::partitions::{PartitionCatalog, Partitioned};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
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

struct ParentIdentity {
    column: usize,
    seed: IdentitySeed,
    furthest: Option<i64>,
    read_stored: bool,
    first_generated: Option<u64>,
    ignores_given: bool,
}

impl ParentIdentity {
    fn of(table: &TableDefinition, overriding: Option<Overriding>) -> Option<Self> {
        Some(Self {
            column: table.auto_increment_column()?,
            seed: table.identity_seed(),
            furthest: None,
            read_stored: false,
            first_generated: None,
            ignores_given: overriding == Some(Overriding::UserValue),
        })
    }

    fn reach(&mut self, value: i64) {
        self.furthest = Some(match self.furthest {
            Some(best) if !self.seed.is_beyond(value, best) => best,
            _ => value,
        });
    }

    fn generate(
        &mut self,
        reader: &dyn KeyValueRead,
        catalog: &PartitionCatalog,
        table: &TableDefinition,
        column: &Column,
    ) -> SqlResult<Value> {
        if !self.read_stored {
            if let Some(stored) =
                furthest_identity_value(reader, catalog, table, self.column, self.seed)?
            {
                self.reach(stored);
            }
            self.read_stored = true;
        }
        let next = self
            .seed
            .next_after(self.furthest)
            .ok_or_else(|| execute::auto_increment_exhausted(column))?;
        let value = convert(Value::BigInt(next), column.sql_type, Conversion::Assignment)
            .map_err(|_| execute::auto_increment_exhausted(column))?;
        self.first_generated.get_or_insert(next as u64);
        self.furthest = Some(next);
        Ok(value)
    }

    fn fill(
        &mut self,
        reader: &dyn KeyValueRead,
        catalog: &PartitionCatalog,
        table: &TableDefinition,
        columns: &mut Vec<usize>,
        values: &mut Vec<InsertValue>,
    ) -> SqlResult<()> {
        let Some(column) = table.columns.get(self.column) else {
            return Ok(());
        };
        let slot = columns.iter().position(|&target| target == self.column);
        let given = match slot.and_then(|at| values.get(at)) {
            Some(InsertValue::Expr(expr)) if !self.ignores_given => {
                Some(evaluated(expr, table, self.column)?)
            }
            _ => None,
        };
        let given_number = given.as_ref().and_then(Value::as_integer);
        let kept = match given_number {
            Some(0) => crate::constraints::current_dialect() != Dialect::MySql,
            other => other.is_some(),
        };
        let value = match (given, given_number) {
            (Some(value), Some(number)) if kept => {
                self.reach(number);
                value
            }
            _ => self.generate(reader, catalog, table, column)?,
        };
        match slot {
            Some(at) => {
                if let Some(written) = values.get_mut(at) {
                    *written = InsertValue::Expr(constant(value));
                }
            }
            None => {
                columns.push(self.column);
                values.push(InsertValue::Expr(constant(value)));
            }
        }
        Ok(())
    }
}

pub(crate) fn furthest_in_table(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    column: usize,
    seed: IdentitySeed,
) -> SqlResult<Option<i64>> {
    let direction = if seed.scans_backward() {
        ScanDirection::Backward
    } else {
        ScanDirection::Forward
    };
    let rows = rows_prefix(table.id);
    let mut found = None;
    if table.primary_key.first() == Some(&column) {
        reader.scan(&rows, &prefix_end(&rows), direction, &mut |_, bytes| {
            found = decode_row(bytes, &table.columns)?
                .get(column)
                .and_then(Value::as_integer);
            Ok(found.is_none())
        })?;
        return Ok(found);
    }
    let Some(sql_type) = table
        .columns
        .get(column)
        .map(|definition| definition.sql_type)
    else {
        return Ok(None);
    };
    if let Some(index) = table.indexes.iter().find(|index| {
        index.columns.first() == Some(&column)
            && index.method == IndexMethod::BTree
            && index.sort_options.is_default()
    }) {
        let entries = index_prefix(table.id, index.id);
        let mut decoded = Vec::with_capacity(1);
        reader.scan(&entries, &prefix_end(&entries), direction, &mut |key, _| {
            decoded.clear();
            let Some(values) = key.get(entries.len()..) else {
                return Err(crate::encoding::corrupt("index entry"));
            };
            decode_key_values(values, std::iter::once(sql_type), &mut decoded)?;
            found = decoded.first().and_then(Value::as_integer);
            Ok(found.is_none())
        })?;
        return Ok(found);
    }
    reader.scan(
        &rows,
        &prefix_end(&rows),
        ScanDirection::Forward,
        &mut |_, bytes| {
            if let Some(value) = decode_row(bytes, &table.columns)?
                .get(column)
                .and_then(Value::as_integer)
            {
                found = Some(match found {
                    Some(best) if !seed.is_beyond(value, best) => best,
                    _ => value,
                });
            }
            Ok(true)
        },
    )?;
    Ok(found)
}

fn furthest_identity_value(
    reader: &dyn KeyValueRead,
    catalog: &PartitionCatalog,
    table: &TableDefinition,
    column: usize,
    seed: IdentitySeed,
) -> SqlResult<Option<i64>> {
    let Some(name) = table
        .columns
        .get(column)
        .map(|definition| definition.name.as_str())
    else {
        return Ok(None);
    };
    let mut furthest: Option<i64> = None;
    for partition in catalog.descendants_of(&table.name) {
        if catalog.key_of(&partition).is_some() {
            continue;
        }
        let damaged = || crate::encoding::corrupt("partition tree");
        let definition =
            crate::partitions::stored_definition(reader, &partition)?.ok_or_else(damaged)?;
        let at = definition.column_index(name).ok_or_else(damaged)?;
        if let Some(value) = furthest_in_table(reader, &definition, at, seed)? {
            furthest = Some(match furthest {
                Some(best) if !seed.is_beyond(value, best) => best,
                _ => value,
            });
        }
    }
    Ok(furthest)
}

/// The value the identity or `AUTO_INCREMENT` column of `table` gives its next row: for a
/// partitioned table, the one past the furthest value any of its partitions holds.
pub(crate) fn next_identity_value(
    reader: &dyn KeyValueRead,
    catalog: &PartitionCatalog,
    table: &TableDefinition,
) -> SqlResult<Option<i64>> {
    if catalog.key_of(&table.name).is_none() {
        return execute::StoredRows::next_identity_value(reader, table);
    }
    let Some(column) = table.auto_increment_column() else {
        return Ok(None);
    };
    let seed = table.identity_seed();
    Ok(seed.next_after(furthest_identity_value(
        reader, catalog, table, column, seed,
    )?))
}

pub(crate) fn numbered_for_partition(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
    table: &TableDefinition,
) -> SqlResult<Option<Insert>> {
    if table.auto_increment_column().is_some() {
        return Ok(None);
    }
    let catalog = crate::partitions::catalog(writer, tables)?;
    let Some(mut root) = catalog.parent_of(&table.name).map(|(parent, _)| parent.to_owned()) else {
        return Ok(None);
    };
    while let Some((parent, _)) = catalog.parent_of(&root) {
        root = parent.to_owned();
    }
    let root = tables.require_target(
        writer,
        &Name {
            text: root,
            position: insert.table.name.position,
        },
        "insert into",
    )?;
    let Some(column) = root.auto_increment_column() else {
        return Ok(None);
    };
    let same_place = table
        .column_index(&root.columns[column].name)
        .is_some_and(|index| index == column);
    if !same_place {
        return Ok(None);
    }
    let all_defaults: Vec<Vec<InsertValue>> = vec![Vec::new()];
    let rows = match &insert.source {
        InsertSource::Values(rows) => rows,
        InsertSource::DefaultValues => &all_defaults,
        InsertSource::Query(_) => return Ok(None),
    };
    let mut targets = match &insert.columns {
        Some(names) => execute::target_columns(table, names)?,
        None => (0..table.columns.len()).collect(),
    };
    execute::check_generated_always(&root, &targets, rows, insert.overriding)?;
    let adds_column = !targets.contains(&column);
    if adds_column {
        targets.push(column);
    }
    let Some(mut identity) = ParentIdentity::of(&root, insert.overriding) else {
        return Ok(None);
    };
    let mut numbered = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = row.clone();
        let mut columns = targets.clone();
        if adds_column {
            values.push(InsertValue::Default);
        }
        identity.fill(&*writer, &catalog, &root, &mut columns, &mut values)?;
        numbered.push(values);
    }
    let names = targets
        .iter()
        .map(|&index| Name {
            text: table.columns[index].name.clone(),
            position: insert.table.name.position,
        })
        .collect();
    Ok(Some(Insert {
        columns: Some(names),
        source: InsertSource::Values(numbered),
        ..insert.clone()
    }))
}

enum Routed {
    Done(QueryResult),
    NeedsPartition(Vec<Value>),
}

pub(crate) fn insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
    table: &TableDefinition,
    partitioned: &Partitioned,
) -> SqlResult<QueryResult> {
    let mut refreshed: Option<Partitioned> = None;
    loop {
        let routing = refreshed.as_ref().unwrap_or(partitioned);
        let key = match insert_routed(writer, tables, insert, table, routing)? {
            Routed::Done(result) => return Ok(result),
            Routed::NeedsPartition(key) => key,
        };
        let root = tables.require_target(
            writer,
            &Name {
                text: routing.table.clone(),
                position: insert.table.name.position,
            },
            "insert into",
        )?;
        let no_partition = routing.no_partition_for_key(&key);
        let sql_type = routing.key_types().first().copied();
        let interval = crate::partitions::stored(writer, &root.name)?.interval;
        let (Some(interval), Some(sql_type)) = (interval, sql_type) else {
            return Err(no_partition);
        };
        let dialect = crate::constraints::current_dialect();
        if !crate::partition_interval::create_for(
            writer, tables, &root, &interval, &key, sql_type, dialect,
        )? {
            return Err(no_partition);
        }
        refreshed = Some(
            crate::partitions::partitioned(writer, tables, table)?
                .ok_or_else(|| crate::encoding::corrupt("partition tree"))?,
        );
    }
}

fn insert_routed(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
    table: &TableDefinition,
    partitioned: &Partitioned,
) -> SqlResult<Routed> {
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
    execute::check_generated_always(table, &targets, rows, insert.overriding)?;
    let mut identity = ParentIdentity::of(table, insert.overriding);
    let catalog = if identity.is_some() || insert.table.partition_selection.is_some() {
        Some(crate::partitions::catalog(writer, tables)?)
    } else {
        None
    };
    let selected = match &catalog {
        Some(catalog) => SelectedPartitions::of_reference(catalog, table, &insert.table)?,
        None => None,
    };
    let mut groups: Vec<(String, Vec<usize>, Vec<Vec<InsertValue>>)> = Vec::new();
    let ignores_rows = matches!(insert.on_duplicate, OnDuplicate::Ignore)
        && crate::constraints::current_dialect() == Dialect::MySql;
    let grows_by_interval = selected.is_none()
        && crate::partitions::stored(&*writer, &partitioned.table)?
            .interval
            .is_some();
    let mut skipped: Vec<crate::mysql_errors::Warning> = Vec::new();
    let mut key_row = vec![Value::Null; table.columns.len()];
    let key_columns = partitioned.columns_read();
    for row in rows {
        let mut values: Vec<InsertValue> = row.clone();
        let mut columns = targets.clone();
        if let (Some(identity), Some(catalog)) = (identity.as_mut(), catalog.as_deref()) {
            identity.fill(&*writer, catalog, table, &mut columns, &mut values)?;
        }
        for &key in &key_columns {
            let value = match columns.iter().position(|&target| target == key) {
                Some(at) => {
                    let value = match values.get(at) {
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
            if grows_by_interval {
                return Ok(Routed::NeedsPartition(partitioned.key_of_row(&key_row)?));
            }
            if ignores_rows {
                skipped.push(skipped_row(partitioned.no_partition(&key_row)));
                continue;
            }
            return Err(partitioned.no_partition(&key_row));
        };
        if let (Some(selected), Some(catalog)) = (&selected, &catalog)
            && !selected.admits(catalog, child)
        {
            let outside =
                crate::partition_selection::row_outside_selection().at(insert.table.name.position);
            if ignores_rows {
                skipped.push(skipped_row(outside));
                continue;
            }
            return Err(outside);
        }
        match groups
            .iter_mut()
            .find(|(name, group_columns, _)| name == child && *group_columns == columns)
        {
            Some((_, _, group)) => group.push(values),
            None => groups.push((child.to_owned(), columns, vec![values])),
        }
    }
    let conflict_target = match &insert.conflict_target {
        Some(target) => Some(execute::routed_conflict_target(table, target)?),
        None => None,
    };
    let mut merged = Merged::default();
    for (child, columns, rows) in groups {
        let names = columns
            .iter()
            .map(|&column| Name {
                text: table.columns[column].name.clone(),
                position: insert.table.name.position,
            })
            .collect();
        let mut routed_table = reference_to(&child, &insert.table);
        if let (Some(selected), Some(catalog)) = (&selected, &catalog) {
            routed_table.partition_selection = selected
                .selection_below(catalog, &child, insert.table.name.position)
                .map(Box::new);
        }
        let routed = Insert {
            table: routed_table,
            columns: Some(names),
            source: InsertSource::Values(rows),
            returning: insert.returning.clone(),
            on_duplicate: insert.on_duplicate.clone(),
            conflict_target: conflict_target.clone(),
            rollback_on_conflict: insert.rollback_on_conflict,
            keeps_changes_on_conflict: insert.keeps_changes_on_conflict,
            overriding: insert.overriding,
        };
        merged.add(execute::insert(writer, tables, &routed)?);
    }
    let mut result = merged.finish("INSERT 0 0");
    if !skipped.is_empty() {
        skipped.append(&mut result.warnings);
        result.warnings = skipped;
    }
    if let Some(identity) = identity {
        result.last_insert_id = identity.first_generated;
    }
    Ok(Routed::Done(result))
}

fn skipped_row(error: SqlError) -> crate::mysql_errors::Warning {
    crate::mysql_errors::Warning {
        level: "Warning",
        code: error.vendor_code().unwrap_or(1748),
        message: error.message().to_owned(),
    }
}

fn surviving_selected(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    partitioned: &Partitioned,
    written: &TableReference,
    filter: Option<&Expr>,
) -> SqlResult<Vec<String>> {
    let surviving = partitioned
        .surviving(filter, &qualifiers(written))
        .into_iter()
        .map(str::to_owned);
    if written.partition_selection.is_none() {
        return Ok(surviving.collect());
    }
    let catalog = crate::partitions::catalog(reader, tables)?;
    let damaged = || crate::encoding::corrupt("partition tree");
    let table = tables
        .table(reader, &partitioned.table)?
        .ok_or_else(damaged)?;
    let selected =
        SelectedPartitions::of_reference(&catalog, &table, written)?.ok_or_else(damaged)?;
    Ok(surviving.filter(|child| selected.contains(child)).collect())
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
    partitioned: &Partitioned,
    moved: Option<&mut Vec<MovedRow>>,
) -> SqlResult<QueryResult> {
    let children = surviving_selected(
        writer,
        tables,
        partitioned,
        &update.table,
        update.filter.as_ref(),
    )?;
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
    if routes_moves && !own_moves.is_empty() {
        refuse_disabled_row_movement(writer, tables, partitioned)?;
    }
    if routes_moves {
        let mut target = TableReference::named(
            Name {
                text: partitioned.table.clone(),
                position: update.table.name.position,
            },
            None,
        );
        target.partition_selection = update.table.partition_selection.clone();
        insert_moved_rows(writer, tables, target, own_moves)?;
    }
    Ok(merged.finish("UPDATE 0"))
}

fn refuse_disabled_row_movement(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    partitioned: &Partitioned,
) -> SqlResult<()> {
    let catalog = crate::partitions::catalog(reader, tables)?;
    let disabled = catalog
        .record(&partitioned.table)
        .is_some_and(|record| record.row_movement_disabled);
    if disabled {
        return Err(SqlError::new(
            crate::error::SqlState::CHECK_VIOLATION,
            "updating partition key column would cause a partition change",
        )
        .with_oracle_code(14402));
    }
    Ok(())
}

struct LeavingRow {
    old_key: Vec<u8>,
    old_row: Option<Vec<Value>>,
    new_row: Vec<Value>,
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
    let kept_old_rows = !old_rows.is_empty();
    let mut old_iter = std::mem::take(old_rows).into_iter();
    let mut leaving: Vec<LeavingRow> = Vec::new();
    for (old_key, new_key, new_row) in std::mem::take(changes) {
        let old_row = if kept_old_rows { old_iter.next() } else { None };
        if checks.fits_partition(&new_row) {
            changes.push((old_key, new_key, new_row));
            old_rows.extend(old_row);
        } else {
            leaving.push(LeavingRow {
                old_key,
                old_row,
                new_row,
            });
        }
    }
    if leaving.is_empty() {
        return Ok(0);
    }
    let mut deleting = crate::triggers::Firing::for_moved_row(writer, tables, table)?;
    let mut count = 0usize;
    let mut propagated = Vec::new();
    for LeavingRow {
        old_key,
        mut old_row,
        new_row,
    } in leaving
    {
        if let Some(firing) = deleting.as_mut() {
            if old_row.is_none() {
                old_row = execute::stored_row(&*writer, table, &old_key)?;
            }
            let outcome = firing.before_row(writer, tables, old_row.as_deref(), None)?;
            if outcome == crate::triggers::RowOutcome::Skip {
                continue;
            }
        }
        count += 1;
        if let Some(old_row) = &old_row
            && !table.indexes.is_empty()
        {
            crate::indexes::delete_entries(writer, table, old_row, &old_key)?;
        }
        writer.delete(&old_key)?;
        if let Some(firing) = deleting.as_mut() {
            firing.after_row(writer, tables, old_row.as_deref(), None)?;
        }
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
    if let Some(firing) = deleting {
        firing.finish();
    }
    if !propagated.is_empty() {
        crate::constraints::propagate(writer, tables, propagated)?;
    }
    Ok(count)
}

pub(crate) fn reinsert_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &Arc<TableDefinition>,
    rows: Vec<Vec<Value>>,
) -> SqlResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let target = TableReference::named(
        Name {
            text: parent.name.clone(),
            position: 0,
        },
        None,
    );
    let moved = rows
        .into_iter()
        .map(|row| MovedRow {
            from: Arc::clone(parent),
            row,
        })
        .collect();
    insert_moved_rows(writer, tables, target, moved)
}

fn insert_moved_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    target: TableReference,
    moved: Vec<MovedRow>,
) -> SqlResult<()> {
    if moved.is_empty() {
        return Ok(());
    }
    let mut receiving = crate::partitions::catalog(writer, tables)?.descendants_of(&target.name.text);
    receiving.push(target.name.text.clone());
    let _moving = crate::triggers::moving_rows_into(receiving);
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
            table: target.clone(),
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
    let children = surviving_selected(
        writer,
        tables,
        partitioned,
        &delete.table,
        delete.filter.as_ref(),
    )?;
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
