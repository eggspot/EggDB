use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory::WorkMemory;
use crate::session::OutputColumn;
use crate::set_returning::GeneratedRows;
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct TableVariable {
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) rows: Vec<Vec<Value>>,
}

pub(crate) type TableVariables = Arc<HashMap<String, TableVariable>>;

thread_local! {
    static VARIABLES: RefCell<Vec<TableVariables>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct EnteredVariables {
    depth: usize,
}

impl Drop for EnteredVariables {
    fn drop(&mut self) {
        VARIABLES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter(variables: &TableVariables) -> EnteredVariables {
    let depth = VARIABLES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let depth = frames.len();
            frames.push(Arc::clone(variables));
            depth
        }
        Err(_) => usize::MAX,
    });
    EnteredVariables { depth }
}

pub(crate) fn current() -> TableVariables {
    VARIABLES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().cloned())
            .unwrap_or_default()
    })
}

pub(crate) fn release_entered() {
    VARIABLES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(top) = frames.last_mut()
        {
            *top = TableVariables::default();
        }
    });
}

pub(crate) fn undeclared(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("Must declare the table variable \"{name}\"."),
    )
    .with_sqlserver_code(1087)
}

pub(crate) fn generated_rows(name: &str, columns_only: bool) -> SqlResult<GeneratedRows> {
    let found = VARIABLES.with(|frames| {
        frames.try_borrow().ok().and_then(|frames| {
            frames
                .last()
                .and_then(|variables| variables.get(name).cloned())
        })
    });
    let Some(variable) = found else {
        return Err(undeclared(name));
    };
    if columns_only {
        return Ok(GeneratedRows::empty(variable.columns));
    }
    let mut charge = WorkMemory::new();
    for row in &variable.rows {
        charge.charge_row(row)?;
    }
    Ok(GeneratedRows {
        columns: variable.columns,
        rows: variable.rows,
        charge,
    })
}
