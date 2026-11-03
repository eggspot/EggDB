use crate::ast::{
    BinaryOperator, Expr, ExprKind, Insert, InsertSource, InsertValue, MergeStatement, MergeUpdate,
    Name, OnDuplicate, SelectItem,
};
use crate::constraints::PreparedChecks;
use crate::encoding::{Column, TableDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, Sources, check_columns, reject_aggregates};
use crate::execute::{self, TableCache};
use crate::memory::WorkMemory;
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::triggers::{Firing, RowOutcome, TriggerEvent};
use crate::types::{SqlType, Value, equality_key};
use std::collections::HashMap;

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
    for clause in &merge.by_source {
        exprs.extend(clause.assignments.iter().map(|(_, expr)| expr));
        exprs.extend(clause.condition.iter());
    }
    exprs
}

fn assigned_columns(table: &TableDefinition, update: &MergeUpdate) -> SqlResult<Vec<usize>> {
    let mut columns = Vec::with_capacity(update.assignments.len());
    for (name, _) in &update.assignments {
        let index = target_index(table, name)?;
        if columns.contains(&index) {
            return Err(
                SqlError::new(SqlState::DUPLICATE_COLUMN, "duplicate column name")
                    .with_oracle_code(957)
                    .at(name.position),
            );
        }
        columns.push(index);
    }
    Ok(columns)
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

fn same_family(a: SqlType, b: SqlType) -> bool {
    let exact = |sql_type: SqlType| {
        matches!(
            sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt | SqlType::Numeric(_)
        )
    };
    (exact(a) && exact(b)) || crate::join::type_family(a) == crate::join::type_family(b)
}

fn equality_columns(
    on: &Expr,
    scope: Scope<'_>,
    target: &TableDefinition,
    source_types: &[Option<SqlType>],
) -> Vec<(usize, usize)> {
    let width = target.columns.len();
    let conjuncts: Vec<&Expr> = match &on.kind {
        ExprKind::And(parts) => parts.iter().collect(),
        _ => vec![on],
    };
    let offset = |expr: &Expr| match &expr.kind {
        ExprKind::Column { qualifier, name } => {
            scope.column(qualifier.as_deref(), name, expr.position).ok()
        }
        _ => None,
    };
    let mut pairs = Vec::new();
    for conjunct in conjuncts {
        let ExprKind::Binary(BinaryOperator::Equal, left, right) = &conjunct.kind else {
            continue;
        };
        let (Some(a), Some(b)) = (offset(left), offset(right)) else {
            continue;
        };
        let (on_target, on_source) = match (a < width, b < width) {
            (true, false) => (a, b - width),
            (false, true) => (b, a - width),
            _ => continue,
        };
        if source_types[on_source]
            .is_some_and(|source_type| same_family(target.columns[on_target].sql_type, source_type))
        {
            pairs.push((on_target, on_source));
        }
    }
    pairs
}

fn key_of(row: &[Value], columns: impl Iterator<Item = usize>) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    for column in columns {
        let value = row.get(column)?;
        if value.is_null() {
            return None;
        }
        equality_key(&mut key, value);
    }
    Some(key)
}

struct SourceIndex {
    rows: HashMap<Vec<u8>, Vec<usize>>,
}

impl SourceIndex {
    fn build(
        equalities: &[(usize, usize)],
        rows: &[Vec<Value>],
        memory: &mut WorkMemory,
    ) -> SqlResult<Option<Self>> {
        if equalities.is_empty() {
            return Ok(None);
        }
        let mut index: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (position, row) in rows.iter().enumerate() {
            let Some(key) = key_of(row, equalities.iter().map(|(_, source)| *source)) else {
                continue;
            };
            memory.charge(key.len() + std::mem::size_of::<usize>() * 2)?;
            index.entry(key).or_default().push(position);
        }
        Ok(Some(Self { rows: index }))
    }

