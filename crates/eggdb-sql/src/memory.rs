use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const WORK_MEMORY_LIMIT: usize = 1 << 30;

const ROW_OVERHEAD: usize = 24;
const VALUE_SIZE: usize = std::mem::size_of::<Value>();

pub(crate) struct WorkMemory {
    used: usize,
}

impl WorkMemory {
    pub(crate) fn new() -> Self {
        Self { used: 0 }
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> SqlResult<()> {
        self.used = self.used.saturating_add(bytes);
        if self.used > WORK_MEMORY_LIMIT {
            return Err(SqlError::new(SqlState::OUT_OF_MEMORY, "out of memory")
                .with_detail(format!(
                    "The statement needs more than {} MiB to hold its joined tables, groups, distinct rows, sorted rows and result.",
                    WORK_MEMORY_LIMIT >> 20
                ))
                .with_hint(
                    "Narrow the statement with WHERE or LIMIT; spilling to disk is not supported yet.",
                ));
        }
        Ok(())
    }

    pub(crate) fn charge_row(&mut self, row: &[Value]) -> SqlResult<()> {
        let text: usize = row
            .iter()
            .map(|value| match value {
                Value::Text(text) => text.len(),
                _ => 0,
            })
            .sum();
        self.charge(ROW_OVERHEAD + row.len() * VALUE_SIZE + text)
    }
}
