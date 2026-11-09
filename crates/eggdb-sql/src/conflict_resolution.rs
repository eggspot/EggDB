use crate::ast::ConflictResolution;
use crate::constraints::{self, PreparedChecks, RowChange};
use crate::encoding::{TableDefinition, encode_row};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, RowUpdate, TableCache};
use crate::indexes;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::Value;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn conflict_resolution_applies(error: &SqlError) -> bool {
    matches!(
        error.state(),
        SqlState::UNIQUE_VIOLATION | SqlState::NOT_NULL_VIOLATION | SqlState::CHECK_VIOLATION
    )
}

pub(crate) fn settle_row_constraints(
    table: &TableDefinition,
    checks: &PreparedChecks,
    row: &mut [Value],
    resolution: ConflictResolution,
    may_move: bool,
) -> SqlResult<bool> {
    if resolution == ConflictResolution::Replace {
        replace_nulls_with_defaults(table, row)?;
    }
    let row: &[Value] = row;
    let checked = execute::check_not_null_of(table, row).and_then(|()| {
        if !checks.is_empty() && !(may_move && !checks.fits_partition(row)) {
            checks.check(table, row)
        } else {
            Ok(())
        }
    });
    match checked {
        Ok(()) => Ok(true),
        Err(error)
            if resolution == ConflictResolution::Ignore && conflict_resolution_applies(&error) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn replace_nulls_with_defaults(table: &TableDefinition, row: &mut [Value]) -> SqlResult<()> {
    for (index, column) in table.columns.iter().enumerate() {
        if !column.not_null || !row.get(index).is_some_and(Value::is_null) {
            continue;
        }
        let default = execute::default_value(table, index)?;
        if let Some(slot) = row.get_mut(index) {
            *slot = default;
        }
    }
    Ok(())
}

pub(crate) struct ResolvedWrites {
    pub(crate) written: Vec<bool>,
    pub(crate) stopped: Option<SqlError>,
}

struct ConflictingRow {
    index: Option<usize>,
    row_key: Vec<u8>,
}

impl ConflictingRow {
    fn error(&self, table: &TableDefinition, row: &[Value]) -> SqlError {
        match self.index.and_then(|position| table.indexes.get(position)) {
            Some(index) => indexes::unique_violation(table, index, row),
            None => execute::duplicate_key(table, row),
        }
    }
}

fn conflicting_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    change: &RowUpdate,
    every_conflict: bool,
) -> SqlResult<Vec<ConflictingRow>> {
    let (old_key, new_key, new_row) = change;
    let mut found: Vec<ConflictingRow> = Vec::new();
    if old_key != new_key && reader.get(new_key)?.is_some() {
        found.push(ConflictingRow {
            index: None,
            row_key: new_key.clone(),
        });
        if !every_conflict {
            return Ok(found);
        }
    }
    for (position, index) in table.indexes.iter().enumerate() {
        let Some(row_key) = indexes::unique_conflict(reader, table, index, new_row, old_key)?
        else {
            continue;
        };
        if found.iter().any(|conflict| conflict.row_key == row_key) {
            continue;
        }
        found.push(ConflictingRow {
            index: Some(position),
            row_key,
        });
        if !every_conflict {
            break;
        }
    }
    Ok(found)
}

pub(crate) fn write_updated_rows_resolving_conflicts(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    changes: &[RowUpdate],
    old_rows: &[Vec<Value>],
    resolution: ConflictResolution,
) -> SqlResult<ResolvedWrites> {
    let mut written = vec![false; changes.len()];
    let mut stopped = None;
    let mut replaced_keys: HashSet<Vec<u8>> = HashSet::new();
    let referenced = constraints::is_referenced(table);
    let mut propagated = Vec::new();
    let replaces = resolution == ConflictResolution::Replace;
    for (position, change) in changes.iter().enumerate() {
        let (old_key, new_key, new_row) = change;
        if replaced_keys.contains(old_key) {
            continue;
        }
        let conflicts = conflicting_rows(&*writer, table, change, replaces)?;
        if !conflicts.is_empty() {
            match resolution {
                ConflictResolution::Ignore => continue,
                ConflictResolution::Replace => {
                    for conflict in conflicts {
                        let removed = execute::delete_row(writer, table, &conflict.row_key)?;
                        if referenced {
                            propagated.push(RowChange {
                                table: Arc::clone(table),
                                old: removed,
                                new: None,
                            });
                        }
                        replaced_keys.insert(conflict.row_key);
                    }
                }
                ConflictResolution::Fail
                | ConflictResolution::Abort
                | ConflictResolution::Rollback => {
                    stopped = conflicts
                        .first()
                        .map(|conflict| conflict.error(table, new_row));
                    break;
                }
            }
        }
        let old_row = old_rows.get(position);
        if let Some(old_row) = old_row
            && !table.indexes.is_empty()
        {
            indexes::delete_entries(writer, table, old_row, old_key)?;
        }
        if old_key != new_key {
            writer.delete(old_key)?;
        }
        writer.put(new_key, &encode_row(new_row))?;
        indexes::put_entries(writer, table, new_row, new_key)?;
        if let Some(slot) = written.get_mut(position) {
            *slot = true;
        }
        if referenced && let Some(old_row) = old_row {
            propagated.push(RowChange {
                table: Arc::clone(table),
                old: old_row.clone(),
                new: Some(new_row.clone()),
            });
        }
    }
    if !propagated.is_empty() {
        constraints::propagate(writer, tables, propagated)?;
    }
    if !table.foreign_keys.is_empty() {
        for (((_, _, new_row), old_row), _) in changes
            .iter()
            .zip(old_rows)
            .zip(&written)
            .filter(|(_, written)| **written)
        {
            constraints::check_parents(&*writer, tables, table, new_row, Some(old_row))?;
        }
    }
    Ok(ResolvedWrites { written, stopped })
}
