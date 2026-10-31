mod bcp;

pub use bcp::{BcpCharacterReader, BcpTerminators};

use crate::ast::Name;
use crate::constraints::PreparedChecks;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{
    ChildRows, ColumnDefaults, StoredRows, TableCache, assignment_rule, target_columns,
};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};
use crate::types::Value;

const FOREIGN_KEY_BATCH_ROWS: usize = 4096;
const WRONG_COLUMN_COUNT: u16 = 4815;

/// The rows of a bulk load: what SQL Server's `bcp in` and `SqlBulkCopy` send after an
/// `INSERT BULK` statement, read by [`crate::Session::execute_bulk_load`]. A TDS server reads
/// them from the bulk load message that follows the statement; [`BcpCharacterReader`] reads
/// them from a file in bcp's character format.
pub trait BulkRows {
    /// Reads the next row into `row`, replacing what it held: `columns` values, one for each
    /// column the `INSERT BULK` named (every column of the table when it named none), in that
    /// order, NULL for a missing value. A value of another type converts to its column's type
    /// as SQL Server's `INSERT` converts it, so text read from a file loads into any column.
    /// `false`, with `row` left empty, once the rows have ended. An error ends the load, which
    /// then loads nothing.
    fn next_row(&mut self, columns: usize, row: &mut Vec<Value>) -> SqlResult<bool>;
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InsertBulk {
    pub(crate) table: Name,
    pub(crate) columns: Vec<Name>,
    pub(crate) keep_nulls: bool,
}

fn targets_of(table: &TableDefinition, columns: &[Name]) -> SqlResult<Vec<usize>> {
    if columns.is_empty() {
        Ok((0..table.columns.len()).collect())
    } else {
        target_columns(table, columns)
    }
}

pub(crate) fn check_target(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    bulk: &InsertBulk,
) -> SqlResult<QueryResult> {
    let table = tables.require_target(reader, &bulk.table, "insert into")?;
    targets_of(&table, &bulk.columns)?;
    Ok(QueryResult::command("INSERT BULK"))
}

fn wrong_column_count(found: usize, expected: usize) -> SqlError {
    SqlError::new(
        SqlState::BAD_COPY_FILE_FORMAT,
        format!(
            "The bulk load data has {found} columns in a row, but the INSERT BULK statement takes {expected}."
        ),
    )
    .with_sqlserver_code(WRONG_COLUMN_COUNT)
}

fn with_row(error: SqlError, row: u64) -> SqlError {
    if error.context().is_some() {
        error
    } else {
        error.with_context(format!("bulk load, row {row}"))
    }
}

pub(crate) fn load(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    bulk: &InsertBulk,
    rows: &mut dyn BulkRows,
) -> SqlResult<QueryResult> {
    let table = tables.require_target(writer, &bulk.table, "insert into")?;
    let targets = targets_of(&table, &bulk.columns)?;
    if crate::sequences::mentions_nextval(&table) {
        crate::sequences::load_all(writer)?;
    }
    let mut slots: Vec<Option<usize>> = vec![None; table.columns.len()];
    for (slot, &index) in targets.iter().enumerate() {
        if let Some(entry) = slots.get_mut(index) {
            *entry = Some(slot);
        }
    }
    let defaults = ColumnDefaults::of(&table)?;
    let checks = PreparedChecks::of(&table)?.with_partitions(writer, tables, &table)?;
    let mut stored = StoredRows::checked(&table, &checks);
    stored.set_identity_insert(
        !bulk.columns.is_empty()
            && table
                .auto_increment_column()
                .is_some_and(|identity| targets.contains(&identity)),
    );
    let mut children = ChildRows::new(&table);
    let deferred_parents = table
        .foreign_keys
        .iter()
        .any(|key| key.parent == table.name);
    let assign = assignment_rule(crate::constraints::current_dialect());
    let mut given: Vec<Value> = Vec::with_capacity(targets.len());
    let mut row: Vec<Value> = Vec::with_capacity(table.columns.len());
    let mut count = 0u64;
    while rows.next_row(targets.len(), &mut given)? {
        let number = count + 1;
        if given.len() != targets.len() {
            return Err(with_row(
                wrong_column_count(given.len(), targets.len()),
                number,
            ));
        }
        row.clear();
        for (index, column) in table.columns.iter().enumerate() {
            let value = match slots.get(index).copied().flatten() {
                None => defaults.value(index, column),
                Some(slot) => {
                    match given
                        .get_mut(slot)
                        .map_or(Value::Null, |value| std::mem::replace(value, Value::Null))
                    {
                        Value::Null if bulk.keep_nulls => Ok(Value::Null),
                        Value::Null => defaults.value(index, column),
                        value => assign(value, column),
                    }
                }
            }
            .map_err(|error| with_row(error, number))?;
            row.push(value);
        }
        stored
            .insert(writer, &mut row)
            .map_err(|error| with_row(error, number))?;
        children.push(&row).map_err(|error| with_row(error, number))?;
        count = number;
        if !deferred_parents && children.len() >= FOREIGN_KEY_BATCH_ROWS {
            children.check_and_forget(writer, tables)?;
        }
    }
    children.check(writer, tables)?;
    Ok(QueryResult::command(format!("INSERT 0 {count}")))
}
