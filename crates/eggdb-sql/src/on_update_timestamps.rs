use crate::datetime;
use crate::encoding::TableDefinition;
use crate::error::SqlResult;
use crate::types::{Conversion, SqlType, Value, convert};

fn stampable(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Timestamp(_) | SqlType::TimestampTz(_))
}

impl TableDefinition {
    pub(crate) fn stamped_columns(&self) -> Vec<usize> {
        let mut stamped: Vec<usize> = self
            .on_update_current_timestamp
            .iter()
            .copied()
            .filter(|&column| {
                self.columns
                    .get(column)
                    .is_some_and(|found| stampable(found.sql_type))
            })
            .collect();
        stamped.sort_unstable();
        stamped.dedup();
        stamped
    }

    pub(crate) fn set_on_update_current_timestamp(&mut self, column: usize, stamped: bool) {
        self.on_update_current_timestamp.retain(|&found| found != column);
        if stamped {
            self.on_update_current_timestamp.push(column);
            self.on_update_current_timestamp.sort_unstable();
        }
    }

    pub(crate) fn on_update_without_column(&self, removed: usize) -> Vec<usize> {
        self.on_update_current_timestamp
            .iter()
            .filter(|&&column| column != removed)
            .map(|&column| if column > removed { column - 1 } else { column })
            .collect()
    }

    pub(crate) fn stamps_on_update(&self, column: usize) -> bool {
        self.on_update_current_timestamp.contains(&column)
    }
}

fn current_value(sql_type: SqlType) -> SqlResult<Value> {
    let now = datetime::statement_now();
    let value = match sql_type {
        SqlType::TimestampTz(_) => Value::TimestampTz(now),
        _ => Value::Timestamp(datetime::utc_to_local(now)?),
    };
    convert(value, sql_type, Conversion::Assignment)
}

/// MySQL's `ON UPDATE CURRENT_TIMESTAMP`: when an `UPDATE` changes any other column of the row
/// and doesn't assign the stamped column itself, the column takes the statement's time.
pub(crate) fn stamp(
    table: &TableDefinition,
    old_row: &[Value],
    new_row: &mut [Value],
    assigned: &[usize],
) -> SqlResult<()> {
    if table.on_update_current_timestamp.is_empty() {
        return Ok(());
    }
    let stamped = table.stamped_columns();
    let changed = old_row
        .iter()
        .zip(new_row.iter())
        .enumerate()
        .any(|(column, (old, new))| !stamped.contains(&column) && old != new);
    if !changed {
        return Ok(());
    }
    for column in stamped {
        if assigned.contains(&column) {
            continue;
        }
        let (Some(definition), Some(slot)) = (table.columns.get(column), new_row.get_mut(column))
        else {
            continue;
        };
        *slot = current_value(definition.sql_type)?;
    }
    Ok(())
}
