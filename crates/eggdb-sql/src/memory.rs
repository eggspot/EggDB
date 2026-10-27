use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const WORK_MEMORY_LIMIT: usize = 1 << 30;

const ROW_OVERHEAD: usize = 24;
const VALUE_SIZE: usize = std::mem::size_of::<Value>();

pub(crate) struct WorkMemory {
    used: usize,
    limit: usize,
}

impl WorkMemory {
    pub(crate) fn new() -> Self {
        Self::with_limit(WORK_MEMORY_LIMIT)
    }

    pub(crate) fn with_limit(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    pub(crate) fn used(&self) -> usize {
        self.used
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> SqlResult<()> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.limit {
            return Err(SqlError::new(SqlState::OUT_OF_MEMORY, "out of memory")
                .with_detail(format!(
                    "The statement needs more than {} MiB to hold its joined tables, groups, distinct rows, sorted rows and result.",
                    self.limit >> 20
                ))
                .with_hint(
                    "Narrow the statement with WHERE or LIMIT; spilling to disk is not supported yet.",
                ));
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    pub(crate) fn row_bytes(row: &[Value]) -> usize {
        let text: usize = row
            .iter()
            .map(|value| match value {
                Value::Text(text) => text.len(),
                _ => 0,
            })
            .sum();
        ROW_OVERHEAD + row.len() * VALUE_SIZE + text
    }

    pub(crate) fn charge_row(&mut self, row: &[Value]) -> SqlResult<()> {
        self.charge(Self::row_bytes(row))
    }
}

#[cfg(test)]
mod tests {
    use super::{WORK_MEMORY_LIMIT, WorkMemory};
    use crate::types::Value;

    #[test]
    fn a_statement_past_the_limit_fails_with_out_of_memory() {
        let mut memory = WorkMemory::new();
        assert!(memory.charge_row(&[Value::Text("x".repeat(100))]).is_ok());
        let error = memory.charge(WORK_MEMORY_LIMIT).unwrap_err();
        assert_eq!(error.state().code(), "53200");
        assert!(error.hint().is_some());
        assert!(memory.charge(usize::MAX).is_err());
    }
}