    fn candidates(&self, equalities: &[(usize, usize)], target_row: &[Value]) -> &[usize] {
        key_of(target_row, equalities.iter().map(|(target, _)| *target))
            .and_then(|key| self.rows.get(&key))
            .map_or(&[], Vec::as_slice)
    }
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
        .unwrap_or_else(|| crate::schemas::base_name(&table.name).to_owned());
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
        updated_columns = assigned_columns(&table, update)?;
        for ((name, _), index) in update.assignments.iter().zip(&updated_columns) {
            if on_columns.contains(index) {
                return Err(on_column_updated(&name.text, name.position));
            }
        }
    }
    let by_source_columns = merge
        .by_source
        .iter()
        .map(|clause| assigned_columns(&table, clause))
        .collect::<SqlResult<Vec<_>>>()?;
    let target_rows = execute::every_row(writer, &table)?;
    for (key, row) in &target_rows {
        memory.charge(WorkMemory::row_bytes(row) + key.len())?;
    }
    let source_types: Vec<Option<SqlType>> = source_columns
        .iter()
        .map(|column| column.sql_type)
        .collect();
    let equalities = equality_columns(&merge.on, joined.scope, &table, &source_types);
    let candidates = SourceIndex::build(&equalities, &source.rows, &mut memory)?;
    let every_source: Vec<usize> = (0..source.rows.len()).collect();
    let mut matched_sources = vec![false; source.rows.len()];
    let mut matched_targets = vec![false; target_rows.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (target_index, (_, target_row)) in target_rows.iter().enumerate() {
        let mut chosen = None;
        let sources = match &candidates {
            Some(index) => index.candidates(&equalities, target_row),
            None => &every_source,
        };
        for &source_index in sources {
            let source_row = &source.rows[source_index];
            let row = joined.row(target_row, source_row);
            if !joined.holds(Some(&merge.on), &row, "ON")? {
                continue;
            }
            matched_sources[source_index] = true;
            matched_targets[target_index] = true;
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
    let mut outputs: Vec<OutputRow> = Vec::new();
    if let Some(update) = &merge.update {
        let mut planned = Vec::with_capacity(pairs.len());
        for &(target_index, source_index) in &pairs {
            let old_row = &target_rows[target_index].1;
            let row = joined.row(old_row, &source.rows[source_index]);
            let new_row = assigned_row(&joined, &table, update, &updated_columns, old_row, &row)?;
            planned.push(PlannedUpdate {
                target_index,
                new_row,
                source_index: Some(source_index),
            });
        }
        let stored = store_updates(
            writer,
            tables,
            &table,
            &target_rows,
            &updated_columns,
            planned,
            &mut memory,
        )?;
        affected += stored.changes.len();
        let first_output = outputs.len();
        if merge.output.is_some() {
            push_update_outputs(&stored, &mut outputs);
        }
        if let Some(condition) = &update.delete_condition {
            let mut deleted = Vec::new();
            for (at, ((_, new_key, new_row), source_index)) in
                stored.changes.into_iter().zip(&stored.sources).enumerate()
            {
                let source_row = source_index.and_then(|index| source.rows.get(index));
                let row = joined.row(&new_row, source_row.map_or(&[][..], Vec::as_slice));
                if joined.holds(Some(condition), &row, "DELETE WHERE")? {
                    if let Some(output) = outputs.get_mut(first_output + at) {
                        output.inserted = None;
                    }
                    deleted.push((new_key, new_row));
                }
            }
            execute::delete_rows(writer, tables, &table, deleted, &mut |_| Ok(()))?;
        }
    }
    if !merge.by_source.is_empty() {
        let absent_source = vec![Value::Null; source_table.columns.len()];
        let mut planned: Vec<Vec<PlannedUpdate>> =
            merge.by_source.iter().map(|_| Vec::new()).collect();
        let mut deleted = Vec::new();
        for (target_index, (key, target_row)) in target_rows.iter().enumerate() {
            if matched_targets[target_index] {
                continue;
            }
            let row = joined.row(target_row, &absent_source);
            let mut chosen = None;
            for (clause_index, clause) in merge.by_source.iter().enumerate() {
                if joined.holds(clause.condition.as_ref(), &row, "AND")? {
                    chosen = Some(clause_index);
                    break;
                }
            }
            let Some(clause_index) = chosen else {
                continue;
            };
            let clause = &merge.by_source[clause_index];
            if clause.delete_condition.is_some() {
                memory.charge(WorkMemory::row_bytes(target_row) + key.len())?;
                if merge.output.is_some() {
                    outputs.push(OutputRow {
                        inserted: None,
                        deleted: Some(target_row.clone()),
                        source_index: None,
                    });
                }
                deleted.push((key.clone(), target_row.clone()));
                continue;
            }
            let new_row = assigned_row(
                &joined,
                &table,
                clause,
                &by_source_columns[clause_index],
                target_row,
                &row,
            )?;
            planned[clause_index].push(PlannedUpdate {
                target_index,
                new_row,
                source_index: None,
            });
        }
        for (clause_planned, columns) in planned.into_iter().zip(&by_source_columns) {
            if clause_planned.is_empty() {
                continue;
            }
            let stored = store_updates(
                writer,
                tables,
                &table,
                &target_rows,
                columns,
                clause_planned,
                &mut memory,
            )?;
            affected += stored.changes.len();
            if merge.output.is_some() {
                push_update_outputs(&stored, &mut outputs);
            }
        }
        affected += execute::delete_rows(writer, tables, &table, deleted, &mut |_| Ok(()))?;
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
        let mut inserted_sources = Vec::new();
        for (source_index, source_row) in source.rows.iter().enumerate() {
            if matched_sources[source_index] {
                continue;
            }
            let row = joined.row(&[], source_row);
            if !joined.holds(insert.condition.as_ref(), &row, "WHERE")? {
                continue;
            }
            inserted_sources.push(source_index);
            let mut values = Vec::with_capacity(targets.len());
            for (expr, &index) in insert.values.iter().zip(&targets) {
                let sql_type = table.columns[index].sql_type;
                values.push(joined.value(expr, &row, Some(sql_type))?);
            }
            memory.charge_row(&values)?;
            rows.push(
                values
                    .into_iter()
                    .zip(&insert.values)
                    .map(|(value, expr)| {
                        InsertValue::Expr(Expr {
                            kind: ExprKind::Constant {
                                sql_type: value.sql_type(),
                                value,
                            },
                            position: expr.position,
                        })
                    })
                    .collect(),
            );
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
            let stored = execute::insert(
                writer,
                tables,
                &Insert {
                    table: merge.target.clone(),
                    columns: Some(columns),
                    source: InsertSource::Values(rows),
                    returning: merge.output.as_ref().map(|_| {
                        vec![SelectItem::Wildcard {
                            qualifier: None,
                            position: merge.target.name.position,
                        }]
                    }),
                    on_duplicate: OnDuplicate::Fail,
                    conflict_target: None,
                    rollback_on_conflict: false,
                    overriding: None,
                },
            )?;
            affected += count;
            for (inserted, source_index) in stored.rows.into_iter().zip(inserted_sources) {
                outputs.push(OutputRow {
                    inserted: Some(inserted),
                    deleted: None,
                    source_index: Some(source_index),
                });
            }
        }
    }
    let tag = format!("MERGE {affected}");
    match &merge.output {
        Some(items) => {
            let (columns, rows) = output_rows(
                items,
                &table,
                &source_table,
                &source_label,
                &source.rows,
                outputs,
            )?;
            Ok(QueryResult::rows(tag, columns, rows))
        }
        None => Ok(QueryResult::command(tag)),
    }
}

struct OutputRow {
    inserted: Option<Vec<Value>>,
    deleted: Option<Vec<Value>>,
    source_index: Option<usize>,
}

struct PlannedUpdate {
    target_index: usize,
    new_row: Vec<Value>,
    source_index: Option<usize>,
}

struct StoredUpdates {
    changes: Vec<execute::RowUpdate>,
    old_rows: Vec<Vec<Value>>,
    sources: Vec<Option<usize>>,
}

fn assigned_row(
    joined: &Joined<'_>,
    table: &TableDefinition,
    update: &MergeUpdate,
    columns: &[usize],
    old_row: &[Value],
    row: &[Value],
) -> SqlResult<Vec<Value>> {
    let rule = execute::assignment_rule(crate::constraints::current_dialect());
    let mut new_row = old_row.to_vec();
    for ((_, expr), &index) in update.assignments.iter().zip(columns) {
        let column = &table.columns[index];
        let value = joined.value(expr, row, Some(column.sql_type))?;
        new_row[index] =
            rule(value, column).map_err(|error| error.at_if_unplaced(expr.position))?;
    }
    Ok(new_row)
}

fn store_updates(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &std::sync::Arc<TableDefinition>,
    target_rows: &[(Vec<u8>, Vec<Value>)],
    updated_columns: &[usize],
    planned: Vec<PlannedUpdate>,
    memory: &mut WorkMemory,
) -> SqlResult<StoredUpdates> {
    let checks = PreparedChecks::of(table)?.with_partitions(writer, tables, table)?;
    let mut stored = StoredUpdates {
        changes: Vec::with_capacity(planned.len()),
        old_rows: Vec::with_capacity(planned.len()),
        sources: Vec::with_capacity(planned.len()),
    };
    let mut firing = Firing::for_table(
        &mut *writer,
        tables,
        table,
        TriggerEvent::Update,
        updated_columns,
    )?;
    for PlannedUpdate {
        target_index,
        mut new_row,
        source_index,
    } in planned
    {
        let (old_key, old_row) = &target_rows[target_index];
        if let Some(firing) = firing.as_mut()
            && firing.has_before()
            && firing.before_row(writer, tables, Some(old_row.as_slice()), Some(&mut new_row))?
                == RowOutcome::Skip
        {
            continue;
        }
        execute::check_not_null_of(table, &new_row)?;
        if !checks.is_empty() {
            checks.check(table, &new_row)?;
        }
        let new_key = if table.primary_key.is_empty() {
            old_key.clone()
        } else {
            execute::row_key(table, &new_row)
        };
        memory.charge_row(&new_row)?;
        memory.charge_row(old_row)?;
        stored.old_rows.push(old_row.clone());
        stored.changes.push((old_key.clone(), new_key, new_row));
        stored.sources.push(source_index);
    }
    execute::write_updated_rows(
        writer,
        tables,
        table,
        &stored.changes,
        &stored.old_rows,
        &mut |_| Ok(()),
    )?;
    if let Some(mut firing) = firing {
        for ((_, _, new_row), old_row) in stored.changes.iter().zip(&stored.old_rows) {
            firing.after_row(
                writer,
                tables,
                Some(old_row.as_slice()),
                Some(new_row.as_slice()),
            )?;
        }
        firing.finish();
    }
    Ok(stored)
}

fn push_update_outputs(stored: &StoredUpdates, outputs: &mut Vec<OutputRow>) {
    for (((_, _, new_row), old_row), source_index) in stored
        .changes
        .iter()
        .zip(&stored.old_rows)
        .zip(&stored.sources)
    {
        outputs.push(OutputRow {
            inserted: Some(new_row.clone()),
            deleted: Some(old_row.clone()),
            source_index: *source_index,
        });
    }
}

fn output_rows(
    items: &[SelectItem],
    table: &TableDefinition,
    source_table: &TableDefinition,
    source_label: &str,
    source_rows: &[Vec<Value>],
    outputs: Vec<OutputRow>,
) -> SqlResult<(Vec<crate::session::OutputColumn>, Vec<Vec<Value>>)> {
    let width = table.columns.len();
    let sources = Sources {
        list: vec![
            Source {
                table,
                label: "inserted",
                offset: 0,
            },
            Source {
                table,
                label: "deleted",
                offset: width,
            },
            Source {
                table: source_table,
                label: source_label,
                offset: width * 2,
            },
        ],
        merged: Vec::new(),
        out_of_reach: Vec::new(),
        star: (0..width * 2 + source_table.columns.len()).collect(),
    };
    let mut rows = Vec::with_capacity(outputs.len());
    for output in outputs {
        let mut row = Vec::with_capacity(width * 2 + source_table.columns.len());
        for side in [output.inserted, output.deleted] {
            match side {
                Some(values) => row.extend(values),
                None => row.resize(row.len() + width, Value::Null),
            }
        }
        match output.source_index.and_then(|index| source_rows.get(index)) {
            Some(source_row) => row.extend_from_slice(source_row),
            None => row.resize(row.len() + source_table.columns.len(), Value::Null),
        }
        rows.push(row);
    }
    execute::project_rows(items, Scope::Many(&sources), &rows)
}
