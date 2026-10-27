use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const WORK_MEMORY_LIMIT: usize = 1 << 30;

const ROW_OVERHEAD: usize = 24;
const VALUE_SIZE: usize = std::mem::size_of::<Value>();

thread_local! {
    static SHARED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

pub(crate) struct SharedStatementMemory {
    outermost: bool,
}

impl Drop for SharedStatementMemory {
    fn drop(&mut self) {
        if self.outermost {
            SHARED.with(|shared| shared.set(None));
        }
    }
}

pub(crate) fn share_statement_memory() -> SharedStatementMemory {
    let outermost = SHARED.with(|shared| {
        if shared.get().is_some() {
            false
        } else {
            shared.set(Some(0));
            true
        }
    });
    SharedStatementMemory { outermost }
}

fn shared_add(bytes: usize) -> Option<usize> {
    SHARED.with(|shared| {
        let total = shared.get()?.saturating_add(bytes);
        shared.set(Some(total));
        Some(total)
    })
}

fn shared_subtract(bytes: usize) {
    SHARED.with(|shared| {
        if let Some(total) = shared.get() {
            shared.set(Some(total.saturating_sub(bytes)));
        }
    });
}

pub(crate) struct WorkMemory {
    used: usize,
    limit: usize,
    shared: bool,
}

impl Drop for WorkMemory {
    fn drop(&mut self) {
        if self.shared {
            shared_subtract(self.used);
        }
    }
}

impl WorkMemory {
    pub(crate) fn new() -> Self {
        Self {
            used: 0,
            limit: WORK_MEMORY_LIMIT,
            shared: SHARED.with(|shared| shared.get().is_some()),
        }
    }

    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            used: 0,
            limit,
            shared: false,
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> SqlResult<()> {
        self.used = self.used.saturating_add(bytes);
        let total = if self.shared {
            shared_add(bytes).unwrap_or(self.used)
        } else {
            self.used
        };
        if total > self.limit {
            return Err(SqlError::new(SqlState::OUT_OF_MEMORY, "out of memory")
                .with_detail(format!(
                    "The statement needs more than {} MiB to hold its joined tables, groups, distinct rows, sorted rows, subquery results and result.",
                    self.limit >> 20
                ))
                .with_hint(
                    "Narrow the statement with WHERE or LIMIT; spilling to disk is not supported yet.",
                ));
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, bytes: usize) {
        let released = bytes.min(self.used);
        self.used -= released;
        if self.shared {
            shared_subtract(released);
        }
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
