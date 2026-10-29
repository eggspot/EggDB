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
    static VALUE_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    static ROW_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

pub(crate) fn held_bytes(value: &Value) -> usize {
    match value {
        Value::Text(text) | Value::Json(text) | Value::Jsonb(text) => text.len(),
        Value::Bytea(bytes) => bytes.len(),
        Value::Array(array) => crate::arrays::held_bytes(array),
        Value::Reg(reg) => reg.name.len(),
        _ => 0,
    }
}

pub(crate) struct EnteredRowLimit {
    previous: usize,
}

impl Drop for EnteredRowLimit {
    fn drop(&mut self) {
        ROW_LIMIT.with(|limit| limit.set(self.previous));
    }
}

pub(crate) fn enter_row_limit(bytes: usize) -> EnteredRowLimit {
    EnteredRowLimit {
        previous: ROW_LIMIT.with(|limit| limit.replace(bytes)),
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RowBytes {
    total: usize,
}

impl RowBytes {
    pub(crate) fn add(&mut self, value: &Value) -> SqlResult<()> {
        self.add_bytes(VALUE_SIZE.saturating_add(held_bytes(value)))
    }

    pub(crate) fn add_bytes(&mut self, bytes: usize) -> SqlResult<()> {
        self.total = self.total.saturating_add(bytes);
        let limit = ROW_LIMIT.with(std::cell::Cell::get);
        if self.total <= limit {
            return Ok(());
        }
        Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "a row of more than {limit} bytes is larger than this session's limit on one row"
            ),
        )
        .with_hint("Select fewer or smaller columns: the session that runs this statement limits the size of one row."))
    }
}

pub(crate) struct EnteredValueLimit {
    previous: usize,
}

impl Drop for EnteredValueLimit {
    fn drop(&mut self) {
        VALUE_LIMIT.with(|limit| limit.set(self.previous));
    }
}

pub(crate) fn enter_value_limit(bytes: usize) -> EnteredValueLimit {
    EnteredValueLimit {
        previous: VALUE_LIMIT.with(|limit| limit.replace(bytes)),
    }
}

pub(crate) fn check_value_bytes(bytes: usize) -> SqlResult<()> {
    let limit = VALUE_LIMIT.with(std::cell::Cell::get);
    if bytes <= limit {
        return Ok(());
    }
    Err(SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a value of {bytes} bytes is larger than this session's limit of {limit} bytes"),
    )
    .with_hint(
        "Build smaller values: the session that runs this statement limits the size of one value.",
    ))
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
        let held: usize = row.iter().map(held_bytes).sum();
        ROW_OVERHEAD + row.len() * VALUE_SIZE + held
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

    #[test]
    fn the_value_limit_holds_while_entered_and_is_restored_after() {
        use super::{check_value_bytes, enter_value_limit};
        assert!(check_value_bytes(1 << 40).is_ok());
        {
            let _outer = enter_value_limit(100);
            assert!(check_value_bytes(100).is_ok());
            assert_eq!(check_value_bytes(101).unwrap_err().state().code(), "54000");
            {
                let _inner = enter_value_limit(10);
                assert!(check_value_bytes(11).is_err());
            }
            assert!(check_value_bytes(100).is_ok());
        }
        assert!(check_value_bytes(1 << 40).is_ok());
    }

    #[test]
    fn a_row_stops_growing_at_the_row_limit_and_binary_values_count() {
        use super::{RowBytes, WorkMemory, enter_row_limit};
        let big = Value::Bytea(vec![0; 1000]);
        assert!(WorkMemory::row_bytes(std::slice::from_ref(&big)) > 1000);
        let _limit = enter_row_limit(2500);
        let mut row = RowBytes::default();
        assert!(row.add(&big).is_ok());
        assert!(row.add(&big).is_ok());
        assert_eq!(row.add(&big).unwrap_err().state().code(), "54000");
    }
}
