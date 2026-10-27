use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const WORK_MEMORY_LIMIT: usize = 1 << 30;

const ROW_OVERHEAD: usize = 24;
const VALUE_SIZE: usize = std::mem::size_of::<Value>();

#[derive(Clone, Copy)]
struct Shared {
    statement: u64,
    used: usize,
}

thread_local! {
    static SHARED: std::cell::Cell<Option<Shared>> = const { std::cell::Cell::new(None) };
    static STATEMENTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
            return false;
        }
        let statement = STATEMENTS.with(|statements| {
            let next = statements.get().wrapping_add(1);
            statements.set(next);
            next
        });
        shared.set(Some(Shared { statement, used: 0 }));
        true
    });
    SharedStatementMemory { outermost }
}

fn current_statement() -> Option<u64> {
    SHARED.with(|shared| shared.get().map(|shared| shared.statement))
}

fn shared_add(statement: u64, bytes: usize) -> Option<usize> {
    SHARED.with(|shared| {
        let current = shared
            .get()
            .filter(|current| current.statement == statement)?;
        let used = current.used.saturating_add(bytes);
        shared.set(Some(Shared { statement, used }));
        Some(used)
    })
}

fn shared_subtract(statement: u64, bytes: usize) {
    SHARED.with(|shared| {
        if let Some(current) = shared
            .get()
            .filter(|current| current.statement == statement)
        {
            shared.set(Some(Shared {
                statement,
                used: current.used.saturating_sub(bytes),
            }));
        }
    });
}

pub(crate) struct HeldMemory(WorkMemory);

impl HeldMemory {
    pub(crate) fn holding(bytes: usize) -> SqlResult<Self> {
        let mut memory = WorkMemory::new();
        memory.charge(bytes)?;
        Ok(Self(memory))
    }
}

impl std::fmt::Debug for HeldMemory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "HeldMemory({})", self.0.used)
    }
}

pub(crate) struct WorkMemory {
    used: usize,
    limit: usize,
    statement: Option<u64>,
}

impl Drop for WorkMemory {
    fn drop(&mut self) {
        if let Some(statement) = self.statement {
            shared_subtract(statement, self.used);
        }
    }
}

impl WorkMemory {
    pub(crate) fn new() -> Self {
        Self {
            used: 0,
            limit: WORK_MEMORY_LIMIT,
            statement: current_statement(),
        }
    }

    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            used: 0,
            limit,
            statement: None,
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> SqlResult<()> {
        self.used = self.used.saturating_add(bytes);
        let total = self
            .statement
            .and_then(|statement| shared_add(statement, bytes))
            .unwrap_or(self.used);
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
        if let Some(statement) = self.statement {
            shared_subtract(statement, released);
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
    fn memory_charged_in_one_statement_never_counts_against_another() {
        let first = super::share_statement_memory();
        let mut held = WorkMemory::new();
        held.charge(1 << 20).unwrap();
        drop(first);
        let second = super::share_statement_memory();
        let mut fresh = WorkMemory::new();
        fresh.charge(1 << 10).unwrap();
        drop(held);
        assert_eq!(
            super::SHARED.with(|shared| shared.get().map(|now| now.used)),
            Some(1 << 10)
        );
        let mut before = WorkMemory::with_limit(WORK_MEMORY_LIMIT);
        before.charge(10).unwrap();
        drop(fresh);
        drop(second);
        assert!(super::SHARED.with(|shared| shared.get().is_none()));
    }

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
